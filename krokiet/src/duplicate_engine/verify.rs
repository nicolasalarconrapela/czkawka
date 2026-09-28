use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

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
        verify_group_with_io_strategy(group, &mut workspace, IO_SEQUENTIAL_HINT)
    }

    #[cfg(feature = "fast_duplicates")]
    fn verify_result(&self, result: &mut DuplicateScanResult) -> io::Result<usize> {
        verify_result_with_io_strategy(result, VERIFY_BUFFER_SIZE, IO_SEQUENTIAL_HINT)
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

    #[cfg(feature = "fast_duplicates")]
    #[derive(Debug, Clone, Copy)]
    struct SyncIoVerifier {
        name: &'static str,
        strategy: VerifierIoStrategy,
    }

    #[cfg(feature = "fast_duplicates")]
    impl ExactVerifier for SyncIoVerifier {
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

    #[cfg(all(feature = "fast_duplicates", windows))]
    #[derive(Debug, Clone, Copy)]
    struct OverlappedIoVerifier {
        name: &'static str,
        sequential_hint: bool,
    }

    #[cfg(all(feature = "fast_duplicates", windows))]
    impl ExactVerifier for OverlappedIoVerifier {
        fn name(&self) -> &'static str {
            self.name
        }

        fn verify_group(&self, group: &mut DuplicateGroup) -> io::Result<bool> {
            let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
            verify_group_overlapped(group, &mut workspace, self.sequential_hint)
        }

        fn verify_result(&self, result: &mut DuplicateScanResult) -> io::Result<usize> {
            let mut verified = 0;
            let mut workspace = VerificationWorkspace::new(VERIFY_BUFFER_SIZE);
            for group in &mut result.groups {
                if verify_group_overlapped(group, &mut workspace, self.sequential_hint)? {
                    verified += 1;
                }
            }
            Ok(verified)
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
        println!(" KROKIET - BENCHMARK OVERLAPPED I/O WINDOWS");
        println!("============================================================");
        println!("Carpeta       : {}", path.display());
        println!("Detector      : fclones (sin modificar)");
        println!("Comparacion   : std slice equality exacta");
        println!("Buffer        : 1 MiB fijo");
        println!("Rondas        : {runs}");
        println!("Baseline      : sync + FILE_FLAG_SEQUENTIAL_SCAN");
        #[cfg(windows)]
        println!("Candidatos    : overlapped / overlapped+sequential");
        #[cfg(not(windows))]
        println!("Candidatos    : overlapped solo esta disponible en Windows");
        println!("Seguridad     : mismos fingerprints antes/despues");
        println!();

        let request = DuplicateScanRequest::for_paths([path]);
        let candidates = FclonesEngine.scan(&request).expect("fallo el escaneo de fclones");
        assert!(!candidates.groups.is_empty(), "el dataset no contiene grupos duplicados");

        println!("Fast Engine   : {:.3?}", candidates.elapsed);
        println!("Grupos        : {}", candidates.groups.len());
        println!("Archivos      : {}", candidates.file_count());

        let baseline = SyncIoVerifier {
            name: "seq-sync",
            strategy: IO_SEQUENTIAL_HINT,
        };

        #[cfg(windows)]
        let overlapped = OverlappedIoVerifier {
            name: "overlapped",
            sequential_hint: false,
        };
        #[cfg(windows)]
        let overlapped_seq = OverlappedIoVerifier {
            name: "overlapped+seq",
            sequential_hint: true,
        };

        #[cfg(windows)]
        let verifiers: [&dyn ExactVerifier; 3] = [&baseline, &overlapped, &overlapped_seq];
        #[cfg(not(windows))]
        let verifiers: [&dyn ExactVerifier; 1] = [&baseline];

        // Warm every strategy once, then rotate order each round so the same
        // backend is not systematically first or last in the OS cache state.
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
                let verifier = verifiers[index];
                let mut sample = candidates.clone();

                let started = Instant::now();
                let verified = verifier.verify_result(&mut sample).expect("fallo la verificacion");
                let elapsed = started.elapsed();

                assert_eq!(verified, sample.groups.len(), "{} no verifico todos los grupos", verifier.name());
                println!("  {:<16}: {:.3?} | {verified}/{} grupos [OK]", verifier.name(), elapsed, sample.groups.len());
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
            println!(" {:<16}: {:.3?}", verifier.name(), median);
        }

        let (best_index, best_median) = medians
            .iter()
            .enumerate()
            .min_by_key(|(_, duration)| duration.as_nanos())
            .expect("benchmark sin resultados");

        let baseline_median = medians[0];
        println!("------------------------------------------------------------");
        println!(" Mejor I/O     : {} ({:.3?})", verifiers[best_index].name(), best_median);
        if baseline_median.as_secs_f64() > 0.0 && best_median.as_secs_f64() > 0.0 {
            println!(
                " Mejor vs seq   : {:.3}x mas rapido",
                baseline_median.as_secs_f64() / best_median.as_secs_f64()
            );
        }
        println!("============================================================");
    }
}
