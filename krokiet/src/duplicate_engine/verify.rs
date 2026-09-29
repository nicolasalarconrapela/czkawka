use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

#[cfg(all(test, feature = "fast_duplicates"))]
use std::time::Duration;
#[cfg(all(test, feature = "fast_duplicates"))]
use std::sync::OnceLock;

#[cfg(all(test, feature = "fast_duplicates"))]
use rayon::prelude::*;

use super::types::{DuplicateGroup, metadata_fingerprint};
#[cfg(feature = "fast_duplicates")]
use super::types::DuplicateScanResult;

const VERIFY_BUFFER_SIZE: usize = 1024 * 1024;
const MAX_CANDIDATES_PER_BATCH: usize = 16;
const MAX_PINNED_FILES_PER_GROUP: usize = 256;

// Win32 FILE_FLAG_SEQUENTIAL_SCAN. Kept local so the experiment does not need
// another direct Windows dependency just to pass an OpenOptions hint.
#[cfg(windows)]
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VerifierIoStrategy {
    sequential_hint: bool,
}

#[cfg(test)]
const IO_BASELINE: VerifierIoStrategy = VerifierIoStrategy { sequential_hint: false };
#[cfg(test)]
const IO_SEQUENTIAL_HINT: VerifierIoStrategy = VerifierIoStrategy { sequential_hint: true };

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

/// Production exact verifier.
///
/// It keeps normal-sized duplicate groups open for the whole comparison, reads
/// the reference once, uses 1 MiB buffers and Rust slice equality, and on
/// Windows requests sequential access. It checks both path-level and handle-level
/// fingerprints before/after the byte comparison. Groups above the handle cap
/// fall back to the proven batched implementation.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct StdBufferedVerifier;

impl ExactVerifier for StdBufferedVerifier {
    fn name(&self) -> &'static str {
        "std-buffered"
    }

    fn verify_group(&self, group: &mut DuplicateGroup) -> io::Result<bool> {
        let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
        verify_group_pinned_handles(group, &mut workspace, true)
    }

    #[cfg(feature = "fast_duplicates")]
    fn verify_result(&self, result: &mut DuplicateScanResult) -> io::Result<usize> {
        let mut verified = 0;
        let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);

        for group in &mut result.groups {
            if verify_group_pinned_handles(group, &mut workspace, true)? {
                verified += 1;
            }
        }

        Ok(verified)
    }
}

#[cfg(all(test, feature = "fast_duplicates"))]
static VERIFY_POOL_2: OnceLock<rayon::ThreadPool> = OnceLock::new();
#[cfg(all(test, feature = "fast_duplicates"))]
static VERIFY_POOL_3: OnceLock<rayon::ThreadPool> = OnceLock::new();
#[cfg(all(test, feature = "fast_duplicates"))]
static VERIFY_POOL_4: OnceLock<rayon::ThreadPool> = OnceLock::new();
#[cfg(all(test, feature = "fast_duplicates"))]
static VERIFY_POOL_6: OnceLock<rayon::ThreadPool> = OnceLock::new();
#[cfg(all(test, feature = "fast_duplicates"))]
static VERIFY_POOL_8: OnceLock<rayon::ThreadPool> = OnceLock::new();
#[cfg(all(test, feature = "fast_duplicates"))]
static VERIFY_POOL_12: OnceLock<rayon::ThreadPool> = OnceLock::new();
#[cfg(all(test, feature = "fast_duplicates"))]
static VERIFY_POOL_16: OnceLock<rayon::ThreadPool> = OnceLock::new();

#[cfg(all(test, feature = "fast_duplicates"))]
fn verification_pool(threads: usize) -> io::Result<&'static rayon::ThreadPool> {
    let slot = match threads {
        2 => &VERIFY_POOL_2,
        3 => &VERIFY_POOL_3,
        4 => &VERIFY_POOL_4,
        6 => &VERIFY_POOL_6,
        8 => &VERIFY_POOL_8,
        12 => &VERIFY_POOL_12,
        16 => &VERIFY_POOL_16,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported exact-verifier thread count: {threads}"),
            ));
        }
    };

    if let Some(pool) = slot.get() {
        return Ok(pool);
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(move |index| format!("krokiet-exact-{threads}-{index}"))
        .build()
        .map_err(|error| io::Error::other(format!("failed to build exact-verifier thread pool: {error}")))?;

    // Another caller can race us only during initialization. If it wins, use
    // the already-installed pool and simply drop this equivalent one.
    let _ = slot.set(pool);
    Ok(slot.get().expect("verification pool must be initialized"))
}

/// Experimental group-parallel verifier used only by correctness tests and benchmarks.
///
/// Candidate reads inside one duplicate group stay serial because that already
/// benchmarked better. This variant parallelizes only independent groups, so
/// each worker owns its files and 1 MiB workspace and never shares mutable I/O
/// state with another group.
#[cfg(all(test, feature = "fast_duplicates"))]
fn verify_result_parallel_groups(result: &mut DuplicateScanResult, threads: usize) -> io::Result<usize> {
    if threads <= 1 || result.groups.len() <= 1 {
        return StdBufferedVerifier.verify_result(result);
    }

    let pool = verification_pool(threads)?;
    let results: Vec<io::Result<bool>> = pool.install(|| {
        result
            .groups
            .par_iter_mut()
            .map(|group| {
                let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
                verify_group_pinned_handles(group, &mut workspace, true)
            })
            .collect()
    });

    let mut verified = 0;
    for result in results {
        if result? {
            verified += 1;
        }
    }
    Ok(verified)
}

#[inline]
fn buffers_equal(left: &[u8], right: &[u8]) -> bool {
    left == right
}

struct VerificationWorkspace {
    buffer_size: usize,
    reference_buffer: Vec<u8>,
    candidate_buffer: Vec<u8>,
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
            #[cfg(test)]
            reference_bytes_read: 0,
            #[cfg(test)]
            candidate_bytes_read: 0,
        }
    }

    fn buffer_size(&self) -> usize {
        self.buffer_size
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

fn group_fingerprints_unchanged(group: &mut DuplicateGroup, before: &[(u64, Option<std::time::SystemTime>)]) -> io::Result<bool> {
    let mut final_metadata = Vec::with_capacity(group.files.len());

    for (file, before_fingerprint) in group.files.iter().zip(before) {
        let metadata = std::fs::metadata(&file.path)?;
        let after_fingerprint = (metadata.len(), metadata.modified().ok());
        if after_fingerprint != *before_fingerprint {
            return Ok(false);
        }
        final_metadata.push(metadata);
    }

    for (file, metadata) in group.files.iter_mut().zip(&final_metadata) {
        file.refresh_from_metadata(metadata);
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

#[cfg(test)]
fn verify_group_buffered(group: &mut DuplicateGroup, workspace: &mut VerificationWorkspace) -> io::Result<bool> {
    verify_group_with_io_strategy(group, workspace, IO_BASELINE)
}

fn file_fingerprint(file: &File) -> io::Result<(u64, Option<std::time::SystemTime>)> {
    let metadata = file.metadata()?;
    Ok((metadata.len(), metadata.modified().ok()))
}

/// Pinned-handle exact verifier.
///
/// It opens every file in a normal-sized duplicate group once, keeps those
/// handles pinned for the whole comparison and reads the reference exactly once.
/// Path fingerprints are retained before/after verification, while handle
/// fingerprints prove that the opened files themselves stayed stable. Very large
/// groups fall back to the proven batched verifier so we do not exhaust handles.
fn verify_group_pinned_handles(
    group: &mut DuplicateGroup,
    workspace: &mut VerificationWorkspace,
    sequential_hint: bool,
) -> io::Result<bool> {
    group.verified = false;

    if group.files.len() < 2 {
        return Ok(false);
    }

    if group.files.len() > MAX_PINNED_FILES_PER_GROUP {
        return verify_group_with_io_strategy(
            group,
            workspace,
            VerifierIoStrategy { sequential_hint },
        );
    }

    // Preserve the original path-level safety checks even though the actual
    // byte I/O uses pinned handles.
    let path_before = capture_group_fingerprints(group)?;
    if !all_sizes_match(&path_before) {
        return Ok(false);
    }

    let mut files = group
        .files
        .iter()
        .map(|entry| open_for_verification(&entry.path, sequential_hint))
        .collect::<io::Result<Vec<_>>>()?;

    // Bind the just-opened handles to the path state captured immediately
    // before opening. A mismatch means the path changed during the handoff.
    let handle_before = files.iter().map(file_fingerprint).collect::<io::Result<Vec<_>>>()?;
    if handle_before != path_before || !all_sizes_match(&handle_before) {
        return Ok(false);
    }

    let reference_size = handle_before[0].0;
    let (reference_file, candidate_files) = files.split_first_mut().expect("group length checked above");
    let mut remaining = reference_size;

    while remaining > 0 {
        let amount = remaining.min(workspace.buffer_size() as u64) as usize;

        reference_file.read_exact(&mut workspace.reference_buffer[..amount])?;
        workspace.record_reference_read(amount);

        for candidate_file in candidate_files.iter_mut() {
            candidate_file.read_exact(&mut workspace.candidate_buffer[..amount])?;
            workspace.record_candidate_read(amount);

            if !buffers_equal(&workspace.reference_buffer[..amount], &workspace.candidate_buffer[..amount]) {
                return Ok(false);
            }
        }

        remaining -= amount as u64;
    }

    // Prove that the opened files stayed stable while their bytes were read.
    let handle_after = files.iter().map(file_fingerprint).collect::<io::Result<Vec<_>>>()?;
    if handle_after != handle_before {
        return Ok(false);
    }

    // Re-check the paths themselves and finalize the deferred metadata. This
    // restores the path-level race detection semantics from the pre-pinned
    // verifier instead of trusting only the open handles.
    if !group_fingerprints_unchanged(group, &path_before)? {
        return Ok(false);
    }

    group.verified = true;
    Ok(true)
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

        let mut remaining = reference_size;
        while remaining > 0 {
            let amount = remaining.min(workspace.buffer_size() as u64) as usize;

            reference_file.read_exact(&mut workspace.reference_buffer[..amount])?;
            workspace.record_reference_read(amount);

            for candidate_file in &mut candidate_files {
                candidate_file.read_exact(&mut workspace.candidate_buffer[..amount])?;
                workspace.record_candidate_read(amount);

                if !buffers_equal(&workspace.reference_buffer[..amount], &workspace.candidate_buffer[..amount]) {
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

#[cfg(all(test, feature = "fast_duplicates"))]
fn median_f64(values: &mut [f64]) -> f64 {
    values.sort_by(|left, right| left.total_cmp(right));
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
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

        for strategy in [IO_BASELINE, IO_SEQUENTIAL_HINT] {
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

    #[test]
    fn pinned_handles_preserve_exactness_and_read_reference_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let size = VERIFY_BUFFER_SIZE + 4096;
        let reference = vec![0xA7; size];
        let mut different = reference.clone();
        different[size - 3] ^= 0xFF;

        let reference_path = dir.path().join("reference.bin");
        let same_a_path = dir.path().join("same-a.bin");
        let same_b_path = dir.path().join("same-b.bin");
        let different_path = dir.path().join("different.bin");

        fs::write(&reference_path, &reference).expect("write reference");
        fs::write(&same_a_path, &reference).expect("write same a");
        fs::write(&same_b_path, &reference).expect("write same b");
        fs::write(&different_path, &different).expect("write different");

        let mut equal = group_from_paths(vec![reference_path.clone(), same_a_path, same_b_path]);
        let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
        assert!(verify_group_pinned_handles(&mut equal, &mut workspace, true).expect("pinned equal verification"));
        assert_eq!(workspace.reference_bytes_read, size as u64);
        assert_eq!(workspace.candidate_bytes_read, (size as u64) * 2);

        let mut unequal = group_from_paths(vec![reference_path, different_path]);
        let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
        assert!(!verify_group_pinned_handles(&mut unequal, &mut workspace, true).expect("pinned unequal verification"));
    }

    #[test]
    fn deferred_metadata_is_finalized_by_exact_verifier() {
        let dir = tempdir().expect("tempdir");
        let data = vec![0x51; 96 * 1024 + 31];
        let left = dir.path().join("left.bin");
        let right = dir.path().join("right.bin");
        fs::write(&left, &data).expect("write left");
        fs::write(&right, &data).expect("write right");

        let expected_left = DuplicateFile::from_path(left.clone()).expect("left metadata");
        let expected_right = DuplicateFile::from_path(right.clone()).expect("right metadata");

        let mut group = DuplicateGroup::new(vec![
            DuplicateFile::from_scanned_size(left, data.len() as u64),
            DuplicateFile::from_scanned_size(right, data.len() as u64),
        ]);

        assert_eq!(group.files[0].modified_date, 0);
        assert_eq!(group.files[1].modified_date, 0);

        assert!(StdBufferedVerifier.verify_group(&mut group).expect("exact verification"));
        assert!(group.verified);
        assert_eq!(group.files[0].size, expected_left.size);
        assert_eq!(group.files[1].size, expected_right.size);
        assert_eq!(group.files[0].modified_date, expected_left.modified_date);
        assert_eq!(group.files[1].modified_date, expected_right.modified_date);
    }

    #[cfg(feature = "fast_duplicates")]
    #[test]
    fn parallel_group_verification_matches_sequential() {
        use crate::duplicate_engine::{compare_results, DuplicateEngine, DuplicateScanRequest, FclonesEngine};

        let dir = tempdir().expect("tempdir");
        for group_index in 0..12_u8 {
            let size = 192 * 1024 + group_index as usize * 257;
            let mut data = vec![group_index.wrapping_mul(17).wrapping_add(3); size];
            if let Some(last) = data.last_mut() {
                *last = group_index;
            }
            fs::write(dir.path().join(format!("group-{group_index:02}-a.bin")), &data).expect("write a");
            fs::write(dir.path().join(format!("group-{group_index:02}-b.bin")), &data).expect("write b");
        }

        let request = DuplicateScanRequest::for_paths([dir.path().to_path_buf()]);
        let candidates = FclonesEngine.scan(&request).expect("fclones scan");
        assert_eq!(candidates.groups.len(), 12);

        let mut sequential = candidates.clone();
        let seq_verified = StdBufferedVerifier.verify_result(&mut sequential).expect("sequential verify");
        assert_eq!(seq_verified, 12);
        assert!(sequential.groups.iter().all(|group| group.verified));

        for threads in [2, 3, 4, 6, 8, 12, 16] {
            let mut parallel = candidates.clone();
            let verified = verify_result_parallel_groups(&mut parallel, threads)
                .unwrap_or_else(|error| panic!("parallel-{threads} verify failed: {error}"));
            assert_eq!(verified, 12, "parallel-{threads}");
            assert!(parallel.groups.iter().all(|group| group.verified), "parallel-{threads}");
            assert!(compare_results(&sequential, &parallel).identical(), "parallel-{threads}");
        }
    }

    #[cfg(feature = "fast_duplicates")]
    #[test]
    #[ignore = "benchmark manual; definir KROKIET_DUP_BENCH_PATH"]
    fn exact_verifier_real_dataset_benchmark() {
        use crate::duplicate_engine::{compare_results, DuplicateEngine, DuplicateScanRequest, FclonesEngine};
        use std::path::PathBuf;
        use std::time::Instant;

        let path = std::env::var("KROKIET_DUP_BENCH_PATH")
            .map(PathBuf::from)
            .expect("debes definir KROKIET_DUP_BENCH_PATH antes de ejecutar el benchmark");
        let runs = std::env::var("KROKIET_DUP_BENCH_RUNS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(9)
            .clamp(1, 15);

        println!();
        println!("============================================================");
        println!(" KROKIET - BENCHMARK EXACT VERIFY: PARES POR RONDA");
        println!("============================================================");
        println!("Carpeta       : {}", path.display());
        println!("Detector      : fclones full hash (una vez por ronda)");
        println!("Verifier      : pinned handles + path/handle fingerprints + 1 MiB");
        println!("Estrategias   : secuencial / 6 / 8 / 12 / 16 grupos");
        println!(
            "CPU logicos   : {}",
            std::thread::available_parallelism().map(|value| value.get()).unwrap_or(1)
        );
        println!("Rondas        : {runs}");
        println!("Medicion      : mismo scan y mismo resultado para todas las estrategias");
        println!("Resumen       : ratios/deltas emparejados contra secuencial");
        println!();

        let request = DuplicateScanRequest::for_paths([path]);

        let baseline_candidates = FclonesEngine.scan(&request).expect("fallo fclones warm-up");
        assert!(!baseline_candidates.groups.is_empty(), "el dataset no contiene grupos duplicados");

        let mut sequential_warm = baseline_candidates.clone();
        let seq_count = StdBufferedVerifier
            .verify_result(&mut sequential_warm)
            .expect("fallo warm-up secuencial");
        assert_eq!(seq_count, sequential_warm.groups.len());

        for threads in [6, 8, 12, 16] {
            let mut parallel_warm = baseline_candidates.clone();
            let count = verify_result_parallel_groups(&mut parallel_warm, threads)
                .unwrap_or_else(|error| panic!("fallo warm-up groups-{threads}: {error}"));
            assert_eq!(count, parallel_warm.groups.len(), "groups-{threads}");
            assert!(compare_results(&sequential_warm, &parallel_warm).identical(), "groups-{threads}");
        }

        println!("Warm-up       : resultados exactos identicos [OK]");
        println!("Grupos        : {}", baseline_candidates.groups.len());
        println!("Archivos      : {}", baseline_candidates.file_count());

        let strategies: [(&str, usize); 5] = [
            ("sequential", 1),
            ("groups-6", 6),
            ("groups-8", 8),
            ("groups-12", 12),
            ("groups-16", 16),
        ];
        let mut scan_times = Vec::with_capacity(runs);
        let mut exact_times: Vec<Vec<Duration>> = (0..strategies.len()).map(|_| Vec::with_capacity(runs)).collect();

        for round in 0..runs {
            println!();
            println!("RONDA {}/{}", round + 1, runs);

            let scan_started = Instant::now();
            let round_candidates = FclonesEngine.scan(&request).expect("fallo fclones");
            let scan_elapsed = scan_started.elapsed();
            scan_times.push(scan_elapsed);
            assert!(compare_results(&baseline_candidates, &round_candidates).identical());
            println!("  scan comun : {:>10.3?}", scan_elapsed);

            for offset in 0..strategies.len() {
                let index = (round + offset) % strategies.len();
                let (name, threads) = strategies[index];
                let mut result = round_candidates.clone();

                let exact_started = Instant::now();
                let verified = if threads == 1 {
                    StdBufferedVerifier.verify_result(&mut result).expect("fallo sequential")
                } else {
                    verify_result_parallel_groups(&mut result, threads)
                        .unwrap_or_else(|error| panic!("fallo {name}: {error}"))
                };
                let exact_elapsed = exact_started.elapsed();

                assert_eq!(verified, result.groups.len(), "{name} no verifico todos los grupos");
                assert!(result.groups.iter().all(|group| group.verified));
                assert!(compare_results(&sequential_warm, &result).identical(), "{name} produjo resultados distintos");

                exact_times[index].push(exact_elapsed);
                println!(
                    "  {:<10}: exact {:>10.3?} | pipeline {:>10.3?} | {verified}/{} [OK]",
                    name,
                    exact_elapsed,
                    scan_elapsed + exact_elapsed,
                    result.groups.len(),
                );
            }
        }

        let scan_median = median_duration(&mut scan_times);
        let mut exact_medians = Vec::with_capacity(strategies.len());
        for values in &mut exact_times {
            exact_medians.push(median_duration(values));
        }

        println!();
        println!("------------------------------------------------------------");
        println!(" MEDIANA DE {runs} RONDA(S)");
        println!("------------------------------------------------------------");
        println!(" scan comun : {:>10.3?}", scan_median);
        for index in 0..strategies.len() {
            println!(" {:<10}: exact {:>10.3?}", strategies[index].0, exact_medians[index]);
        }

        println!("------------------------------------------------------------");
        println!(" COMPARACION EMPAREJADA CONTRA SECUENCIAL");
        println!("------------------------------------------------------------");

        let sequential_times = &exact_times[0];
        let mut best_index = 0_usize;
        let mut best_paired_speedup = 1.0_f64;

        for index in 1..strategies.len() {
            let mut ratios = Vec::with_capacity(runs);
            let mut deltas_ms = Vec::with_capacity(runs);
            let mut wins = 0_usize;

            for round in 0..runs {
                let seq = sequential_times[round].as_secs_f64();
                let candidate = exact_times[index][round].as_secs_f64();
                if candidate < seq {
                    wins += 1;
                }
                ratios.push(seq / candidate.max(f64::MIN_POSITIVE));
                deltas_ms.push((seq - candidate) * 1000.0);
            }

            let paired_speedup = median_f64(&mut ratios);
            let paired_delta_ms = median_f64(&mut deltas_ms);
            if paired_speedup > best_paired_speedup {
                best_paired_speedup = paired_speedup;
                best_index = index;
            }

            println!(
                " {:<10}: speedup mediano {:>6.3}x | delta mediana {:+8.3} ms | gana {wins}/{runs}",
                strategies[index].0,
                paired_speedup,
                paired_delta_ms,
            );
        }

        println!("------------------------------------------------------------");
        if best_index == 0 {
            println!(" Mejor pareado : sequential (ningun paralelo supera 1.000x en mediana)");
        } else {
            println!(
                " Mejor pareado : {} ({:.3}x mediana vs sequential)",
                strategies[best_index].0,
                best_paired_speedup,
            );
        }
        println!(" Produccion    : sequential (paralelismo sigue experimental)");
        println!("============================================================");
    }

}
