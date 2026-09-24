//! Reproducible manual benchmarks for the experimental duplicate engines.
//!
//! These are ignored tests on purpose: correctness tests remain fast, while performance
//! tests are opt-in and should be run directly with `cargo test --release`, keeping one
//! Cargo job on low-memory Windows machines.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tempfile::{Builder, TempDir};

use super::test_console;
use super::{compare_results, verify_result, CzkawkaEngine, DuplicateEngine, DuplicateScanRequest, FclonesEngine};

const DEFAULT_RUNS: usize = 3;
const DEFAULT_SCALE: usize = 1;

#[derive(Debug)]
struct GeneratedDataset {
    _temp_dir: TempDir,
    root: PathBuf,
    total_files: usize,
    total_bytes: u64,
    expected_groups: usize,
    expected_duplicate_files: usize,
}

#[derive(Default)]
struct Timings {
    czkawka: Vec<Duration>,
    fclones: Vec<Duration>,
    verify: Vec<Duration>,
    safe_total: Vec<Duration>,
}

#[test]
#[ignore = "manual visual benchmark; run this test explicitly with --ignored --nocapture"]
fn visual_duplicate_engine_benchmark() {
    let scale = env_usize("KROKIET_DUP_BENCH_SCALE", DEFAULT_SCALE).clamp(1, 16);
    let runs = env_usize("KROKIET_DUP_BENCH_RUNS", DEFAULT_RUNS).clamp(1, 9);
    let dataset = create_generated_dataset(scale);

    test_console::banner("KROKIET FAST ENGINE - GENERATED BENCHMARK");
    test_console::info("dataset", dataset.root.display());
    test_console::info("scale", scale);
    test_console::info("runs", runs);
    test_console::info("files", dataset.total_files);
    test_console::info("dataset bytes", human_bytes(dataset.total_bytes));
    test_console::info("expected groups", dataset.expected_groups);
    test_console::info("expected dup files", dataset.expected_duplicate_files);
    test_console::info("persistent cache", "disabled");
    test_console::section_break();
    println!("OS filesystem cache affects later rounds; compare medians on the same machine/dataset.");

    let request = DuplicateScanRequest::for_paths([dataset.root.clone()]);
    let timings = run_benchmark(&request, runs, Some((dataset.expected_groups, dataset.expected_duplicate_files)));
    print_summary(&timings);
}

#[test]
#[ignore = "manual benchmark; set KROKIET_DUP_BENCH_PATH"]
fn duplicate_engine_real_dataset_benchmark() {
    let path = env::var_os("KROKIET_DUP_BENCH_PATH").expect("set KROKIET_DUP_BENCH_PATH first");
    let path = PathBuf::from(path);
    assert!(path.exists(), "benchmark path does not exist: {}", path.display());
    let runs = env_usize("KROKIET_DUP_BENCH_RUNS", DEFAULT_RUNS).clamp(1, 9);

    test_console::banner("KROKIET FAST ENGINE - REAL DATASET BENCHMARK");
    test_console::info("dataset", path.display());
    test_console::info("runs", runs);
    test_console::info("persistent cache", "disabled");
    test_console::section_break();
    println!("READ ONLY: engines scan and the exact verifier only reads files.");
    println!("OS filesystem cache affects later rounds; compare medians on the same machine/dataset.");

    let request = DuplicateScanRequest::for_paths([path]);
    let timings = run_benchmark(&request, runs, None);
    print_summary(&timings);
}

fn run_benchmark(request: &DuplicateScanRequest, runs: usize, expected: Option<(usize, usize)>) -> Timings {
    let mut timings = Timings::default();

    for round in 0..runs {
        // Alternate the execution order to reduce the systematic advantage of always
        // running one engine second on already-warmed filesystem data.
        let (old, mut fast, order) = if round % 2 == 0 {
            let old = CzkawkaEngine.scan(request).expect("Czkawka scan failed");
            let fast = FclonesEngine.scan(request).expect("fclones scan failed");
            (old, fast, "Czkawka -> fclones")
        } else {
            let fast = FclonesEngine.scan(request).expect("fclones scan failed");
            let old = CzkawkaEngine.scan(request).expect("Czkawka scan failed");
            (old, fast, "fclones -> Czkawka")
        };

        test_console::round_header(round + 1, runs, order);
        test_console::round_result("Czkawka", old.elapsed);
        test_console::round_result("fclones", fast.elapsed);

        let comparison = compare_results(&old, &fast);
        let parity_ok = comparison.identical();
        test_console::step(1, 2, "Result parity", parity_ok);
        test_console::comparison_details(&comparison);
        assert!(parity_ok, "engines returned different duplicate groups: {comparison:#?}");

        if let Some((groups, files)) = expected {
            assert_eq!(old.groups.len(), groups, "unexpected Czkawka group count");
            assert_eq!(fast.groups.len(), groups, "unexpected fclones group count");
            assert_eq!(old.file_count(), files, "unexpected Czkawka duplicate-file count");
            assert_eq!(fast.file_count(), files, "unexpected fclones duplicate-file count");
        }

        let verify_started = Instant::now();
        let verified_groups = verify_result(&mut fast).expect("exact verification failed");
        let verify_elapsed = verify_started.elapsed();
        let verification_ok = verified_groups == fast.groups.len();
        test_console::step(2, 2, "Exact byte verification", verification_ok);
        assert!(verification_ok, "at least one fclones group failed exact byte verification");

        let safe_total = fast.elapsed + verify_elapsed;
        test_console::round_result("exact verify", verify_elapsed);
        test_console::round_result("fast safe total", safe_total);
        test_console::round_counts(fast.groups.len(), fast.file_count(), &human_bytes(fast.reclaimable_bytes()));

        timings.czkawka.push(old.elapsed);
        timings.fclones.push(fast.elapsed);
        timings.verify.push(verify_elapsed);
        timings.safe_total.push(safe_total);
    }

    timings
}

fn print_summary(timings: &Timings) {
    let czkawka = median(&timings.czkawka);
    let fclones = median(&timings.fclones);
    let verify = median(&timings.verify);
    let safe = median(&timings.safe_total);

    test_console::benchmark_summary(czkawka, fclones, verify, safe);
}

fn median(values: &[Duration]) -> Duration {
    assert!(!values.is_empty());
    let mut sorted = values.to_vec();
    sorted.sort_unstable_by_key(Duration::as_nanos);
    sorted[sorted.len() / 2]
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name).ok().and_then(|value| value.parse().ok()).unwrap_or(default)
}

fn create_generated_dataset(scale: usize) -> GeneratedDataset {
    let temp_dir = create_temp_dir();
    let root = temp_dir.path().to_path_buf();
    let mut total_files = 0usize;
    let mut total_bytes = 0u64;

    let unique_dir = root.join("01_unique_sizes");
    let same_size_dir = root.join("02_same_size_different");
    let pair_dir = root.join("03_duplicate_pairs");
    let quad_dir = root.join("04_duplicate_groups_of_four");
    let edge_dir = root.join("05_same_edges_different_middle");
    for dir in [&unique_dir, &same_size_dir, &pair_dir, &quad_dir, &edge_dir] {
        fs::create_dir_all(dir).expect("failed to create benchmark directory");
    }

    // Unique by size: cheap traversal/metadata pressure without forcing full hashing.
    for i in 0..(256 * scale) {
        let len = 4 * 1024 + i * 17;
        write_seeded(&unique_dir.join(format!("unique-{i:05}.bin")), 10_000 + i as u64, len, &mut total_files, &mut total_bytes);
    }

    // Same size, all different: forces prefix/suffix/full grouping logic to reject candidates.
    for i in 0..(64 * scale) {
        let len = 64 * 1024;
        write_seeded(&same_size_dir.join(format!("same-size-{i:05}-a.bin")), 100_000 + (i * 2) as u64, len, &mut total_files, &mut total_bytes);
        write_seeded(&same_size_dir.join(format!("same-size-{i:05}-b.bin")), 100_001 + (i * 2) as u64, len, &mut total_files, &mut total_bytes);
    }

    // True duplicate pairs.
    for i in 0..(32 * scale) {
        let data = seeded_bytes(200_000 + i as u64, 128 * 1024);
        for suffix in ['a', 'b'] {
            write_bytes(&pair_dir.join(format!("pair-{i:05}-{suffix}.bin")), &data, &mut total_files, &mut total_bytes);
        }
    }

    // True duplicate groups of four, larger than the pair dataset.
    for i in 0..(8 * scale) {
        let data = seeded_bytes(300_000 + i as u64, 512 * 1024);
        for member in 0..4 {
            write_bytes(&quad_dir.join(format!("quad-{i:05}-{member}.bin")), &data, &mut total_files, &mut total_bytes);
        }
    }

    // Same size + same 32 KiB prefix + same 32 KiB suffix, but different middle.
    // These must NEVER appear as duplicates after the complete engine pipeline.
    for i in 0..(16 * scale) {
        let len = 256 * 1024;
        let a = edge_trap_bytes(400_000 + i as u64, 1, len);
        let b = edge_trap_bytes(400_000 + i as u64, 2, len);
        write_bytes(&edge_dir.join(format!("edge-trap-{i:05}-a.bin")), &a, &mut total_files, &mut total_bytes);
        write_bytes(&edge_dir.join(format!("edge-trap-{i:05}-b.bin")), &b, &mut total_files, &mut total_bytes);
    }

    GeneratedDataset {
        _temp_dir: temp_dir,
        root,
        total_files,
        total_bytes,
        expected_groups: 40 * scale,
        expected_duplicate_files: 96 * scale,
    }
}

fn create_temp_dir() -> TempDir {
    if let Some(root) = env::var_os("KROKIET_DUP_BENCH_ROOT") {
        fs::create_dir_all(&root).expect("failed to create KROKIET_DUP_BENCH_ROOT");
        Builder::new().prefix("krokiet-dup-bench-").tempdir_in(root).expect("failed to create benchmark temp dir")
    } else {
        Builder::new().prefix("krokiet-dup-bench-").tempdir().expect("failed to create benchmark temp dir")
    }
}

fn write_seeded(path: &Path, seed: u64, len: usize, total_files: &mut usize, total_bytes: &mut u64) {
    let data = seeded_bytes(seed, len);
    write_bytes(path, &data, total_files, total_bytes);
}

fn write_bytes(path: &Path, data: &[u8], total_files: &mut usize, total_bytes: &mut u64) {
    fs::write(path, data).unwrap_or_else(|error| panic!("failed to write {}: {error}", path.display()));
    *total_files += 1;
    *total_bytes += data.len() as u64;
}

fn seeded_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut data = vec![0u8; len];
    for (i, byte) in data.iter_mut().enumerate() {
        let n = seed
            .wrapping_mul(0x9E37_79B1_85EB_CA87)
            .wrapping_add((i as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
        *byte = (n ^ (n >> 23) ^ (n >> 41)) as u8;
    }
    if len >= 8 {
        data[..8].copy_from_slice(&seed.to_le_bytes());
    }
    data
}

fn edge_trap_bytes(group_seed: u64, middle_variant: u64, len: usize) -> Vec<u8> {
    assert!(len > 96 * 1024);
    let edge_len = 32 * 1024;
    let mut data = vec![0u8; len];
    let prefix = seeded_bytes(group_seed, edge_len);
    let suffix = seeded_bytes(group_seed.wrapping_add(1), edge_len);
    let middle_len = len - 2 * edge_len;
    let middle = seeded_bytes(group_seed.wrapping_mul(10).wrapping_add(middle_variant), middle_len);
    data[..edge_len].copy_from_slice(&prefix);
    data[edge_len..edge_len + middle_len].copy_from_slice(&middle);
    data[len - edge_len..].copy_from_slice(&suffix);
    data
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.2} GiB", value / GIB)
    } else if value >= MIB {
        format!("{:.2} MiB", value / MIB)
    } else if value >= KIB {
        format!("{:.2} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}
