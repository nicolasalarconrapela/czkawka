//! Storage-specific calibration for the exact duplicate verifier.
//!
//! This module is deliberately independent from duplicate scanning. A caller can
//! calibrate a writable directory once, persist the resulting profile, and later
//! feed only the selected worker count into the exact verifier. Normal scans do
//! not benchmark or auto-tune anything.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::types::{DuplicateFile, DuplicateGroup, DuplicateScanResult};
use super::verify::ExactVerifierExecutor;

// Version 2 changes the benchmark methodology: five base rounds, finalist
// rechecks and stability metadata. Old profiles are intentionally invalidated.
const TUNING_FORMAT_VERSION: u64 = 2;
const DEFAULT_FILE_SIZE: u64 = 4 * 1024 * 1024;
const DEFAULT_ROUNDS: usize = 5;
const DEFAULT_RECHECK_ROUNDS: usize = 4;
const DEFAULT_MAX_WORKERS: usize = 16;
const DEFAULT_TIE_MARGIN: f64 = 0.05;
const DEFAULT_RECHECK_MARGIN: f64 = 0.10;
const MIN_GROUPS: usize = 8;
const HIGH_STABILITY_RELATIVE_MAD: f64 = 0.04;
const LOW_STABILITY_RELATIVE_MAD: f64 = 0.08;

/// Controls the one-off calibration workload.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CalibrationOptions {
    /// Size of each temporary file. Every group contains two identical files.
    pub file_size: u64,
    /// Number of timed repetitions for every candidate worker count.
    pub rounds: usize,
    /// Extra repetitions for candidates close enough to the initial winner.
    pub recheck_rounds: usize,
    /// Safety ceiling for the number of verifier workers tested.
    pub max_workers: usize,
    /// If several candidates are within this fraction of the fastest median,
    /// prefer the one using fewer workers.
    pub tie_margin: f64,
    /// Candidates within this fraction of the fastest initial median are
    /// re-measured before the final selection.
    pub recheck_margin: f64,
}

impl Default for CalibrationOptions {
    fn default() -> Self {
        Self {
            file_size: DEFAULT_FILE_SIZE,
            rounds: DEFAULT_ROUNDS,
            recheck_rounds: DEFAULT_RECHECK_ROUNDS,
            max_workers: DEFAULT_MAX_WORKERS,
            tie_margin: DEFAULT_TIE_MARGIN,
            recheck_margin: DEFAULT_RECHECK_MARGIN,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TuningMeasurement {
    pub workers: usize,
    pub samples: usize,
    pub median: Duration,
    pub median_mib_per_second: f64,
    /// Median absolute deviation divided by the median. Lower is more stable.
    pub relative_mad: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CalibrationStability {
    High,
    Medium,
    Low,
}

impl CalibrationStability {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }

    pub(crate) fn label_es(self) -> &'static str {
        match self {
            Self::High => "alta",
            Self::Medium => "media",
            Self::Low => "baja",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VerifierTuningProfile {
    /// Stable storage identifier for the calibrated directory.
    pub storage_key: String,
    /// Informational path used for the calibration.
    pub calibrated_path: PathBuf,
    /// Worker count selected by the calibration.
    pub workers: usize,
    /// Logical CPU count observed while calibrating.
    pub logical_cpus: usize,
    /// Throughput of the selected configuration on the synthetic exact-verify workload.
    pub median_mib_per_second: f64,
    /// Number of timed samples that contributed to the selected result.
    pub selected_samples: usize,
    /// Robust relative dispersion of the selected candidate.
    pub relative_mad: f64,
    /// How much slower the selected candidate is than the absolute fastest
    /// candidate. This can be non-zero because the tie policy prefers fewer workers.
    pub selected_slowdown_vs_fastest: f64,
    pub stability: CalibrationStability,
    pub calibrated_at_unix_seconds: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct CalibrationReport {
    pub profile: VerifierTuningProfile,
    pub measurements: Vec<TuningMeasurement>,
    pub groups: usize,
    pub bytes_per_run: u64,
    pub initial_rounds: usize,
    pub recheck_rounds: usize,
    pub initial_selected_workers: usize,
    pub rechecked_workers: Vec<usize>,
}

/// Small persistent store. Krokiet can keep this alongside its normal settings.
/// Profiles are keyed by storage rather than by PC, so one machine can have
/// different settings for NVMe, SATA, HDD and network shares.
#[derive(Clone, Debug, Default)]
pub(crate) struct VerifierTuningStore {
    profiles: Vec<VerifierTuningProfile>,
}

impl VerifierTuningStore {
    pub(crate) fn profiles(&self) -> &[VerifierTuningProfile] {
        &self.profiles
    }

    pub(crate) fn upsert(&mut self, profile: VerifierTuningProfile) {
        if let Some(existing) = self
            .profiles
            .iter_mut()
            .find(|existing| existing.storage_key == profile.storage_key)
        {
            *existing = profile;
        } else {
            self.profiles.push(profile);
            self.profiles
                .sort_by(|left, right| left.storage_key.cmp(&right.storage_key));
        }
    }

    /// Returns the worker budget for all requested paths.
    ///
    /// If every path has a profile, the most conservative worker count is used
    /// across storages. If at least one storage is unknown, `None` is returned so
    /// the caller can keep the verifier sequential or ask for calibration.
    pub(crate) fn workers_for_paths(&self, paths: &[PathBuf]) -> io::Result<Option<usize>> {
        if paths.is_empty() {
            return Ok(None);
        }

        let mut selected: Option<usize> = None;
        for path in paths {
            let key = storage_key_for_path(path)?;
            let Some(profile) = self
                .profiles
                .iter()
                .find(|profile| profile.storage_key == key)
            else {
                return Ok(None);
            };

            selected = Some(match selected {
                Some(current) => current.min(profile.workers),
                None => profile.workers,
            });
        }

        Ok(selected)
    }

    pub(crate) fn load(path: &Path) -> io::Result<Self> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };

        let value: Value = serde_json::from_reader(file)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

        if value.get("version").and_then(Value::as_u64) != Some(TUNING_FORMAT_VERSION) {
            // Unknown/old versions are ignored rather than risking a stale tuning
            // decision after the benchmark methodology changes.
            return Ok(Self::default());
        }

        let Some(items) = value.get("profiles").and_then(Value::as_array) else {
            return Ok(Self::default());
        };

        let mut store = Self::default();
        for item in items {
            let Some(storage_key) = item.get("storage_key").and_then(Value::as_str) else {
                continue;
            };
            let Some(calibrated_path) = item.get("calibrated_path").and_then(Value::as_str) else {
                continue;
            };
            let Some(workers) = item.get("workers").and_then(Value::as_u64) else {
                continue;
            };
            let Some(logical_cpus) = item.get("logical_cpus").and_then(Value::as_u64) else {
                continue;
            };
            let Some(selected_samples) = item.get("selected_samples").and_then(Value::as_u64) else {
                continue;
            };
            let Some(stability) = item
                .get("stability")
                .and_then(Value::as_str)
                .and_then(CalibrationStability::from_str)
            else {
                continue;
            };

            let median_mib_per_second = item
                .get("median_mib_per_second")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let relative_mad = item
                .get("relative_mad")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let selected_slowdown_vs_fastest = item
                .get("selected_slowdown_vs_fastest")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let calibrated_at_unix_seconds = item
                .get("calibrated_at_unix_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(0);

            let Ok(workers) = usize::try_from(workers) else {
                continue;
            };
            let Ok(logical_cpus) = usize::try_from(logical_cpus) else {
                continue;
            };
            let Ok(selected_samples) = usize::try_from(selected_samples) else {
                continue;
            };
            if workers == 0 || logical_cpus == 0 || selected_samples == 0 {
                continue;
            }

            store.upsert(VerifierTuningProfile {
                storage_key: storage_key.to_owned(),
                calibrated_path: PathBuf::from(calibrated_path),
                workers,
                logical_cpus,
                median_mib_per_second,
                selected_samples,
                relative_mad,
                selected_slowdown_vs_fastest,
                stability,
                calibrated_at_unix_seconds,
            });
        }

        Ok(store)
    }

    pub(crate) fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        let profiles = self
            .profiles
            .iter()
            .map(|profile| {
                json!({
                    "storage_key": profile.storage_key,
                    "calibrated_path": profile.calibrated_path.to_string_lossy(),
                    "workers": profile.workers,
                    "logical_cpus": profile.logical_cpus,
                    "median_mib_per_second": profile.median_mib_per_second,
                    "selected_samples": profile.selected_samples,
                    "relative_mad": profile.relative_mad,
                    "selected_slowdown_vs_fastest": profile.selected_slowdown_vs_fastest,
                    "stability": profile.stability.as_str(),
                    "calibrated_at_unix_seconds": profile.calibrated_at_unix_seconds,
                })
            })
            .collect::<Vec<_>>();

        let document = json!({
            "version": TUNING_FORMAT_VERSION,
            "profiles": profiles,
        });

        let temporary = path.with_extension("tmp");
        {
            let file = File::create(&temporary)?;
            let mut writer = BufWriter::new(file);
            serde_json::to_writer_pretty(&mut writer, &document).map_err(io::Error::other)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
        }

        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(temporary, path)
    }
}

/// Performs a one-off calibration on `directory`.
///
/// The function creates a temporary exact-verification dataset on that storage,
/// benchmarks candidate worker counts, rechecks the candidates close to the
/// initial winner, chooses the least-concurrent option within `tie_margin` of
/// the fastest final median, and removes the temporary files.
///
/// It never runs fclones and is completely independent from duplicate scanning.
pub(crate) fn calibrate_and_store(
    directory: &Path,
    store_path: &Path,
    options: CalibrationOptions,
) -> io::Result<CalibrationReport> {
    let report = calibrate_directory(directory, options)?;
    let mut store = VerifierTuningStore::load(store_path)?;
    store.upsert(report.profile.clone());
    store.save(store_path)?;
    Ok(report)
}

pub(crate) fn calibrate_directory(
    directory: &Path,
    options: CalibrationOptions,
) -> io::Result<CalibrationReport> {
    validate_options(options)?;

    let calibrated_path = fs::canonicalize(directory)?;
    if !fs::metadata(&calibrated_path)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "calibration path is not a directory: {}",
                calibrated_path.display()
            ),
        ));
    }

    let logical_cpus = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    let candidates = candidate_worker_counts(logical_cpus, options.max_workers);
    let largest_candidate = candidates.iter().copied().max().unwrap_or(1);
    let group_count = MIN_GROUPS.max(largest_candidate.saturating_mul(2));

    let dataset = CalibrationDataset::create(&calibrated_path, group_count, options.file_size)?;

    let mut executors = HashMap::with_capacity(candidates.len());
    for &workers in &candidates {
        executors.insert(workers, ExactVerifierExecutor::new(workers)?);
    }

    // Warm the same path/metadata/page-cache path the real post-fclones verifier
    // normally sees. The warm-up is not timed.
    let mut warm = dataset.result.clone();
    executors
        .get(&1)
        .expect("candidate list always contains one worker")
        .verify_result(&mut warm)?;

    let sample_capacity = options.rounds.saturating_add(options.recheck_rounds);
    let mut samples: HashMap<usize, Vec<Duration>> = candidates
        .iter()
        .copied()
        .map(|workers| (workers, Vec::with_capacity(sample_capacity)))
        .collect();

    run_calibration_rounds(
        &candidates,
        options.rounds,
        0,
        &executors,
        &dataset,
        &mut samples,
    )?;

    let initial_measurements = build_measurements(&candidates, &samples, dataset.bytes_per_run);
    let initial_selected = select_measurement(&initial_measurements, options.tie_margin);
    let initial_selected_workers = initial_selected.workers;

    let rechecked_workers = if options.recheck_rounds == 0 {
        Vec::new()
    } else {
        recheck_candidates(&initial_measurements, options.recheck_margin)
    };

    if !rechecked_workers.is_empty() {
        run_calibration_rounds(
            &rechecked_workers,
            options.recheck_rounds,
            options.rounds,
            &executors,
            &dataset,
            &mut samples,
        )?;
    }

    let measurements = build_measurements(&candidates, &samples, dataset.bytes_per_run);
    let selected = select_measurement(&measurements, options.tie_margin);
    let fastest = fastest_measurement(&measurements);
    let selected_slowdown_vs_fastest = slowdown_ratio(selected.median, fastest.median);
    let stability = classify_stability(
        initial_selected_workers,
        selected.workers,
        selected.relative_mad,
    );

    let storage_key = storage_key_for_path(&calibrated_path)?;
    let calibrated_at_unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    Ok(CalibrationReport {
        profile: VerifierTuningProfile {
            storage_key,
            calibrated_path,
            workers: selected.workers,
            logical_cpus,
            median_mib_per_second: selected.median_mib_per_second,
            selected_samples: selected.samples,
            relative_mad: selected.relative_mad,
            selected_slowdown_vs_fastest,
            stability,
            calibrated_at_unix_seconds,
        },
        measurements,
        groups: dataset.group_count,
        bytes_per_run: dataset.bytes_per_run,
        initial_rounds: options.rounds,
        recheck_rounds: options.recheck_rounds,
        initial_selected_workers,
        rechecked_workers,
    })
}

fn run_calibration_rounds(
    candidates: &[usize],
    rounds: usize,
    round_offset: usize,
    executors: &HashMap<usize, ExactVerifierExecutor>,
    dataset: &CalibrationDataset,
    samples: &mut HashMap<usize, Vec<Duration>>,
) -> io::Result<()> {
    if candidates.is_empty() || rounds == 0 {
        return Ok(());
    }

    for local_round in 0..rounds {
        let global_round = round_offset.saturating_add(local_round);
        let mut order = candidates.to_vec();
        let order_len = order.len();
        order.rotate_left(global_round % order_len);
        if global_round % 2 == 1 {
            order.reverse();
        }

        for workers in order {
            let mut result = dataset.result.clone();
            let started = Instant::now();
            let verified = executors
                .get(&workers)
                .expect("executor exists for every candidate")
                .verify_result(&mut result)?;
            let elapsed = started.elapsed();

            if verified != dataset.group_count || result.groups.len() != dataset.group_count {
                return Err(io::Error::other(format!(
                    "calibration verifier returned {verified}/{} groups with {workers} worker(s)",
                    dataset.group_count
                )));
            }

            samples
                .get_mut(&workers)
                .expect("sample bucket exists")
                .push(elapsed);
        }
    }

    Ok(())
}

fn build_measurements(
    candidates: &[usize],
    samples: &HashMap<usize, Vec<Duration>>,
    bytes_per_run: u64,
) -> Vec<TuningMeasurement> {
    let mut measurements = Vec::with_capacity(candidates.len());

    for &workers in candidates {
        let values = samples
            .get(&workers)
            .expect("sample bucket exists for every candidate");
        let median = median_duration(values);
        let seconds = median.as_secs_f64().max(f64::MIN_POSITIVE);
        let median_mib_per_second = bytes_per_run as f64 / (1024.0 * 1024.0) / seconds;
        measurements.push(TuningMeasurement {
            workers,
            samples: values.len(),
            median,
            median_mib_per_second,
            relative_mad: relative_mad(values, median),
        });
    }

    measurements.sort_by_key(|measurement| measurement.workers);
    measurements
}

fn select_measurement(
    measurements: &[TuningMeasurement],
    tie_margin: f64,
) -> &TuningMeasurement {
    let fastest_seconds = fastest_measurement(measurements).median.as_secs_f64();
    let acceptable_seconds = fastest_seconds * (1.0 + tie_margin);

    measurements
        .iter()
        .filter(|measurement| measurement.median.as_secs_f64() <= acceptable_seconds)
        .min_by_key(|measurement| measurement.workers)
        .expect("at least one tuning measurement")
}

fn fastest_measurement(measurements: &[TuningMeasurement]) -> &TuningMeasurement {
    measurements
        .iter()
        .min_by(|left, right| left.median.cmp(&right.median))
        .expect("at least one tuning measurement")
}

fn recheck_candidates(measurements: &[TuningMeasurement], recheck_margin: f64) -> Vec<usize> {
    if measurements.len() <= 1 {
        return Vec::new();
    }

    let fastest_seconds = fastest_measurement(measurements).median.as_secs_f64();
    let threshold = fastest_seconds * (1.0 + recheck_margin);

    measurements
        .iter()
        .filter(|measurement| measurement.median.as_secs_f64() <= threshold)
        .map(|measurement| measurement.workers)
        .collect()
}

fn classify_stability(
    initial_selected_workers: usize,
    final_selected_workers: usize,
    selected_relative_mad: f64,
) -> CalibrationStability {
    if selected_relative_mad > LOW_STABILITY_RELATIVE_MAD {
        CalibrationStability::Low
    } else if initial_selected_workers != final_selected_workers
        || selected_relative_mad > HIGH_STABILITY_RELATIVE_MAD
    {
        CalibrationStability::Medium
    } else {
        CalibrationStability::High
    }
}

fn slowdown_ratio(selected: Duration, fastest: Duration) -> f64 {
    let fastest_seconds = fastest.as_secs_f64().max(f64::MIN_POSITIVE);
    (selected.as_secs_f64() / fastest_seconds - 1.0).max(0.0)
}

fn validate_options(options: CalibrationOptions) -> io::Result<()> {
    if options.file_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "calibration file size must be greater than zero",
        ));
    }
    if options.rounds == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "calibration rounds must be greater than zero",
        ));
    }
    if options.max_workers == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "calibration max_workers must be greater than zero",
        ));
    }
    if !options.tie_margin.is_finite() || !(0.0..=0.50).contains(&options.tie_margin) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "calibration tie_margin must be between 0.0 and 0.50",
        ));
    }
    if !options.recheck_margin.is_finite()
        || !(0.0..=0.50).contains(&options.recheck_margin)
        || options.recheck_margin < options.tie_margin
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "calibration recheck_margin must be between tie_margin and 0.50",
        ));
    }
    Ok(())
}

fn candidate_worker_counts(logical_cpus: usize, max_workers: usize) -> Vec<usize> {
    let limit = logical_cpus.max(1).min(max_workers.max(1));
    let mut candidates = BTreeSet::new();
    candidates.insert(1);

    let mut power = 2_usize;
    while power <= limit {
        candidates.insert(power);
        let Some(next) = power.checked_mul(2) else {
            break;
        };
        power = next;
    }

    // Keep the points that have proven useful for the exact verifier while still
    // adapting to the available logical CPU count. In particular, a 16-thread
    // machine now measures 6 workers instead of jumping directly from 4 to 8.
    if (3..=4).contains(&limit) {
        candidates.insert(3);
    }
    if limit >= 6 {
        candidates.insert(6);
    }
    if limit >= 12 {
        candidates.insert(12);
    }
    candidates.insert(limit);

    candidates
        .into_iter()
        .filter(|workers| *workers <= limit)
        .collect()
}

fn median_duration(values: &[Duration]) -> Duration {
    assert!(!values.is_empty(), "median requires at least one sample");
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        Duration::from_secs_f64(
            (sorted[middle - 1].as_secs_f64() + sorted[middle].as_secs_f64()) / 2.0,
        )
    }
}

fn median_f64(values: &mut [f64]) -> f64 {
    assert!(!values.is_empty(), "median requires at least one sample");
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    }
}

fn relative_mad(values: &[Duration], median: Duration) -> f64 {
    let median_seconds = median.as_secs_f64().max(f64::MIN_POSITIVE);
    let mut deviations = values
        .iter()
        .map(|value| (value.as_secs_f64() - median_seconds).abs())
        .collect::<Vec<_>>();
    median_f64(&mut deviations) / median_seconds
}

struct CalibrationDataset {
    root: PathBuf,
    result: DuplicateScanResult,
    group_count: usize,
    bytes_per_run: u64,
}

impl CalibrationDataset {
    fn create(parent: &Path, group_count: usize, file_size: u64) -> io::Result<Self> {
        let root = create_unique_calibration_dir(parent)?;
        let creation_result = Self::populate(root.clone(), group_count, file_size);
        if creation_result.is_err() {
            let _ = fs::remove_dir_all(&root);
        }
        creation_result
    }

    fn populate(root: PathBuf, group_count: usize, file_size: u64) -> io::Result<Self> {
        let mut groups = Vec::with_capacity(group_count);

        for group_index in 0..group_count {
            let left = root.join(format!("group-{group_index:03}-a.bin"));
            let right = root.join(format!("group-{group_index:03}-b.bin"));
            write_identical_pair(
                &left,
                &right,
                file_size,
                0x9E37_79B9_7F4A_7C15_u64 ^ group_index as u64,
            )?;

            groups.push(DuplicateGroup::new(vec![
                DuplicateFile::from_path(left)?,
                DuplicateFile::from_path(right)?,
            ]));
        }

        let bytes_per_run = file_size
            .saturating_mul(group_count as u64)
            .saturating_mul(2);

        Ok(Self {
            root,
            result: DuplicateScanResult {
                engine: "tuning",
                groups,
                elapsed: Duration::ZERO,
            },
            group_count,
            bytes_per_run,
        })
    }
}

impl Drop for CalibrationDataset {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn create_unique_calibration_dir(parent: &Path) -> io::Result<PathBuf> {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    for attempt in 0..32_u32 {
        let candidate = parent.join(format!(
            ".krokiet-verifier-tuning-{pid}-{nanos}-{attempt}"
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to create a unique verifier calibration directory",
    ))
}

fn write_identical_pair(left: &Path, right: &Path, size: u64, seed: u64) -> io::Result<()> {
    const WRITE_BUFFER: usize = 1024 * 1024;

    let mut left_writer = BufWriter::new(File::create(left)?);
    let mut right_writer = BufWriter::new(File::create(right)?);
    let mut buffer = vec![0_u8; WRITE_BUFFER];
    let mut state = seed.max(1);
    let mut remaining = size;

    while remaining > 0 {
        let amount = remaining.min(WRITE_BUFFER as u64) as usize;
        fill_pseudorandom(&mut buffer[..amount], &mut state);
        left_writer.write_all(&buffer[..amount])?;
        right_writer.write_all(&buffer[..amount])?;
        remaining -= amount as u64;
    }

    left_writer.flush()?;
    right_writer.flush()?;
    left_writer.get_ref().sync_all()?;
    right_writer.get_ref().sync_all()?;
    Ok(())
}

fn fill_pseudorandom(buffer: &mut [u8], state: &mut u64) {
    for chunk in buffer.chunks_mut(8) {
        // xorshift64*: deterministic, cheap and sufficiently non-repetitive for
        // a storage benchmark. This is not cryptographic data.
        let mut value = *state;
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
        *state = value;
        let bytes = value.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

#[cfg(windows)]
pub(crate) fn storage_key_for_path(path: &Path) -> io::Result<String> {
    use std::path::{Component, Prefix};

    let canonical = fs::canonicalize(path)?;
    let Some(Component::Prefix(prefix_component)) = canonical.components().next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path has no Windows volume prefix: {}", canonical.display()),
        ));
    };

    let key = match prefix_component.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            format!("windows-disk:{}", (letter as char).to_ascii_uppercase())
        }
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => format!(
            "windows-unc:{}\\{}",
            server.to_string_lossy().to_ascii_lowercase(),
            share.to_string_lossy().to_ascii_lowercase(),
        ),
        Prefix::DeviceNS(device) => {
            format!(
                "windows-device:{}",
                device.to_string_lossy().to_ascii_lowercase()
            )
        }
        Prefix::Verbatim(value) => {
            format!(
                "windows-verbatim:{}",
                value.to_string_lossy().to_ascii_lowercase()
            )
        }
    };

    Ok(key)
}

#[cfg(unix)]
pub(crate) fn storage_key_for_path(path: &Path) -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(path)?;
    Ok(format!("unix-dev:{}", metadata.dev()))
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn storage_key_for_path(path: &Path) -> io::Result<String> {
    let canonical = fs::canonicalize(path)?;
    Ok(format!("path-root:{}", canonical.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measurement(workers: usize, milliseconds: u64, relative_mad: f64) -> TuningMeasurement {
        let median = Duration::from_millis(milliseconds);
        TuningMeasurement {
            workers,
            samples: 5,
            median,
            median_mib_per_second: 1000.0 / median.as_secs_f64(),
            relative_mad,
        }
    }

    #[test]
    fn eight_logical_cpus_include_the_development_sweet_spots() {
        assert_eq!(candidate_worker_counts(8, 16), vec![1, 2, 4, 6, 8]);
    }

    #[test]
    fn worker_candidates_respect_cpu_and_safety_caps() {
        assert_eq!(candidate_worker_counts(1, 16), vec![1]);
        assert_eq!(candidate_worker_counts(4, 16), vec![1, 2, 3, 4]);
        assert_eq!(
            candidate_worker_counts(32, 16),
            vec![1, 2, 4, 6, 8, 12, 16]
        );
    }

    #[test]
    fn tie_policy_prefers_fewer_workers_within_margin() {
        let measurements = vec![
            measurement(4, 100, 0.01),
            measurement(8, 97, 0.01),
            measurement(12, 110, 0.01),
        ];
        assert_eq!(select_measurement(&measurements, 0.05).workers, 4);
    }

    #[test]
    fn recheck_window_keeps_nearby_finalists() {
        let measurements = vec![
            measurement(4, 100, 0.01),
            measurement(8, 96, 0.01),
            measurement(12, 104, 0.01),
            measurement(16, 120, 0.01),
        ];
        assert_eq!(recheck_candidates(&measurements, 0.10), vec![4, 8, 12]);
    }

    #[test]
    fn stability_drops_when_final_selection_changes() {
        assert_eq!(
            classify_stability(4, 8, 0.01),
            CalibrationStability::Medium
        );
        assert_eq!(
            classify_stability(4, 4, 0.09),
            CalibrationStability::Low
        );
        assert_eq!(
            classify_stability(4, 4, 0.02),
            CalibrationStability::High
        );
    }

    #[test]
    fn tuning_store_round_trips_and_matches_storage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join("verifier-tuning.json");
        let storage_key = storage_key_for_path(dir.path()).expect("storage key");

        let mut store = VerifierTuningStore::default();
        store.upsert(VerifierTuningProfile {
            storage_key,
            calibrated_path: dir.path().to_path_buf(),
            workers: 3,
            logical_cpus: 4,
            median_mib_per_second: 1234.5,
            selected_samples: 9,
            relative_mad: 0.02,
            selected_slowdown_vs_fastest: 0.01,
            stability: CalibrationStability::High,
            calibrated_at_unix_seconds: 42,
        });
        store.save(&settings).expect("save");

        let loaded = VerifierTuningStore::load(&settings).expect("load");
        assert_eq!(loaded.profiles().len(), 1);
        assert_eq!(loaded.profiles()[0].workers, 3);
        assert_eq!(loaded.profiles()[0].selected_samples, 9);
        assert_eq!(loaded.profiles()[0].stability, CalibrationStability::High);
        assert_eq!(
            loaded
                .workers_for_paths(&[dir.path().to_path_buf()])
                .expect("match"),
            Some(3)
        );
    }

    #[test]
    fn unknown_storage_returns_no_worker_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = VerifierTuningStore::default();
        assert_eq!(
            store
                .workers_for_paths(&[dir.path().to_path_buf()])
                .expect("lookup"),
            None
        );
    }

    #[cfg(feature = "fast_duplicates")]
    #[test]
    #[ignore = "manual storage calibration; set KROKIET_TUNING_PATH"]
    fn manual_storage_calibration() {
        let path = std::env::var_os("KROKIET_TUNING_PATH")
            .or_else(|| std::env::var_os("KROKIET_DUP_BENCH_PATH"))
            .map(PathBuf::from)
            .expect("set KROKIET_TUNING_PATH or KROKIET_DUP_BENCH_PATH");

        let report = calibrate_directory(&path, CalibrationOptions::default())
            .expect("storage calibration failed");

        println!();
        println!("============================================================");
        println!(" KROKIET - CALIBRACION PREVIA DEL EXACT VERIFIER");
        println!("============================================================");
        println!("Ruta          : {}", report.profile.calibrated_path.display());
        println!("Storage key   : {}", report.profile.storage_key);
        println!("CPU logicos   : {}", report.profile.logical_cpus);
        println!("Grupos        : {}", report.groups);
        println!(
            "Lectura/ronda : {:.1} MiB",
            report.bytes_per_run as f64 / 1024.0 / 1024.0
        );
        println!("Rondas base   : {}", report.initial_rounds);
        if report.rechecked_workers.is_empty() {
            println!("Recheck       : no necesario");
        } else {
            println!(
                "Recheck       : {} ronda(s) extra para {:?}",
                report.recheck_rounds, report.rechecked_workers
            );
        }
        println!();
        for measurement in &report.measurements {
            println!(
                "workers-{:>2}: {:>10.3?} | {:>9.1} MiB/s | n={:<2} | MAD {:>5.1}%",
                measurement.workers,
                measurement.median,
                measurement.median_mib_per_second,
                measurement.samples,
                measurement.relative_mad * 100.0,
            );
        }
        println!("------------------------------------------------------------");
        println!(
            "Seleccion     : {} worker(s)",
            report.profile.workers
        );
        if report.initial_selected_workers != report.profile.workers {
            println!(
                "Seleccion ini : {} worker(s)",
                report.initial_selected_workers
            );
        }
        println!(
            "Rendimiento   : {:.1} MiB/s",
            report.profile.median_mib_per_second
        );
        println!(
            "Margen mejor  : {:.2}%",
            report.profile.selected_slowdown_vs_fastest * 100.0
        );
        println!(
            "Estabilidad   : {} (MAD {:.2}%)",
            report.profile.stability.label_es(),
            report.profile.relative_mad * 100.0
        );
        println!("============================================================");
    }
}
