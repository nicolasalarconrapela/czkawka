use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use super::types::{DuplicateGroup, metadata_fingerprint};
#[cfg(feature = "fast_duplicates")]
use super::types::DuplicateScanResult;

// A larger buffer reduces the number of read syscalls without requiring a
// significant amount of memory. The workspace owns only two buffers and reuses
// them for every group verified by `verify_result`.
const VERIFY_BUFFER_SIZE: usize = 4 * 1024 * 1024;

// Comparing every candidate against the same reference chunk lets us read the
// reference only once for normal-sized groups. Bound the number of simultaneously
// open candidate files so very large duplicate groups cannot exhaust OS handles.
const MAX_CANDIDATES_PER_BATCH: usize = 16;

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

/// Verifies every file in the group against the first file byte-for-byte.
///
/// A hash match is only a candidate signal. `verified` becomes true exclusively
/// after this exact comparison succeeds and the compared files stayed unchanged
/// (size + modification time) during verification.
///
/// The optimized verifier keeps two reusable buffers, opens the reference file
/// only once per group and compares a reference chunk against several candidates
/// before reading the next reference chunk. For the common case of <= 17 files in
/// a group, the reference file is therefore read only once instead of once per
/// candidate.
pub(crate) fn verify_group(group: &mut DuplicateGroup) -> io::Result<bool> {
    let mut workspace = VerificationWorkspace::new();
    verify_group_with_workspace(group, &mut workspace)
}

/// Verifies all groups while reusing the same allocation workspace.
///
/// Keeping the buffers alive across groups removes the repeated multi-megabyte
/// allocations that the previous implementation performed for every file pair.
#[cfg(feature = "fast_duplicates")]
pub(crate) fn verify_result(result: &mut DuplicateScanResult) -> io::Result<usize> {
    let mut verified = 0;
    let mut workspace = VerificationWorkspace::new();

    for group in &mut result.groups {
        if verify_group_with_workspace(group, &mut workspace)? {
            verified += 1;
        }
    }

    Ok(verified)
}

// Older Phase 1 `mod.rs` revisions re-exported `verify_result` even when the
// experimental feature was disabled. Keep a loud compatibility shim so this
// optimized file can be dropped into either layout without breaking normal
// non-Fast-Engine builds. No production path is expected to call it.
#[cfg(not(feature = "fast_duplicates"))]
pub(crate) fn verify_result<T>(_result: &mut T) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Fast Engine exact-result verification requires the fast_duplicates feature",
    ))
}

fn verify_group_with_workspace(group: &mut DuplicateGroup, workspace: &mut VerificationWorkspace) -> io::Result<bool> {
    group.verified = false;

    if group.files.len() < 2 {
        return Ok(false);
    }

    // Capture the full group fingerprint before opening/reading any file. This
    // preserves the safety rule from the old verifier: if a file changes while
    // exact verification is running, the group is not marked as verified.
    let before = group
        .files
        .iter()
        .map(|file| metadata_fingerprint(&file.path))
        .collect::<io::Result<Vec<_>>>()?;

    let reference_size = before[0].0;
    if before.iter().any(|fingerprint| fingerprint.0 != reference_size) {
        return Ok(false);
    }

    let reference_path = &group.files[0].path;
    let mut reference_file = File::open(reference_path)?;

    for candidate_batch in group.files[1..].chunks(MAX_CANDIDATES_PER_BATCH) {
        // When a group is larger than the handle-safe batch size the reference
        // must be read once per batch. Normal duplicate groups fit in one batch.
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

    // Re-read metadata only after all byte comparisons are complete. Any size
    // or modification-time change invalidates the verification result.
    for (file, before_fingerprint) in group.files.iter().zip(&before) {
        let after_fingerprint = metadata_fingerprint(&file.path)?;
        if *before_fingerprint != after_fingerprint {
            return Ok(false);
        }
    }

    group.verified = true;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::duplicate_engine::DuplicateFile;

    fn group_from_paths(paths: Vec<std::path::PathBuf>) -> DuplicateGroup {
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

        assert!(verify_group_with_workspace(&mut group, &mut workspace).expect("verify"));
        assert!(group.verified);

        // Six files in one batch => one full read of the reference and one full
        // read of each of the five candidates.
        assert_eq!(workspace.reference_bytes_read, size as u64);
        assert_eq!(workspace.candidate_bytes_read, (size as u64) * 5);
    }

    #[test]
    fn optimized_verifier_batches_very_large_groups() {
        let dir = tempdir().expect("tempdir");
        let size = 64 * 1024 + 13;
        let data = vec![0x6D; size];

        // 18 files = 17 candidates. With a maximum of 16 candidates per
        // batch, the reference must be read exactly twice.
        let mut paths = Vec::new();
        for index in 0..18 {
            let path = dir.path().join(format!("large-group-{index}.bin"));
            fs::write(&path, &data).expect("write test file");
            paths.push(path);
        }

        let mut group = group_from_paths(paths);
        let mut workspace = VerificationWorkspace::new();

        assert!(verify_group_with_workspace(&mut group, &mut workspace).expect("verify"));
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

        assert!(!verify_group_with_workspace(&mut group, &mut workspace).expect("verify"));
        assert!(!group.verified);
    }
}
