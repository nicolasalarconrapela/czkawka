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

// Version 3 changes the calibration methodology substantially: every timed
// sample now spans a minimum verifier-time window and close finalists receive a
// paired confirmation. Old profiles are deliberately ignored.
const TUNING_FORMAT_VERSION: u64 = 3;
const DEFAULT_FILE_SIZE: u64 = 4 * 1024 * 1024;
const DEFAULT_ROUNDS: usize = 5;
const DEFAULT_RECHECK_ROUNDS: usize = 4;
const DEFAULT_PAIR_ROUNDS: usize = 5;
const DEFAULT_MAX_WORKERS: usize = 16;
const DEFAULT_TIE_MARGIN: f64 = 0.05;
const DEFAULT_RECHECK_MARGIN: f64 = 0.10;
const DEFAULT_TARGET_SAMPLE_TIME: Duration = Duration::from_millis(250);
const MIN_GROUPS: usize = 8;
const HIGH_STABILITY_RELATIVE_MAD: f64 = 0.04;
const LOW_STABILITY_RELATIVE_MAD: f64 = 0.08;
const INVALID_STABILITY_RELATIVE_MAD: f64 = 0.12;

/// Controls the one-off calibration workload.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CalibrationOptions {
    /// Size of each temporary file. Every group contains two identical files.
    pub file_size: u64,
    /// Number of robust timed windows for every candidate worker count.
    pub rounds: usize,
    /// Extra timed windows for candidates close to the initial winner.
    pub recheck_rounds: usize,
    /// Number of paired confirmation rounds between finalists.
    pub pair_rounds: usize,
    /// Minimum amount of actual verifier time represented by one timed sample.
    pub target_sample_time: Duration,
    /// Safety ceiling for the number of verifier workers tested.
    pub max_workers: usize,
    /// If several candidates are within this fraction of the fastest median,
    /// prefer the one using fewer workers.
    pub tie_margin: f64,
    /// Candidates within this fraction of the fastest initial median are
    /// re-measured and considered for paired confirmation.
    pub recheck_margin: f64,
}

impl Default for CalibrationOptions {
    fn default() -> Self {
        Self {
            file_size: DEFAULT_FILE_SIZE,
            rounds: DEFAULT_ROUNDS,
            recheck_rounds: DEFAULT_RECHECK_ROUNDS,
            pair_rounds: DEFAULT_PAIR_ROUNDS,
            target_sample_time: DEFAULT_TARGET_SAMPLE_TIME,
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
    /// Total exact-verifier invocations represented by the robust samples.
    pub verification_runs: usize,
    /// Median normalized time for one exact-verifier pass.
    pub median: Duration,
    pub median_mib_per_second: f64,
    /// Median absolute deviation divided by the median. Lower is more stable.
    pub relative_mad: f64,
}

#[derive(Clone, Debug)]
pub(crate) struct PairwiseMeasurement {
    pub incumbent_workers: usize,
    pub challenger_workers: usize,
    pub pairs: usize,
    /// challenger_time / incumbent_time. Values below 1.0 favor the challenger.
    pub median_ratio: f64,
    /// Number of pairs where the challenger was at least `tie_margin` faster.
    pub decisive_challenger_wins: usize,
    pub promoted: bool,
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
    /// Number of robust samples that contributed to the selected result.
    pub selected_samples: usize,
    /// Total exact-verifier invocations represented by those samples.
    pub selected_verification_runs: usize,
    /// Robust relative dispersion of the selected candidate.
    pub relative_mad: f64,
    /// Median robust dispersion across all tested candidates. This catches
    /// system-wide instability even when the final candidate itself looks quiet.
    pub global_relative_mad: f64,
    /// How much slower the selected candidate is than the absolute fastest
    /// aggregate candidate. This can be non-zero because the tie policy prefers
    /// lower concurrency.
    pub selected_slowdown_vs_fastest: f64,
    pub stability: CalibrationStability,
    pub calibrated_at_unix_seconds: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct CalibrationReport {
    pub profile: VerifierTuningProfile,
    pub measurements: Vec<TuningMeasurement>,
    pub pairwise: Vec<PairwiseMeasurement>,
    pub groups: usize,
    pub bytes_per_run: u64,
    pub initial_rounds: usize,
    pub recheck_rounds: usize,
    pub pair_rounds: usize,
    pub target_sample_time: Duration,
    pub initial_selected_workers: usize,
    pub aggregate_selected_workers: usize,
    pub rechecked_workers: Vec<usize>,
    pub paired_workers: Vec<usize>,
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
    /// If every path has a valid profile, the most conservative worker count is
    /// used across storages. If at least one storage is unknown or invalid,
    /// `None` is returned so the caller can stay sequential or request tuning.
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

            if profile.stability == CalibrationStability::Invalid {
                return Ok(None);
            }

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
            let Some(selected_samples) = item.get("selected_samples").and_then(Value::as_u64)
            else {
                continue;
            };
            let Some(selected_verification_runs) = item
                .get("selected_verification_runs")
                .and_then(Value::as_u64)
            else {
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
            let global_relative_mad = item
                .get("global_relative_mad")
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
            let Ok(selected_verification_runs) = usize::try_from(selected_verification_runs) else {
                continue;
            };
            if workers == 0
                || logical_cpus == 0
                || selected_samples == 0
                || selected_verification_runs == 0
            {
                continue;
            }

            store.upsert(VerifierTuningProfile {
                storage_key: storage_key.to_owned(),
                calibrated_path: PathBuf::from(calibrated_path),
                workers,
                logical_cpus,
                median_mib_per_second,
                selected_samples,
                selected_verification_runs,
                relative_mad,
                global_relative_mad,
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
                    "selected_verification_runs": profile.selected_verification_runs,
                    "relative_mad": profile.relative_mad,
                    "global_relative_mad": profile.global_relative_mad,
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

/// Performs a one-off calibration and persists it only when the measurement is
/// considered usable. An invalid/noisy calibration is returned as an error so a
/// previous good profile cannot be overwritten by a bad run.
pub(crate) fn calibrate_and_store(
    directory: &Path,
    store_path: &Path,
    options: CalibrationOptions,
) -> io::Result<CalibrationReport> {
    let report = calibrate_directory(directory, options)?;
    if report.profile.stability == CalibrationStability::Invalid {
        return Err(io::Error::other(
            "verifier calibration is too unstable; profile was not stored",
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

    let dataset = CalibrationDataset::create(&calibrated_path, group_count, options.file_size)?;

    let mut executors = HashMap::with_capacity(candidates.len());
    for &workers in &candidates {
        executors.insert(workers, ExactVerifierExecutor::new(workers)?);
    }

    // Warm the same file/metadata/page-cache path that the real post-fclones
    // verifier commonly sees. Warm-up is not timed.
    let mut warm = dataset.result.clone();
    executors
        .get(&1)
        .expect("candidate list always contains one worker")
        .verify_result(&mut warm)?;

    let sample_capacity = options.rounds.saturating_add(options.recheck_rounds);
    let mut buckets: HashMap<usize, SampleBucket> = candidates
        .iter()
        .copied()
        .map(|workers| (workers, SampleBucket::with_capacity(sample_capacity)))
        .collect();

    run_calibration_rounds(
        &candidates,
        options.rounds,
        0,
        options.target_sample_time,
        &executors,
        &dataset,
        &mut buckets,
    )?;

    let initial_measurements = build_measurements(&candidates, &buckets, dataset.bytes_per_run);
    let initial_selected_workers = select_measurement(&initial_measurements, options.tie_margin).workers;

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
            &executors,
            &dataset,
            &mut buckets,
        )?;
    }

    let measurements = build_measurements(&candidates, &buckets, dataset.bytes_per_run);
    let aggregate_selected_workers = select_measurement(&measurements, options.tie_margin).workers;
    let paired_workers = close_candidates(&measurements, options.recheck_margin);

    let (workers, pairwise) = if options.pair_rounds == 0 || paired_workers.len() <= 1 {
        (aggregate_selected_workers, Vec::new())
    } else {
        paired_confirmation(
            &paired_workers,
            options.pair_rounds,
            options.target_sample_time,
            options.tie_margin,
            &executors,
            &dataset,
        )?
    };

    let selected = measurements
        .iter()
        .find(|measurement| measurement.workers == workers)
        .expect("paired selection must refer to a measured worker count");
    let fastest = fastest_measurement(&measurements);
    let selected_slowdown_vs_fastest = slowdown_ratio(selected.median, fastest.median);
    let global_relative_mad = global_relative_mad(&measurements);
    let stability = classify_stability(
        initial_selected_workers,
        aggregate_selected_workers,
        workers,
        selected.relative_mad,
        global_relative_mad,
        &pairwise,
        options.tie_margin,
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
            workers,
            logical_cpus,
            median_mib_per_second: selected.median_mib_per_second,
            selected_samples: selected.samples,
            selected_verification_runs: selected.verification_runs,
            relative_mad: selected.relative_mad,
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
        initial_selected_workers,
        aggregate_selected_workers,
        rechecked_workers,
        paired_workers,
    })
}

#[derive(Clone, Debug, Default)]
struct SampleBucket {
    normalized_times: Vec<Duration>,
    verification_runs: usize,
}

impl SampleBucket {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            normalized_times: Vec::with_capacity(capacity),
            verification_runs: 0,
        }
    }
}

fn run_calibration_rounds(
    candidates: &[usize],
    rounds: usize,
    round_offset: usize,
    target_sample_time: Duration,
    executors: &HashMap<usize, ExactVerifierExecutor>,
    dataset: &CalibrationDataset,
    buckets: &mut HashMap<usize, SampleBucket>,
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
            let (normalized, verification_runs) = measure_candidate_window(
                executors
                    .get(&workers)
                    .expect("executor exists for every candidate"),
                dataset,
                target_sample_time,
            )?;

            let bucket = buckets
                .get_mut(&workers)
                .expect("sample bucket exists for every candidate");
            bucket.normalized_times.push(normalized);
            bucket.verification_runs = bucket.verification_runs.saturating_add(verification_runs);
        }
    }

    Ok(())
}

fn measure_candidate_window(
    executor: &ExactVerifierExecutor,
    dataset: &CalibrationDataset,
    target_sample_time: Duration,
) -> io::Result<(Duration, usize)> {
    let mut measured = Duration::ZERO;
    let mut verification_runs = 0_usize;

    while verification_runs == 0 || measured < target_sample_time {
        // Result cloning is intentionally outside the timed verifier interval.
        let mut result = dataset.result.clone();
        let started = Instant::now();
        let verified = executor.verify_result(&mut result)?;
        let elapsed = started.elapsed();

        if verified != dataset.group_count || result.groups.len() != dataset.group_count {
            return Err(io::Error::other(format!(
                "calibration verifier returned {verified}/{} groups with {} worker(s)",
                dataset.group_count,
                executor.workers()
            )));
        }

        measured = measured.saturating_add(elapsed);
        verification_runs = verification_runs.saturating_add(1);
    }

    let normalized = Duration::from_secs_f64(
        measured.as_secs_f64() / verification_runs.max(1) as f64,
    );
    Ok((normalized, verification_runs))
}

fn build_measurements(
    candidates: &[usize],
    buckets: &HashMap<usize, SampleBucket>,
    bytes_per_run: u64,
) -> Vec<TuningMeasurement> {
    let mut measurements = Vec::with_capacity(candidates.len());

    for &workers in candidates {
        let bucket = buckets
            .get(&workers)
            .expect("sample bucket exists for every candidate");
        let median = median_duration(&bucket.normalized_times);
        let seconds = median.as_secs_f64().max(f64::MIN_POSITIVE);
        let median_mib_per_second = bytes_per_run as f64 / (1024.0 * 1024.0) / seconds;
        measurements.push(TuningMeasurement {
            workers,
            samples: bucket.normalized_times.len(),
            verification_runs: bucket.verification_runs,
            median,
            median_mib_per_second,
            relative_mad: relative_mad(&bucket.normalized_times, median),
        });
    }

    measurements.sort_by_key(|measurement| measurement.workers);
    measurements
}

fn select_measurement(measurements: &[TuningMeasurement], tie_margin: f64) -> &TuningMeasurement {
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

fn close_candidates(measurements: &[TuningMeasurement], margin: f64) -> Vec<usize> {
    if measurements.is_empty() {
        return Vec::new();
    }

    let fastest_seconds = fastest_measurement(measurements).median.as_secs_f64();
    let threshold = fastest_seconds * (1.0 + margin);

    measurements
        .iter()
        .filter(|measurement| measurement.median.as_secs_f64() <= threshold)
        .map(|measurement| measurement.workers)
        .collect()
}

fn paired_confirmation(
    candidates: &[usize],
    pair_rounds: usize,
    target_sample_time: Duration,
    tie_margin: f64,
    executors: &HashMap<usize, ExactVerifierExecutor>,
    dataset: &CalibrationDataset,
) -> io::Result<(usize, Vec<PairwiseMeasurement>)> {
    let mut ordered = candidates.to_vec();
    ordered.sort_unstable();
    ordered.dedup();

    let Some(&first) = ordered.first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "paired confirmation requires at least one candidate",
        ));
    };

    let mut incumbent = first;
    let mut comparisons = Vec::with_capacity(ordered.len().saturating_sub(1));

    for challenger in ordered.into_iter().skip(1) {
        let mut ratios = Vec::with_capacity(pair_rounds);

        for pair_index in 0..pair_rounds {
            let (incumbent_time, challenger_time) = if pair_index % 2 == 0 {
                let incumbent_time = measure_candidate_window(
                    executors
                        .get(&incumbent)
                        .expect("executor exists for paired incumbent"),
                    dataset,
                    target_sample_time,
                )?
                .0;
                let challenger_time = measure_candidate_window(
                    executors
                        .get(&challenger)
                        .expect("executor exists for paired challenger"),
                    dataset,
                    target_sample_time,
                )?
                .0;
                (incumbent_time, challenger_time)
            } else {
                let challenger_time = measure_candidate_window(
                    executors
                        .get(&challenger)
                        .expect("executor exists for paired challenger"),
                    dataset,
                    target_sample_time,
                )?
                .0;
                let incumbent_time = measure_candidate_window(
                    executors
                        .get(&incumbent)
                        .expect("executor exists for paired incumbent"),
                    dataset,
                    target_sample_time,
                )?
                .0;
                (incumbent_time, challenger_time)
            };

            let incumbent_seconds = incumbent_time.as_secs_f64().max(f64::MIN_POSITIVE);
            ratios.push(challenger_time.as_secs_f64() / incumbent_seconds);
        }

        let median_ratio = median_f64_owned(&ratios);
        let decisive_threshold = 1.0 - tie_margin;
        let decisive_challenger_wins = ratios
            .iter()
            .filter(|ratio| **ratio < decisive_threshold)
            .count();
        let required_wins = pair_rounds / 2 + 1;
        let promoted = median_ratio < decisive_threshold
            && decisive_challenger_wins >= required_wins;

        comparisons.push(PairwiseMeasurement {
            incumbent_workers: incumbent,
            challenger_workers: challenger,
            pairs: pair_rounds,
            median_ratio,
            decisive_challenger_wins,
            promoted,
        });

        if promoted {
            incumbent = challenger;
        }
    }

    Ok((incumbent, comparisons))
}

fn classify_stability(
    initial_selected_workers: usize,
    aggregate_selected_workers: usize,
    final_selected_workers: usize,
    selected_relative_mad: f64,
    global_relative_mad: f64,
    pairwise: &[PairwiseMeasurement],
    tie_margin: f64,
) -> CalibrationStability {
    if selected_relative_mad > INVALID_STABILITY_RELATIVE_MAD
        || global_relative_mad > INVALID_STABILITY_RELATIVE_MAD
    {
        return CalibrationStability::Invalid;
    }

    // A comparison sitting very close to the promotion boundary means the exact
    // worker count is sensitive to small system noise, even if its MAD is low.
    let pair_boundary_uncertain = pairwise.iter().any(|comparison| {
        let boundary = 1.0 - tie_margin;
        (comparison.median_ratio - boundary).abs() <= 0.015
    });

    if selected_relative_mad > LOW_STABILITY_RELATIVE_MAD
        || global_relative_mad > LOW_STABILITY_RELATIVE_MAD
    {
        CalibrationStability::Low
    } else if initial_selected_workers != aggregate_selected_workers
        || aggregate_selected_workers != final_selected_workers
        || selected_relative_mad > HIGH_STABILITY_RELATIVE_MAD
        || global_relative_mad > HIGH_STABILITY_RELATIVE_MAD
        || pair_boundary_uncertain
    {
        CalibrationStability::Medium
    } else {
        CalibrationStability::High
    }
}

fn global_relative_mad(measurements: &[TuningMeasurement]) -> f64 {
    let mut values = measurements
        .iter()
        .map(|measurement| measurement.relative_mad)
        .collect::<Vec<_>>();
    median_f64(&mut values)
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
    if options.target_sample_time.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "calibration target_sample_time must be greater than zero",
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

fn median_f64_owned(values: &[f64]) -> f64 {
    let mut values = values.to_vec();
    median_f64(&mut values)
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
            verification_runs: 25,
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
        assert_eq!(close_candidates(&measurements, 0.10), vec![4, 8, 12]);
    }

    #[test]
    fn global_mad_detects_system_wide_noise() {
        let measurements = vec![
            measurement(1, 100, 0.02),
            measurement(2, 80, 0.19),
            measurement(4, 70, 0.21),
            measurement(8, 68, 0.03),
            measurement(16, 67, 0.18),
        ];
        assert!(global_relative_mad(&measurements) > 0.12);
    }

    #[test]
    fn invalid_stability_rejects_globally_noisy_calibration() {
        assert_eq!(
            classify_stability(4, 4, 8, 0.03, 0.19, &[], 0.05),
            CalibrationStability::Invalid
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
            selected_verification_runs: 45,
            relative_mad: 0.02,
            global_relative_mad: 0.03,
            selected_slowdown_vs_fastest: 0.01,
            stability: CalibrationStability::High,
            calibrated_at_unix_seconds: 42,
        });
        store.save(&settings).expect("save");

        let loaded = VerifierTuningStore::load(&settings).expect("load");
        assert_eq!(loaded.profiles().len(), 1);
        assert_eq!(loaded.profiles()[0].workers, 3);
        assert_eq!(loaded.profiles()[0].selected_samples, 9);
        assert_eq!(loaded.profiles()[0].selected_verification_runs, 45);
        assert_eq!(loaded.profiles()[0].stability, CalibrationStability::High);
        assert_eq!(
            loaded
                .workers_for_paths(&[dir.path().to_path_buf()])
                .expect("match"),
            Some(3)
        );
    }

    #[test]
    fn invalid_profile_is_not_used_for_scanning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage_key = storage_key_for_path(dir.path()).expect("storage key");
        let mut store = VerifierTuningStore::default();
        store.upsert(VerifierTuningProfile {
            storage_key,
            calibrated_path: dir.path().to_path_buf(),
            workers: 8,
            logical_cpus: 16,
            median_mib_per_second: 6000.0,
            selected_samples: 9,
            selected_verification_runs: 45,
            relative_mad: 0.03,
            global_relative_mad: 0.20,
            selected_slowdown_vs_fastest: 0.0,
            stability: CalibrationStability::Invalid,
            calibrated_at_unix_seconds: 42,
        });
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

        print_report(&report);
    }

    #[cfg(feature = "fast_duplicates")]
    fn print_report(report: &CalibrationReport) {
        println!();
        println!("============================================================");
        println!(" KROKIET - CALIBRACION ROBUSTA DEL EXACT VERIFIER");
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
        println!(
            "Ventana/sample: {:.0} ms",
            report.target_sample_time.as_secs_f64() * 1000.0
        );
        if report.rechecked_workers.is_empty() {
            println!("Recheck       : no necesario");
        } else {
            println!(
                "Recheck       : {} ronda(s) extra para {:?}",
                report.recheck_rounds, report.rechecked_workers
            );
        }
        println!(
            "Final pareada : {} ronda(s) para {:?}",
            report.pair_rounds, report.paired_workers
        );
        println!();
        for measurement in &report.measurements {
            println!(
                "workers-{:>2}: {:>10.3?} | {:>9.1} MiB/s | n={:<2} | runs={:<3} | MAD {:>5.1}%",
                measurement.workers,
                measurement.median,
                measurement.median_mib_per_second,
                measurement.samples,
                measurement.verification_runs,
                measurement.relative_mad * 100.0,
            );
        }
        if !report.pairwise.is_empty() {
            println!("------------------------------------------------------------");
            println!(" CONFIRMACION PAREADA");
            for comparison in &report.pairwise {
                println!(
                    " {:>2} vs {:>2}: ratio {:>6.3} | victorias decisivas {}/{} | {}",
                    comparison.incumbent_workers,
                    comparison.challenger_workers,
                    comparison.median_ratio,
                    comparison.decisive_challenger_wins,
                    comparison.pairs,
                    if comparison.promoted { "PROMUEVE" } else { "MANTIENE" },
                );
            }
        }
        println!("------------------------------------------------------------");
        println!("Seleccion ini : {} worker(s)", report.initial_selected_workers);
        println!(
            "Seleccion agg : {} worker(s)",
            report.aggregate_selected_workers
        );
        println!("Seleccion     : {} worker(s)", report.profile.workers);
        println!(
            "Rendimiento   : {:.1} MiB/s",
            report.profile.median_mib_per_second
        );
        println!(
            "Margen mejor  : {:.2}%",
            report.profile.selected_slowdown_vs_fastest * 100.0
        );
        println!(
            "MAD seleccionado: {:.2}%",
            report.profile.relative_mad * 100.0
        );
        println!(
            "MAD global    : {:.2}%",
            report.profile.global_relative_mad * 100.0
        );
        println!("Estabilidad   : {}", report.profile.stability.label_es());
        if report.profile.stability == CalibrationStability::Invalid {
            println!("Perfil        : NO GUARDAR; repetir con el sistema menos cargado");
        }
        println!("============================================================");
    }
}
