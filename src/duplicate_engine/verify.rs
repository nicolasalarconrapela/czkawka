use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use super::types::{DuplicateGroup, DuplicateScanResult, metadata_fingerprint};

const VERIFY_BUFFER_SIZE: usize = 1024 * 1024;

/// Verifies every file in the group against the first file byte-for-byte.
///
/// A hash match is only a candidate signal. `verified` becomes true exclusively
/// after this exact comparison succeeds and the compared files stayed unchanged
/// (size + modification time) during verification.
pub(crate) fn verify_group(group: &mut DuplicateGroup) -> io::Result<bool> {
    group.verified = false;
    let Some(reference) = group.files.first() else {
        return Ok(false);
    };
    if group.files.len() < 2 {
        return Ok(false);
    }

    for candidate in group.files.iter().skip(1) {
        if !files_equal_stable(&reference.path, &candidate.path)? {
            return Ok(false);
        }
    }

    group.verified = true;
    Ok(true)
}

pub(crate) fn verify_result(result: &mut DuplicateScanResult) -> io::Result<usize> {
    let mut verified = 0;
    for group in &mut result.groups {
        if verify_group(group)? {
            verified += 1;
        }
    }
    Ok(verified)
}

fn files_equal_stable(left: &Path, right: &Path) -> io::Result<bool> {
    let left_before = metadata_fingerprint(left)?;
    let right_before = metadata_fingerprint(right)?;
    if left_before.0 != right_before.0 {
        return Ok(false);
    }

    let mut left_file = File::open(left)?;
    let mut right_file = File::open(right)?;
    let mut left_buffer = vec![0_u8; VERIFY_BUFFER_SIZE];
    let mut right_buffer = vec![0_u8; VERIFY_BUFFER_SIZE];
    let mut remaining = left_before.0;

    while remaining > 0 {
        let amount = remaining.min(VERIFY_BUFFER_SIZE as u64) as usize;
        left_file.read_exact(&mut left_buffer[..amount])?;
        right_file.read_exact(&mut right_buffer[..amount])?;
        if left_buffer[..amount] != right_buffer[..amount] {
            return Ok(false);
        }
        remaining -= amount as u64;
    }

    let left_after = metadata_fingerprint(left)?;
    let right_after = metadata_fingerprint(right)?;
    if left_before != left_after || right_before != right_after {
        return Ok(false);
    }

    Ok(true)
}
