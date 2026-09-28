use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use rayon::prelude::*;

#[cfg(all(test, feature = "fast_duplicates"))]
use std::time::Duration;

use super::types::{DuplicateGroup, metadata_fingerprint};
#[cfg(feature = "fast_duplicates")]
use super::types::DuplicateScanResult;

const VERIFY_BUFFER_SIZE: usize = 1024 * 1024;
const MAX_CANDIDATES_PER_BATCH: usize = 16;

// Win32 FILE_FLAG_SEQUENTIAL_SCAN. Kept local so the experiment does not need
// another direct Windows dependency just to pass an OpenOptions hint.
#[cfg(windows)]
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VerifierIoStrategy {
    sequential_hint: bool,
    parallel_candidates: bool,
}

const IO_BASELINE: VerifierIoStrategy = VerifierIoStrategy {
    sequential_hint: false,
    parallel_candidates: false,
};

const IO_SEQUENTIAL_HINT: VerifierIoStrategy = VerifierIoStrategy {
    sequential_hint: true,
    parallel_candidates: false,
};

const IO_PARALLEL_CANDIDATES: VerifierIoStrategy = VerifierIoStrategy {
    sequential_hint: false,
    parallel_candidates: true,
};

const IO_PARALLEL_SEQUENTIAL: VerifierIoStrategy = VerifierIoStrategy {
    sequential_hint: true,
    parallel_candidates: true,
};

/// Backend-independent exact byte verifier.
///
/// Duplicate detection and exact verification are deliberately separate.
/// fclones/Czkawka decide which files are candidates; an `ExactVerifier`
/// decides whether a candidate group is truly byte-identical.
pub(crate) trait ExactVerifier {
    #[allow(dead_code)]
    fn name(&self) -> &'static str;

    fn verify_group(&self, group: &mut DuplicateGroup) -> io::Result<bool>;

    #[cfg(feature = "fast_duplicates")]
    fn verify_result(&self, result: &mut DuplicateScanResult) -> io::Result<usize> {
        let mut verified = 0;
        for group in &mut result.groups {
            if self.verify_group(group)? {
                verified += 1;
            }
        }
        Ok(verified)
    }
}

/// Production baseline for exact verification.
///
/// It keeps Krokiet's optimized grouped I/O strategy and uses Rust slice
/// equality for the in-memory comparison. For large byte slices this is the
/// baseline we want to keep measuring; no external comparison crate is needed.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct StdBufferedVerifier;

impl ExactVerifier for StdBufferedVerifier {
    fn name(&self) -> &'static str {
        "std-buffered"
    }

    fn verify_group(&self, group: &mut DuplicateGroup) -> io::Result<bool> {
        let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
        verify_group_with_io_strategy(group, &mut workspace, IO_BASELINE)
    }

    #[cfg(feature = "fast_duplicates")]
    fn verify_result(&self, result: &mut DuplicateScanResult) -> io::Result<usize> {
        verify_result_with_io_strategy(result, VERIFY_BUFFER_SIZE, IO_BASELINE)
    }
}

#[inline]
fn buffers_equal(left: &[u8], right: &[u8]) -> bool {
    left == right
}

struct VerificationWorkspace {
    buffer_size: usize,
    reference_buffer: Vec<u8>,
    candidate_buffer: Vec<u8>,
    parallel_candidate_buffers: Vec<Vec<u8>>,

    #[cfg(test)]
    reference_bytes_read: u64,
    #[cfg(test)]
    candidate_bytes_read: u64,
}

impl VerificationWorkspace {
    fn new(buffer_size: usize) -> Self {
        assert!(buffer_size > 0, "verification buffer size must be greater than zero");
        Self {
            buffer_size,
            reference_buffer: vec![0_u8; buffer_size],
            candidate_buffer: vec![0_u8; buffer_size],
            parallel_candidate_buffers: Vec::new(),
            #[cfg(test)]
            reference_bytes_read: 0,
            #[cfg(test)]
            candidate_bytes_read: 0,
        }
    }

    fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    fn ensure_parallel_candidate_buffers(&mut self, count: usize) {
        while self.parallel_candidate_buffers.len() < count {
            self.parallel_candidate_buffers.push(vec![0_u8; self.buffer_size]);
        }
    }

    #[cfg(test)]
    fn record_reference_read(&mut self, bytes: usize) {
        self.reference_bytes_read += bytes as u64;
    }

    #[cfg(not(test))]
    fn record_reference_read(&mut self, _bytes: usize) {}

    #[cfg(test)]
    fn record_candidate_read(&mut self, bytes: usize) {
        self.candidate_bytes_read += bytes as u64;
    }

    #[cfg(not(test))]
    fn record_candidate_read(&mut self, _bytes: usize) {}
}

fn capture_group_fingerprints(group: &DuplicateGroup) -> io::Result<Vec<(u64, Option<std::time::SystemTime>)>> {
    group
        .files
        .iter()
        .map(|file| metadata_fingerprint(&file.path))
        .collect()
}

fn all_sizes_match(fingerprints: &[(u64, Option<std::time::SystemTime>)]) -> bool {
    let Some(reference_size) = fingerprints.first().map(|fingerprint| fingerprint.0) else {
        return false;
    };

    fingerprints.iter().all(|fingerprint| fingerprint.0 == reference_size)
}

fn group_fingerprints_unchanged(group: &DuplicateGroup, before: &[(u64, Option<std::time::SystemTime>)]) -> io::Result<bool> {
    for (file, before_fingerprint) in group.files.iter().zip(before) {
        if metadata_fingerprint(&file.path)? != *before_fingerprint {
            return Ok(false);
        }
    }
    Ok(true)
}

fn open_for_verification(path: &Path, sequential_hint: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);

    #[cfg(windows)]
    if sequential_hint {
        options.custom_flags(FILE_FLAG_SEQUENTIAL_SCAN);
    }

    #[cfg(not(windows))]
    let _ = sequential_hint;

    options.open(path)
}

fn verify_group_buffered(group: &mut DuplicateGroup, workspace: &mut VerificationWorkspace) -> io::Result<bool> {
    verify_group_with_io_strategy(group, workspace, IO_BASELINE)
}

fn verify_group_with_io_strategy(
    group: &mut DuplicateGroup,
    workspace: &mut VerificationWorkspace,
    strategy: VerifierIoStrategy,
) -> io::Result<bool> {
    group.verified = false;

    if group.files.len() < 2 {
        return Ok(false);
    }

    let before = capture_group_fingerprints(group)?;
    if !all_sizes_match(&before) {
        return Ok(false);
    }

    let reference_size = before[0].0;
    let mut reference_file = open_for_verification(&group.files[0].path, strategy.sequential_hint)?;

    for candidate_batch in group.files[1..].chunks(MAX_CANDIDATES_PER_BATCH) {
        reference_file.seek(SeekFrom::Start(0))?;

        let mut candidate_files = candidate_batch
            .iter()
            .map(|candidate| open_for_verification(&candidate.path, strategy.sequential_hint))
            .collect::<io::Result<Vec<_>>>()?;

        if strategy.parallel_candidates {
            workspace.ensure_parallel_candidate_buffers(candidate_files.len());
        }

        let mut remaining = reference_size;
        while remaining > 0 {
            let amount = remaining.min(workspace.buffer_size() as u64) as usize;

            reference_file.read_exact(&mut workspace.reference_buffer[..amount])?;
            workspace.record_reference_read(amount);

            if strategy.parallel_candidates {
                let reference_buffer = &workspace.reference_buffer[..amount];
                let candidate_buffers = &mut workspace.parallel_candidate_buffers[..candidate_files.len()];

                let results = candidate_files
                    .par_iter_mut()
                    .zip(candidate_buffers.par_iter_mut())
                    .map(|(candidate_file, candidate_buffer)| -> io::Result<bool> {
                        candidate_file.read_exact(&mut candidate_buffer[..amount])?;
                        Ok(buffers_equal(reference_buffer, &candidate_buffer[..amount]))
                    })
                    .collect::<Vec<_>>();

                workspace.record_candidate_read(amount * candidate_files.len());

                for result in results {
                    if !result? {
                        return Ok(false);
                    }
                }
            } else {
                for candidate_file in &mut candidate_files {
                    candidate_file.read_exact(&mut workspace.candidate_buffer[..amount])?;
                    workspace.record_candidate_read(amount);

                    if !buffers_equal(&workspace.reference_buffer[..amount], &workspace.candidate_buffer[..amount]) {
                        return Ok(false);
                    }
                }
            }

            remaining -= amount as u64;
        }
    }

    if !group_fingerprints_unchanged(group, &before)? {
        return Ok(false);
    }

    group.verified = true;
    Ok(true)
}

#[cfg(feature = "fast_duplicates")]
fn verify_result_with_io_strategy(
    result: &mut DuplicateScanResult,
    buffer_size: usize,
    strategy: VerifierIoStrategy,
) -> io::Result<usize> {
    let mut verified = 0;
    let mut workspace = VerificationWorkspace::new(buffer_size);

    for group in &mut result.groups {
        if verify_group_with_io_strategy(group, &mut workspace, strategy)? {
            verified += 1;
        }
    }

    Ok(verified)
}

/// Compatibility entry point used by the existing tests and benchmark.
pub(crate) fn verify_group(group: &mut DuplicateGroup) -> io::Result<bool> {
    StdBufferedVerifier.verify_group(group)
}

/// Compatibility entry point used by the existing Fast Engine tests.
#[cfg(feature = "fast_duplicates")]
pub(crate) fn verify_result(result: &mut DuplicateScanResult) -> io::Result<usize> {
    StdBufferedVerifier.verify_result(result)
}

/// Compatibility shim for older `mod.rs` revisions that re-export
/// `verify_result` even without `fast_duplicates` enabled.
#[cfg(not(feature = "fast_duplicates"))]
#[allow(dead_code)]
pub(crate) fn verify_result<T>(_result: &mut T) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "exact result verification requires the fast_duplicates feature",
    ))
}

#[cfg(all(test, feature = "fast_duplicates"))]
fn median_duration(values: &mut [Duration]) -> Duration {
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        Duration::from_secs_f64((values[middle - 1].as_secs_f64() + values[middle].as_secs_f64()) / 2.0)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::*;
    use crate::duplicate_engine::DuplicateFile;

    fn group_from_paths(paths: Vec<PathBuf>) -> DuplicateGroup {
        DuplicateGroup::new(
            paths
                .into_iter()
                .map(|path| DuplicateFile::from_path(path).expect("metadata"))
                .collect(),
        )
    }

    fn create_equal_and_different_groups() -> (tempfile::TempDir, DuplicateGroup, DuplicateGroup) {
        let dir = tempdir().expect("tempdir");
        let identical = vec![0x47; 512 * 1024 + 19];
        let mut different = identical.clone();
        different[300_001] ^= 0x7F;

        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.bin");
        let c = dir.path().join("c.bin");
        fs::write(&a, &identical).expect("write a");
        fs::write(&b, &identical).expect("write b");
        fs::write(&c, &different).expect("write c");

        (
            dir,
            group_from_paths(vec![a.clone(), b]),
            group_from_paths(vec![a, c]),
        )
    }

    #[test]
    fn optimized_verifier_reads_reference_once_for_normal_group() {
        let dir = tempdir().expect("tempdir");
        let size = VERIFY_BUFFER_SIZE + 257;
        let data = vec![0xA5; size];

        let mut paths = Vec::new();
        for index in 0..6 {
            let path = dir.path().join(format!("same-{index}.bin"));
            fs::write(&path, &data).expect("write test file");
            paths.push(path);
        }

        let mut group = group_from_paths(paths);
        let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);

        assert!(verify_group_buffered(&mut group, &mut workspace).expect("verify"));
        assert!(group.verified);
        assert_eq!(workspace.reference_bytes_read, size as u64);
        assert_eq!(workspace.candidate_bytes_read, (size as u64) * 5);
    }

    #[test]
    fn optimized_verifier_batches_very_large_groups() {
        let dir = tempdir().expect("tempdir");
        let size = 64 * 1024 + 13;
        let data = vec![0x6D; size];

        let mut paths = Vec::new();
        for index in 0..18 {
            let path = dir.path().join(format!("large-group-{index}.bin"));
            fs::write(&path, &data).expect("write test file");
            paths.push(path);
        }

        let mut group = group_from_paths(paths);
        let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);

        assert!(verify_group_buffered(&mut group, &mut workspace).expect("verify"));
        assert!(group.verified);
        assert_eq!(workspace.reference_bytes_read, (size as u64) * 2);
        assert_eq!(workspace.candidate_bytes_read, (size as u64) * 17);
    }

    #[test]
    fn optimized_verifier_rejects_difference_in_late_chunk() {
        let dir = tempdir().expect("tempdir");
        let size = VERIFY_BUFFER_SIZE + 4096;
        let reference = vec![0x3C; size];
        let mut different = reference.clone();
        different[size - 17] ^= 0xFF;

        let reference_path = dir.path().join("reference.bin");
        let same_path = dir.path().join("same.bin");
        let different_path = dir.path().join("different.bin");

        fs::write(&reference_path, &reference).expect("write reference");
        fs::write(&same_path, &reference).expect("write same");
        fs::write(&different_path, &different).expect("write different");

        let mut group = group_from_paths(vec![reference_path, same_path, different_path]);
        let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);

        assert!(!verify_group_buffered(&mut group, &mut workspace).expect("verify"));
        assert!(!group.verified);
    }

    #[test]
    fn configured_buffer_sizes_preserve_exactness() {
        let (_dir, equal_group, different_group) = create_equal_and_different_groups();

        for buffer_size in [1_usize, 2, 4, 8, 16].map(|mib| mib * 1024 * 1024) {
            let mut equal = equal_group.clone();
            let mut equal_workspace = VerificationWorkspace::new(buffer_size);
            assert!(
                verify_group_buffered(&mut equal, &mut equal_workspace).expect("equal verification"),
                "equal files failed with buffer size {buffer_size}"
            );

            let mut different = different_group.clone();
            let mut different_workspace = VerificationWorkspace::new(buffer_size);
            assert!(
                !verify_group_buffered(&mut different, &mut different_workspace).expect("different verification"),
                "different files passed with buffer size {buffer_size}"
            );
        }
    }

    #[test]
    fn io_strategies_preserve_exactness() {
        let (_dir, equal_group, different_group) = create_equal_and_different_groups();
        let strategies = [
            IO_BASELINE,
            IO_SEQUENTIAL_HINT,
            IO_PARALLEL_CANDIDATES,
            IO_PARALLEL_SEQUENTIAL,
        ];

        for strategy in strategies {
            let mut equal = equal_group.clone();
            let mut equal_workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
            assert!(
                verify_group_with_io_strategy(&mut equal, &mut equal_workspace, strategy).expect("equal verification"),
                "equal files failed with {strategy:?}"
            );

            let mut different = different_group.clone();
            let mut different_workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
            assert!(
                !verify_group_with_io_strategy(&mut different, &mut different_workspace, strategy).expect("different verification"),
                "different files passed with {strategy:?}"
            );
        }
    }

    #[cfg(feature = "fast_duplicates")]
    #[derive(Debug, Clone, Copy)]
    struct IoStrategyVerifier {
        name: &'static str,
        strategy: VerifierIoStrategy,
    }

    #[cfg(feature = "fast_duplicates")]
    impl ExactVerifier for IoStrategyVerifier {
        fn name(&self) -> &'static str {
            self.name
        }

        fn verify_group(&self, group: &mut DuplicateGroup) -> io::Result<bool> {
            let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
            verify_group_with_io_strategy(group, &mut workspace, self.strategy)
        }

        fn verify_result(&self, result: &mut DuplicateScanResult) -> io::Result<usize> {
            verify_result_with_io_strategy(result, VERIFY_BUFFER_SIZE, self.strategy)
        }
    }

    #[cfg(feature = "fast_duplicates")]
    #[test]
    #[ignore = "benchmark manual; definir KROKIET_DUP_BENCH_PATH"]
    fn exact_verifier_real_dataset_benchmark() {
        use std::time::Instant;

        use crate::duplicate_engine::{DuplicateEngine, DuplicateScanRequest, FclonesEngine};

        let path = std::env::var_os("KROKIET_DUP_BENCH_PATH")
            .expect("debes definir KROKIET_DUP_BENCH_PATH antes de ejecutar el benchmark");
        let path = PathBuf::from(path);
        let runs = std::env::var("KROKIET_DUP_BENCH_RUNS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(9)
            .clamp(1, 15);

        println!();
        println!("============================================================");
        println!(" KROKIET - BENCHMARK DE ESTRATEGIAS DE I/O EXACTO");
        println!("============================================================");
        println!("Carpeta       : {}", path.display());
        println!("Detector      : fclones (sin modificar)");
        println!("Comparacion   : std slice equality exacta");
        println!("Buffer        : 1 MiB fijo");
        println!("Rondas        : {runs}");
        println!("Estrategias   : baseline / sequential hint / paralelo / ambos");
        println!("Seguridad     : mismos fingerprints antes/despues");
        println!();

        let request = DuplicateScanRequest::for_paths([path]);
        let candidates = FclonesEngine.scan(&request).expect("fallo el escaneo de fclones");
        assert!(!candidates.groups.is_empty(), "el dataset no contiene grupos duplicados");

        println!("Fast Engine   : {:.3?}", candidates.elapsed);
        println!("Grupos        : {}", candidates.groups.len());
        println!("Archivos      : {}", candidates.file_count());

        let verifiers = [
            IoStrategyVerifier {
                name: "baseline",
                strategy: IO_BASELINE,
            },
            IoStrategyVerifier {
                name: "seq-hint",
                strategy: IO_SEQUENTIAL_HINT,
            },
            IoStrategyVerifier {
                name: "parallel",
                strategy: IO_PARALLEL_CANDIDATES,
            },
            IoStrategyVerifier {
                name: "parallel+seq",
                strategy: IO_PARALLEL_SEQUENTIAL,
            },
        ];

        // Warm every strategy once, then rotate the execution order each round.
        // The benchmark intentionally compares strategies on the same candidate
        // groups; fclones is run only once.
        for verifier in &verifiers {
            let mut sample = candidates.clone();
            let verified = verifier.verify_result(&mut sample).expect("fallo el warm-up");
            assert_eq!(verified, sample.groups.len(), "warm-up fallo con {}", verifier.name());
        }

        let mut times: Vec<Vec<Duration>> = verifiers.iter().map(|_| Vec::with_capacity(runs)).collect();

        for round in 0..runs {
            println!();
            println!("RONDA {}/{}", round + 1, runs);

            for offset in 0..verifiers.len() {
                let index = (round + offset) % verifiers.len();
                let verifier = &verifiers[index];
                let mut sample = candidates.clone();

                let started = Instant::now();
                let verified = verifier.verify_result(&mut sample).expect("fallo la verificacion");
                let elapsed = started.elapsed();

                assert_eq!(verified, sample.groups.len(), "{} no verifico todos los grupos", verifier.name());
                println!("  {:<14}: {:.3?} | {verified}/{} grupos [OK]", verifier.name(), elapsed, sample.groups.len());
                times[index].push(elapsed);
            }
        }

        let mut medians = Vec::with_capacity(verifiers.len());
        for values in &mut times {
            medians.push(median_duration(values));
        }

        println!();
        println!("------------------------------------------------------------");
        println!(" MEDIANA DE {runs} RONDA(S)");
        println!("------------------------------------------------------------");
        for (verifier, median) in verifiers.iter().zip(&medians) {
            println!(" {:<14}: {:.3?}", verifier.name(), median);
        }

        let (best_index, best_median) = medians
            .iter()
            .enumerate()
            .min_by_key(|(_, duration)| duration.as_nanos())
            .expect("benchmark sin resultados");

        let baseline = medians[0];
        println!("------------------------------------------------------------");
        println!(" Mejor I/O     : {} ({:.3?})", verifiers[best_index].name(), best_median);
        if baseline.as_secs_f64() > 0.0 && best_median.as_secs_f64() > 0.0 {
            println!(
                " Mejor vs base : {:.3}x mas rapido",
                baseline.as_secs_f64() / best_median.as_secs_f64()
            );
        }
        println!("============================================================");
    }
}
