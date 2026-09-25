use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

#[cfg(all(test, feature = "fast_duplicates", feature = "verify_fastcmp", feature = "verify_memx"))]
use std::time::Duration;

use super::types::{DuplicateGroup, metadata_fingerprint};
#[cfg(feature = "fast_duplicates")]
use super::types::DuplicateScanResult;

const VERIFY_BUFFER_SIZE: usize = 4 * 1024 * 1024;
const MAX_CANDIDATES_PER_BATCH: usize = 16;

type BufferCompareFn = fn(&[u8], &[u8]) -> bool;

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

/// Current Krokiet verifier kept as the baseline implementation.
///
/// It uses reusable 4 MiB buffers and compares one reference chunk against a
/// bounded batch of candidate files. The reference is read once for normal
/// duplicate groups (<= 17 files total).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct StdBufferedVerifier;

impl ExactVerifier for StdBufferedVerifier {
    fn name(&self) -> &'static str {
        "std-buffered"
    }

    fn verify_group(&self, group: &mut DuplicateGroup) -> io::Result<bool> {
        let mut workspace = VerificationWorkspace::new();
        verify_group_buffered(group, &mut workspace, std_buffers_equal)
    }

    #[cfg(feature = "fast_duplicates")]
    fn verify_result(&self, result: &mut DuplicateScanResult) -> io::Result<usize> {
        verify_result_buffered(result, std_buffers_equal)
    }
}

/// Same exact I/O path as `StdBufferedVerifier`, but delegates comparison of
/// each in-memory chunk to `fastcmp`.
///
/// This intentionally benchmarks only the memory-comparison primitive. fclones
/// and the file-reading strategy remain untouched.
#[cfg(feature = "verify_fastcmp")]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FastcmpVerifier;

#[cfg(feature = "verify_fastcmp")]
impl ExactVerifier for FastcmpVerifier {
    fn name(&self) -> &'static str {
        "fastcmp"
    }

    fn verify_group(&self, group: &mut DuplicateGroup) -> io::Result<bool> {
        let mut workspace = VerificationWorkspace::new();
        verify_group_buffered(group, &mut workspace, fastcmp_buffers_equal)
    }

    #[cfg(feature = "fast_duplicates")]
    fn verify_result(&self, result: &mut DuplicateScanResult) -> io::Result<usize> {
        verify_result_buffered(result, fastcmp_buffers_equal)
    }
}

/// Same exact I/O path as `StdBufferedVerifier`, but delegates comparison of
/// each in-memory chunk to `memx::memeq`.
///
/// `memx` can use optimized x86/x86-64 implementations while Krokiet keeps
/// ownership of file handles, buffering, metadata stability checks and group
/// semantics.
#[cfg(feature = "verify_memx")]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct MemxVerifier;

#[cfg(feature = "verify_memx")]
impl ExactVerifier for MemxVerifier {
    fn name(&self) -> &'static str {
        "memx"
    }

    fn verify_group(&self, group: &mut DuplicateGroup) -> io::Result<bool> {
        let mut workspace = VerificationWorkspace::new();
        verify_group_buffered(group, &mut workspace, memx_buffers_equal)
    }

    #[cfg(feature = "fast_duplicates")]
    fn verify_result(&self, result: &mut DuplicateScanResult) -> io::Result<usize> {
        verify_result_buffered(result, memx_buffers_equal)
    }
}

#[inline]
fn std_buffers_equal(left: &[u8], right: &[u8]) -> bool {
    left == right
}

#[cfg(feature = "verify_fastcmp")]
#[inline]
fn fastcmp_buffers_equal(left: &[u8], right: &[u8]) -> bool {
    use fastcmp::Compare;

    // fastcmp 1.0.1 uses generated wide pointer loads for slices up to
    // 256 bytes. Keep tiny/tail comparisons on Rust slice equality so this
    // experimental backend only exercises fastcmp's large-slice memcmp path.
    if left.len() <= 256 || right.len() <= 256 {
        left == right
    } else {
        left.feq(right)
    }
}

#[cfg(feature = "verify_memx")]
#[inline]
fn memx_buffers_equal(left: &[u8], right: &[u8]) -> bool {
    memx::memeq(left, right)
}

struct VerificationWorkspace {
    reference_buffer: Vec<u8>,
    candidate_buffer: Vec<u8>,

    #[cfg(test)]
    reference_bytes_read: u64,
    #[cfg(test)]
    candidate_bytes_read: u64,
}

impl VerificationWorkspace {
    fn new() -> Self {
        Self {
            reference_buffer: vec![0_u8; VERIFY_BUFFER_SIZE],
            candidate_buffer: vec![0_u8; VERIFY_BUFFER_SIZE],
            #[cfg(test)]
            reference_bytes_read: 0,
            #[cfg(test)]
            candidate_bytes_read: 0,
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

fn verify_group_buffered(group: &mut DuplicateGroup, workspace: &mut VerificationWorkspace, compare: BufferCompareFn) -> io::Result<bool> {
    group.verified = false;

    if group.files.len() < 2 {
        return Ok(false);
    }

    let before = capture_group_fingerprints(group)?;
    if !all_sizes_match(&before) {
        return Ok(false);
    }

    let reference_size = before[0].0;
    let mut reference_file = File::open(&group.files[0].path)?;

    for candidate_batch in group.files[1..].chunks(MAX_CANDIDATES_PER_BATCH) {
        reference_file.seek(SeekFrom::Start(0))?;

        let mut candidate_files = candidate_batch
            .iter()
            .map(|candidate| File::open(&candidate.path))
            .collect::<io::Result<Vec<_>>>()?;

        let mut remaining = reference_size;
        while remaining > 0 {
            let amount = remaining.min(VERIFY_BUFFER_SIZE as u64) as usize;

            reference_file.read_exact(&mut workspace.reference_buffer[..amount])?;
            workspace.record_reference_read(amount);

            for candidate_file in &mut candidate_files {
                candidate_file.read_exact(&mut workspace.candidate_buffer[..amount])?;
                workspace.record_candidate_read(amount);

                if !compare(&workspace.reference_buffer[..amount], &workspace.candidate_buffer[..amount]) {
                    return Ok(false);
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
fn verify_result_buffered(result: &mut DuplicateScanResult, compare: BufferCompareFn) -> io::Result<usize> {
    let mut verified = 0;
    let mut workspace = VerificationWorkspace::new();

    for group in &mut result.groups {
        if verify_group_buffered(group, &mut workspace, compare)? {
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

#[cfg(all(test, feature = "fast_duplicates", feature = "verify_fastcmp", feature = "verify_memx"))]
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
        let mut workspace = VerificationWorkspace::new();

        assert!(verify_group_buffered(&mut group, &mut workspace, std_buffers_equal).expect("verify"));
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
        let mut workspace = VerificationWorkspace::new();

        assert!(verify_group_buffered(&mut group, &mut workspace, std_buffers_equal).expect("verify"));
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
        let mut workspace = VerificationWorkspace::new();

        assert!(!verify_group_buffered(&mut group, &mut workspace, std_buffers_equal).expect("verify"));
        assert!(!group.verified);
    }

    #[cfg(feature = "verify_fastcmp")]
    #[test]
    fn fastcmp_verifier_matches_std_for_identical_and_different_files() {
        let (_dir, equal_group, different_group) = create_equal_and_different_groups();
        let std = StdBufferedVerifier;
        let alternative = FastcmpVerifier;

        assert_eq!(alternative.name(), "fastcmp");

        let mut std_equal = equal_group.clone();
        let mut alt_equal = equal_group;
        assert!(std.verify_group(&mut std_equal).expect("std equal"));
        assert!(alternative.verify_group(&mut alt_equal).expect("fastcmp equal"));

        let mut std_different = different_group.clone();
        let mut alt_different = different_group;
        assert!(!std.verify_group(&mut std_different).expect("std different"));
        assert!(!alternative.verify_group(&mut alt_different).expect("fastcmp different"));
    }

    #[cfg(feature = "verify_memx")]
    #[test]
    fn memx_verifier_matches_std_for_identical_and_different_files() {
        let (_dir, equal_group, different_group) = create_equal_and_different_groups();
        let std = StdBufferedVerifier;
        let alternative = MemxVerifier;

        assert_eq!(alternative.name(), "memx");

        let mut std_equal = equal_group.clone();
        let mut alt_equal = equal_group;
        assert!(std.verify_group(&mut std_equal).expect("std equal"));
        assert!(alternative.verify_group(&mut alt_equal).expect("memx equal"));

        let mut std_different = different_group.clone();
        let mut alt_different = different_group;
        assert!(!std.verify_group(&mut std_different).expect("std different"));
        assert!(!alternative.verify_group(&mut alt_different).expect("memx different"));
    }

    #[cfg(all(feature = "verify_fastcmp", feature = "verify_memx", feature = "fast_duplicates"))]
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
            .unwrap_or(5)
            .clamp(1, 9);

        println!();
        println!("============================================================");
        println!(" KROKIET - BENCHMARK DE COMPARADORES EXACTOS");
        println!("============================================================");
        println!("Carpeta       : {}", path.display());
        println!("Detector      : fclones (sin modificar)");
        println!("Comparacion   : std slices vs fastcmp vs memx");
        println!("Rondas        : {runs}");
        println!("Medicion      : MISMO I/O; solo cambia comparador de buffers");
        println!();

        let request = DuplicateScanRequest::for_paths([path]);
        let candidates = FclonesEngine.scan(&request).expect("fallo el escaneo de fclones");
        assert!(!candidates.groups.is_empty(), "el dataset no contiene grupos duplicados");

        println!("Fast Engine   : {:.3?}", candidates.elapsed);
        println!("Grupos        : {}", candidates.groups.len());
        println!("Archivos      : {}", candidates.file_count());

        let std = StdBufferedVerifier;
        let fastcmp = FastcmpVerifier;
        let memx = MemxVerifier;

        let verifiers: [&dyn ExactVerifier; 3] = [&std, &fastcmp, &memx];

        // Warm-up: filesystem cache + verifier code paths. Not measured.
        for verifier in verifiers {
            let mut sample = candidates.clone();
            let verified = verifier.verify_result(&mut sample).expect("fallo el warm-up");
            assert_eq!(verified, sample.groups.len(), "warm-up fallo con {}", verifier.name());
        }

        let mut std_times = Vec::with_capacity(runs);
        let mut fastcmp_times = Vec::with_capacity(runs);
        let mut memx_times = Vec::with_capacity(runs);

        for round in 0..runs {
            println!();
            println!("RONDA {}/{}", round + 1, runs);

            // Rotate the first verifier each round to reduce ordering/cache bias.
            let order: [&dyn ExactVerifier; 3] = match round % 3 {
                0 => [&std, &fastcmp, &memx],
                1 => [&fastcmp, &memx, &std],
                _ => [&memx, &std, &fastcmp],
            };

            for verifier in order {
                let mut sample = candidates.clone();
                let started = Instant::now();
                let verified = verifier.verify_result(&mut sample).expect("fallo la verificacion");
                let elapsed = started.elapsed();

                assert_eq!(verified, sample.groups.len(), "{} no verifico todos los grupos", verifier.name());
                println!("  {:<14}: {:.3?} | {verified}/{} grupos [OK]", verifier.name(), elapsed, sample.groups.len());

                match verifier.name() {
                    "std-buffered" => std_times.push(elapsed),
                    "fastcmp" => fastcmp_times.push(elapsed),
                    "memx" => memx_times.push(elapsed),
                    other => panic!("verificador inesperado: {other}"),
                }
            }
        }

        let std_median = median_duration(&mut std_times);
        let fastcmp_median = median_duration(&mut fastcmp_times);
        let memx_median = median_duration(&mut memx_times);

        println!();
        println!("------------------------------------------------------------");
        println!(" MEDIANA DE {runs} RONDA(S)");
        println!("------------------------------------------------------------");
        println!(" StdBufferedVerifier : {:.3?}", std_median);
        println!(" FastcmpVerifier     : {:.3?}", fastcmp_median);
        println!(" MemxVerifier        : {:.3?}", memx_median);

        print_relative_result("Fastcmp vs std", std_median, fastcmp_median);
        print_relative_result("Memx vs std", std_median, memx_median);
        println!("============================================================");
    }

    #[cfg(all(feature = "verify_fastcmp", feature = "verify_memx", feature = "fast_duplicates"))]
    fn print_relative_result(label: &str, baseline: Duration, candidate: Duration) {
        if baseline.as_secs_f64() <= 0.0 || candidate.as_secs_f64() <= 0.0 {
            return;
        }

        if candidate <= baseline {
            println!(" {label:<19}: {:.2}x mas rapido", baseline.as_secs_f64() / candidate.as_secs_f64());
        } else {
            println!(" {label:<19}: {:.2}x el tiempo", candidate.as_secs_f64() / baseline.as_secs_f64());
        }
    }
}
