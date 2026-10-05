use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

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

// Win32 FILE_FLAG_SEQUENTIAL_SCAN. Kept local so the exact verifier does not
// need another direct Windows dependency just to pass an OpenOptions hint.
#[cfg(windows)]
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

/// Backend-independent exact byte verifier.
///
/// Duplicate detection and exact verification are deliberately separate.
/// fclones/Czkawka decide which files are candidates; an `ExactVerifier`
/// decides whether a candidate group is truly byte-identical.
pub(crate) trait ExactVerifier {
    #[allow(dead_code)]
    fn name(&self) -> &'static str;

    fn verify_group(&self, group: &mut DuplicateGroup) -> io::Result<bool>;

    /// Default result verification is intentionally sequential.
    ///
    /// Worker selection belongs to the independent tuning layer. Callers that
    /// already have a calibrated worker budget should use `ExactVerifierExecutor`.
    #[cfg(feature = "fast_duplicates")]
    fn verify_result(&self, result: &mut DuplicateScanResult) -> io::Result<usize> {
        verify_result_sequential(result)
    }
}

/// Production exact verifier for one duplicate group.
///
/// Normal-sized groups keep all handles open for the whole comparison, read the
/// reference once, use 1 MiB buffers and Rust slice equality, and on Windows
/// request sequential access. Both path-level and handle-level fingerprints are
/// checked before/after the byte comparison. Groups above the handle cap fall
/// back to a bounded-handle batched implementation with the same safety checks.
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
}

/// Reusable result-level executor with an explicit worker budget.
///
/// It contains no auto-tuning logic. A calibration/profile layer chooses the
/// number once; this executor only applies that decision to a scan result.
#[cfg(feature = "fast_duplicates")]
pub(crate) struct ExactVerifierExecutor {
    workers: usize,
    pool: Option<rayon::ThreadPool>,
}

#[cfg(feature = "fast_duplicates")]
impl ExactVerifierExecutor {
    pub(crate) fn new(workers: usize) -> io::Result<Self> {
        let workers = workers.max(1);
        let pool = if workers == 1 {
            None
        } else {
            Some(
                rayon::ThreadPoolBuilder::new()
                    .num_threads(workers)
                    .thread_name(move |index| format!("krokiet-exact-{workers}-{index}"))
                    .build()
                    .map_err(|error| io::Error::other(format!("failed to build exact-verifier thread pool: {error}")))?,
            )
        };

        Ok(Self { workers, pool })
    }

    pub(crate) fn workers(&self) -> usize {
        self.workers
    }

    pub(crate) fn verify_result(&self, result: &mut DuplicateScanResult) -> io::Result<usize> {
        if self.workers == 1 || result.groups.len() <= 1 {
            verify_result_sequential(result)
        } else {
            verify_result_parallel_groups(result, self.pool.as_ref().expect("parallel executor has a pool"))
        }
    }
}

#[cfg(feature = "fast_duplicates")]
fn retain_verified_groups(result: &mut DuplicateScanResult) -> usize {
    result.groups.retain(|group| group.verified);
    result.groups.len()
}

#[cfg(feature = "fast_duplicates")]
fn verify_result_sequential(result: &mut DuplicateScanResult) -> io::Result<usize> {
    let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);

    for group in &mut result.groups {
        verify_group_pinned_handles(group, &mut workspace, true)?;
    }

    Ok(retain_verified_groups(result))
}

/// Group-parallel verifier used only when a caller supplies an explicit worker
/// budget. Candidate reads inside each duplicate group remain serial. Each Rayon
/// worker reuses its own 1 MiB reference and candidate buffers.
#[cfg(feature = "fast_duplicates")]
fn verify_result_parallel_groups(
    result: &mut DuplicateScanResult,
    pool: &rayon::ThreadPool,
) -> io::Result<usize> {
    if result.groups.len() <= 1 {
        return verify_result_sequential(result);
    }

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

    for result in results {
        result?;
    }

    Ok(retain_verified_groups(result))
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

type FileFingerprint = (u64, Option<std::time::SystemTime>);

fn capture_group_fingerprints(group: &DuplicateGroup) -> io::Result<Vec<FileFingerprint>> {
    group
        .files
        .iter()
        .map(|file| metadata_fingerprint(&file.path))
        .collect()
}

fn all_sizes_match(fingerprints: &[FileFingerprint]) -> bool {
    let Some(reference_size) = fingerprints.first().map(|fingerprint| fingerprint.0) else {
        return false;
    };

    fingerprints
        .iter()
        .all(|fingerprint| fingerprint.0 == reference_size)
}

fn group_fingerprints_unchanged(group: &mut DuplicateGroup, before: &[FileFingerprint]) -> io::Result<bool> {
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

fn file_fingerprint(file: &File) -> io::Result<FileFingerprint> {
    let metadata = file.metadata()?;
    Ok((metadata.len(), metadata.modified().ok()))
}

/// Pinned-handle exact verifier.
///
/// It opens every file in a normal-sized duplicate group once, keeps those
/// handles pinned for the whole comparison and reads the reference exactly once.
/// Path fingerprints are retained before/after verification, while handle
/// fingerprints prove that the opened files themselves stayed stable. Very large
/// groups fall back to the bounded-handle verifier so we do not exhaust handles.
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
        return verify_group_batched(group, workspace, sequential_hint);
    }

    // Preserve path-level safety even though the byte I/O uses pinned handles.
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
    let handle_before = files
        .iter()
        .map(file_fingerprint)
        .collect::<io::Result<Vec<_>>>()?;
    if handle_before != path_before || !all_sizes_match(&handle_before) {
        return Ok(false);
    }

    let reference_size = handle_before[0].0;
    let (reference_file, candidate_files) = files
        .split_first_mut()
        .expect("group length checked above");
    let mut remaining = reference_size;

    while remaining > 0 {
        let amount = remaining.min(workspace.buffer_size() as u64) as usize;

        reference_file.read_exact(&mut workspace.reference_buffer[..amount])?;
        workspace.record_reference_read(amount);

        for candidate_file in candidate_files.iter_mut() {
            candidate_file.read_exact(&mut workspace.candidate_buffer[..amount])?;
            workspace.record_candidate_read(amount);

            if !buffers_equal(
                &workspace.reference_buffer[..amount],
                &workspace.candidate_buffer[..amount],
            ) {
                return Ok(false);
            }
        }

        remaining -= amount as u64;
    }

    // Prove that the opened files stayed stable while their bytes were read.
    let handle_after = files
        .iter()
        .map(file_fingerprint)
        .collect::<io::Result<Vec<_>>>()?;
    if handle_after != handle_before {
        return Ok(false);
    }

    // Re-check the paths themselves and finalize deferred metadata. This keeps
    // path replacement/rename detection in addition to handle stability.
    if !group_fingerprints_unchanged(group, &path_before)? {
        return Ok(false);
    }

    group.verified = true;
    Ok(true)
}

/// Bounded-handle fallback for groups above `MAX_PINNED_FILES_PER_GROUP`.
///
/// The reference stays open, candidates are processed in batches, and both path
/// and handle fingerprints are checked. This bounds handle pressure while
/// preserving the exactness guarantees of the normal pinned path.
fn verify_group_batched(
    group: &mut DuplicateGroup,
    workspace: &mut VerificationWorkspace,
    sequential_hint: bool,
) -> io::Result<bool> {
    group.verified = false;

    if group.files.len() < 2 {
        return Ok(false);
    }

    let path_before = capture_group_fingerprints(group)?;
    if !all_sizes_match(&path_before) {
        return Ok(false);
    }

    let reference_size = path_before[0].0;
    let mut reference_file = open_for_verification(&group.files[0].path, sequential_hint)?;
    let reference_handle_before = file_fingerprint(&reference_file)?;
    if reference_handle_before != path_before[0] {
        return Ok(false);
    }

    for (batch_index, candidate_batch) in group.files[1..]
        .chunks(MAX_CANDIDATES_PER_BATCH)
        .enumerate()
    {
        reference_file.seek(SeekFrom::Start(0))?;

        let candidate_start = 1 + batch_index * MAX_CANDIDATES_PER_BATCH;
        let candidate_end = candidate_start + candidate_batch.len();
        let expected_fingerprints = &path_before[candidate_start..candidate_end];

        let mut candidate_files = candidate_batch
            .iter()
            .map(|candidate| open_for_verification(&candidate.path, sequential_hint))
            .collect::<io::Result<Vec<_>>>()?;

        let candidate_handle_before = candidate_files
            .iter()
            .map(file_fingerprint)
            .collect::<io::Result<Vec<_>>>()?;
        if candidate_handle_before != expected_fingerprints {
            return Ok(false);
        }

        let mut remaining = reference_size;
        while remaining > 0 {
            let amount = remaining.min(workspace.buffer_size() as u64) as usize;

            reference_file.read_exact(&mut workspace.reference_buffer[..amount])?;
            workspace.record_reference_read(amount);

            for candidate_file in &mut candidate_files {
                candidate_file.read_exact(&mut workspace.candidate_buffer[..amount])?;
                workspace.record_candidate_read(amount);

                if !buffers_equal(
                    &workspace.reference_buffer[..amount],
                    &workspace.candidate_buffer[..amount],
                ) {
                    return Ok(false);
                }
            }

            remaining -= amount as u64;
        }

        let candidate_handle_after = candidate_files
            .iter()
            .map(file_fingerprint)
            .collect::<io::Result<Vec<_>>>()?;
        if candidate_handle_after != candidate_handle_before {
            return Ok(false);
        }
    }

    if file_fingerprint(&reference_file)? != reference_handle_before {
        return Ok(false);
    }

    if !group_fingerprints_unchanged(group, &path_before)? {
        return Ok(false);
    }

    group.verified = true;
    Ok(true)
}

/// Compatibility entry point used by the existing tests and benchmark.
pub(crate) fn verify_group(group: &mut DuplicateGroup) -> io::Result<bool> {
    StdBufferedVerifier.verify_group(group)
}

/// Compatibility entry point used by the Fast Engine tests and, later, the GUI
/// integration. On success only exact-verified groups remain in `result.groups`.
#[cfg(feature = "fast_duplicates")]
pub(crate) fn verify_result(result: &mut DuplicateScanResult) -> io::Result<usize> {
    StdBufferedVerifier.verify_result(result)
}

/// Applies an externally selected worker budget. This is the entry point used
/// by the independent tuning/profile layer and, later, by the GUI integration.
#[cfg(feature = "fast_duplicates")]
pub(crate) fn verify_result_with_workers(
    result: &mut DuplicateScanResult,
    workers: usize,
) -> io::Result<usize> {
    ExactVerifierExecutor::new(workers)?.verify_result(result)
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
        Duration::from_secs_f64(
            (values[middle - 1].as_secs_f64() + values[middle].as_secs_f64()) / 2.0,
        )
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
        assert!(
            verify_group_pinned_handles(&mut equal, &mut workspace, true)
                .expect("pinned equal verification")
        );
        assert_eq!(workspace.reference_bytes_read, size as u64);
        assert_eq!(workspace.candidate_bytes_read, (size as u64) * 2);

        let mut unequal = group_from_paths(vec![reference_path, different_path]);
        let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
        assert!(
            !verify_group_pinned_handles(&mut unequal, &mut workspace, true)
                .expect("pinned unequal verification")
        );
        assert!(!unequal.verified);
    }

    #[test]
    fn pinned_handles_reject_difference_in_late_chunk() {
        let dir = tempdir().expect("tempdir");
        let size = VERIFY_BUFFER_SIZE * 2 + 4096;
        let reference = vec![0x3C; size];
        let mut different = reference.clone();
        different[size - 17] ^= 0xFF;

        let reference_path = dir.path().join("reference.bin");
        let different_path = dir.path().join("different.bin");
        fs::write(&reference_path, &reference).expect("write reference");
        fs::write(&different_path, &different).expect("write different");

        let mut group = group_from_paths(vec![reference_path, different_path]);
        assert!(!StdBufferedVerifier.verify_group(&mut group).expect("verify"));
        assert!(!group.verified);
    }

    #[test]
    fn empty_files_are_exact_duplicates() {
        let dir = tempdir().expect("tempdir");
        let left = dir.path().join("empty-a.bin");
        let right = dir.path().join("empty-b.bin");
        fs::write(&left, []).expect("write left");
        fs::write(&right, []).expect("write right");

        let mut group = group_from_paths(vec![left, right]);
        assert!(StdBufferedVerifier.verify_group(&mut group).expect("verify empty files"));
        assert!(group.verified);
    }

    #[test]
    fn singleton_group_is_never_verified() {
        let dir = tempdir().expect("tempdir");
        let only = dir.path().join("only.bin");
        fs::write(&only, b"one file").expect("write file");

        let mut group = group_from_paths(vec![only]);
        assert!(!StdBufferedVerifier.verify_group(&mut group).expect("verify singleton"));
        assert!(!group.verified);
    }

    #[test]
    fn pinned_handle_cap_falls_back_safely() {
        let dir = tempdir().expect("tempdir");
        let data = vec![0x6D; 4096];
        let file_count = MAX_PINNED_FILES_PER_GROUP + 1;

        let mut paths = Vec::with_capacity(file_count);
        for index in 0..file_count {
            let path = dir.path().join(format!("large-group-{index:03}.bin"));
            fs::write(&path, &data).expect("write test file");
            paths.push(path);
        }

        let mut equal_group = group_from_paths(paths.clone());
        let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
        assert!(
            verify_group_pinned_handles(&mut equal_group, &mut workspace, true)
                .expect("fallback equal verification")
        );
        assert!(equal_group.verified);

        let expected_reference_passes = (file_count - 1).div_ceil(MAX_CANDIDATES_PER_BATCH);
        assert_eq!(
            workspace.reference_bytes_read,
            (data.len() as u64) * expected_reference_passes as u64
        );
        assert_eq!(
            workspace.candidate_bytes_read,
            (data.len() as u64) * (file_count as u64 - 1)
        );

        let mut changed = data.clone();
        *changed.last_mut().expect("non-empty") ^= 0xFF;
        fs::write(paths.last().expect("last path"), &changed).expect("overwrite last candidate");

        let mut different_group = group_from_paths(paths);
        let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
        assert!(
            !verify_group_pinned_handles(&mut different_group, &mut workspace, true)
                .expect("fallback unequal verification")
        );
        assert!(!different_group.verified);
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
    fn verify_result_retains_only_exact_verified_groups() {
        let dir = tempdir().expect("tempdir");

        let equal = vec![0x31; 64 * 1024];
        let different_a = vec![0x41; 64 * 1024];
        let different_b = vec![0x42; 64 * 1024];

        let equal_a = dir.path().join("equal-a.bin");
        let equal_b = dir.path().join("equal-b.bin");
        let diff_a = dir.path().join("diff-a.bin");
        let diff_b = dir.path().join("diff-b.bin");
        fs::write(&equal_a, &equal).expect("write equal a");
        fs::write(&equal_b, &equal).expect("write equal b");
        fs::write(&diff_a, &different_a).expect("write diff a");
        fs::write(&diff_b, &different_b).expect("write diff b");

        let mut result = DuplicateScanResult {
            engine: "test",
            groups: vec![
                group_from_paths(vec![equal_a, equal_b]),
                group_from_paths(vec![diff_a, diff_b]),
            ],
            elapsed: Duration::ZERO,
        };

        let verified = StdBufferedVerifier
            .verify_result(&mut result)
            .expect("verify result");
        assert_eq!(verified, 1);
        assert_eq!(result.groups.len(), 1);
        assert!(result.groups[0].verified);
        assert!(result.groups[0]
            .files
            .iter()
            .all(|file| file.path.file_name().unwrap().to_string_lossy().starts_with("equal-")));
    }

    #[cfg(feature = "fast_duplicates")]
    #[test]
    fn explicit_executor_keeps_worker_selection_external() {
        let sequential = ExactVerifierExecutor::new(0).expect("sequential executor");
        let parallel = ExactVerifierExecutor::new(6).expect("parallel executor");
        assert_eq!(sequential.workers(), 1);
        assert_eq!(parallel.workers(), 6);
    }

    #[cfg(feature = "fast_duplicates")]
    #[test]
    fn parallel_group_verification_matches_sequential() {
        use crate::duplicate_engine::{
            DuplicateEngine, DuplicateScanRequest, FclonesEngine, compare_results,
        };

        let dir = tempdir().expect("tempdir");
        for group_index in 0..12_u8 {
            let size = 192 * 1024 + group_index as usize * 257;
            let mut data = vec![group_index.wrapping_mul(17).wrapping_add(3); size];
            if let Some(last) = data.last_mut() {
                *last = group_index;
            }
            fs::write(
                dir.path().join(format!("group-{group_index:02}-a.bin")),
                &data,
            )
            .expect("write a");
            fs::write(
                dir.path().join(format!("group-{group_index:02}-b.bin")),
                &data,
            )
            .expect("write b");
        }

        let request = DuplicateScanRequest::for_paths([dir.path().to_path_buf()]);
        let candidates = FclonesEngine.scan(&request).expect("fclones scan");
        assert_eq!(candidates.groups.len(), 12);

        let mut sequential = candidates.clone();
        let seq_verified = verify_result_sequential(&mut sequential).expect("sequential verify");
        assert_eq!(seq_verified, 12);
        assert_eq!(sequential.groups.len(), 12);
        assert!(sequential.groups.iter().all(|group| group.verified));

        for workers in [2_usize, 4, 6, 8] {
            let mut parallel = candidates.clone();
            let executor = ExactVerifierExecutor::new(workers).expect("parallel executor");
            let parallel_verified = executor
                .verify_result(&mut parallel)
                .expect("parallel verify");
            assert_eq!(parallel_verified, 12);
            assert_eq!(parallel.groups.len(), 12);
            assert!(parallel.groups.iter().all(|group| group.verified));
            assert!(
                compare_results(&sequential, &parallel).identical(),
                "workers={workers}"
            );
        }

        // The compatibility entry point is intentionally sequential now. Worker
        // selection is injected by the tuning/profile layer instead of being
        // guessed inside the verifier.
        let mut default_path = candidates;
        let default_verified = StdBufferedVerifier
            .verify_result(&mut default_path)
            .expect("default verify");
        assert_eq!(default_verified, 12);
        assert!(compare_results(&sequential, &default_path).identical());
    }

    #[cfg(feature = "fast_duplicates")]
    #[test]
    #[ignore = "benchmark manual; definir KROKIET_DUP_BENCH_PATH"]
    fn exact_verifier_real_dataset_benchmark() {
        use std::path::PathBuf;
        use std::time::Instant;

        use crate::duplicate_engine::{
            DuplicateEngine, DuplicateScanRequest, FclonesEngine, compare_results,
        };

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
        let configured_workers = std::env::var("KROKIET_VERIFIER_WORKERS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_else(|| logical_cpus.min(6).max(1));
        let executor = ExactVerifierExecutor::new(configured_workers)
            .expect("failed to create configured verifier executor");

        println!();
        println!("============================================================");
        println!(" KROKIET - BENCHMARK VERIFIER CONFIGURABLE");
        println!("============================================================");
        println!("Carpeta       : {}", path.display());
        println!("Detector      : fclones full hash (una vez por ronda)");
        println!("Verifier      : pinned handles + path/handle fingerprints + 1 MiB");
        println!("Comparacion   : secuencial vs workers configurados");
        println!("CPU logicos   : {logical_cpus}");
        println!("Workers       : {} (seleccion externa; KROKIET_VERIFIER_WORKERS)", executor.workers());
        println!("Rondas        : {runs}");
        println!("Medicion      : mismo scan y mismo resultado para ambos caminos");
        println!("Resumen       : ratio/delta emparejado contra secuencial");
        println!();

        let request = DuplicateScanRequest::for_paths([path]);

        let baseline_candidates = FclonesEngine.scan(&request).expect("fallo fclones warm-up");
        assert!(
            !baseline_candidates.groups.is_empty(),
            "el dataset no contiene grupos duplicados"
        );

        let mut sequential_warm = baseline_candidates.clone();
        let seq_count =
            verify_result_sequential(&mut sequential_warm).expect("fallo warm-up secuencial");
        assert_eq!(seq_count, sequential_warm.groups.len());

        let mut production_warm = baseline_candidates.clone();
        let production_count = executor
            .verify_result(&mut production_warm)
            .expect("fallo warm-up configurado");
        assert_eq!(production_count, production_warm.groups.len());
        assert!(compare_results(&sequential_warm, &production_warm).identical());

        println!("Warm-up       : resultados exactos identicos [OK]");
        println!("Grupos        : {}", baseline_candidates.groups.len());
        println!("Archivos      : {}", baseline_candidates.file_count());
        if executor.workers() == 1 {
            println!("Configurado   : sequential");
        } else {
            println!("Configurado   : groups-{}", executor.workers());
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

            // Alternate order so neither path systematically benefits from being
            // first or second consumer of the page cache.
            let production_first = round % 2 == 1;

            let run_sequential = |candidates: &DuplicateScanResult| {
                let mut result = candidates.clone();
                let started = Instant::now();
                let verified =
                    verify_result_sequential(&mut result).expect("fallo sequential");
                let elapsed = started.elapsed();
                assert_eq!(verified, result.groups.len());
                assert!(result.groups.iter().all(|group| group.verified));
                assert!(compare_results(&sequential_warm, &result).identical());
                elapsed
            };

            let run_production = |candidates: &DuplicateScanResult| {
                let mut result = candidates.clone();
                let started = Instant::now();
                let verified = executor
                    .verify_result(&mut result)
                    .expect("fallo configurado");
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
                "  configured : exact {:>10.3?} | pipeline {:>10.3?}",
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
        println!(" configured   : exact {:>10.3?}", production_median);
        println!("------------------------------------------------------------");
        println!(" COMPARACION EMPAREJADA");
        println!("------------------------------------------------------------");
        println!(
            " configured   : speedup mediano {:>6.3}x | delta mediana {:+8.3} ms | gana {wins}/{runs}",
            paired_speedup,
            paired_delta_ms,
        );
        println!("============================================================");
    }
}
