//! Storage-specific calibration for the exact duplicate verifier.
//!
//! The normal scan does not need write access. Calibration is optional:
//! - reuse a saved profile when available;
//! - calibrate only when a writable place exists on the same storage;
//! - otherwise continue with a conservative fallback.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::types::{DuplicateFile, DuplicateGroup, DuplicateScanResult};
use super::verify::ExactVerifierExecutor;

// Version 6 keeps the adaptive rescue pass and stable Windows volume identity.
// The read-only planning tests below do not change the persisted profile format.
const TUNING_FORMAT_VERSION: u64 = 6;

const DEFAULT_FILE_SIZE: u64 = 4 * 1024 * 1024;
const DEFAULT_ROUNDS: usize = 5;
const DEFAULT_MAX_WORKERS: usize = 16;
const DEFAULT_TIE_MARGIN: f64 = 0.05;
const DEFAULT_RECHECK_ROUNDS: usize = 4;
const DEFAULT_RECHECK_MARGIN: f64 = 0.10;
const DEFAULT_PAIR_ROUNDS: usize = 5;
const DEFAULT_TARGET_SAMPLE_TIME: Duration = Duration::from_millis(250);
const DEFAULT_RESCUE_SAMPLE_TIME: Duration = Duration::from_millis(750);
const DEFAULT_RESCUE_PAIR_ROUNDS: usize = 7;
const MIN_GROUPS: usize = 8;

const INVALID_STABILITY_RELATIVE_MAD: f64 = 0.12;
const LOW_STABILITY_RELATIVE_MAD: f64 = 0.08;
const MEDIUM_STABILITY_RELATIVE_MAD: f64 = 0.04;

/// Safe value used when no saved profile exists and calibration cannot write
/// temporary files on the target storage.
pub(crate) const SAFE_FALLBACK_WORKERS: usize = 1;

#[derive(Clone, Copy, Debug)]
pub(crate) struct CalibrationOptions {
    pub file_size: u64,
    pub rounds: usize,
    pub max_workers: usize,
    pub tie_margin: f64,
    pub recheck_rounds: usize,
    pub recheck_margin: f64,
    pub pair_rounds: usize,
    pub target_sample_time: Duration,
}

impl Default for CalibrationOptions {
    fn default() -> Self {
        Self {
            file_size: DEFAULT_FILE_SIZE,
            rounds: DEFAULT_ROUNDS,
            max_workers: DEFAULT_MAX_WORKERS,
            tie_margin: DEFAULT_TIE_MARGIN,
            recheck_rounds: DEFAULT_RECHECK_ROUNDS,
            recheck_margin: DEFAULT_RECHECK_MARGIN,
            pair_rounds: DEFAULT_PAIR_ROUNDS,
            target_sample_time: DEFAULT_TARGET_SAMPLE_TIME,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CalibrationStability {
    High,
    Medium,
    Low,
    Invalid,
}

impl CalibrationStability {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Invalid => "invalid",
        }
    }

    pub(crate) fn label_es(self) -> &'static str {
        match self {
            Self::High => "alta",
            Self::Medium => "media",
            Self::Low => "baja",
            Self::Invalid => "invalida",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            "invalid" => Some(Self::Invalid),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TuningMeasurement {
    pub workers: usize,
    pub median: Duration,
    pub median_mib_per_second: f64,
    pub samples: usize,
    pub verification_runs: usize,
    pub relative_mad: f64,
}

#[derive(Clone, Debug)]
pub(crate) struct PairwiseConfirmation {
    pub incumbent_workers: usize,
    pub challenger_workers: usize,
    pub median_challenger_to_incumbent_ratio: f64,
    pub decisive_challenger_wins: usize,
    pub rounds: usize,
    pub promoted: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct VerifierTuningProfile {
    pub storage_key: String,
    pub calibrated_path: PathBuf,
    pub workers: usize,
    pub logical_cpus: usize,
    pub median_mib_per_second: f64,
    pub selected_samples: usize,
    pub selected_verification_runs: usize,
    pub relative_mad: f64,
    pub decision_relative_mad: f64,
    pub global_relative_mad: f64,
    pub selected_slowdown_vs_fastest: f64,
    pub stability: CalibrationStability,
    pub calibrated_at_unix_seconds: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct CalibrationReport {
    pub profile: VerifierTuningProfile,
    pub measurements: Vec<TuningMeasurement>,
    pub pairwise: Vec<PairwiseConfirmation>,
    pub groups: usize,
    pub bytes_per_run: u64,
    pub initial_rounds: usize,
    pub recheck_rounds: usize,
    pub pair_rounds: usize,
    pub target_sample_time: Duration,
    pub rescue_attempted: bool,
    pub initial_target_sample_time: Duration,
    pub initial_stability: Option<CalibrationStability>,
    pub initial_selected_workers: usize,
    pub aggregate_selected_workers: usize,
    pub rechecked_workers: Vec<usize>,
    pub paired_workers: Vec<usize>,
}

#[derive(Clone, Debug)]
pub(crate) enum VerifierTuningPlan {
    UseProfile {
        storage_key: String,
        workers: usize,
        stability: CalibrationStability,
    },
    Calibrate {
        storage_key: String,
        directory: PathBuf,
    },
    Fallback {
        storage_key: String,
        workers: usize,
    },
}

impl VerifierTuningPlan {
    pub(crate) fn storage_key(&self) -> &str {
        match self {
            Self::UseProfile { storage_key, .. }
            | Self::Calibrate { storage_key, .. }
            | Self::Fallback { storage_key, .. } => storage_key,
        }
    }

    pub(crate) fn workers(&self) -> Option<usize> {
        match self {
            Self::UseProfile { workers, .. } | Self::Fallback { workers, .. } => Some(*workers),
            Self::Calibrate { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct VerifierTuningStore {
    profiles: Vec<VerifierTuningProfile>,
}

impl VerifierTuningStore {
    pub(crate) fn profiles(&self) -> &[VerifierTuningProfile] {
        &self.profiles
    }

    pub(crate) fn profile_for_storage_key(
        &self,
        storage_key: &str,
    ) -> Option<&VerifierTuningProfile> {
        self.profiles.iter().find(|profile| {
            profile.storage_key == storage_key
                && profile.stability != CalibrationStability::Invalid
        })
    }

    pub(crate) fn profile_for_path(
        &self,
        path: &Path,
    ) -> io::Result<Option<&VerifierTuningProfile>> {
        let storage_key = storage_key_for_path(path)?;
        Ok(self.profile_for_storage_key(&storage_key))
    }

    pub(crate) fn upsert(&mut self, profile: VerifierTuningProfile) {
        if profile.stability == CalibrationStability::Invalid {
            return;
        }

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

    pub(crate) fn workers_for_paths(
        &self,
        paths: &[PathBuf],
    ) -> io::Result<Option<usize>> {
        if paths.is_empty() {
            return Ok(None);
        }

        let mut selected: Option<usize> = None;
        let mut seen = BTreeSet::new();

        for path in paths {
            let key = storage_key_for_path(path)?;
            if !seen.insert(key.clone()) {
                continue;
            }

            let Some(profile) = self.profile_for_storage_key(&key) else {
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
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error),
        };

        let value: Value = serde_json::from_reader(file)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

        if value.get("version").and_then(Value::as_u64) != Some(TUNING_FORMAT_VERSION) {
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
            let Some(stability) = item
                .get("stability")
                .and_then(Value::as_str)
                .and_then(CalibrationStability::from_str)
            else {
                continue;
            };

            if stability == CalibrationStability::Invalid {
                continue;
            }

            store.upsert(VerifierTuningProfile {
                storage_key: storage_key.to_owned(),
                calibrated_path: PathBuf::from(calibrated_path),
                workers: workers as usize,
                logical_cpus: logical_cpus as usize,
                median_mib_per_second: json_f64(item, "median_mib_per_second"),
                selected_samples: json_u64(item, "selected_samples") as usize,
                selected_verification_runs: json_u64(item, "selected_verification_runs")
                    as usize,
                relative_mad: json_f64(item, "relative_mad"),
                decision_relative_mad: json_f64(item, "decision_relative_mad"),
                global_relative_mad: json_f64(item, "global_relative_mad"),
                selected_slowdown_vs_fastest: json_f64(
                    item,
                    "selected_slowdown_vs_fastest",
                ),
                stability,
                calibrated_at_unix_seconds: json_u64(
                    item,
                    "calibrated_at_unix_seconds",
                ),
            });
        }

        Ok(store)
    }

    pub(crate) fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let profiles = self
            .profiles
            .iter()
            .filter(|profile| profile.stability != CalibrationStability::Invalid)
            .map(|profile| {
                json!({
                    "storage_key": profile.storage_key,
                    "calibrated_path": profile.calibrated_path.to_string_lossy(),
                    "workers": profile.workers,
                    "logical_cpus": profile.logical_cpus,
                    "median_mib_per_second": profile.median_mib_per_second,
                    "selected_samples": profile.selected_samples,
                    "selected_verification_runs": profile.selected_verification_runs,
                    "relative_mad": profile.relative_mad,
                    "decision_relative_mad": profile.decision_relative_mad,
                    "global_relative_mad": profile.global_relative_mad,
                    "selected_slowdown_vs_fastest": profile.selected_slowdown_vs_fastest,
                    "stability": profile.stability.as_str(),
                    "calibrated_at_unix_seconds": profile.calibrated_at_unix_seconds,
                })
            })
            .collect::<Vec<_>>();

        let value = json!({
            "version": TUNING_FORMAT_VERSION,
            "profiles": profiles,
        });

        let temp_path = path.with_extension("tmp");
        {
            let file = File::create(&temp_path)?;
            let mut writer = BufWriter::new(file);
            serde_json::to_writer_pretty(&mut writer, &value)
                .map_err(io::Error::other)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }

        fs::rename(temp_path, path)?;
        Ok(())
    }
}

fn json_u64(value: &Value, field: &str) -> u64 {
    value.get(field).and_then(Value::as_u64).unwrap_or(0)
}

fn json_f64(value: &Value, field: &str) -> f64 {
    value.get(field).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Production planning entry point.
///
/// Existing profiles are reused without probing write access. If no profile
/// exists, Krokiet tries to find a writable directory on the same storage.
/// If that is impossible, scanning continues with SAFE_FALLBACK_WORKERS.
pub(crate) fn tuning_plan_for_path(
    store: &VerifierTuningStore,
    path: &Path,
) -> io::Result<VerifierTuningPlan> {
    tuning_plan_for_path_with_probe(store, path, calibration_write_probe)
}

fn tuning_plan_for_path_with_probe<F>(
    store: &VerifierTuningStore,
    path: &Path,
    mut can_write: F,
) -> io::Result<VerifierTuningPlan>
where
    F: FnMut(&Path) -> bool,
{
    let storage_key = storage_key_for_path(path)?;

    if let Some(profile) = store.profile_for_storage_key(&storage_key) {
        return Ok(VerifierTuningPlan::UseProfile {
            storage_key,
            workers: profile.workers,
            stability: profile.stability,
        });
    }

    if let Some(directory) =
        find_writable_calibration_directory_with_probe(path, &mut can_write)?
    {
        return Ok(VerifierTuningPlan::Calibrate {
            storage_key,
            directory,
        });
    }

    Ok(VerifierTuningPlan::Fallback {
        storage_key,
        workers: SAFE_FALLBACK_WORKERS,
    })
}

pub(crate) fn find_writable_calibration_directory(
    path: &Path,
) -> io::Result<Option<PathBuf>> {
    find_writable_calibration_directory_with_probe(path, &mut calibration_write_probe)
}

fn find_writable_calibration_directory_with_probe<F>(
    path: &Path,
    can_write: &mut F,
) -> io::Result<Option<PathBuf>>
where
    F: FnMut(&Path) -> bool,
{
    let canonical = fs::canonicalize(path)?;
    let metadata = fs::metadata(&canonical)?;

    let start = if metadata.is_dir() {
        canonical
    } else {
        canonical
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path has no parent directory",
                )
            })?
    };

    let target_storage = storage_key_for_path(&start)?;

    for candidate in start.ancestors() {
        if !candidate.is_dir() {
            continue;
        }

        let Ok(candidate_storage) = storage_key_for_path(candidate) else {
            continue;
        };

        if candidate_storage != target_storage {
            continue;
        }

        if can_write(candidate) {
            return Ok(Some(candidate.to_path_buf()));
        }
    }

    Ok(None)
}

fn calibration_write_probe(parent: &Path) -> bool {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    for attempt in 0..4_u32 {
        let root = parent.join(format!(
            ".krokiet-verifier-write-probe-{pid}-{nanos}-{attempt}"
        ));

        match fs::create_dir(&root) {
            Ok(()) => {
                let probe_file = root.join("probe.bin");
                let result = (|| -> io::Result<()> {
                    let mut file = File::create(&probe_file)?;
                    file.write_all(&[0x4B])?;
                    file.flush()?;
                    Ok(())
                })();

                let _ = fs::remove_dir_all(&root);
                return result.is_ok();
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return false,
        }
    }

    false
}

pub(crate) fn calibrate_and_store(
    directory: &Path,
    store_path: &Path,
    options: CalibrationOptions,
) -> io::Result<CalibrationReport> {
    let report = calibrate_directory(directory, options)?;

    if report.profile.stability == CalibrationStability::Invalid {
        return Err(io::Error::other(
            "verifier calibration is too unstable to persist",
        ));
    }

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

    let dataset =
        CalibrationDataset::create(&calibrated_path, group_count, options.file_size)?;

    let mut executors = HashMap::with_capacity(candidates.len());
    for &workers in &candidates {
        executors.insert(workers, ExactVerifierExecutor::new(workers)?);
    }

    let mut warm = dataset.result.clone();
    executors
        .get(&1)
        .expect("candidate list always contains one worker")
        .verify_result(&mut warm)?;

    let storage_key = storage_key_for_path(&calibrated_path)?;

    let initial_report = run_calibration_pass(
        &calibrated_path,
        &storage_key,
        logical_cpus,
        &candidates,
        options,
        &executors,
        &dataset,
    )?;

    if initial_report.profile.stability != CalibrationStability::Invalid {
        return Ok(initial_report);
    }

    let mut rescue_options = options;
    rescue_options.target_sample_time =
        rescue_target_sample_time(options.target_sample_time);
    rescue_options.pair_rounds = rescue_pair_rounds(options.pair_rounds);

    let mut rescue_report = run_calibration_pass(
        &calibrated_path,
        &storage_key,
        logical_cpus,
        &candidates,
        rescue_options,
        &executors,
        &dataset,
    )?;

    rescue_report.rescue_attempted = true;
    rescue_report.initial_target_sample_time = options.target_sample_time;
    rescue_report.initial_stability = Some(initial_report.profile.stability);

    Ok(rescue_report)
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
            "max workers must be greater than zero",
        ));
    }
    if options.target_sample_time.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target sample time must be greater than zero",
        ));
    }
    Ok(())
}

fn run_calibration_pass(
    calibrated_path: &Path,
    storage_key: &str,
    logical_cpus: usize,
    candidates: &[usize],
    options: CalibrationOptions,
    executors: &HashMap<usize, ExactVerifierExecutor>,
    dataset: &CalibrationDataset,
) -> io::Result<CalibrationReport> {
    let capacity = options.rounds.saturating_add(options.recheck_rounds);
    let mut buckets: HashMap<usize, SampleBucket> = candidates
        .iter()
        .copied()
        .map(|workers| (workers, SampleBucket::with_capacity(capacity)))
        .collect();

    run_calibration_rounds(
        candidates,
        options.rounds,
        0,
        options.target_sample_time,
        executors,
        dataset,
        &mut buckets,
    )?;

    let initial_measurements =
        build_measurements(candidates, &buckets, dataset.bytes_per_run);
    let initial_selected_workers =
        select_measurement(&initial_measurements, options.tie_margin).workers;

    let rechecked_workers = if options.recheck_rounds == 0 {
        Vec::new()
    } else {
        close_candidates(&initial_measurements, options.recheck_margin)
    };

    if !rechecked_workers.is_empty() {
        run_calibration_rounds(
            &rechecked_workers,
            options.recheck_rounds,
            options.rounds,
            options.target_sample_time,
            executors,
            dataset,
            &mut buckets,
        )?;
    }

    let measurements = build_measurements(candidates, &buckets, dataset.bytes_per_run);
    let aggregate_selected_workers =
        select_measurement(&measurements, options.tie_margin).workers;
    let paired_workers = close_candidates(&measurements, options.recheck_margin);

    let (workers, pairwise) =
        if options.pair_rounds == 0 || paired_workers.len() <= 1 {
            (aggregate_selected_workers, Vec::new())
        } else {
            paired_confirmation(
                &paired_workers,
                options.pair_rounds,
                options.target_sample_time,
                options.tie_margin,
                executors,
                dataset,
            )?
        };

    let selected = measurements
        .iter()
        .find(|measurement| measurement.workers == workers)
        .expect("selected workers must exist");
    let fastest = fastest_measurement(&measurements);

    let selected_slowdown_vs_fastest =
        slowdown_ratio(selected.median, fastest.median);
    let decision_relative_mad =
        decision_relative_mad(&measurements, &paired_workers, workers);
    let global_relative_mad = global_relative_mad(&measurements);

    let stability = classify_stability(
        initial_selected_workers,
        aggregate_selected_workers,
        workers,
        selected.relative_mad,
        decision_relative_mad,
        &pairwise,
        options.tie_margin,
    );

    let calibrated_at_unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    Ok(CalibrationReport {
        profile: VerifierTuningProfile {
            storage_key: storage_key.to_owned(),
            calibrated_path: calibrated_path.to_path_buf(),
            workers,
            logical_cpus,
            median_mib_per_second: selected.median_mib_per_second,
            selected_samples: selected.samples,
            selected_verification_runs: selected.verification_runs,
            relative_mad: selected.relative_mad,
            decision_relative_mad,
            global_relative_mad,
            selected_slowdown_vs_fastest,
            stability,
            calibrated_at_unix_seconds,
        },
        measurements,
        pairwise,
        groups: dataset.group_count,
        bytes_per_run: dataset.bytes_per_run,
        initial_rounds: options.rounds,
        recheck_rounds: options.recheck_rounds,
        pair_rounds: options.pair_rounds,
        target_sample_time: options.target_sample_time,
        rescue_attempted: false,
        initial_target_sample_time: options.target_sample_time,
        initial_stability: None,
        initial_selected_workers,
        aggregate_selected_workers,
        rechecked_workers,
        paired_workers,
    })
}

fn rescue_target_sample_time(initial: Duration) -> Duration {
    initial.max(DEFAULT_RESCUE_SAMPLE_TIME)
}

fn rescue_pair_rounds(initial: usize) -> usize {
    if initial == 0 {
        0
    } else {
        initial.max(DEFAULT_RESCUE_PAIR_ROUNDS)
    }
}

#[derive(Clone, Debug, Default)]
struct SampleBucket {
    samples: Vec<Duration>,
    verification_runs: usize,
}

impl SampleBucket {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            samples: Vec::with_capacity(capacity),
            verification_runs: 0,
        }
    }
}

fn run_calibration_rounds(
    workers: &[usize],
    rounds: usize,
    round_offset: usize,
    target_sample_time: Duration,
    executors: &HashMap<usize, ExactVerifierExecutor>,
    dataset: &CalibrationDataset,
    buckets: &mut HashMap<usize, SampleBucket>,
) -> io::Result<()> {
    for round in 0..rounds {
        let order = rotated_order(workers, round + round_offset);

        for workers in order {
            let executor = executors
                .get(&workers)
                .expect("executor must exist for candidate");

            let sample =
                timed_window(executor, dataset, target_sample_time)?;

            let bucket = buckets
                .get_mut(&workers)
                .expect("sample bucket must exist");
            bucket.samples.push(sample.average);
            bucket.verification_runs += sample.runs;
        }
    }

    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct TimedWindow {
    average: Duration,
    runs: usize,
}

fn timed_window(
    executor: &ExactVerifierExecutor,
    dataset: &CalibrationDataset,
    target_sample_time: Duration,
) -> io::Result<TimedWindow> {
    let mut timed = Duration::ZERO;
    let mut runs = 0usize;

    while timed < target_sample_time || runs == 0 {
        let mut result = dataset.result.clone();
        let started = Instant::now();
        executor.verify_result(&mut result)?;
        timed += started.elapsed();
        runs += 1;
    }

    Ok(TimedWindow {
        average: timed / runs as u32,
        runs,
    })
}

fn rotated_order(workers: &[usize], round: usize) -> Vec<usize> {
    if workers.is_empty() {
        return Vec::new();
    }

    let mut order = workers.to_vec();
    let len = order.len();
    order.rotate_left(round % len);

    if round % 2 == 1 {
        order.reverse();
    }

    order
}

fn build_measurements(
    candidates: &[usize],
    buckets: &HashMap<usize, SampleBucket>,
    bytes_per_run: u64,
) -> Vec<TuningMeasurement> {
    candidates
        .iter()
        .map(|workers| {
            let bucket = buckets.get(workers).expect("bucket must exist");
            measurement_from_samples(
                *workers,
                &bucket.samples,
                bucket.verification_runs,
                bytes_per_run,
            )
        })
        .collect()
}

fn measurement_from_samples(
    workers: usize,
    samples: &[Duration],
    verification_runs: usize,
    bytes_per_run: u64,
) -> TuningMeasurement {
    let median = median_duration(samples);
    let seconds = median.as_secs_f64().max(f64::MIN_POSITIVE);
    let mib = bytes_per_run as f64 / (1024.0 * 1024.0);

    TuningMeasurement {
        workers,
        median,
        median_mib_per_second: mib / seconds,
        samples: samples.len(),
        verification_runs,
        relative_mad: relative_mad(samples, median),
    }
}

fn select_measurement(
    measurements: &[TuningMeasurement],
    tie_margin: f64,
) -> &TuningMeasurement {
    let fastest = fastest_measurement(measurements);
    let threshold =
        fastest.median.as_secs_f64() * (1.0 + tie_margin.max(0.0));

    measurements
        .iter()
        .filter(|measurement| {
            measurement.median.as_secs_f64() <= threshold
        })
        .min_by_key(|measurement| measurement.workers)
        .unwrap_or(fastest)
}

fn fastest_measurement(
    measurements: &[TuningMeasurement],
) -> &TuningMeasurement {
    measurements
        .iter()
        .min_by_key(|measurement| measurement.median)
        .expect("at least one measurement")
}

fn close_candidates(
    measurements: &[TuningMeasurement],
    margin: f64,
) -> Vec<usize> {
    let fastest = fastest_measurement(measurements);
    let threshold =
        fastest.median.as_secs_f64() * (1.0 + margin.max(0.0));

    measurements
        .iter()
        .filter(|measurement| {
            measurement.median.as_secs_f64() <= threshold
        })
        .map(|measurement| measurement.workers)
        .collect()
}

fn paired_confirmation(
    candidates: &[usize],
    rounds: usize,
    target_sample_time: Duration,
    tie_margin: f64,
    executors: &HashMap<usize, ExactVerifierExecutor>,
    dataset: &CalibrationDataset,
) -> io::Result<(usize, Vec<PairwiseConfirmation>)> {
    let mut ordered = candidates.to_vec();
    ordered.sort_unstable();

    let mut incumbent = ordered[0];
    let mut reports = Vec::new();

    for challenger in ordered.into_iter().skip(1) {
        let mut ratios = Vec::with_capacity(rounds);
        let mut decisive_wins = 0usize;

        for round in 0..rounds {
            let (incumbent_time, challenger_time) = if round % 2 == 0 {
                let incumbent_time = timed_window(
                    executors.get(&incumbent).expect("incumbent executor"),
                    dataset,
                    target_sample_time,
                )?
                .average;
                let challenger_time = timed_window(
                    executors.get(&challenger).expect("challenger executor"),
                    dataset,
                    target_sample_time,
                )?
                .average;
                (incumbent_time, challenger_time)
            } else {
                let challenger_time = timed_window(
                    executors.get(&challenger).expect("challenger executor"),
                    dataset,
                    target_sample_time,
                )?
                .average;
                let incumbent_time = timed_window(
                    executors.get(&incumbent).expect("incumbent executor"),
                    dataset,
                    target_sample_time,
                )?
                .average;
                (incumbent_time, challenger_time)
            };

            let ratio = duration_ratio(challenger_time, incumbent_time);
            ratios.push(ratio);

            if ratio <= 1.0 - tie_margin {
                decisive_wins += 1;
            }
        }

        let median_ratio = median_f64(&ratios);
        let majority = rounds / 2 + 1;
        let promoted =
            median_ratio <= 1.0 - tie_margin && decisive_wins >= majority;

        reports.push(PairwiseConfirmation {
            incumbent_workers: incumbent,
            challenger_workers: challenger,
            median_challenger_to_incumbent_ratio: median_ratio,
            decisive_challenger_wins: decisive_wins,
            rounds,
            promoted,
        });

        if promoted {
            incumbent = challenger;
        }
    }

    Ok((incumbent, reports))
}

fn classify_stability(
    initial_workers: usize,
    aggregate_workers: usize,
    selected_workers: usize,
    selected_relative_mad: f64,
    decision_relative_mad: f64,
    pairwise: &[PairwiseConfirmation],
    tie_margin: f64,
) -> CalibrationStability {
    if selected_relative_mad > INVALID_STABILITY_RELATIVE_MAD
        || decision_relative_mad > INVALID_STABILITY_RELATIVE_MAD
    {
        return CalibrationStability::Invalid;
    }

    if selected_relative_mad > LOW_STABILITY_RELATIVE_MAD
        || decision_relative_mad > LOW_STABILITY_RELATIVE_MAD
    {
        return CalibrationStability::Low;
    }

    let changed =
        initial_workers != aggregate_workers || aggregate_workers != selected_workers;

    let near_boundary = pairwise.iter().any(|pair| {
        let promotion_boundary = 1.0 - tie_margin;
        (pair.median_challenger_to_incumbent_ratio - promotion_boundary).abs()
            <= 0.015
    });

    if changed
        || selected_relative_mad > MEDIUM_STABILITY_RELATIVE_MAD
        || decision_relative_mad > MEDIUM_STABILITY_RELATIVE_MAD
        || near_boundary
    {
        return CalibrationStability::Medium;
    }

    CalibrationStability::High
}

fn decision_relative_mad(
    measurements: &[TuningMeasurement],
    paired_workers: &[usize],
    selected_workers: usize,
) -> f64 {
    let workers = if paired_workers.is_empty() {
        vec![selected_workers]
    } else {
        paired_workers.to_vec()
    };

    let values = workers
        .iter()
        .filter_map(|workers| {
            measurements
                .iter()
                .find(|measurement| measurement.workers == *workers)
                .map(|measurement| measurement.relative_mad)
        })
        .collect::<Vec<_>>();

    median_f64(&values)
}

fn global_relative_mad(measurements: &[TuningMeasurement]) -> f64 {
    let values = measurements
        .iter()
        .map(|measurement| measurement.relative_mad)
        .collect::<Vec<_>>();

    median_f64(&values)
}

fn slowdown_ratio(selected: Duration, fastest: Duration) -> f64 {
    let fastest = fastest.as_secs_f64();
    if fastest <= f64::MIN_POSITIVE {
        return 0.0;
    }

    (selected.as_secs_f64() / fastest - 1.0).max(0.0)
}

fn relative_mad(samples: &[Duration], median: Duration) -> f64 {
    let median_seconds = median.as_secs_f64();
    if samples.is_empty() || median_seconds <= f64::MIN_POSITIVE {
        return 0.0;
    }

    let deviations = samples
        .iter()
        .map(|sample| (sample.as_secs_f64() - median_seconds).abs())
        .collect::<Vec<_>>();

    median_f64(&deviations) / median_seconds
}

fn median_duration(values: &[Duration]) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }

    let mut values = values.to_vec();
    values.sort_unstable();
    values[values.len() / 2]
}

fn median_f64(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }

    let mut values = values.to_vec();
    values.sort_by(|left, right| left.total_cmp(right));
    values[values.len() / 2]
}

fn duration_ratio(left: Duration, right: Duration) -> f64 {
    let right = right.as_secs_f64();
    if right <= f64::MIN_POSITIVE {
        return 1.0;
    }
    left.as_secs_f64() / right
}

fn candidate_worker_counts(
    logical_cpus: usize,
    max_workers: usize,
) -> Vec<usize> {
    let ceiling = logical_cpus.max(1).min(max_workers.max(1)).min(16);
    let preferred = [1usize, 2, 4, 6, 8, 12, 16];

    let mut candidates = preferred
        .into_iter()
        .filter(|workers| *workers <= ceiling)
        .collect::<Vec<_>>();

    if !candidates.contains(&ceiling) {
        candidates.push(ceiling);
    }

    candidates.sort_unstable();
    candidates.dedup();
    candidates
}

struct CalibrationDataset {
    root: PathBuf,
    result: DuplicateScanResult,
    group_count: usize,
    bytes_per_run: u64,
}

impl CalibrationDataset {
    fn create(
        parent: &Path,
        group_count: usize,
        file_size: u64,
    ) -> io::Result<Self> {
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        let root = parent.join(format!(
            ".krokiet-verifier-tuning-{pid}-{nanos}"
        ));
        fs::create_dir(&root)?;

        let mut groups = Vec::with_capacity(group_count);
        let mut bytes_per_run = 0u64;

        for group_index in 0..group_count {
            let left = root.join(format!("group-{group_index:04}-a.bin"));
            let right = root.join(format!("group-{group_index:04}-b.bin"));

            write_pattern_file(&left, file_size, group_index as u64)?;
            fs::copy(&left, &right)?;

            let left_file = DuplicateFile::from_path(left)?;
            let right_file = DuplicateFile::from_path(right)?;

            bytes_per_run =
                bytes_per_run.saturating_add(file_size.saturating_mul(2));

            groups.push(DuplicateGroup::new(vec![left_file, right_file]));
        }

        Ok(Self {
            root,
            result: DuplicateScanResult {
                engine: "verifier-tuning",
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

fn write_pattern_file(
    path: &Path,
    file_size: u64,
    seed: u64,
) -> io::Result<()> {
    const CHUNK: usize = 1024 * 1024;
    let mut file = BufWriter::new(File::create(path)?);
    let mut buffer = vec![0u8; CHUNK];

    for (index, value) in buffer.iter_mut().enumerate() {
        *value = ((index as u64)
            .wrapping_mul(31)
            .wrapping_add(seed.wrapping_mul(17))
            & 0xFF) as u8;
    }

    let mut remaining = file_size;
    while remaining > 0 {
        let amount = remaining.min(buffer.len() as u64) as usize;
        file.write_all(&buffer[..amount])?;
        remaining -= amount as u64;
    }

    file.flush()
}

#[cfg(windows)]
pub(crate) fn storage_key_for_path(path: &Path) -> io::Result<String> {
    use std::path::{Component, Prefix};

    let canonical = fs::canonicalize(path)?;
    let Some(Component::Prefix(prefix_component)) = canonical.components().next()
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "path has no Windows volume prefix: {}",
                canonical.display()
            ),
        ));
    };

    let key = match prefix_component.kind() {
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            format!(
                "windows-unc:{}\\{}",
                server.to_string_lossy().to_ascii_lowercase(),
                share.to_string_lossy().to_ascii_lowercase(),
            )
        }
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            windows_volume_key(&canonical).unwrap_or_else(|_| {
                format!(
                    "windows-disk:{}",
                    (letter as char).to_ascii_uppercase()
                )
            })
        }
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

#[cfg(windows)]
fn windows_volume_key(path: &Path) -> io::Result<String> {
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetVolumePathNameW(
            lpsz_file_name: *const u16,
            lpsz_volume_path_name: *mut u16,
            cch_buffer_length: u32,
        ) -> i32;

        fn GetVolumeNameForVolumeMountPointW(
            lpsz_volume_mount_point: *const u16,
            lpsz_volume_name: *mut u16,
            cch_buffer_length: u32,
        ) -> i32;
    }

    fn wide_null(value: &OsStr) -> Vec<u16> {
        value
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn wide_to_string(buffer: &[u16]) -> String {
        let end = buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(buffer.len());

        OsString::from_wide(&buffer[..end])
            .to_string_lossy()
            .into_owned()
    }

    const PATH_CAPACITY: usize = 32_768;
    const VOLUME_CAPACITY: usize = 1_024;

    let mut input = path.as_os_str().encode_wide().collect::<Vec<_>>();

    if input.starts_with(&[
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
    ]) {
        input.drain(..4);
    }

    input.push(0);

    let mut mount_point = vec![0u16; PATH_CAPACITY];
    let mount_ok = unsafe {
        GetVolumePathNameW(
            input.as_ptr(),
            mount_point.as_mut_ptr(),
            PATH_CAPACITY as u32,
        )
    };

    if mount_ok == 0 {
        return Err(io::Error::last_os_error());
    }

    let mount_point_string = wide_to_string(&mount_point);
    let mount_point_wide =
        wide_null(OsStr::new(&mount_point_string));

    let mut volume_name = vec![0u16; VOLUME_CAPACITY];
    let volume_ok = unsafe {
        GetVolumeNameForVolumeMountPointW(
            mount_point_wide.as_ptr(),
            volume_name.as_mut_ptr(),
            VOLUME_CAPACITY as u32,
        )
    };

    if volume_ok == 0 {
        return Err(io::Error::last_os_error());
    }

    let raw = wide_to_string(&volume_name);
    let without_prefix = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    let normalized = without_prefix
        .trim_end_matches(|character| character == '\\' || character == '/')
        .to_ascii_lowercase();

    if normalized.is_empty() {
        return Err(io::Error::other(
            "Windows returned an empty volume identifier",
        ));
    }

    Ok(format!("windows-volume:{normalized}"))
}

#[cfg(unix)]
pub(crate) fn storage_key_for_path(path: &Path) -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;

    let canonical = fs::canonicalize(path)?;
    let metadata = fs::metadata(canonical)?;
    Ok(format!("unix-dev:{}", metadata.dev()))
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn storage_key_for_path(path: &Path) -> io::Result<String> {
    let canonical = fs::canonicalize(path)?;
    Ok(format!(
        "path:{}",
        canonical.to_string_lossy().to_ascii_lowercase()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profile(
        storage_key: String,
        calibrated_path: PathBuf,
        workers: usize,
        stability: CalibrationStability,
    ) -> VerifierTuningProfile {
        VerifierTuningProfile {
            storage_key,
            calibrated_path,
            workers,
            logical_cpus: 8,
            median_mib_per_second: 4000.0,
            selected_samples: 9,
            selected_verification_runs: 45,
            relative_mad: 0.03,
            decision_relative_mad: 0.04,
            global_relative_mad: 0.05,
            selected_slowdown_vs_fastest: 0.01,
            stability,
            calibrated_at_unix_seconds: 42,
        }
    }

    #[test]
    fn candidate_workers_cover_common_machine_sizes() {
        assert_eq!(
            candidate_worker_counts(8, 16),
            vec![1, 2, 4, 6, 8]
        );
        assert_eq!(
            candidate_worker_counts(16, 16),
            vec![1, 2, 4, 6, 8, 12, 16]
        );
    }

    #[test]
    fn rescue_uses_longer_fresh_measurements() {
        assert_eq!(
            rescue_target_sample_time(Duration::from_millis(250)),
            Duration::from_millis(750)
        );
        assert_eq!(
            rescue_target_sample_time(Duration::from_millis(900)),
            Duration::from_millis(900)
        );
        assert_eq!(rescue_pair_rounds(5), 7);
        assert_eq!(rescue_pair_rounds(9), 9);
        assert_eq!(rescue_pair_rounds(0), 0);
    }

    #[test]
    fn writable_storage_without_profile_requests_calibration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = VerifierTuningStore::default();
        let mut probes = 0usize;

        let plan = tuning_plan_for_path_with_probe(
            &store,
            dir.path(),
            |_| {
                probes += 1;
                true
            },
        )
        .expect("plan");

        match plan {
            VerifierTuningPlan::Calibrate {
                directory,
                storage_key,
            } => {
                assert_eq!(
                    fs::canonicalize(directory).expect("selected canonical"),
                    fs::canonicalize(dir.path()).expect("temp canonical")
                );
                assert_eq!(
                    storage_key,
                    storage_key_for_path(dir.path()).expect("storage key")
                );
            }
            other => panic!("expected calibration plan, got {other:?}"),
        }

        assert!(probes >= 1);
    }

    #[test]
    fn readonly_storage_without_profile_uses_safe_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = VerifierTuningStore::default();
        let mut probes = 0usize;

        let plan = tuning_plan_for_path_with_probe(
            &store,
            dir.path(),
            |_| {
                probes += 1;
                false
            },
        )
        .expect("plan");

        match plan {
            VerifierTuningPlan::Fallback {
                workers,
                storage_key,
            } => {
                assert_eq!(workers, SAFE_FALLBACK_WORKERS);
                assert_eq!(
                    storage_key,
                    storage_key_for_path(dir.path()).expect("storage key")
                );
            }
            other => panic!("expected fallback plan, got {other:?}"),
        }

        assert!(probes >= 1);
    }

    #[test]
    fn existing_profile_wins_even_when_storage_is_readonly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage_key =
            storage_key_for_path(dir.path()).expect("storage key");

        let mut store = VerifierTuningStore::default();
        store.upsert(sample_profile(
            storage_key.clone(),
            dir.path().to_path_buf(),
            6,
            CalibrationStability::Medium,
        ));

        let mut probes = 0usize;
        let plan = tuning_plan_for_path_with_probe(
            &store,
            dir.path(),
            |_| {
                probes += 1;
                false
            },
        )
        .expect("plan");

        match plan {
            VerifierTuningPlan::UseProfile {
                workers,
                stability,
                storage_key: key,
            } => {
                assert_eq!(workers, 6);
                assert_eq!(stability, CalibrationStability::Medium);
                assert_eq!(key, storage_key);
            }
            other => panic!("expected existing profile, got {other:?}"),
        }

        // Important: a saved profile must be reusable without attempting any
        // write at all on a read-only USB/storage.
        assert_eq!(probes, 0);
    }

    #[test]
    fn invalid_profile_is_not_reused_and_readonly_falls_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage_key =
            storage_key_for_path(dir.path()).expect("storage key");

        let mut store = VerifierTuningStore::default();
        // Direct push is intentional here because upsert rejects Invalid.
        store.profiles.push(sample_profile(
            storage_key,
            dir.path().to_path_buf(),
            8,
            CalibrationStability::Invalid,
        ));

        let plan =
            tuning_plan_for_path_with_probe(&store, dir.path(), |_| false)
                .expect("plan");

        assert!(matches!(
            plan,
            VerifierTuningPlan::Fallback {
                workers: SAFE_FALLBACK_WORKERS,
                ..
            }
        ));
    }

    #[test]
    fn writable_directory_lookup_prefers_requested_directory() {
        let dir = tempfile::tempdir().expect("tempdir");

        let selected = find_writable_calibration_directory_with_probe(
            dir.path(),
            &mut |candidate| candidate == dir.path(),
        )
        .expect("lookup")
        .expect("directory should be selected");

        assert_eq!(
            fs::canonicalize(selected).expect("selected canonical"),
            fs::canonicalize(dir.path()).expect("temp canonical")
        );
    }

    #[test]
    fn noisy_eliminated_candidate_does_not_invalidate_stable_decision() {
        let measurements = vec![
            TuningMeasurement {
                workers: 1,
                median: Duration::from_millis(100),
                median_mib_per_second: 1000.0,
                samples: 5,
                verification_runs: 10,
                relative_mad: 0.40,
            },
            TuningMeasurement {
                workers: 4,
                median: Duration::from_millis(50),
                median_mib_per_second: 2000.0,
                samples: 9,
                verification_runs: 30,
                relative_mad: 0.03,
            },
            TuningMeasurement {
                workers: 6,
                median: Duration::from_millis(49),
                median_mib_per_second: 2040.0,
                samples: 9,
                verification_runs: 30,
                relative_mad: 0.04,
            },
        ];

        let decision_mad =
            decision_relative_mad(&measurements, &[4, 6], 4);

        let stability = classify_stability(
            4,
            4,
            4,
            0.03,
            decision_mad,
            &[],
            0.05,
        );

        assert_ne!(stability, CalibrationStability::Invalid);
    }

    #[test]
    fn unstable_decision_set_is_invalid() {
        let stability = classify_stability(
            4,
            4,
            4,
            0.13,
            0.02,
            &[],
            0.05,
        );

        assert_eq!(stability, CalibrationStability::Invalid);
    }

    #[cfg(feature = "fast_duplicates")]
    #[test]
    #[ignore = "manual storage calibration; set KROKIET_TUNING_PATH"]
    fn manual_storage_calibration() {
        let path = std::env::var_os("KROKIET_TUNING_PATH")
            .map(PathBuf::from)
            .expect("set KROKIET_TUNING_PATH");

        let report = calibrate_directory(
            &path,
            CalibrationOptions::default(),
        )
        .expect("calibration");

        println!("Storage key: {}", report.profile.storage_key);
        println!("Workers: {}", report.profile.workers);
        println!(
            "Stability: {}",
            report.profile.stability.label_es()
        );
        println!(
            "Decision MAD: {:.2}%",
            report.profile.decision_relative_mad * 100.0
        );
    }
}
