//! Storage-specific calibration for the exact duplicate verifier.
//!
//! This module is deliberately independent from duplicate scanning. A caller can
//! calibrate a writable directory once, persist the resulting profile, and later
//! feed only the selected worker count into the exact verifier. It also exposes a
//! resolver that can reuse an existing profile, calibrate when needed, or fall
//! back safely when calibration is unavailable.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::types::{DuplicateFile, DuplicateGroup, DuplicateScanResult};
use super::verify::ExactVerifierExecutor;

// Persistence schema version 6. The 2.9.3 behaviour keeps the same on-disk
// format and adds safe profile replacement/reuse rules, so no format bump is
// needed.
const TUNING_FORMAT_VERSION: u64 = 6;
const DEFAULT_FILE_SIZE: u64 = 4 * 1024 * 1024;
const DEFAULT_ROUNDS: usize = 5;
const DEFAULT_RECHECK_ROUNDS: usize = 4;
const DEFAULT_PAIR_ROUNDS: usize = 5;
const DEFAULT_MAX_WORKERS: usize = 16;
const DEFAULT_TIE_MARGIN: f64 = 0.05;
const DEFAULT_RECHECK_MARGIN: f64 = 0.10;
const DEFAULT_TARGET_SAMPLE_TIME: Duration = Duration::from_millis(250);
const DEFAULT_RESCUE_SAMPLE_TIME: Duration = Duration::from_millis(750);
const DEFAULT_RESCUE_PAIR_ROUNDS: usize = 7;
const MIN_GROUPS: usize = 8;
const HIGH_STABILITY_RELATIVE_MAD: f64 = 0.04;
const LOW_STABILITY_RELATIVE_MAD: f64 = 0.08;
const INVALID_STABILITY_RELATIVE_MAD: f64 = 0.12;

/// Conservative verifier concurrency used when a storage has no usable profile
/// and no writable place where Krokiet can calibrate it. Correctness is unchanged;
/// only performance can be lower until a real profile becomes available.
pub(crate) const SAFE_FALLBACK_WORKERS: usize = 1;

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
pub(crate) enum VerifierTuningPlan {
    /// A previously calibrated profile can be reused immediately.
    UseProfile {
        storage_key: String,
        workers: usize,
        stability: CalibrationStability,
    },
    /// No profile exists, but Krokiet found a writable directory on the same
    /// storage and can calibrate there.
    Calibrate {
        storage_key: String,
        directory: PathBuf,
    },
    /// No profile exists and calibration cannot safely create temporary files.
    /// Scanning should continue with a conservative worker count.
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

/// Final worker choice returned to a future CLI/UI integration. The caller can
/// expose the source for diagnostics without needing to understand persistence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum VerifierTuningResolution {
    ReusedProfile {
        storage_key: String,
        workers: usize,
        stability: CalibrationStability,
    },
    Calibrated {
        storage_key: String,
        workers: usize,
        stability: CalibrationStability,
    },
    Fallback {
        storage_key: String,
        workers: usize,
    },
}

impl VerifierTuningResolution {
    pub(crate) fn workers(&self) -> usize {
        match self {
            Self::ReusedProfile { workers, .. }
            | Self::Calibrated { workers, .. }
            | Self::Fallback { workers, .. } => *workers,
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
    /// Robust dispersion of the decision set (paired finalists when available,
    /// otherwise the aggregate finalists). This is the stability signal used to
    /// decide whether a profile is trustworthy.
    pub decision_relative_mad: f64,
    /// Median robust dispersion across every tested candidate. This is retained
    /// for diagnostics only: clearly inferior noisy candidates must not invalidate
    /// an otherwise stable final decision.
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
    /// True when the normal pass was rejected and the result comes from a
    /// completely fresh, longer rescue pass.
    pub rescue_attempted: bool,
    /// Window used by the first pass. Useful for diagnostics when rescue ran.
    pub initial_target_sample_time: Duration,
    /// Stability returned by the first pass. Present only when rescue ran.
    pub initial_stability: Option<CalibrationStability>,
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

    pub(crate) fn profile_for_storage_key(&self, storage_key: &str) -> Option<&VerifierTuningProfile> {
        self.profiles
            .iter()
            .find(|profile| profile.storage_key == storage_key && profile.stability != CalibrationStability::Invalid)
    }

    pub(crate) fn profile_for_path(&self, path: &Path) -> io::Result<Option<&VerifierTuningProfile>> {
        let storage_key = storage_key_for_path(path)?;
        Ok(self.profile_for_storage_key(&storage_key))
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

    /// Stores a freshly calibrated profile only when doing so cannot reduce the
    /// confidence of the profile already known for that storage.
    ///
    /// Policy:
    /// - Invalid is never stored.
    /// - High can replace High/Medium/Low.
    /// - Medium can replace Medium/Low.
    /// - Low is stored only when no usable profile exists yet.
    pub(crate) fn upsert_calibrated_if_preferred(
        &mut self,
        profile: VerifierTuningProfile,
    ) -> bool {
        if profile.stability == CalibrationStability::Invalid {
            return false;
        }

        let existing_index = self
            .profiles
            .iter()
            .position(|existing| existing.storage_key == profile.storage_key);

        let Some(index) = existing_index else {
            self.upsert(profile);
            return true;
        };

        let existing_stability = self.profiles[index].stability;
        let should_replace = match profile.stability {
            CalibrationStability::High => true,
            CalibrationStability::Medium => matches!(
                existing_stability,
                CalibrationStability::Medium
                    | CalibrationStability::Low
                    | CalibrationStability::Invalid
            ),
            CalibrationStability::Low => existing_stability == CalibrationStability::Invalid,
            CalibrationStability::Invalid => false,
        };

        if should_replace {
            self.profiles[index] = profile;
            true
        } else {
            false
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
            let decision_relative_mad = item
                .get("decision_relative_mad")
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
                decision_relative_mad,
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
                    "decision_relative_mad": profile.decision_relative_mad,
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

/// Decides what Krokiet should do for one path without ever requiring write
/// access just to continue a scan. Existing profiles win first; otherwise a
/// writable location on the same storage is selected for calibration. If none
/// exists, Krokiet falls back to one verifier worker and skips calibration.
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

    if let Some(directory) = find_writable_calibration_directory_with_probe(path, &mut can_write)? {
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

/// Finds a place on the same storage where temporary calibration files can be
/// created. The requested directory is preferred. If it is protected, ancestors
/// on the same storage are tried. Nothing outside that storage is used.
pub(crate) fn find_writable_calibration_directory(path: &Path) -> io::Result<Option<PathBuf>> {
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
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory"))?
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

/// Performs a one-off calibration and persists it only when the measurement is
/// considered usable. An invalid/noisy calibration is returned as an error so a
/// previous good profile cannot be overwritten by a bad run.
pub(crate) fn calibrate_and_store(
    directory: &Path,
    store_path: &Path,
    options: CalibrationOptions,
) -> io::Result<CalibrationReport> {
    calibrate_and_store_with(directory, store_path, options, calibrate_directory)
}

fn calibrate_and_store_with<F>(
    directory: &Path,
    store_path: &Path,
    options: CalibrationOptions,
    calibrate: F,
) -> io::Result<CalibrationReport>
where
    F: FnOnce(&Path, CalibrationOptions) -> io::Result<CalibrationReport>,
{
    let report = calibrate(directory, options)?;
    if report.profile.stability == CalibrationStability::Invalid {
        return Err(io::Error::other(
            "verifier calibration is too unstable; profile was not stored",
        ));
    }

    let mut store = VerifierTuningStore::load(store_path)?;
    if store.upsert_calibrated_if_preferred(report.profile.clone()) {
        store.save(store_path)?;
    }
    Ok(report)
}

/// Resolves the verifier worker count for one storage and persists a useful
/// calibration when necessary. This is the single entry point intended for the
/// upcoming command/UI integration. A failed or unstable calibration never
/// prevents the duplicate scan: it falls back to the conservative worker count.
pub(crate) fn resolve_tuning_for_path(
    path: &Path,
    store_path: &Path,
    options: CalibrationOptions,
) -> io::Result<VerifierTuningResolution> {
    resolve_tuning_for_path_with(
        path,
        store_path,
        options,
        calibration_write_probe,
        calibrate_directory,
    )
}

fn resolve_tuning_for_path_with<P, C>(
    path: &Path,
    store_path: &Path,
    options: CalibrationOptions,
    probe: P,
    calibrate: C,
) -> io::Result<VerifierTuningResolution>
where
    P: FnMut(&Path) -> bool,
    C: FnOnce(&Path, CalibrationOptions) -> io::Result<CalibrationReport>,
{
    let store = VerifierTuningStore::load(store_path)?;
    let plan = tuning_plan_for_path_with_probe(&store, path, probe)?;

    match plan {
        VerifierTuningPlan::UseProfile {
            storage_key,
            workers,
            stability,
        } => Ok(VerifierTuningResolution::ReusedProfile {
            storage_key,
            workers,
            stability,
        }),
        VerifierTuningPlan::Fallback {
            storage_key,
            workers,
        } => Ok(VerifierTuningResolution::Fallback {
            storage_key,
            workers,
        }),
        VerifierTuningPlan::Calibrate {
            storage_key,
            directory,
        } => match calibrate_and_store_with(&directory, store_path, options, calibrate) {
            Ok(report) => {
                // The replacement policy may deliberately keep a stronger old
                // profile. Reload the persisted store so the returned worker
                // count always matches what future scans will reuse.
                let persisted = VerifierTuningStore::load(store_path)?;
                if let Some(profile) = persisted.profile_for_storage_key(&storage_key) {
                    Ok(VerifierTuningResolution::Calibrated {
                        storage_key,
                        workers: profile.workers,
                        stability: profile.stability,
                    })
                } else {
                    Ok(VerifierTuningResolution::Calibrated {
                        storage_key,
                        workers: report.profile.workers,
                        stability: report.profile.stability,
                    })
                }
            }
            Err(_) => Ok(VerifierTuningResolution::Fallback {
                storage_key,
                workers: SAFE_FALLBACK_WORKERS,
            }),
        },
    }
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

    // The first pass proved that the current machine/storage combination is too
    // noisy for short samples. Start over from empty buckets: noisy 250 ms data
    // must not influence the rescue decision.
    let mut rescue_options = options;
    rescue_options.target_sample_time = rescue_target_sample_time(options.target_sample_time);
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

fn run_calibration_pass(
    calibrated_path: &Path,
    storage_key: &str,
    logical_cpus: usize,
    candidates: &[usize],
    options: CalibrationOptions,
    executors: &HashMap<usize, ExactVerifierExecutor>,
    dataset: &CalibrationDataset,
) -> io::Result<CalibrationReport> {
    let sample_capacity = options.rounds.saturating_add(options.recheck_rounds);
    let mut buckets: HashMap<usize, SampleBucket> = candidates
        .iter()
        .copied()
        .map(|workers| (workers, SampleBucket::with_capacity(sample_capacity)))
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

    let initial_measurements = build_measurements(candidates, &buckets, dataset.bytes_per_run);
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
            executors,
            dataset,
        )?
    };

    let selected = measurements
        .iter()
        .find(|measurement| measurement.workers == workers)
        .expect("paired selection must refer to a measured worker count");
    let fastest = fastest_measurement(&measurements);
    let selected_slowdown_vs_fastest = slowdown_ratio(selected.median, fastest.median);
    let decision_relative_mad = decision_relative_mad(&measurements, &paired_workers, workers);
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
    decision_relative_mad: f64,
    pairwise: &[PairwiseMeasurement],
    tie_margin: f64,
) -> CalibrationStability {
    // Only the selected candidate and the candidates that actually participated
    // in the final decision can invalidate a profile. Slow/noisy candidates that
    // were already eliminated remain useful diagnostics, but are not a reason to
    // throw away a stable winner.
    if selected_relative_mad > INVALID_STABILITY_RELATIVE_MAD
        || decision_relative_mad > INVALID_STABILITY_RELATIVE_MAD
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
        || decision_relative_mad > LOW_STABILITY_RELATIVE_MAD
    {
        CalibrationStability::Low
    } else if initial_selected_workers != aggregate_selected_workers
        || aggregate_selected_workers != final_selected_workers
        || selected_relative_mad > HIGH_STABILITY_RELATIVE_MAD
        || decision_relative_mad > HIGH_STABILITY_RELATIVE_MAD
        || pair_boundary_uncertain
    {
        CalibrationStability::Medium
    } else {
        CalibrationStability::High
    }
}

fn decision_relative_mad(
    measurements: &[TuningMeasurement],
    decision_workers: &[usize],
    selected_workers: usize,
) -> f64 {
    let mut values = if decision_workers.is_empty() {
        measurements
            .iter()
            .filter(|measurement| measurement.workers == selected_workers)
            .map(|measurement| measurement.relative_mad)
            .collect::<Vec<_>>()
    } else {
        measurements
            .iter()
            .filter(|measurement| decision_workers.contains(&measurement.workers))
            .map(|measurement| measurement.relative_mad)
            .collect::<Vec<_>>()
    };

    // `selected_workers` always refers to an existing measurement. Keep this
    // fallback defensive in case the decision-set construction changes later.
    if values.is_empty() {
        values = measurements
            .iter()
            .filter(|measurement| measurement.workers == selected_workers)
            .map(|measurement| measurement.relative_mad)
            .collect();
    }

    median_f64(&mut values)
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
        // Network shares are already identified independently from a mapped drive
        // letter, so keep server/share as the stable key.
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => format!(
            "windows-unc:{}\\{}",
            server.to_string_lossy().to_ascii_lowercase(),
            share.to_string_lossy().to_ascii_lowercase(),
        ),
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            // Prefer the Windows volume GUID. This survives a USB changing from,
            // for example, Q: to F: on the same machine. If Windows cannot expose
            // a volume GUID, retain the old drive-letter fallback rather than
            // making tuning fail.
            windows_volume_key(&canonical).unwrap_or_else(|_| {
                format!("windows-disk:{}", (letter as char).to_ascii_uppercase())
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
    use std::ffi::OsString;
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

    fn wide_null(value: &std::ffi::OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn wide_to_string(buffer: &[u16]) -> String {
        let end = buffer.iter().position(|value| *value == 0).unwrap_or(buffer.len());
        OsString::from_wide(&buffer[..end])
            .to_string_lossy()
            .into_owned()
    }

    const PATH_CAPACITY: usize = 32_768;
    const VOLUME_CAPACITY: usize = 1_024;

    let mut input = path.as_os_str().encode_wide().collect::<Vec<_>>();
    // `canonicalize` commonly returns `\\?\C:\...`. The volume APIs are
    // most compatible with the regular `C:\...` spelling for drive paths.
    if input.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16]) {
        input.drain(..4);
    }
    input.push(0);

    let mut mount_point = vec![0_u16; PATH_CAPACITY];
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
    let mount_point_wide = wide_null(std::ffi::OsStr::new(&mount_point_string));
    let mut volume_name = vec![0_u16; VOLUME_CAPACITY];
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
        return Err(io::Error::other("Windows returned an empty volume identifier"));
    }

    Ok(format!("windows-volume:{normalized}"))
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

    fn test_profile(
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

    fn test_report(profile: VerifierTuningProfile) -> CalibrationReport {
        CalibrationReport {
            profile,
            measurements: Vec::new(),
            pairwise: Vec::new(),
            groups: 16,
            bytes_per_run: 128 * 1024 * 1024,
            initial_rounds: 5,
            recheck_rounds: 4,
            pair_rounds: 5,
            target_sample_time: Duration::from_millis(250),
            rescue_attempted: false,
            initial_target_sample_time: Duration::from_millis(250),
            initial_stability: None,
            initial_selected_workers: 6,
            aggregate_selected_workers: 6,
            rechecked_workers: vec![4, 6, 8],
            paired_workers: vec![4, 6, 8],
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
    fn noisy_eliminated_candidates_do_not_invalidate_a_stable_decision() {
        let measurements = vec![
            measurement(1, 100, 0.25),
            measurement(2, 80, 0.31),
            measurement(4, 70, 0.039),
            measurement(6, 68, 0.137),
            measurement(8, 69, 0.044),
        ];
        let finalists = vec![4, 6, 8];
        let decision_mad = decision_relative_mad(&measurements, &finalists, 4);

        assert!(global_relative_mad(&measurements) > 0.12);
        assert!(decision_mad < 0.05);
        assert_ne!(
            classify_stability(6, 4, 4, 0.039, decision_mad, &[], 0.05),
            CalibrationStability::Invalid
        );
    }

    #[test]
    fn unstable_decision_set_is_still_invalid() {
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
            decision_relative_mad: 0.025,
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
            decision_relative_mad: 0.20,
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

    #[test]
    fn writable_calibration_directory_prefers_requested_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let selected = find_writable_calibration_directory(dir.path())
            .expect("writable lookup")
            .expect("temporary directory should be writable");
        assert_eq!(
            fs::canonicalize(&selected).expect("selected canonical"),
            fs::canonicalize(dir.path()).expect("temp canonical")
        );
    }

    #[test]
    fn tuning_plan_reuses_an_existing_profile_without_probing_for_calibration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage_key = storage_key_for_path(dir.path()).expect("storage key");
        let mut store = VerifierTuningStore::default();
        store.upsert(VerifierTuningProfile {
            storage_key: storage_key.clone(),
            calibrated_path: dir.path().to_path_buf(),
            workers: 6,
            logical_cpus: 8,
            median_mib_per_second: 4000.0,
            selected_samples: 9,
            selected_verification_runs: 45,
            relative_mad: 0.03,
            decision_relative_mad: 0.04,
            global_relative_mad: 0.05,
            selected_slowdown_vs_fastest: 0.01,
            stability: CalibrationStability::Medium,
            calibrated_at_unix_seconds: 42,
        });

        match tuning_plan_for_path(&store, dir.path()).expect("plan") {
            VerifierTuningPlan::UseProfile {
                storage_key: key,
                workers,
                stability,
            } => {
                assert_eq!(key, storage_key);
                assert_eq!(workers, 6);
                assert_eq!(stability, CalibrationStability::Medium);
            }
            other => panic!("expected stored profile, got {other:?}"),
        }
    }

    #[test]
    fn writable_storage_without_profile_requests_calibration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = VerifierTuningStore::default();
        let mut probes = 0_usize;

        let plan = tuning_plan_for_path_with_probe(&store, dir.path(), |_| {
            probes += 1;
            true
        })
        .expect("plan");

        match plan {
            VerifierTuningPlan::Calibrate {
                storage_key,
                directory,
            } => {
                assert_eq!(storage_key, storage_key_for_path(dir.path()).expect("storage key"));
                assert_eq!(
                    fs::canonicalize(directory).expect("selected canonical"),
                    fs::canonicalize(dir.path()).expect("temp canonical")
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
        let mut probes = 0_usize;

        let plan = tuning_plan_for_path_with_probe(&store, dir.path(), |_| {
            probes += 1;
            false
        })
        .expect("plan");

        match plan {
            VerifierTuningPlan::Fallback {
                storage_key,
                workers,
            } => {
                assert_eq!(storage_key, storage_key_for_path(dir.path()).expect("storage key"));
                assert_eq!(workers, SAFE_FALLBACK_WORKERS);
            }
            other => panic!("expected fallback plan, got {other:?}"),
        }

        assert!(probes >= 1);
    }

    #[test]
    fn existing_profile_wins_even_when_storage_is_readonly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage_key = storage_key_for_path(dir.path()).expect("storage key");
        let mut store = VerifierTuningStore::default();
        store.upsert(test_profile(
            storage_key.clone(),
            dir.path().to_path_buf(),
            6,
            CalibrationStability::Medium,
        ));

        let mut probes = 0_usize;
        let plan = tuning_plan_for_path_with_probe(&store, dir.path(), |_| {
            probes += 1;
            false
        })
        .expect("plan");

        match plan {
            VerifierTuningPlan::UseProfile {
                storage_key: key,
                workers,
                stability,
            } => {
                assert_eq!(key, storage_key);
                assert_eq!(workers, 6);
                assert_eq!(stability, CalibrationStability::Medium);
            }
            other => panic!("expected stored profile, got {other:?}"),
        }

        assert_eq!(probes, 0, "saved profiles must not require a write probe");
    }

    #[test]
    fn writable_storage_without_profile_calibrates_and_persists_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store_path = dir.path().join("verifier-tuning.json");
        let storage_key = storage_key_for_path(dir.path()).expect("storage key");
        let store = VerifierTuningStore::default();

        let plan = tuning_plan_for_path_with_probe(&store, dir.path(), |_| true)
            .expect("plan");
        let VerifierTuningPlan::Calibrate { directory, .. } = plan else {
            panic!("expected calibration plan");
        };

        let expected_profile = test_profile(
            storage_key.clone(),
            directory.clone(),
            6,
            CalibrationStability::Medium,
        );

        let report = calibrate_and_store_with(
            &directory,
            &store_path,
            CalibrationOptions::default(),
            |_, _| Ok(test_report(expected_profile.clone())),
        )
        .expect("calibrate and store");

        assert_eq!(report.profile.workers, 6);
        let loaded = VerifierTuningStore::load(&store_path).expect("load stored profile");
        let saved = loaded
            .profile_for_storage_key(&storage_key)
            .expect("stored profile");
        assert_eq!(saved.workers, 6);
        assert_eq!(saved.stability, CalibrationStability::Medium);
    }

    #[test]
    fn invalid_calibration_does_not_overwrite_good_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store_path = dir.path().join("verifier-tuning.json");
        let storage_key = storage_key_for_path(dir.path()).expect("storage key");

        let mut store = VerifierTuningStore::default();
        store.upsert(test_profile(
            storage_key.clone(),
            dir.path().to_path_buf(),
            6,
            CalibrationStability::High,
        ));
        store.save(&store_path).expect("save good profile");

        let invalid_profile = test_profile(
            storage_key.clone(),
            dir.path().to_path_buf(),
            8,
            CalibrationStability::Invalid,
        );

        let error = calibrate_and_store_with(
            dir.path(),
            &store_path,
            CalibrationOptions::default(),
            |_, _| Ok(test_report(invalid_profile)),
        )
        .expect_err("invalid calibration must not be stored");

        assert!(error.to_string().contains("too unstable"));

        let loaded = VerifierTuningStore::load(&store_path).expect("reload good profile");
        let saved = loaded
            .profile_for_storage_key(&storage_key)
            .expect("good profile must remain");
        assert_eq!(saved.workers, 6);
        assert_eq!(saved.stability, CalibrationStability::High);
    }

    #[test]
    fn same_volume_with_different_path_reuses_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::create_dir_all(&first).expect("first dir");
        fs::create_dir_all(&second).expect("second dir");

        let first_key = storage_key_for_path(&first).expect("first storage key");
        let second_key = storage_key_for_path(&second).expect("second storage key");
        assert_eq!(first_key, second_key);

        let mut store = VerifierTuningStore::default();
        store.upsert(test_profile(
            first_key.clone(),
            first.clone(),
            6,
            CalibrationStability::Medium,
        ));

        match tuning_plan_for_path(&store, &second).expect("plan") {
            VerifierTuningPlan::UseProfile {
                storage_key,
                workers,
                ..
            } => {
                assert_eq!(storage_key, first_key);
                assert_eq!(workers, 6);
            }
            other => panic!("expected same-volume profile reuse, got {other:?}"),
        }
    }

    #[test]
    fn temporary_calibration_files_are_removed_after_dataset_drop() {
        let dir = tempfile::tempdir().expect("tempdir");

        {
            let dataset = CalibrationDataset::create(dir.path(), 2, 64 * 1024)
                .expect("create calibration dataset");
            assert!(dataset.root.exists());
            assert!(dataset.root.starts_with(dir.path()));
        }

        let leftovers = fs::read_dir(dir.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".krokiet-verifier-tuning-")
            })
            .collect::<Vec<_>>();

        assert!(
            leftovers.is_empty(),
            "calibration temporary directories were not removed: {leftovers:?}"
        );
    }

    #[test]
    fn high_profile_replaces_medium_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = storage_key_for_path(dir.path()).expect("storage key");
        let mut store = VerifierTuningStore::default();
        store.upsert(test_profile(
            key.clone(),
            dir.path().to_path_buf(),
            4,
            CalibrationStability::Medium,
        ));

        let replaced = store.upsert_calibrated_if_preferred(test_profile(
            key.clone(),
            dir.path().to_path_buf(),
            6,
            CalibrationStability::High,
        ));

        assert!(replaced);
        let profile = store.profile_for_storage_key(&key).expect("profile");
        assert_eq!(profile.workers, 6);
        assert_eq!(profile.stability, CalibrationStability::High);
    }

    #[test]
    fn medium_profile_does_not_replace_high_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = storage_key_for_path(dir.path()).expect("storage key");
        let mut store = VerifierTuningStore::default();
        store.upsert(test_profile(
            key.clone(),
            dir.path().to_path_buf(),
            6,
            CalibrationStability::High,
        ));

        let replaced = store.upsert_calibrated_if_preferred(test_profile(
            key.clone(),
            dir.path().to_path_buf(),
            8,
            CalibrationStability::Medium,
        ));

        assert!(!replaced);
        let profile = store.profile_for_storage_key(&key).expect("profile");
        assert_eq!(profile.workers, 6);
        assert_eq!(profile.stability, CalibrationStability::High);
    }

    #[test]
    fn low_profile_does_not_replace_stronger_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = storage_key_for_path(dir.path()).expect("storage key");
        let mut store = VerifierTuningStore::default();
        store.upsert(test_profile(
            key.clone(),
            dir.path().to_path_buf(),
            6,
            CalibrationStability::High,
        ));

        let replaced = store.upsert_calibrated_if_preferred(test_profile(
            key.clone(),
            dir.path().to_path_buf(),
            8,
            CalibrationStability::Low,
        ));

        assert!(!replaced);
        let profile = store.profile_for_storage_key(&key).expect("profile");
        assert_eq!(profile.workers, 6);
        assert_eq!(profile.stability, CalibrationStability::High);
    }

    #[test]
    fn low_profile_is_saved_when_storage_has_no_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = storage_key_for_path(dir.path()).expect("storage key");
        let mut store = VerifierTuningStore::default();

        let stored = store.upsert_calibrated_if_preferred(test_profile(
            key.clone(),
            dir.path().to_path_buf(),
            4,
            CalibrationStability::Low,
        ));

        assert!(stored);
        assert_eq!(
            store.profile_for_storage_key(&key).expect("profile").workers,
            4
        );
    }

    #[test]
    fn resolver_reuses_saved_profile_without_calibrating() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store_path = dir.path().join("verifier-tuning.json");
        let key = storage_key_for_path(dir.path()).expect("storage key");
        let mut store = VerifierTuningStore::default();
        store.upsert(test_profile(
            key.clone(),
            dir.path().to_path_buf(),
            6,
            CalibrationStability::High,
        ));
        store.save(&store_path).expect("save profile");

        let mut probes = 0_usize;
        let resolution = resolve_tuning_for_path_with(
            dir.path(),
            &store_path,
            CalibrationOptions::default(),
            |_| {
                probes += 1;
                false
            },
            |_, _| panic!("saved profile must skip calibration"),
        )
        .expect("resolve");

        assert_eq!(probes, 0);
        assert_eq!(resolution.workers(), 6);
        assert!(matches!(
            resolution,
            VerifierTuningResolution::ReusedProfile {
                stability: CalibrationStability::High,
                ..
            }
        ));
    }

    #[test]
    fn resolver_calibrates_persists_and_next_run_reuses_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store_path = dir.path().join("verifier-tuning.json");
        let key = storage_key_for_path(dir.path()).expect("storage key");
        let expected = test_profile(
            key.clone(),
            dir.path().to_path_buf(),
            6,
            CalibrationStability::Medium,
        );

        let first = resolve_tuning_for_path_with(
            dir.path(),
            &store_path,
            CalibrationOptions::default(),
            |_| true,
            |_, _| Ok(test_report(expected.clone())),
        )
        .expect("first resolve");
        assert_eq!(first.workers(), 6);
        assert!(matches!(first, VerifierTuningResolution::Calibrated { .. }));

        let mut probes = 0_usize;
        let second = resolve_tuning_for_path_with(
            dir.path(),
            &store_path,
            CalibrationOptions::default(),
            |_| {
                probes += 1;
                false
            },
            |_, _| panic!("second run must reuse persisted profile"),
        )
        .expect("second resolve");

        assert_eq!(probes, 0);
        assert_eq!(second.workers(), 6);
        assert!(matches!(second, VerifierTuningResolution::ReusedProfile { .. }));
    }

    #[test]
    fn resolver_falls_back_when_calibration_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store_path = dir.path().join("verifier-tuning.json");

        let resolution = resolve_tuning_for_path_with(
            dir.path(),
            &store_path,
            CalibrationOptions::default(),
            |_| true,
            |_, _| Err(io::Error::other("synthetic calibration failure")),
        )
        .expect("resolve");

        assert_eq!(resolution.workers(), SAFE_FALLBACK_WORKERS);
        assert!(matches!(resolution, VerifierTuningResolution::Fallback { .. }));
        assert!(!store_path.exists());
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
            "MAD decision  : {:.2}%",
            report.profile.decision_relative_mad * 100.0
        );
        println!(
            "MAD global    : {:.2}% (diagnostico)",
            report.profile.global_relative_mad * 100.0
        );
        println!("Estabilidad   : {}", report.profile.stability.label_es());
        if report.profile.stability == CalibrationStability::Invalid {
            println!("Perfil        : NO GUARDAR; repetir con el sistema menos cargado");
        } else {
            println!("Perfil        : GUARDABLE");
        }
        println!("============================================================");
    }
}
