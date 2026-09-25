use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::time::Duration;

use super::types::{DuplicateGroup, metadata_fingerprint};
#[cfg(feature = "fast_duplicates")]
use super::types::DuplicateScanResult;

const VERIFY_BUFFER_SIZE: usize = 4 * 1024 * 1024;
const MAX_CANDIDATES_PER_BATCH: usize = 16;

/// Backend-independent exact byte verifier.
///
/// Duplicate detection and exact verification are deliberately separate.
/// fclones/Czkawka decide which files are candidates; an `ExactVerifier`
/// decides whether a candidate group is truly byte-identical.
pub(crate) trait ExactVerifier {
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
        verify_group_std(group, &mut workspace)
    }

    #[cfg(feature = "fast_duplicates")]
    fn verify_result(&self, result: &mut DuplicateScanResult) -> io::Result<usize> {
        let mut verified = 0;
        let mut workspace = VerificationWorkspace::new();

        for group in &mut result.groups {
            if verify_group_std(group, &mut workspace)? {
                verified += 1;
            }
        }

        Ok(verified)
    }
}

/// Optional verifier delegated to the external `biff` crate.
///
/// This implementation is intentionally isolated behind `verify_biff` so the
/// dependency is only compiled when explicitly requested. fclones is not
/// modified and has no dependency on biff.
#[cfg(feature = "verify_biff")]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct BiffVerifier;

#[cfg(feature = "verify_biff")]
impl ExactVerifier for BiffVerifier {
    fn name(&self) -> &'static str {
        "biff"
    }

    fn verify_group(&self, group: &mut DuplicateGroup) -> io::Result<bool> {
        use biff::{ComparisonOptions, ComparisonResult, compare};

        group.verified = false;
        if group.files.len() < 2 {
            return Ok(false);
        }

        let before = capture_group_fingerprints(group)?;
        if !all_sizes_match(&before) {
            return Ok(false);
        }

        let mut reference = File::open(&group.files[0].path)?;
        let options = ComparisonOptions::default();

        for candidate in group.files.iter().skip(1) {
            reference.seek(SeekFrom::Start(0))?;
            let candidate_file = File::open(&candidate.path)?;

            match compare(&mut reference, candidate_file, &options) {
                ComparisonResult::Identical => {}
                ComparisonResult::Error(message) => {
                    return Err(io::Error::other(format!("biff comparison failed: {message}")));
                }
                _ => return Ok(false),
            }
        }

        if !group_fingerprints_unchanged(group, &before)? {
            return Ok(false);
        }

        group.verified = true;
        Ok(true)
    }
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

fn verify_group_std(group: &mut DuplicateGroup, workspace: &mut VerificationWorkspace) -> io::Result<bool> {
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

                if workspace.reference_buffer[..amount] != workspace.candidate_buffer[..amount] {
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
pub(crate) fn verify_result<T>(_result: &mut T) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "exact result verification requires the fast_duplicates feature",
    ))
}

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
    use std::time::Instant;

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

        assert!(verify_group_std(&mut group, &mut workspace).expect("verify"));
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

        assert!(verify_group_std(&mut group, &mut workspace).expect("verify"));
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

        assert!(!verify_group_std(&mut group, &mut workspace).expect("verify"));
        assert!(!group.verified);
    }

    #[cfg(feature = "verify_biff")]
    #[test]
    fn biff_verifier_matches_std_for_identical_and_different_files() {
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

        let std = StdBufferedVerifier;
        let biff = BiffVerifier;

        let mut std_equal = group_from_paths(vec![a.clone(), b.clone()]);
        let mut biff_equal = std_equal.clone();
        assert!(std.verify_group(&mut std_equal).expect("std equal"));
        assert!(biff.verify_group(&mut biff_equal).expect("biff equal"));

        let mut std_different = group_from_paths(vec![a, c]);
        let mut biff_different = std_different.clone();
        assert!(!std.verify_group(&mut std_different).expect("std different"));
        assert!(!biff.verify_group(&mut biff_different).expect("biff different"));
    }

    #[cfg(all(feature = "verify_biff", feature = "fast_duplicates"))]
    #[test]
    #[ignore = "benchmark manual; definir KROKIET_DUP_BENCH_PATH"]
    fn exact_verifier_real_dataset_benchmark() {
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
        println!(" KROKIET - BENCHMARK DE VERIFICADORES EXACTOS");
        println!("============================================================");
        println!("Carpeta       : {}", path.display());
        println!("Detector      : fclones (sin modificar)");
        println!("Comparacion   : StdBufferedVerifier vs BiffVerifier");
        println!("Rondas        : {runs}");
        println!("Medicion      : SOLO etapa byte a byte");
        println!();

        let request = DuplicateScanRequest::for_paths([path]);
        let candidates = FclonesEngine.scan(&request).expect("fallo el escaneo de fclones");
        assert!(!candidates.groups.is_empty(), "el dataset no contiene grupos duplicados");

        println!("Fast Engine   : {:.3?}", candidates.elapsed);
        println!("Grupos        : {}", candidates.groups.len());
        println!("Archivos      : {}", candidates.file_count());

        let std = StdBufferedVerifier;
        let biff = BiffVerifier;

        // Warm-up: filesystem cache + verifier code paths. Not measured.
        for verifier in [&std as &dyn ExactVerifier, &biff as &dyn ExactVerifier] {
            let mut sample = candidates.clone();
            let verified = verifier.verify_result(&mut sample).expect("fallo el warm-up");
            assert_eq!(verified, sample.groups.len(), "warm-up fallo con {}", verifier.name());
        }

        let mut std_times = Vec::with_capacity(runs);
        let mut biff_times = Vec::with_capacity(runs);

        for round in 0..runs {
            println!();
            println!("RONDA {}/{}", round + 1, runs);

            let order: [&dyn ExactVerifier; 2] = if round % 2 == 0 {
                [&std, &biff]
            } else {
                [&biff, &std]
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
                    "biff" => biff_times.push(elapsed),
                    other => panic!("verificador inesperado: {other}"),
                }
            }
        }

        let std_median = median_duration(&mut std_times);
        let biff_median = median_duration(&mut biff_times);

        println!();
        println!("------------------------------------------------------------");
        println!(" MEDIANA DE {runs} RONDA(S)");
        println!("------------------------------------------------------------");
        println!(" StdBufferedVerifier : {:.3?}", std_median);
        println!(" BiffVerifier        : {:.3?}", biff_median);

        if std_median.as_secs_f64() > 0.0 && biff_median.as_secs_f64() > 0.0 {
            if biff_median <= std_median {
                println!(" Biff vs std         : {:.2}x mas rapido", std_median.as_secs_f64() / biff_median.as_secs_f64());
            } else {
                println!(" Biff vs std         : {:.2}x el tiempo", biff_median.as_secs_f64() / std_median.as_secs_f64());
            }
        }

        println!("============================================================");
    }
}
