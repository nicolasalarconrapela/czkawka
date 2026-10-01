#![cfg_attr(test, allow(dead_code, unused_imports))]

// Standalone calibration executable for Krokiet's exact verifier.
//
// Cargo also compiles binary targets as test crates during `cargo test`. The
// duplicate-engine sources contain tests that expect Krokiet's main crate layout,
// so these source modules are included only for the normal executable build.

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
        return Err(format!(
            "la ruta no existe o no es un directorio: {}",
            path.display()
        )
        .into());
    }

    println!("============================================================");
    println!(" KROKIET - VERIFIER TUNING 2.9.1");
    println!("============================================================");
    println!("Ruta solicitada : {}", path.display());
    println!("Modo            : calibracion local independiente");
    println!();

    let report = calibrate_directory(&path, CalibrationOptions::default())?;

    println!(
        "Ruta canonica   : {}",
        report.profile.calibrated_path.display()
    );
    println!("Storage key     : {}", report.profile.storage_key);
    println!("CPU logicos     : {}", report.profile.logical_cpus);
    println!("Grupos          : {}", report.groups);
    println!(
        "Lectura/ronda   : {:.1} MiB",
        report.bytes_per_run as f64 / 1024.0 / 1024.0
    );
    println!("Rondas base     : {}", report.initial_rounds);
    if report.rechecked_workers.is_empty() {
        println!("Recheck         : no necesario");
    } else {
        println!(
            "Recheck         : {} ronda(s) extra para {:?}",
            report.recheck_rounds, report.rechecked_workers
        );
    }
    println!();

    for measurement in &report.measurements {
        println!(
            "workers-{:>2}: {:>10.3?} | {:>9.1} MiB/s | n={:<2} | MAD {:>5.1}%",
            measurement.workers,
            measurement.median,
            measurement.median_mib_per_second,
            measurement.samples,
            measurement.relative_mad * 100.0,
        );
    }

    println!("------------------------------------------------------------");
    println!("Seleccion       : {} worker(s)", report.profile.workers);
    if report.initial_selected_workers != report.profile.workers {
        println!(
            "Seleccion inicial: {} worker(s)",
            report.initial_selected_workers
        );
    }
    println!(
        "Rendimiento     : {:.1} MiB/s",
        report.profile.median_mib_per_second
    );
    println!(
        "Margen al mejor : {:.2}%",
        report.profile.selected_slowdown_vs_fastest * 100.0
    );
    println!(
        "Estabilidad     : {} (MAD {:.2}%)",
        report.profile.stability.label_es(),
        report.profile.relative_mad * 100.0
    );
    println!("============================================================");

    Ok(())
}
