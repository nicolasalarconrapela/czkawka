use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

#[cfg(all(test, feature = "fast_duplicates"))]
use std::time::Duration;
#[cfg(feature = "fast_duplicates")]
use std::sync::OnceLock;

#[cfg(feature = "fast_duplicates")]
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

const IO_BASELINE: VerifierIoStrategy = VerifierIoStrategy { sequential_hint: false };
const IO_SEQUENTIAL_HINT: VerifierIoStrategy = VerifierIoStrategy { sequential_hint: true };

// Win32 FILE_FLAG_OVERLAPPED. Phase 1.9 uses real asynchronous ReadFile calls
// with explicit offsets while keeping fclones completely untouched.
#[cfg(windows)]
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;

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
/// Windows requests sequential access. Groups above the handle cap fall back
/// to the proven batched implementation.
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

#[cfg(feature = "fast_duplicates")]
static VERIFY_POOL_2: OnceLock<rayon::ThreadPool> = OnceLock::new();
#[cfg(feature = "fast_duplicates")]
static VERIFY_POOL_3: OnceLock<rayon::ThreadPool> = OnceLock::new();
#[cfg(feature = "fast_duplicates")]
static VERIFY_POOL_4: OnceLock<rayon::ThreadPool> = OnceLock::new();
#[cfg(feature = "fast_duplicates")]
static VERIFY_POOL_6: OnceLock<rayon::ThreadPool> = OnceLock::new();
#[cfg(feature = "fast_duplicates")]
static VERIFY_POOL_8: OnceLock<rayon::ThreadPool> = OnceLock::new();
#[cfg(feature = "fast_duplicates")]
static VERIFY_POOL_12: OnceLock<rayon::ThreadPool> = OnceLock::new();
#[cfg(feature = "fast_duplicates")]
static VERIFY_POOL_16: OnceLock<rayon::ThreadPool> = OnceLock::new();

#[cfg(feature = "fast_duplicates")]
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

/// Experimental Phase 2.3 verifier.
///
/// Candidate reads inside one duplicate group stay serial because that already
/// benchmarked better. This variant parallelizes only independent groups, so
/// each worker owns its files and 1 MiB workspace and never shares mutable I/O
/// state with another group.
#[cfg(feature = "fast_duplicates")]
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
    overlapped_candidate_buffers: Vec<Vec<u8>>,

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
            overlapped_candidate_buffers: Vec::new(),
            #[cfg(test)]
            reference_bytes_read: 0,
            #[cfg(test)]
            candidate_bytes_read: 0,
        }
    }

    fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    fn ensure_overlapped_candidate_buffers(&mut self, count: usize) {
        while self.overlapped_candidate_buffers.len() < count {
            self.overlapped_candidate_buffers.push(vec![0_u8; self.buffer_size]);
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

fn verify_group_buffered(group: &mut DuplicateGroup, workspace: &mut VerificationWorkspace) -> io::Result<bool> {
    verify_group_with_io_strategy(group, workspace, IO_BASELINE)
}

fn file_fingerprint(file: &File) -> io::Result<(u64, Option<std::time::SystemTime>)> {
    let metadata = file.metadata()?;
    Ok((metadata.len(), metadata.modified().ok()))
}

/// Experimental Phase 2 verifier.
///
/// It opens every file in a normal-sized duplicate group once, keeps those
/// handles pinned for the whole comparison, captures fingerprints from those
/// same handles, and reads the reference exactly once. This removes repeated
/// path metadata lookups and removes the 16-candidate reference reread for
/// groups that fit under the handle cap. Very large groups fall back to the
/// proven batched verifier so we do not exhaust process handles.
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

    let mut files = group
        .files
        .iter()
        .map(|entry| open_for_verification(&entry.path, sequential_hint))
        .collect::<io::Result<Vec<_>>>()?;

    let before = files.iter().map(file_fingerprint).collect::<io::Result<Vec<_>>>()?;
    if !all_sizes_match(&before) {
        return Ok(false);
    }

    let reference_size = before[0].0;
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

    let mut final_metadata = Vec::with_capacity(files.len());
    for (file, before_fingerprint) in files.iter().zip(&before) {
        let metadata = file.metadata()?;
        let after_fingerprint = (metadata.len(), metadata.modified().ok());
        if after_fingerprint != *before_fingerprint {
            return Ok(false);
        }
        final_metadata.push(metadata);
    }

    for (file, metadata) in group.files.iter_mut().zip(&final_metadata) {
        file.refresh_from_metadata(metadata);
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

#[cfg(windows)]
mod overlapped_windows {
    use std::ffi::c_void;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::ptr;

    use super::{FILE_FLAG_OVERLAPPED, FILE_FLAG_SEQUENTIAL_SCAN};

    const ERROR_IO_PENDING: u32 = 997;
    const TRUE: i32 = 1;

    type Handle = *mut c_void;

    // ABI-compatible subset of Win32 OVERLAPPED. The anonymous union in the
    // Windows definition contains either (Offset, OffsetHigh) or a pointer;
    // the verifier only needs explicit file offsets.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        h_event: Handle,
    }

    impl Default for Overlapped {
        fn default() -> Self {
            Self {
                internal: 0,
                internal_high: 0,
                offset: 0,
                offset_high: 0,
                h_event: ptr::null_mut(),
            }
        }
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ReadFile(
            file: Handle,
            buffer: *mut c_void,
            bytes_to_read: u32,
            bytes_read: *mut u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn GetOverlappedResult(file: Handle, overlapped: *mut Overlapped, bytes_transferred: *mut u32, wait: i32) -> i32;
        fn CreateEventW(attributes: *const c_void, manual_reset: i32, initial_state: i32, name: *const u16) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetLastError() -> u32;
    }

    #[derive(Debug)]
    struct EventHandle(Handle);

    impl EventHandle {
        fn new() -> io::Result<Self> {
            // Manual-reset events make the completion state explicit. ReadFile
            // resets the event when a new overlapped operation begins.
            let handle = unsafe { CreateEventW(ptr::null(), TRUE, 0, ptr::null()) };
            if handle.is_null() {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }
    }

    impl Drop for EventHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
    }

    #[derive(Debug)]
    pub(super) struct OverlappedFile {
        file: File,
        event: EventHandle,
    }

    impl OverlappedFile {
        pub(super) fn open(path: &Path, sequential_hint: bool) -> io::Result<Self> {
            let mut options = OpenOptions::new();
            options.read(true);

            let mut flags = FILE_FLAG_OVERLAPPED;
            if sequential_hint {
                flags |= FILE_FLAG_SEQUENTIAL_SCAN;
            }
            options.custom_flags(flags);

            Ok(Self {
                file: options.open(path)?,
                event: EventHandle::new()?,
            })
        }

        pub(super) fn issue_read(&self, buffer: &mut [u8], offset: u64, overlapped: &mut Overlapped) -> io::Result<()> {
            debug_assert!(!buffer.is_empty());
            debug_assert!(u32::try_from(buffer.len()).is_ok());

            *overlapped = Overlapped {
                offset: offset as u32,
                offset_high: (offset >> 32) as u32,
                h_event: self.event.0,
                ..Overlapped::default()
            };

            let result = unsafe {
                ReadFile(
                    self.file.as_raw_handle().cast(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    ptr::null_mut(),
                    overlapped,
                )
            };

            if result == 0 {
                // GetLastError must be captured immediately after ReadFile.
                let error = unsafe { GetLastError() };
                if error != ERROR_IO_PENDING {
                    return Err(io::Error::from_raw_os_error(error as i32));
                }
            }

            Ok(())
        }

        pub(super) fn finish_read(&self, overlapped: &mut Overlapped, expected: usize) -> io::Result<()> {
            let mut transferred = 0_u32;
            let result = unsafe {
                GetOverlappedResult(
                    self.file.as_raw_handle().cast(),
                    overlapped,
                    &mut transferred,
                    TRUE,
                )
            };

            if result == 0 {
                return Err(io::Error::last_os_error());
            }

            if transferred as usize != expected {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("overlapped read returned {transferred} of {expected} bytes"),
                ));
            }

            Ok(())
        }
    }
}

#[cfg(windows)]
fn verify_group_overlapped(
    group: &mut DuplicateGroup,
    workspace: &mut VerificationWorkspace,
    sequential_hint: bool,
) -> io::Result<bool> {
    use overlapped_windows::{Overlapped, OverlappedFile};

    group.verified = false;

    if group.files.len() < 2 {
        return Ok(false);
    }

    let before = capture_group_fingerprints(group)?;
    if !all_sizes_match(&before) {
        return Ok(false);
    }

    let reference_size = before[0].0;
    let reference_file = OverlappedFile::open(&group.files[0].path, sequential_hint)?;

    for candidate_batch in group.files[1..].chunks(MAX_CANDIDATES_PER_BATCH) {
        let candidate_files = candidate_batch
            .iter()
            .map(|candidate| OverlappedFile::open(&candidate.path, sequential_hint))
            .collect::<io::Result<Vec<_>>>()?;

        workspace.ensure_overlapped_candidate_buffers(candidate_files.len());
        let mut operations = vec![Overlapped::default(); candidate_files.len() + 1];

        let mut offset = 0_u64;
        while offset < reference_size {
            let amount = (reference_size - offset).min(workspace.buffer_size() as u64) as usize;
            let mut issued = 0_usize;
            let mut first_error: Option<io::Error> = None;

            // Every issued operation is completed below before any buffer or
            // OVERLAPPED record can be reused or dropped. This is required by
            // the Win32 asynchronous-I/O contract.
            if let Err(error) = reference_file.issue_read(&mut workspace.reference_buffer[..amount], offset, &mut operations[0]) {
                first_error = Some(error);
            } else {
                issued = 1;

                for (index, candidate_file) in candidate_files.iter().enumerate() {
                    let candidate_buffer = &mut workspace.overlapped_candidate_buffers[index][..amount];
                    match candidate_file.issue_read(candidate_buffer, offset, &mut operations[index + 1]) {
                        Ok(()) => issued += 1,
                        Err(error) => {
                            first_error = Some(error);
                            break;
                        }
                    }
                }
            }

            // Drain all reads that were successfully issued, even when a later
            // issue failed. That keeps the buffers valid until Windows is done.
            for operation_index in 0..issued {
                let result = if operation_index == 0 {
                    reference_file.finish_read(&mut operations[0], amount)
                } else {
                    candidate_files[operation_index - 1].finish_read(&mut operations[operation_index], amount)
                };

                if let Err(error) = result {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }

            if let Some(error) = first_error {
                return Err(error);
            }

            workspace.record_reference_read(amount);
            workspace.record_candidate_read(amount * candidate_files.len());

            let reference = &workspace.reference_buffer[..amount];
            for candidate_buffer in workspace.overlapped_candidate_buffers.iter().take(candidate_files.len()) {
                if !buffers_equal(reference, &candidate_buffer[..amount]) {
                    return Ok(false);
                }
            }

            offset += amount as u64;
        }
    }

    if !group_fingerprints_unchanged(group, &before)? {
        return Ok(false);
    }

    group.verified = true;
    Ok(true)
}

/// Refines fclones prefix/suffix candidate groups into exact byte-identical
/// duplicate groups.
///
/// Unlike `verify_result`, this function does not assume that every member of
/// an input group is identical. A prefix/suffix candidate group may contain
/// multiple exact equivalence classes or false positives. The refiner splits
/// those classes, drops singletons and marks only byte-identical groups as
/// verified.
#[cfg(feature = "fast_duplicates")]
fn refine_result_exact(result: &mut DuplicateScanResult) -> io::Result<usize> {
    let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
    let input_groups = std::mem::take(&mut result.groups);
    let mut refined_groups = Vec::new();

    for group in input_groups {
        refined_groups.extend(refine_group_exact(group, &mut workspace, true)?);
    }

    let verified_groups = refined_groups.len();
    result.groups = refined_groups;
    Ok(verified_groups)
}

#[cfg(feature = "fast_duplicates")]
fn refine_group_exact(
    mut group: DuplicateGroup,
    workspace: &mut VerificationWorkspace,
    sequential_hint: bool,
) -> io::Result<Vec<DuplicateGroup>> {
    if group.files.len() < 2 {
        return Ok(Vec::new());
    }

    if group.files.len() > MAX_PINNED_FILES_PER_GROUP {
        return refine_group_exact_pairwise(group, workspace, sequential_hint);
    }

    let mut files = group
        .files
        .iter()
        .map(|entry| open_for_verification(&entry.path, sequential_hint))
        .collect::<io::Result<Vec<_>>>()?;

    let before = files.iter().map(file_fingerprint).collect::<io::Result<Vec<_>>>()?;
    let mut by_size = std::collections::BTreeMap::<u64, Vec<usize>>::new();
    for (index, fingerprint) in before.iter().enumerate() {
        by_size.entry(fingerprint.0).or_default().push(index);
    }

    let mut exact_classes: Vec<Vec<usize>> = Vec::new();

    for (size, indices) in by_size {
        if indices.len() < 2 {
            continue;
        }

        let mut remaining = indices;
        while remaining.len() >= 2 {
            let reference_index = remaining[0];
            files[reference_index].seek(SeekFrom::Start(0))?;
            for &candidate_index in &remaining[1..] {
                files[candidate_index].seek(SeekFrom::Start(0))?;
            }

            let mut active = remaining[1..].to_vec();
            let mut remaining_bytes = size;

            while remaining_bytes > 0 && !active.is_empty() {
                let amount = remaining_bytes.min(workspace.buffer_size() as u64) as usize;

                files[reference_index].read_exact(&mut workspace.reference_buffer[..amount])?;
                workspace.record_reference_read(amount);

                let mut still_matching = Vec::with_capacity(active.len());
                for candidate_index in active {
                    files[candidate_index].read_exact(&mut workspace.candidate_buffer[..amount])?;
                    workspace.record_candidate_read(amount);

                    if buffers_equal(&workspace.reference_buffer[..amount], &workspace.candidate_buffer[..amount]) {
                        still_matching.push(candidate_index);
                    }
                }

                active = still_matching;
                remaining_bytes -= amount as u64;
            }

            if !active.is_empty() {
                let mut class = Vec::with_capacity(active.len() + 1);
                class.push(reference_index);
                class.extend(active.iter().copied());
                exact_classes.push(class);
            }

            // Remove the reference and every exact match. Candidates that
            // differed remain to seed or join another exact class.
            remaining.retain(|index| *index != reference_index && !active.contains(index));
        }
    }

    let mut final_metadata = Vec::with_capacity(files.len());
    for (file, before_fingerprint) in files.iter().zip(&before) {
        let metadata = file.metadata()?;
        let after_fingerprint = (metadata.len(), metadata.modified().ok());
        if after_fingerprint != *before_fingerprint {
            return Ok(Vec::new());
        }
        final_metadata.push(metadata);
    }

    for (entry, metadata) in group.files.iter_mut().zip(&final_metadata) {
        entry.refresh_from_metadata(metadata);
    }

    let mut refined = Vec::with_capacity(exact_classes.len());
    for class in exact_classes {
        let files = class.into_iter().map(|index| group.files[index].clone()).collect();
        refined.push(DuplicateGroup { files, verified: true });
    }

    Ok(refined)
}

#[cfg(feature = "fast_duplicates")]
fn refine_group_exact_pairwise(
    mut group: DuplicateGroup,
    workspace: &mut VerificationWorkspace,
    sequential_hint: bool,
) -> io::Result<Vec<DuplicateGroup>> {
    // Large-group fallback: use only two open handles at a time so an unusual
    // group cannot exhaust the process handle budget. This path is slower but
    // preserves exactness and can still split false-positive candidate groups.
    // The whole group gets a before/after fingerprint check because handles are
    // intentionally not pinned for the complete fallback operation.
    let before = capture_group_fingerprints(&group)?;
    let mut remaining = (0..group.files.len()).collect::<Vec<_>>();
    let mut exact_classes: Vec<Vec<usize>> = Vec::new();

    while remaining.len() >= 2 {
        let reference_index = remaining[0];
        let mut matches = Vec::new();

        for &candidate_index in &remaining[1..] {
            if before[reference_index].0 != before[candidate_index].0 {
                continue;
            }

            let mut reference_file = open_for_verification(&group.files[reference_index].path, sequential_hint)?;
            let mut candidate_file = open_for_verification(&group.files[candidate_index].path, sequential_hint)?;
            let mut remaining_bytes = before[reference_index].0;
            let mut equal = true;

            while remaining_bytes > 0 {
                let amount = remaining_bytes.min(workspace.buffer_size() as u64) as usize;
                reference_file.read_exact(&mut workspace.reference_buffer[..amount])?;
                candidate_file.read_exact(&mut workspace.candidate_buffer[..amount])?;
                workspace.record_reference_read(amount);
                workspace.record_candidate_read(amount);

                if !buffers_equal(&workspace.reference_buffer[..amount], &workspace.candidate_buffer[..amount]) {
                    equal = false;
                    break;
                }
                remaining_bytes -= amount as u64;
            }

            if equal {
                matches.push(candidate_index);
            }
        }

        if !matches.is_empty() {
            let mut class = Vec::with_capacity(matches.len() + 1);
            class.push(reference_index);
            class.extend(matches.iter().copied());
            exact_classes.push(class);
        }

        remaining.retain(|index| *index != reference_index && !matches.contains(index));
    }

    if !group_fingerprints_unchanged(&mut group, &before)? {
        return Ok(Vec::new());
    }

    let mut refined = Vec::with_capacity(exact_classes.len());
    for class in exact_classes {
        let files = class.into_iter().map(|index| group.files[index].clone()).collect();
        refined.push(DuplicateGroup { files, verified: true });
    }
    Ok(refined)
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

        #[cfg(windows)]
        for sequential_hint in [false, true] {
            let mut equal = equal_group.clone();
            let mut equal_workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
            assert!(
                verify_group_overlapped(&mut equal, &mut equal_workspace, sequential_hint).expect("overlapped equal verification"),
                "equal files failed with overlapped sequential_hint={sequential_hint}"
            );

            let mut different = different_group.clone();
            let mut different_workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
            assert!(
                !verify_group_overlapped(&mut different, &mut different_workspace, sequential_hint)
                    .expect("overlapped different verification"),
                "different files passed with overlapped sequential_hint={sequential_hint}"
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
    fn prefix_suffix_candidates_are_split_by_exact_content() {
        use crate::duplicate_engine::{compare_results, DuplicateEngine, DuplicateScanRequest, FclonesEngine};

        let dir = tempdir().expect("tempdir");
        let size = 2 * 1024 * 1024 + 123;
        let middle = 1024 * 1024;
        let base = vec![0x33; size];

        // All five files have the same size, prefix and suffix. Only their
        // middle bytes differ. Full-hash fclones sees two duplicate classes and
        // one singleton; the prefix/suffix scan may place all five together.
        let mut class_a = base.clone();
        class_a[middle] = 0x44;
        let mut class_b = base.clone();
        class_b[middle] = 0x55;
        let mut singleton = base;
        singleton[middle] = 0x66;

        fs::write(dir.path().join("a-1.bin"), &class_a).expect("write a1");
        fs::write(dir.path().join("a-2.bin"), &class_a).expect("write a2");
        fs::write(dir.path().join("b-1.bin"), &class_b).expect("write b1");
        fs::write(dir.path().join("b-2.bin"), &class_b).expect("write b2");
        fs::write(dir.path().join("singleton.bin"), &singleton).expect("write singleton");

        let request = DuplicateScanRequest::for_paths([dir.path().to_path_buf()]);
        let mut full_hash = FclonesEngine.scan(&request).expect("full-hash scan");
        let mut candidates = FclonesEngine
            .scan_prefix_suffix_candidates(&request)
            .expect("prefix/suffix candidate scan");

        let full_verified = StdBufferedVerifier.verify_result(&mut full_hash).expect("full exact verification");
        assert_eq!(full_verified, full_hash.groups.len());
        assert_eq!(full_hash.groups.len(), 2);

        let refined = refine_result_exact(&mut candidates).expect("candidate exact refinement");
        assert_eq!(refined, candidates.groups.len());
        assert_eq!(candidates.groups.len(), 2);
        assert!(candidates.groups.iter().all(|group| group.verified));

        let comparison = compare_results(&full_hash, &candidates);
        assert!(comparison.identical(), "exact refinement disagreed with full-hash fclones: {comparison:#?}");
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
        println!(" KROKIET - BENCHMARK EXACT VERIFY: LIMITE DE ESCALADO");
        println!("============================================================");
        println!("Carpeta       : {}", path.display());
        println!("Detector      : fclones full hash (una vez por ronda)");
        println!("Verifier      : pinned handles + 1 MiB + byte exacto");
        println!("Estrategias   : secuencial / 6 / 8 / 12 / 16 grupos");
        println!(
            "CPU logicos    : {}",
            std::thread::available_parallelism().map(|value| value.get()).unwrap_or(1)
        );
        println!("Rondas        : {runs}");
        println!("Medicion      : mismo resultado fclones para todas las estrategias de una ronda");
        println!("Pipeline      : scan comun de la ronda + tiempo exact de cada estrategia");
        println!();

        let request = DuplicateScanRequest::for_paths([path]);

        // Correctness warm-up. Every thread count must produce exactly the same
        // verified groups before any timing is considered.
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
        let mut modeled_total_times: Vec<Vec<Duration>> = (0..strategies.len()).map(|_| Vec::with_capacity(runs)).collect();

        for round in 0..runs {
            println!();
            println!("RONDA {}/{}", round + 1, runs);

            // One scan per round. This removes the biggest source of noise from
            // the previous benchmark: scan time cannot accidentally favor one
            // verifier strategy over another.
            let scan_started = Instant::now();
            let round_candidates = FclonesEngine.scan(&request).expect("fallo fclones");
            let scan_elapsed = scan_started.elapsed();
            scan_times.push(scan_elapsed);
            assert!(compare_results(&baseline_candidates, &round_candidates).identical());
            println!("  scan comun : {:>10.3?}", scan_elapsed);

            // Rotate which verifier runs first so warm-cache effects are spread
            // across all strategies over the benchmark. This phase searches for
            // the saturation point above the previously winning 8-group setting.
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
                let modeled_total = scan_elapsed + exact_elapsed;

                assert_eq!(verified, result.groups.len(), "{name} no verifico todos los grupos");
                assert!(result.groups.iter().all(|group| group.verified));
                assert!(compare_results(&sequential_warm, &result).identical(), "{name} produjo resultados distintos");

                exact_times[index].push(exact_elapsed);
                modeled_total_times[index].push(modeled_total);

                println!(
                    "  {:<10}: exact {:>10.3?} | pipeline {:>10.3?} | {verified}/{} [OK]",
                    name,
                    exact_elapsed,
                    modeled_total,
                    result.groups.len(),
                );
            }
        }

        let scan_median = median_duration(&mut scan_times);
        let mut exact_medians = Vec::with_capacity(strategies.len());
        let mut total_medians = Vec::with_capacity(strategies.len());
        for index in 0..strategies.len() {
            exact_medians.push(median_duration(&mut exact_times[index]));
            total_medians.push(median_duration(&mut modeled_total_times[index]));
        }

        println!();
        println!("------------------------------------------------------------");
        println!(" MEDIANA DE {runs} RONDA(S)");
        println!("------------------------------------------------------------");
        println!(" scan comun : {:>10.3?}", scan_median);
        for index in 0..strategies.len() {
            println!(
                " {:<10}: exact {:>10.3?} | pipeline {:>10.3?}",
                strategies[index].0, exact_medians[index], total_medians[index]
            );
        }

        let (best_exact_index, best_exact) = exact_medians
            .iter()
            .enumerate()
            .min_by_key(|(_, duration)| duration.as_nanos())
            .expect("benchmark sin exact timings");
        let (best_total_index, best_total) = total_medians
            .iter()
            .enumerate()
            .min_by_key(|(_, duration)| duration.as_nanos())
            .expect("benchmark sin pipeline timings");

        println!("------------------------------------------------------------");
        println!(" Mejor exact   : {} ({:.3?})", strategies[best_exact_index].0, best_exact);
        println!(" Mejor pipeline: {} ({:.3?})", strategies[best_total_index].0, best_total);
        if best_exact.as_secs_f64() > 0.0 {
            println!(
                " Exact vs seq   : {:.3}x",
                exact_medians[0].as_secs_f64() / best_exact.as_secs_f64()
            );
        }
        if best_total.as_secs_f64() > 0.0 {
            println!(
                " Pipeline vs seq: {:.3}x",
                total_medians[0].as_secs_f64() / best_total.as_secs_f64()
            );
        }
        println!("============================================================");
    }
}
