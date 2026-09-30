#![cfg_attr(test, allow(dead_code, unused_imports))]

// This utility is a standalone calibration executable.
//
// Important: Cargo also compiles binary targets as test crates during `cargo test`.
// The duplicate-engine sources contain tests that expect to live under
// `crate::duplicate_engine` in Krokiet's main binary.  The tuner only needs these
// modules for its normal executable build, so exclude them from the test-harness
// build.  This keeps `cargo test` for Krokiet from recompiling/running the
// duplicate-engine test suite through this auxiliary binary.

#[cfg(not(test))]
use std::env;
#[cfg(not(test))]
use std::path::PathBuf;
#[cfg(not(test))]
use std::process::ExitCode;

#[cfg(not(test))]
#[path = "../duplicate_engine/types.rs"]
mod types;
#[cfg(not(test))]
#[path = "../duplicate_engine/verify.rs"]
mod verify;
#[cfg(not(test))]
#[path = "../duplicate_engine/tuning.rs"]
mod tuning;

#[cfg(not(test))]
use tuning::{CalibrationOptions, calibrate_directory};

#[cfg(not(test))]
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ERROR: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(test))]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?);

    if !path.is_dir() {
        return Err(format!("la ruta no existe o no es un directorio: {}", path.display()).into());
    }

    println!("============================================================");
    println!(" KROKIET - VERIFIER TUNING");
    println!("============================================================");
    println!("Ruta solicitada : {}", path.display());
    println!("Modo            : calibracion local independiente");
    println!();

    let report = calibrate_directory(&path, CalibrationOptions::default())?;

    println!("Ruta canonica   : {}", report.profile.calibrated_path.display());
    println!("Storage key     : {}", report.profile.storage_key);
    println!("CPU logicos     : {}", report.profile.logical_cpus);
    println!("Grupos          : {}", report.groups);
    println!(
        "Lectura/ronda   : {:.1} MiB",
        report.bytes_per_run as f64 / 1024.0 / 1024.0
    );
    println!();

    for measurement in &report.measurements {
        println!(
            "workers-{:>2}: {:>10.3?} | {:>9.1} MiB/s",
            measurement.workers,
            measurement.median,
            measurement.median_mib_per_second,
        );
    }

    println!("------------------------------------------------------------");
    println!("Seleccion       : {} worker(s)", report.profile.workers);
    println!(
        "Rendimiento     : {:.1} MiB/s",
        report.profile.median_mib_per_second
    );
    println!("============================================================");

    Ok(())
}
