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

const TUNING_FORMAT_VERSION: u64 = 1;
const DEFAULT_FILE_SIZE: u64 = 4 * 1024 * 1024;
const DEFAULT_ROUNDS: usize = 3;
const DEFAULT_MAX_WORKERS: usize = 16;
const DEFAULT_TIE_MARGIN: f64 = 0.05;
const MIN_GROUPS: usize = 8;

/// Controls the one-off calibration workload.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CalibrationOptions {
    /// Size of each temporary file. Every group contains two identical files.
    pub file_size: u64,
    /// Number of timed repetitions for every candidate worker count.
    pub rounds: usize,
    /// Safety ceiling for the number of verifier workers tested.
    pub max_workers: usize,
    /// If several candidates are within this fraction of the fastest median,
    /// prefer the one using fewer workers.
    pub tie_margin: f64,
}

impl Default for CalibrationOptions {
    fn default() -> Self {
        Self {
            file_size: DEFAULT_FILE_SIZE,
            rounds: DEFAULT_ROUNDS,
            max_workers: DEFAULT_MAX_WORKERS,
            tie_margin: DEFAULT_TIE_MARGIN,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TuningMeasurement {
    pub workers: usize,
    pub median: Duration,
    pub median_mib_per_second: f64,
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
    pub calibrated_at_unix_seconds: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct CalibrationReport {
    pub profile: VerifierTuningProfile,
    pub measurements: Vec<TuningMeasurement>,
    pub groups: usize,
    pub bytes_per_run: u64,
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
            // Unknown versions are ignored rather than risking a stale tuning
            // decision after the format or benchmark changes.
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

            let median_mib_per_second = item
                .get("median_mib_per_second")
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
            if workers == 0 || logical_cpus == 0 {
                continue;
            }

            store.upsert(VerifierTuningProfile {
                storage_key: storage_key.to_owned(),
                calibrated_path: PathBuf::from(calibrated_path),
                workers,
                logical_cpus,
                median_mib_per_second,
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
            serde_json::to_writer_pretty(&mut writer, &document)
                .map_err(io::Error::other)?;
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
/// benchmarks a small set of worker counts, chooses the least-concurrent option
/// within `tie_margin` of the fastest median, and removes the temporary files.
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
            format!("calibration path is not a directory: {}", calibrated_path.display()),
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

    let mut samples: HashMap<usize, Vec<Duration>> = candidates
        .iter()
        .copied()
        .map(|workers| (workers, Vec::with_capacity(options.rounds)))
        .collect();

    for round in 0..options.rounds {
        let mut order = candidates.clone();
        if !order.is_empty() {
            let order_len = order.len();
            order.rotate_left(round % order_len);
            if round % 2 == 1 {
                order.reverse();
            }
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

    let mut measurements = Vec::with_capacity(candidates.len());
    for workers in candidates {
        let values = samples
            .get_mut(&workers)
            .expect("sample bucket exists for every candidate");
        let median = median_duration(values);
        let seconds = median.as_secs_f64().max(f64::MIN_POSITIVE);
        let median_mib_per_second = dataset.bytes_per_run as f64 / (1024.0 * 1024.0) / seconds;
        measurements.push(TuningMeasurement {
            workers,
            median,
            median_mib_per_second,
        });
    }
    measurements.sort_by_key(|measurement| measurement.workers);

    let fastest_seconds = measurements
        .iter()
        .map(|measurement| measurement.median.as_secs_f64())
        .fold(f64::INFINITY, f64::min);
    let acceptable_seconds = fastest_seconds * (1.0 + options.tie_margin);
    let selected = measurements
        .iter()
        .filter(|measurement| measurement.median.as_secs_f64() <= acceptable_seconds)
        .min_by_key(|measurement| measurement.workers)
        .expect("at least one tuning measurement");

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
            calibrated_at_unix_seconds,
        },
        measurements,
        groups: dataset.group_count,
        bytes_per_run: dataset.bytes_per_run,
    })
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

    // Include useful non-power-of-two points. On an 8-thread machine this gives
    // 1/2/4/6/8, matching the range that proved useful during development.
    candidates.insert((limit / 2).max(1));
    candidates.insert(((limit.saturating_mul(3)).div_ceil(4)).max(1));
    candidates.insert(limit);

    candidates.into_iter().filter(|workers| *workers <= limit).collect()
}

fn median_duration(values: &mut [Duration]) -> Duration {
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        Duration::from_secs_f64(
            (values[middle - 1].as_secs_f64() + values[middle].as_secs_f64()) / 2.0,
        )
    }
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
            format!("windows-device:{}", device.to_string_lossy().to_ascii_lowercase())
        }
        Prefix::Verbatim(value) => {
            format!("windows-verbatim:{}", value.to_string_lossy().to_ascii_lowercase())
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

    #[test]
    fn eight_logical_cpus_include_the_development_sweet_spots() {
        assert_eq!(candidate_worker_counts(8, 16), vec![1, 2, 4, 6, 8]);
    }

    #[test]
    fn worker_candidates_respect_cpu_and_safety_caps() {
        assert_eq!(candidate_worker_counts(1, 16), vec![1]);
        assert_eq!(candidate_worker_counts(4, 16), vec![1, 2, 3, 4]);
        assert_eq!(candidate_worker_counts(32, 16), vec![1, 2, 4, 8, 12, 16]);
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
            calibrated_at_unix_seconds: 42,
        });
        store.save(&settings).expect("save");

        let loaded = VerifierTuningStore::load(&settings).expect("load");
        assert_eq!(loaded.profiles().len(), 1);
        assert_eq!(loaded.profiles()[0].workers, 3);
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
        println!("Lectura/ronda : {:.1} MiB", report.bytes_per_run as f64 / 1024.0 / 1024.0);
        println!();
        for measurement in &report.measurements {
            println!(
                "workers-{:>2}: {:>10.3?} | {:>9.1} MiB/s",
                measurement.workers,
                measurement.median,
                measurement.median_mib_per_second,
            );
        }
        println!("------------------------------------------------------------");
        println!("Seleccion     : {} worker(s)", report.profile.workers);
        println!(
            "Rendimiento   : {:.1} MiB/s",
            report.profile.median_mib_per_second
        );
        println!("============================================================");
    }
}
