use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

#[cfg(feature = "fast_duplicates")]
use std::sync::OnceLock;
#[cfg(all(test, feature = "fast_duplicates"))]
use std::time::Duration;

#[cfg(feature = "fast_duplicates")]
use rayon::prelude::*;

use super::types::{DuplicateGroup, metadata_fingerprint};
#[cfg(feature = "fast_duplicates")]
use super::types::DuplicateScanResult;

const VERIFY_BUFFER_SIZE: usize = 1024 * 1024;
const MAX_CANDIDATES_PER_BATCH: usize = 16;
const MAX_PINNED_FILES_PER_GROUP: usize = 256;

#[cfg(feature = "fast_duplicates")]
const PRODUCTION_GROUP_THREADS: usize = 6;
#[cfg(feature = "fast_duplicates")]
const MIN_GROUPS_FOR_PARALLEL_VERIFY: usize = 8;

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
        let logical_cpus = std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1);
        let threads = select_production_group_threads(result.groups.len(), logical_cpus);

        if threads == 1 {
            verify_result_sequential(result)
        } else {
            verify_result_parallel_groups(result)
        }
    }
}

#[cfg(feature = "fast_duplicates")]
static VERIFY_POOL_6: OnceLock<rayon::ThreadPool> = OnceLock::new();

#[cfg(feature = "fast_duplicates")]
fn select_production_group_threads(group_count: usize, logical_cpus: usize) -> usize {
    if group_count >= MIN_GROUPS_FOR_PARALLEL_VERIFY && logical_cpus >= PRODUCTION_GROUP_THREADS {
        PRODUCTION_GROUP_THREADS
    } else {
        1
    }
}

#[cfg(feature = "fast_duplicates")]
fn verification_pool() -> io::Result<&'static rayon::ThreadPool> {
    if let Some(pool) = VERIFY_POOL_6.get() {
        return Ok(pool);
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(PRODUCTION_GROUP_THREADS)
        .thread_name(|index| format!("krokiet-exact-{PRODUCTION_GROUP_THREADS}-{index}"))
        .build()
        .map_err(|error| io::Error::other(format!("failed to build exact-verifier thread pool: {error}")))?;

    // Another caller can race us only during initialization. If it wins, use
    // the already-installed equivalent pool and drop this one.
    let _ = VERIFY_POOL_6.set(pool);
    Ok(VERIFY_POOL_6.get().expect("verification pool must be initialized"))
}

#[cfg(feature = "fast_duplicates")]
fn verify_result_sequential(result: &mut DuplicateScanResult) -> io::Result<usize> {
    let mut verified = 0;
    let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);

    for group in &mut result.groups {
        if verify_group_pinned_handles(group, &mut workspace, true)? {
            verified += 1;
        }
    }

    Ok(verified)
}

/// Production group-parallel verifier.
///
/// Candidate reads inside each duplicate group remain serial. Only independent
/// groups run concurrently. Each Rayon worker reuses its own 1 MiB reference
/// and candidate buffers, avoiding one allocation pair per group.
#[cfg(feature = "fast_duplicates")]
fn verify_result_parallel_groups(result: &mut DuplicateScanResult) -> io::Result<usize> {
    if result.groups.len() <= 1 {
        return verify_result_sequential(result);
    }

    let pool = verification_pool()?;
    let results: Vec<io::Result<bool>> = pool.install(|| {
        result
            .groups
            .par_iter_mut()
            .map_init(
                || VerificationWorkspace::new(VERIFY_BUFFER_SIZE),
                |workspace, group| verify_group_pinned_handles(group, workspace, true),
            )
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
    fn production_parallelism_policy_is_conservative() {
        assert_eq!(select_production_group_threads(0, 8), 1);
        assert_eq!(select_production_group_threads(MIN_GROUPS_FOR_PARALLEL_VERIFY - 1, 8), 1);
        assert_eq!(select_production_group_threads(MIN_GROUPS_FOR_PARALLEL_VERIFY, 5), 1);
        assert_eq!(
            select_production_group_threads(MIN_GROUPS_FOR_PARALLEL_VERIFY, 6),
            PRODUCTION_GROUP_THREADS
        );
        assert_eq!(
            select_production_group_threads(MIN_GROUPS_FOR_PARALLEL_VERIFY + 20, 32),
            PRODUCTION_GROUP_THREADS
        );
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
        let seq_verified = verify_result_sequential(&mut sequential).expect("sequential verify");
        assert_eq!(seq_verified, 12);
        assert!(sequential.groups.iter().all(|group| group.verified));

        let mut parallel = candidates.clone();
        let parallel_verified = verify_result_parallel_groups(&mut parallel).expect("parallel verify");
        assert_eq!(parallel_verified, 12);
        assert!(parallel.groups.iter().all(|group| group.verified));
        assert!(compare_results(&sequential, &parallel).identical());

        // The public production verifier must preserve the same result regardless
        // of whether this machine selects the serial or the six-worker path.
        let mut production = candidates;
        let production_verified = StdBufferedVerifier
            .verify_result(&mut production)
            .expect("production verify");
        assert_eq!(production_verified, 12);
        assert!(production.groups.iter().all(|group| group.verified));
        assert!(compare_results(&sequential, &production).identical());
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

        let logical_cpus = std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1);

        println!();
        println!("============================================================");
        println!(" KROKIET - BENCHMARK VERIFIER DE PRODUCCION");
        println!("============================================================");
        println!("Carpeta       : {}", path.display());
        println!("Detector      : fclones full hash (una vez por ronda)");
        println!("Verifier      : pinned handles + path/handle fingerprints + 1 MiB");
        println!("Comparacion   : secuencial vs politica productiva");
        println!("CPU logicos   : {logical_cpus}");
        println!("Politica      : 6 workers si CPU>=6 y grupos>=8; si no, secuencial");
        println!("Rondas        : {runs}");
        println!("Medicion      : mismo scan y mismo resultado para ambos caminos");
        println!("Resumen       : ratio/delta emparejado contra secuencial");
        println!();

        let request = DuplicateScanRequest::for_paths([path]);

        let baseline_candidates = FclonesEngine.scan(&request).expect("fallo fclones warm-up");
        assert!(!baseline_candidates.groups.is_empty(), "el dataset no contiene grupos duplicados");

        let selected_threads = select_production_group_threads(baseline_candidates.groups.len(), logical_cpus);

        let mut sequential_warm = baseline_candidates.clone();
        let seq_count = verify_result_sequential(&mut sequential_warm).expect("fallo warm-up secuencial");
        assert_eq!(seq_count, sequential_warm.groups.len());

        let mut production_warm = baseline_candidates.clone();
        let production_count = StdBufferedVerifier
            .verify_result(&mut production_warm)
            .expect("fallo warm-up produccion");
        assert_eq!(production_count, production_warm.groups.len());
        assert!(compare_results(&sequential_warm, &production_warm).identical());

        println!("Warm-up       : resultados exactos identicos [OK]");
        println!("Grupos        : {}", baseline_candidates.groups.len());
        println!("Archivos      : {}", baseline_candidates.file_count());
        if selected_threads == 1 {
            println!("Produccion    : sequential");
        } else {
            println!("Produccion    : groups-{selected_threads}");
        }

        let mut scan_times = Vec::with_capacity(runs);
        let mut sequential_times = Vec::with_capacity(runs);
        let mut production_times = Vec::with_capacity(runs);

        for round in 0..runs {
            println!();
            println!("RONDA {}/{}", round + 1, runs);

            let scan_started = Instant::now();
            let round_candidates = FclonesEngine.scan(&request).expect("fallo fclones");
            let scan_elapsed = scan_started.elapsed();
            scan_times.push(scan_elapsed);
            assert!(compare_results(&baseline_candidates, &round_candidates).identical());
            println!("  scan comun : {:>10.3?}", scan_elapsed);

            // Alternate order each round so neither path systematically benefits
            // from being the first or second consumer of the page cache.
            let production_first = round % 2 == 1;

            let run_sequential = |candidates: &DuplicateScanResult| {
                let mut result = candidates.clone();
                let started = Instant::now();
                let verified = verify_result_sequential(&mut result).expect("fallo sequential");
                let elapsed = started.elapsed();
                assert_eq!(verified, result.groups.len());
                assert!(result.groups.iter().all(|group| group.verified));
                assert!(compare_results(&sequential_warm, &result).identical());
                elapsed
            };

            let run_production = |candidates: &DuplicateScanResult| {
                let mut result = candidates.clone();
                let started = Instant::now();
                let verified = StdBufferedVerifier
                    .verify_result(&mut result)
                    .expect("fallo produccion");
                let elapsed = started.elapsed();
                assert_eq!(verified, result.groups.len());
                assert!(result.groups.iter().all(|group| group.verified));
                assert!(compare_results(&sequential_warm, &result).identical());
                elapsed
            };

            let (sequential_elapsed, production_elapsed) = if production_first {
                let production_elapsed = run_production(&round_candidates);
                let sequential_elapsed = run_sequential(&round_candidates);
                (sequential_elapsed, production_elapsed)
            } else {
                let sequential_elapsed = run_sequential(&round_candidates);
                let production_elapsed = run_production(&round_candidates);
                (sequential_elapsed, production_elapsed)
            };

            sequential_times.push(sequential_elapsed);
            production_times.push(production_elapsed);

            println!(
                "  sequential : exact {:>10.3?} | pipeline {:>10.3?}",
                sequential_elapsed,
                scan_elapsed + sequential_elapsed,
            );
            println!(
                "  production : exact {:>10.3?} | pipeline {:>10.3?}",
                production_elapsed,
                scan_elapsed + production_elapsed,
            );
        }

        let scan_median = median_duration(&mut scan_times);
        let sequential_median = median_duration(&mut sequential_times.clone());
        let production_median = median_duration(&mut production_times.clone());

        let mut ratios = Vec::with_capacity(runs);
        let mut deltas_ms = Vec::with_capacity(runs);
        let mut wins = 0_usize;

        for round in 0..runs {
            let seq = sequential_times[round].as_secs_f64();
            let production = production_times[round].as_secs_f64();
            if production < seq {
                wins += 1;
            }
            ratios.push(seq / production.max(f64::MIN_POSITIVE));
            deltas_ms.push((seq - production) * 1000.0);
        }

        let paired_speedup = median_f64(&mut ratios);
        let paired_delta_ms = median_f64(&mut deltas_ms);

        println!();
        println!("------------------------------------------------------------");
        println!(" MEDIANA DE {runs} RONDA(S)");
        println!("------------------------------------------------------------");
        println!(" scan comun   : {:>10.3?}", scan_median);
        println!(" sequential   : exact {:>10.3?}", sequential_median);
        println!(" production   : exact {:>10.3?}", production_median);
        println!("------------------------------------------------------------");
        println!(" COMPARACION EMPAREJADA");
        println!("------------------------------------------------------------");
        println!(
            " production   : speedup mediano {:>6.3}x | delta mediana {:+8.3} ms | gana {wins}/{runs}",
            paired_speedup,
            paired_delta_ms,
        );
        println!("============================================================");
    }


}
