//! Experimental duplicate-engine abstraction.
//!
//! Phase 1 deliberately does not replace the GUI's current `DuplicateFinder` path.
//! It gives us two independently testable engines with one result format, so we can
//! prove correctness before switching anything user-visible.
//!
//! Migration plan:
//! 1. Keep `connect_scan/duplicate.rs` as the production path while tests mature.
//! 2. Reach settings/progress/cancellation parity in this abstraction.
//! 3. Route the GUI scan through `DuplicateEngine`.
//! 4. Remove only the duplicated direct `DuplicateFinder` plumbing from the GUI
//!    layer; keep `CzkawkaEngine` as a reference/fallback for comparison.

#[cfg(all(test, feature = "fast_duplicates"))]
mod benchmark;
#[cfg(test)]
mod test_console;
mod compare;
mod czkawka;
#[cfg(feature = "fast_duplicates")]
mod fclones;
mod types;
mod verify;

pub(crate) use compare::{DuplicateComparison, compare_results};
pub(crate) use czkawka::CzkawkaEngine;
#[cfg(feature = "fast_duplicates")]
pub(crate) use fclones::FclonesEngine;
pub(crate) use types::{DuplicateEngine, DuplicateEngineError, DuplicateFile, DuplicateGroup, DuplicateScanRequest, DuplicateScanResult};
pub(crate) use verify::{verify_group, verify_result};

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;

    fn write_file(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).expect("failed to create test file");
    }

    #[test]
    fn exact_verifier_accepts_identical_files() {
        let dir = tempdir().expect("failed to create temp dir");
        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.bin");
        let data = vec![0x5A; 128 * 1024];
        write_file(&a, &data);
        write_file(&b, &data);

        let mut group = DuplicateGroup::new(vec![
            DuplicateFile::from_path(a).expect("metadata"),
            DuplicateFile::from_path(b).expect("metadata"),
        ]);

        assert!(verify_group(&mut group).expect("verification failed"));
        assert!(group.verified);
    }

    #[test]
    fn exact_verifier_rejects_same_edges_but_different_middle() {
        let dir = tempdir().expect("failed to create temp dir");
        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.bin");

        let mut left = vec![b'A'; 64 * 1024];
        let mut right = left.clone();
        left[32 * 1024] = b'B';
        right[32 * 1024] = b'X';
        // Same size, same first bytes and same last bytes; only the middle differs.
        write_file(&a, &left);
        write_file(&b, &right);

        let mut group = DuplicateGroup::new(vec![
            DuplicateFile::from_path(a).expect("metadata"),
            DuplicateFile::from_path(b).expect("metadata"),
        ]);

        assert!(!verify_group(&mut group).expect("verification failed"));
        assert!(!group.verified);
    }

    #[cfg(feature = "fast_duplicates")]
    #[test]
    fn czkawka_and_fclones_agree_on_basic_dataset() {
        let dir = tempdir().expect("failed to create temp dir");

        let duplicate = vec![0x42; 96 * 1024];
        write_file(&dir.path().join("same-a.bin"), &duplicate);
        write_file(&dir.path().join("same-b.bin"), &duplicate);

        // Same size as each other, but different content.
        write_file(&dir.path().join("different-a.bin"), &vec![0x11; 80 * 1024]);
        write_file(&dir.path().join("different-b.bin"), &vec![0x22; 80 * 1024]);

        // Same beginning/end, different middle.
        let mut middle_a = vec![0x33; 128 * 1024];
        let mut middle_b = middle_a.clone();
        middle_a[64 * 1024] = 0x44;
        middle_b[64 * 1024] = 0x55;
        write_file(&dir.path().join("middle-a.bin"), &middle_a);
        write_file(&dir.path().join("middle-b.bin"), &middle_b);

        use std::time::Instant;

        super::test_console::banner("KROKIET FAST ENGINE - CORRECTNESS TEST");
        super::test_console::info("dataset", "generated basic dataset");
        super::test_console::info("expected groups", 1);
        super::test_console::info("expected dup files", 2);
        super::test_console::section_break();
        super::test_console::step(1, 5, "Dataset prepared", true);

        let request = DuplicateScanRequest::for_paths([dir.path().to_path_buf()]);
        let old = CzkawkaEngine.scan(&request).expect("Czkawka scan failed");
        super::test_console::engine_step(2, 5, &old);

        let mut fast = FclonesEngine.scan(&request).expect("fclones scan failed");
        super::test_console::engine_step(3, 5, &fast);

        let comparison = compare_results(&old, &fast);
        let parity_ok = comparison.identical();
        super::test_console::step(4, 5, "Normalized group comparison", parity_ok);
        super::test_console::comparison_details(&comparison);

        let verify_started = Instant::now();
        let verified_groups = verify_result(&mut fast).expect("exact verification failed");
        let verify_elapsed = verify_started.elapsed();
        let verification_ok = verified_groups == fast.groups.len();
        super::test_console::step(5, 5, "Exact byte-for-byte verification", verification_ok);
        super::test_console::correctness_summary(&old, &fast, verify_elapsed, parity_ok, verification_ok);

        assert!(parity_ok, "{comparison:#?}");
        assert_eq!(old.groups.len(), 1, "unexpected Czkawka groups: {old:#?}");
        assert_eq!(fast.groups.len(), 1, "unexpected fclones groups: {fast:#?}");
        assert_eq!(old.file_count(), 2, "unexpected Czkawka duplicate-file count: {old:#?}");
        assert_eq!(fast.file_count(), 2, "unexpected fclones duplicate-file count: {fast:#?}");
        assert_eq!(verified_groups, fast.groups.len(), "at least one fclones group failed exact byte verification");
    }

}
