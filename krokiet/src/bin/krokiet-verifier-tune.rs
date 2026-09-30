use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

#[path = "../duplicate_engine/types.rs"]
mod types;
#[path = "../duplicate_engine/verify.rs"]
mod verify;
#[path = "../duplicate_engine/tuning.rs"]
mod tuning;

use tuning::{CalibrationOptions, calibrate_directory};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ERROR: {error}");
            ExitCode::FAILURE
        }
    }
}

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
