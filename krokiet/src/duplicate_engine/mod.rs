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

    use czkawka_core::common::config_cache_path::set_config_cache_path_test;
    use tempfile::tempdir;

    use super::*;

    fn write_file(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).expect("failed to create test file");
    }

    /// `DuplicateFinder` may access Czkawka's cache during hash/prehash scans.
    /// The normal Krokiet `main()` initializes these paths, but Rust's test
    /// harness does not execute `main()`, so tests must initialize them.
    ///
    /// Keep the directory alive for the whole test process instead of using a
    /// `TempDir`: `czkawka_core` stores the paths in a process-global OnceCell,
    /// and another test may use them after this function returns.
    fn init_test_config_cache() {
        let root = std::env::temp_dir().join(format!("krokiet-duplicate-engine-tests-{}", std::process::id()));
        let cache_path = root.join("cache");
        let config_path = root.join("config");

        fs::create_dir_all(&cache_path).expect("failed to create test cache directory");
        fs::create_dir_all(&config_path).expect("failed to create test config directory");

        set_config_cache_path_test(cache_path, config_path);
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
        init_test_config_cache();

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

        let request = DuplicateScanRequest::for_paths([dir.path().to_path_buf()]);
        let old = CzkawkaEngine.scan(&request).expect("Czkawka scan failed");
        let fast = FclonesEngine.scan(&request).expect("fclones scan failed");
        let comparison = compare_results(&old, &fast);

        assert!(comparison.identical(), "{comparison:#?}");
        assert_eq!(old.groups.len(), 1, "unexpected Czkawka groups: {old:#?}");
        assert_eq!(fast.groups.len(), 1, "unexpected fclones groups: {fast:#?}");
    }

    /// Manual real-dataset benchmark.
    ///
    /// PowerShell example:
    /// $env:KROKIET_DUP_BENCH_PATH = 'D:\\KrokietBenchmark\\mixed'
    /// cargo test -p krokiet --release --features fast_duplicates -j 1 \
    ///     duplicate_engine_real_dataset_benchmark -- --ignored --nocapture --test-threads=1
    #[cfg(feature = "fast_duplicates")]
    #[test]
    #[ignore = "manual benchmark; set KROKIET_DUP_BENCH_PATH"]
    fn duplicate_engine_real_dataset_benchmark() {
        init_test_config_cache();

        let path = std::env::var_os("KROKIET_DUP_BENCH_PATH").expect("set KROKIET_DUP_BENCH_PATH first");
        let request = DuplicateScanRequest::for_paths([path.into()]);

        let old = CzkawkaEngine.scan(&request).expect("Czkawka scan failed");
        let mut fast = FclonesEngine.scan(&request).expect("fclones scan failed");
        let comparison = compare_results(&old, &fast);

        let verify_started = std::time::Instant::now();
        let verified_groups = verify_result(&mut fast).expect("exact verification failed");
        let verify_elapsed = verify_started.elapsed();

        println!("\nKROKIET DUPLICATE ENGINE BENCHMARK");
        println!("---------------------------------");
        println!("Czkawka: {:>10.3?} | groups: {} | files: {}", old.elapsed, old.groups.len(), old.file_count());
        println!("fclones: {:>10.3?} | groups: {} | files: {}", fast.elapsed, fast.groups.len(), fast.file_count());
        println!("verify:  {:>10.3?} | verified groups: {}", verify_elapsed, verified_groups);
        println!("safe total: {:>8.3?}", fast.elapsed + verify_elapsed);
        println!("same results: {}", comparison.identical());
        println!("missing from fclones: {}", comparison.only_left.len());
        println!("extra in fclones: {}", comparison.only_right.len());
        if (fast.elapsed + verify_elapsed).as_secs_f64() > 0.0 {
            println!(
                "safe speedup: {:.2}x",
                old.elapsed.as_secs_f64() / (fast.elapsed + verify_elapsed).as_secs_f64()
            );
        }

        assert!(comparison.identical(), "engines returned different duplicate groups: {comparison:#?}");
        assert_eq!(verified_groups, fast.groups.len(), "at least one fclones group failed exact byte verification");
    }
}
