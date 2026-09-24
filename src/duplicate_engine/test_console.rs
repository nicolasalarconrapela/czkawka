use std::fmt::Display;
use std::path::PathBuf;
use std::time::Duration;

use super::{DuplicateComparison, DuplicateScanResult};

const RULE: &str = "+======================================================================+";
const SUB_RULE: &str = "+----------------------------------------------------------------------+";

pub(super) fn banner(title: &str) {
    println!("\n{RULE}");
    println!("| {title:<68} |");
    println!("{RULE}");
}

pub(super) fn info(label: &str, value: impl Display) {
    let value = value.to_string();
    println!("| {label:<24} {value:<43} |");
}

pub(super) fn section_break() {
    println!("{SUB_RULE}");
}

pub(super) fn step(index: usize, total: usize, label: &str, ok: bool) {
    let status = if ok { "[PASS]" } else { "[FAIL]" };
    println!("[{index}/{total}] {label:<52} {status:>8}");
}

pub(super) fn engine_step(index: usize, total: usize, result: &DuplicateScanResult) {
    println!(
        "[{index}/{total}] {:<18} {:>8} | groups {:>5} | files {:>6}  [PASS]",
        result.engine,
        duration_text(result.elapsed),
        result.groups.len(),
        result.file_count(),
    );
}

pub(super) fn round_header(round: usize, total: usize, order: &str) {
    println!("\nROUND {round}/{total}  ({order})");
    println!("----------------------------------------------------------------------");
}

pub(super) fn round_result(label: &str, elapsed: Duration) {
    println!("  {label:<22} {}", duration_text(elapsed));
}

pub(super) fn round_counts(groups: usize, files: usize, reclaimable: &str) {
    println!("  {:<22} {groups}", "duplicate groups");
    println!("  {:<22} {files}", "duplicate files");
    println!("  {:<22} {reclaimable}", "reclaimable");
}

pub(super) fn comparison_details(comparison: &DuplicateComparison) {
    if comparison.identical() {
        return;
    }

    println!("\nDIFFERENCES");
    println!("-----------");
    print_path_groups("Only in Czkawka", &comparison.only_left);
    print_path_groups("Only in fclones", &comparison.only_right);
}

fn print_path_groups(title: &str, groups: &[Vec<PathBuf>]) {
    if groups.is_empty() {
        return;
    }

    println!("{title}: {} group(s)", groups.len());
    for (group_index, group) in groups.iter().enumerate().take(5) {
        println!("  group {}:", group_index + 1);
        for path in group.iter().take(8) {
            println!("    {}", path.display());
        }
        if group.len() > 8 {
            println!("    ... {} more file(s)", group.len() - 8);
        }
    }
    if groups.len() > 5 {
        println!("  ... {} more group(s)", groups.len() - 5);
    }
}

pub(super) fn correctness_summary(
    czkawka: &DuplicateScanResult,
    fclones: &DuplicateScanResult,
    verify_elapsed: Duration,
    parity_ok: bool,
    verification_ok: bool,
) {
    let safe_total = fclones.elapsed + verify_elapsed;
    let ratio = speed_ratio(czkawka.elapsed, safe_total);

    section_break();
    println!("| {:<68} |", "RESULT");
    section_break();
    info("Czkawka", duration_text(czkawka.elapsed));
    info("fclones", duration_text(fclones.elapsed));
    info("exact verify", duration_text(verify_elapsed));
    info("fast safe total", duration_text(safe_total));
    info("result parity", pass_fail(parity_ok));
    info("byte verification", pass_fail(verification_ok));
    if let Some(ratio) = ratio {
        info("single-run ratio", format!("{ratio:.2}x (informational)"));
    }
    println!("{RULE}");
}

pub(super) fn benchmark_summary(
    czkawka: Duration,
    fclones: Duration,
    verify: Duration,
    safe: Duration,
) {
    banner("KROKIET FAST ENGINE - BENCHMARK RESULT");
    info("median Czkawka", duration_text(czkawka));
    info("median fclones", duration_text(fclones));
    info("median exact verify", duration_text(verify));
    info("median fast safe", duration_text(safe));
    info("result parity", "[PASS]");
    info("byte verification", "[PASS]");
    if let Some(ratio) = speed_ratio(czkawka, safe) {
        info("safe speed ratio", format!("{ratio:.2}x"));
    }
    println!("{RULE}");
}

pub(super) fn duration_text(duration: Duration) -> String {
    if duration.as_secs_f64() >= 1.0 {
        format!("{:.3} s", duration.as_secs_f64())
    } else {
        format!("{:.1} ms", duration.as_secs_f64() * 1000.0)
    }
}

fn pass_fail(ok: bool) -> &'static str {
    if ok { "[PASS]" } else { "[FAIL]" }
}

fn speed_ratio(reference: Duration, candidate: Duration) -> Option<f64> {
    let candidate = candidate.as_secs_f64();
    (candidate > 0.0).then(|| reference.as_secs_f64() / candidate)
}
