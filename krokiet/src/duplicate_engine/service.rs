//! Integration boundary for duplicate scanning.
//!
//! This module deliberately owns orchestration, not detection details. Future CLI
//! and GUI callers can submit one request here without knowing whether candidates
//! came from Czkawka or fclones, how verifier workers were selected, or whether a
//! stored tuning profile was reused.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::czkawka::CzkawkaEngine;
use super::fclones::FclonesEngine;
use super::tuning::{
    CalibrationOptions, CalibrationStability, SAFE_FALLBACK_WORKERS,
    VerifierTuningFallbackReason, VerifierTuningResolution, resolve_tuning_for_path,
    storage_key_for_path,
};
use super::types::{
    DuplicateEngine, DuplicateEngineError, DuplicateScanRequest, DuplicateScanResult,
};
use super::verify::verify_result_with_workers;

/// Engine selected by the future command/UI layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DuplicateServiceEngine {
    Czkawka,
    Fast,
}

/// Where the exact-verifier worker count came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TuningSource {
    ReusedProfile,
    Calibrated,
    Fallback,
}

/// Why the service had to use the conservative verifier worker count.
///
/// These are intentionally category-only reasons: they are useful in production
/// logs without embedding user paths or raw OS error strings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TuningFallbackReason {
    NoWritableCalibrationDirectory,
    CalibrationInvalid,
    CalibrationExecutionFailed,
    ProfilePersistenceFailed,
    TuningUnavailable,
    StorageIdentificationFailed,
}

impl From<VerifierTuningFallbackReason> for TuningFallbackReason {
    fn from(value: VerifierTuningFallbackReason) -> Self {
        match value {
            VerifierTuningFallbackReason::NoWritableCalibrationDirectory => {
                Self::NoWritableCalibrationDirectory
            }
            VerifierTuningFallbackReason::CalibrationInvalid => Self::CalibrationInvalid,
            VerifierTuningFallbackReason::CalibrationExecutionFailed => {
                Self::CalibrationExecutionFailed
            }
            VerifierTuningFallbackReason::ProfilePersistenceFailed => {
                Self::ProfilePersistenceFailed
            }
        }
    }
}

/// Production-friendly tuning trace for one storage.
///
/// This is intentionally small so it can later be written to logs, surfaced by a
/// CLI command, or shown in a diagnostic UI without exposing tuning internals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StorageTuningTrace {
    pub storage_key: String,
    pub workers: usize,
    pub source: TuningSource,
    pub stability: Option<CalibrationStability>,
    pub fallback_reason: Option<TuningFallbackReason>,
    /// Raw diagnostic detail for the standalone diagnostic command/tests. It may
    /// contain a local path or OS error and must not be emitted by future GUI
    /// production logging without sanitization.
    pub diagnostic_detail: Option<String>,
}

/// Options owned by the orchestration layer.
#[derive(Clone, Debug)]
pub(crate) struct DuplicateScanServiceOptions {
    pub engine: DuplicateServiceEngine,
    pub tuning_store_path: Option<PathBuf>,
    pub calibration_options: CalibrationOptions,
}

impl DuplicateScanServiceOptions {
    pub(crate) fn czkawka() -> Self {
        Self {
            engine: DuplicateServiceEngine::Czkawka,
            tuning_store_path: None,
            calibration_options: CalibrationOptions::default(),
        }
    }

    pub(crate) fn fast(tuning_store_path: PathBuf) -> Self {
        Self {
            engine: DuplicateServiceEngine::Fast,
            tuning_store_path: Some(tuning_store_path),
            calibration_options: CalibrationOptions::default(),
        }
    }
}

/// Result returned by the service boundary.
///
/// `result.elapsed` remains the detector's own scan time. The extra timings below
/// make the full Fast pipeline observable without changing the existing common
/// result format.
#[derive(Clone, Debug)]
pub(crate) struct DuplicateScanExecution {
    pub result: DuplicateScanResult,
    pub verifier_workers: Option<usize>,
    pub storage_tuning: Vec<StorageTuningTrace>,
    pub tuning_elapsed: Duration,
    pub verification_elapsed: Duration,
    pub total_elapsed: Duration,
}

/// Future command/UI entry point.
///
/// Czkawka keeps behaving as the reference engine. The Fast path first lets
/// fclones discover real candidate groups. Only when candidates exist does it
/// resolve tuning for the storages that actually contain candidate files. The
/// smallest worker budget is used across those storages, and only groups that
/// pass the independent exact byte-for-byte verifier are returned.
pub(crate) fn run_duplicate_scan(
    request: &DuplicateScanRequest,
    options: &DuplicateScanServiceOptions,
) -> Result<DuplicateScanExecution, DuplicateEngineError> {
    run_duplicate_scan_with_tuning(request, options, |path, store_path, calibration_options| {
        resolve_tuning_for_path(path, store_path, calibration_options)
    })
}

fn run_duplicate_scan_with_tuning<F>(
    request: &DuplicateScanRequest,
    options: &DuplicateScanServiceOptions,
    mut resolve: F,
) -> Result<DuplicateScanExecution, DuplicateEngineError>
where
    F: FnMut(&Path, &Path, CalibrationOptions) -> io::Result<VerifierTuningResolution>,
{
    request.validate()?;
    let total_started = Instant::now();

    match options.engine {
        DuplicateServiceEngine::Czkawka => {
            let result = CzkawkaEngine.scan(request)?;
            Ok(DuplicateScanExecution {
                result,
                verifier_workers: None,
                storage_tuning: Vec::new(),
                tuning_elapsed: Duration::ZERO,
                verification_elapsed: Duration::ZERO,
                total_elapsed: total_started.elapsed(),
            })
        }
        DuplicateServiceEngine::Fast => {
            // Detection comes first. A scan with no candidate groups has nothing
            // to exact-verify, so it must not pay the calibration cost.
            let mut result = FclonesEngine.scan(request)?;
            if result.groups.is_empty() {
                return Ok(DuplicateScanExecution {
                    result,
                    verifier_workers: None,
                    storage_tuning: Vec::new(),
                    tuning_elapsed: Duration::ZERO,
                    verification_elapsed: Duration::ZERO,
                    total_elapsed: total_started.elapsed(),
                });
            }

            let store_path = options.tuning_store_path.as_deref().ok_or_else(|| {
                DuplicateEngineError::Configuration(
                    "fast duplicate service requires a verifier tuning store path".to_string(),
                )
            })?;

            let tuning_started = Instant::now();
            let (storage_tuning, verifier_workers) =
                match candidate_storage_representatives(&result) {
                    Ok(representatives) => {
                        let mut traces = Vec::with_capacity(representatives.len());

                        for (storage_key, representative) in representatives {
                            let trace = match resolve(
                                &representative,
                                store_path,
                                options.calibration_options,
                            ) {
                                Ok(resolution) => trace_from_resolution(resolution),
                                Err(error) => fallback_trace(
                                    storage_key,
                                    TuningFallbackReason::TuningUnavailable,
                                    Some(error.to_string()),
                                ),
                            };
                            traces.push(trace);
                        }

                        // A scan may span NVMe + HDD + USB at the same time. Until
                        // we have a per-storage scheduler, use the smallest budget
                        // so the verifier never assumes every candidate storage is
                        // as fast as the fastest one.
                        let workers = traces
                            .iter()
                            .map(|trace| trace.workers)
                            .min()
                            .unwrap_or(SAFE_FALLBACK_WORKERS)
                            .max(1);

                        (traces, workers)
                    }
                    Err(error) => (
                        vec![fallback_trace(
                            "unavailable".to_string(),
                            TuningFallbackReason::StorageIdentificationFailed,
                            Some(error.to_string()),
                        )],
                        SAFE_FALLBACK_WORKERS,
                    ),
                };
            let tuning_elapsed = tuning_started.elapsed();

            let verification_started = Instant::now();
            verify_result_with_workers(&mut result, verifier_workers)?;
            let verification_elapsed = verification_started.elapsed();

            Ok(DuplicateScanExecution {
                result,
                verifier_workers: Some(verifier_workers),
                storage_tuning,
                tuning_elapsed,
                verification_elapsed,
                total_elapsed: total_started.elapsed(),
            })
        }
    }
}

/// Candidate storage discovery deliberately happens after fclones. This avoids
/// calibrating a requested root that produced no duplicate candidates and also
/// notices candidate files reached through mounts/junctions when one-file-system
/// mode is disabled.
fn candidate_storage_representatives(
    result: &DuplicateScanResult,
) -> io::Result<BTreeMap<String, PathBuf>> {
    let mut representatives = BTreeMap::<String, PathBuf>::new();

    for group in &result.groups {
        for file in &group.files {
            let storage_key = storage_key_for_path(&file.path)?;
            representatives
                .entry(storage_key)
                .or_insert_with(|| file.path.clone());
        }
    }

    Ok(representatives)
}

fn fallback_trace(
    storage_key: String,
    reason: TuningFallbackReason,
    diagnostic_detail: Option<String>,
) -> StorageTuningTrace {
    StorageTuningTrace {
        storage_key,
        workers: SAFE_FALLBACK_WORKERS,
        source: TuningSource::Fallback,
        stability: None,
        fallback_reason: Some(reason),
        diagnostic_detail,
    }
}

fn trace_from_resolution(resolution: VerifierTuningResolution) -> StorageTuningTrace {
    match resolution {
        VerifierTuningResolution::ReusedProfile {
            storage_key,
            workers,
            stability,
        } => StorageTuningTrace {
            storage_key,
            workers,
            source: TuningSource::ReusedProfile,
            stability: Some(stability),
            fallback_reason: None,
            diagnostic_detail: None,
        },
        VerifierTuningResolution::Calibrated {
            storage_key,
            workers,
            stability,
        } => StorageTuningTrace {
            storage_key,
            workers,
            source: TuningSource::Calibrated,
            stability: Some(stability),
            fallback_reason: None,
            diagnostic_detail: None,
        },
        VerifierTuningResolution::Fallback {
            storage_key,
            workers,
            reason,
            detail,
        } => StorageTuningTrace {
            storage_key,
            workers,
            source: TuningSource::Fallback,
            stability: None,
            fallback_reason: Some(reason.into()),
            diagnostic_detail: detail,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::duplicate_engine::compare_results;

    fn write_pair(directory: &Path, stem: &str, value: u8, size: usize) {
        let bytes = vec![value; size];
        fs::write(directory.join(format!("{stem}-a.bin")), &bytes).expect("write a");
        fs::write(directory.join(format!("{stem}-b.bin")), &bytes).expect("write b");
    }

    #[test]
    fn fast_service_runs_detector_and_exact_verifier() {
        let dir = tempdir().expect("tempdir");
        write_pair(dir.path(), "same", 0x4B, 96 * 1024);
        fs::write(
            dir.path().join("different-a.bin"),
            vec![0x11; 80 * 1024],
        )
        .expect("write different a");
        fs::write(
            dir.path().join("different-b.bin"),
            vec![0x22; 80 * 1024],
        )
        .expect("write different b");

        let request = DuplicateScanRequest::for_paths([dir.path().to_path_buf()]);
        let options = DuplicateScanServiceOptions::fast(dir.path().join("tuning.json"));
        let key = storage_key_for_path(dir.path()).expect("storage key");

        let execution = run_duplicate_scan_with_tuning(&request, &options, |_, _, _| {
            Ok(VerifierTuningResolution::ReusedProfile {
                storage_key: key.clone(),
                workers: 4,
                stability: CalibrationStability::High,
            })
        })
        .expect("service scan");

        assert_eq!(execution.verifier_workers, Some(4));
        assert_eq!(execution.storage_tuning.len(), 1);
        assert_eq!(
            execution.storage_tuning[0].source,
            TuningSource::ReusedProfile
        );
        assert_eq!(execution.storage_tuning[0].fallback_reason, None);
        assert_eq!(execution.result.groups.len(), 1);
        assert_eq!(execution.result.file_count(), 2);
        assert!(execution.result.groups.iter().all(|group| group.verified));
    }

    #[test]
    fn same_storage_is_resolved_only_once_for_multiple_paths() {
        let dir = tempdir().expect("tempdir");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::create_dir_all(&first).expect("first");
        fs::create_dir_all(&second).expect("second");
        write_pair(&first, "left", 0x33, 64 * 1024);
        write_pair(&second, "right", 0x77, 64 * 1024);

        let request = DuplicateScanRequest::for_paths([first, second]);
        let options = DuplicateScanServiceOptions::fast(dir.path().join("tuning.json"));
        let key = storage_key_for_path(dir.path()).expect("storage key");
        let mut resolutions = 0_usize;

        let execution = run_duplicate_scan_with_tuning(&request, &options, |_, _, _| {
            resolutions += 1;
            Ok(VerifierTuningResolution::ReusedProfile {
                storage_key: key.clone(),
                workers: 6,
                stability: CalibrationStability::Medium,
            })
        })
        .expect("service scan");

        assert_eq!(resolutions, 1, "same storage must not be tuned twice");
        assert_eq!(execution.storage_tuning.len(), 1);
        assert_eq!(execution.verifier_workers, Some(6));
        assert_eq!(execution.result.groups.len(), 2);
    }

    #[test]
    fn fallback_reason_is_visible_in_execution_trace() {
        let dir = tempdir().expect("tempdir");
        write_pair(dir.path(), "same", 0x5A, 32 * 1024);

        let request = DuplicateScanRequest::for_paths([dir.path().to_path_buf()]);
        let options = DuplicateScanServiceOptions::fast(dir.path().join("tuning.json"));
        let key = storage_key_for_path(dir.path()).expect("storage key");

        let execution = run_duplicate_scan_with_tuning(&request, &options, |_, _, _| {
            Ok(VerifierTuningResolution::Fallback {
                storage_key: key.clone(),
                workers: SAFE_FALLBACK_WORKERS,
                reason: VerifierTuningFallbackReason::CalibrationExecutionFailed,
                detail: Some("synthetic calibration failure".to_string()),
            })
        })
        .expect("service scan");

        assert_eq!(execution.verifier_workers, Some(SAFE_FALLBACK_WORKERS));
        assert_eq!(execution.storage_tuning[0].source, TuningSource::Fallback);
        assert_eq!(execution.storage_tuning[0].stability, None);
        assert_eq!(
            execution.storage_tuning[0].fallback_reason,
            Some(TuningFallbackReason::CalibrationExecutionFailed)
        );
        assert_eq!(
            execution.storage_tuning[0].diagnostic_detail.as_deref(),
            Some("synthetic calibration failure")
        );
        assert!(execution.result.groups.iter().all(|group| group.verified));
    }

    #[test]
    fn no_candidates_skip_tuning_and_verifier() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("a.bin"), vec![0x11; 64 * 1024]).expect("write a");
        fs::write(dir.path().join("b.bin"), vec![0x22; 65 * 1024]).expect("write b");

        let request = DuplicateScanRequest::for_paths([dir.path().to_path_buf()]);
        let options = DuplicateScanServiceOptions::fast(dir.path().join("tuning.json"));

        let execution = run_duplicate_scan_with_tuning(&request, &options, |_, _, _| {
            panic!("tuning must not run when fclones found no candidate groups")
        })
        .expect("service scan");

        assert!(execution.result.groups.is_empty());
        assert_eq!(execution.verifier_workers, None);
        assert!(execution.storage_tuning.is_empty());
        assert_eq!(execution.tuning_elapsed, Duration::ZERO);
        assert_eq!(execution.verification_elapsed, Duration::ZERO);
    }

    #[test]
    fn tuning_error_uses_safe_fallback_without_aborting_scan() {
        let dir = tempdir().expect("tempdir");
        write_pair(dir.path(), "same", 0x6C, 48 * 1024);

        let request = DuplicateScanRequest::for_paths([dir.path().to_path_buf()]);
        let options = DuplicateScanServiceOptions::fast(dir.path().join("tuning.json"));

        let execution = run_duplicate_scan_with_tuning(&request, &options, |_, _, _| {
            Err(io::Error::other("synthetic tuning store failure"))
        })
        .expect("tuning failure must not abort duplicate verification");

        assert_eq!(execution.verifier_workers, Some(SAFE_FALLBACK_WORKERS));
        assert_eq!(execution.storage_tuning.len(), 1);
        assert_eq!(execution.storage_tuning[0].source, TuningSource::Fallback);
        assert_eq!(
            execution.storage_tuning[0].fallback_reason,
            Some(TuningFallbackReason::TuningUnavailable)
        );
        assert_eq!(
            execution.storage_tuning[0].diagnostic_detail.as_deref(),
            Some("synthetic tuning store failure")
        );
        assert!(execution.result.groups.iter().all(|group| group.verified));
    }

    #[test]
    fn fast_service_result_matches_direct_fast_pipeline() {
        let dir = tempdir().expect("tempdir");
        write_pair(dir.path(), "one", 0x12, 72 * 1024);
        write_pair(dir.path(), "two", 0x34, 88 * 1024);

        let request = DuplicateScanRequest::for_paths([dir.path().to_path_buf()]);
        let options = DuplicateScanServiceOptions::fast(dir.path().join("tuning.json"));
        let key = storage_key_for_path(dir.path()).expect("storage key");

        let service = run_duplicate_scan_with_tuning(&request, &options, |_, _, _| {
            Ok(VerifierTuningResolution::ReusedProfile {
                storage_key: key.clone(),
                workers: 2,
                stability: CalibrationStability::High,
            })
        })
        .expect("service scan");

        let mut direct = FclonesEngine.scan(&request).expect("direct fclones");
        verify_result_with_workers(&mut direct, 2).expect("direct verify");

        assert!(compare_results(&service.result, &direct).identical());
    }
}
