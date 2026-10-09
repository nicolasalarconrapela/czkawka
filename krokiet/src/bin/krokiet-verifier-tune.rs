#![cfg_attr(test, allow(dead_code, unused_imports))]

// Standalone calibration executable for Krokiet's exact verifier.
//
// Cargo also compiles binary targets as test crates during `cargo test`. The
// duplicate-engine sources contain tests that expect Krokiet's main crate layout,
// so these source modules are included only for the normal executable build.

#[cfg(not(test))]
use std::env;
#[cfg(not(test))]
use std::ffi::OsString;
#[cfg(not(test))]
use std::path::PathBuf;
#[cfg(not(test))]
use std::process::ExitCode;

#[cfg(not(test))]
use czkawka_core::common::config_cache_path::{get_config_cache_path, set_config_cache_path};

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
use tuning::{
    CalibrationOptions, CalibrationStability, SAFE_FALLBACK_WORKERS, calibrate_and_store,
    cleanup_abandoned_calibration_dirs, find_writable_calibration_directory,
    required_calibration_free_space, storage_key_for_path, storage_snapshot_for_path,
};

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
    let parsed = parse_args(env::args_os().skip(1).collect())?;
    let path = parsed.path.unwrap_or(env::current_dir()?);

    if !path.is_dir() {
        return Err(format!(
            "la ruta no existe o no es un directorio: {}",
            path.display()
        )
        .into());
    }

    let _ = set_config_cache_path("Czkawka", "Krokiet");
    let store_path = if let Some(path) = parsed.tuning_store_path {
        path
    } else {
        let config = get_config_cache_path().ok_or(
            "Krokiet no pudo obtener su directorio de cache/configuracion",
        )?;
        config.cache_folder.join("verifier-tuning.json")
    };

    println!("============================================================");
    println!(" KROKIET - VERIFIER TUNING 3.3.0");
    println!("============================================================");
    println!("Ruta solicitada : {}", path.display());
    println!("Modo            : calibracion local independiente");
    println!("Perfil destino  : {}", store_path.display());
    println!();

    let canonical_requested = std::fs::canonicalize(&path)?;
    let storage_key = storage_key_for_path(&canonical_requested)?;
    let Some(calibration_directory) = find_writable_calibration_directory(&canonical_requested)? else {
        println!("Ruta canonica   : {}", canonical_requested.display());
        println!("Storage key     : {storage_key}");
        println!("Escritura       : no disponible para calibracion sintetica");
        println!("Calibracion     : OMITIDA");
        println!(
            "Workers         : {} (fallback seguro hasta calibrar)",
            SAFE_FALLBACK_WORKERS
        );
        println!();
        println!("El analisis normal NO intentara calibrar por su cuenta.");
        println!("============================================================");
        return Ok(());
    };

    if calibration_directory != canonical_requested {
        println!("Ruta canonica   : {}", canonical_requested.display());
        println!("Storage key     : {storage_key}");
        println!(
            "Carpeta prueba  : {}",
            calibration_directory.display()
        );
        println!();
    }

    let options = CalibrationOptions::default();
    let _ = cleanup_abandoned_calibration_dirs(&calibration_directory);
    let required = required_calibration_free_space(options);

    if let Some(snapshot) = storage_snapshot_for_path(&calibration_directory)
        && let Some(available) = snapshot.available_space
        && available < required
    {
        println!("Ruta canonica   : {}", canonical_requested.display());
        println!("Storage key     : {storage_key}");
        println!("Calibracion     : OMITIDA");
        println!("Motivo          : espacio insuficiente para dataset sintetico");
        println!("Espacio libre   : {:.1} MiB", available as f64 / 1024.0 / 1024.0);
        println!("Espacio minimo  : {:.1} MiB", required as f64 / 1024.0 / 1024.0);
        println!();
        println!("No se ha modificado el perfil guardado.");
        println!("El analisis normal NO intentara calibrar por su cuenta.");
        println!("============================================================");
        return Ok(());
    }

    let report = calibrate_and_store(&calibration_directory, &store_path, options)?;

    if calibration_directory == canonical_requested {
        println!(
            "Ruta canonica   : {}",
            canonical_requested.display()
        );
        println!("Storage key     : {}", report.profile.storage_key);
    }
    println!("CPU logicos     : {}", report.profile.logical_cpus);
    println!("Grupos          : {}", report.groups);
    println!(
        "Lectura/ronda   : {:.1} MiB",
        report.bytes_per_run as f64 / 1024.0 / 1024.0
    );
    if report.rescue_attempted {
        println!(
            "Ventana inicial : {:.0} ms",
            report.initial_target_sample_time.as_secs_f64() * 1000.0
        );
        println!(
            "Resultado inicial: {}",
            report
                .initial_stability
                .map(CalibrationStability::label_es)
                .unwrap_or("desconocida")
        );
        println!("------------------------------------------------------------");
        println!(" MODO DE RESCATE");
    }
    println!("Rondas base     : {}", report.initial_rounds);
    println!(
        "Ventana/sample  : {:.0} ms",
        report.target_sample_time.as_secs_f64() * 1000.0
    );
    if report.rechecked_workers.is_empty() {
        println!("Recheck         : no necesario");
    } else {
        println!(
            "Recheck         : {} ronda(s) extra para {:?}",
            report.recheck_rounds, report.rechecked_workers
        );
    }
    println!(
        "Final pareada   : {} ronda(s) para {:?}",
        report.pair_rounds, report.paired_workers
    );
    println!();

    for measurement in &report.measurements {
        println!(
            "workers-{:>2}: {:>10.3?} | {:>9.1} MiB/s | n={:<2} | runs={:<3} | MAD {:>5.1}%",
            measurement.workers,
            measurement.median,
            measurement.median_mib_per_second,
            measurement.samples,
            measurement.verification_runs,
            measurement.relative_mad * 100.0,
        );
    }

    if !report.pairwise.is_empty() {
        println!("------------------------------------------------------------");
        println!(" CONFIRMACION PAREADA");
        for comparison in &report.pairwise {
            println!(
                " {:>2} vs {:>2}: ratio {:>6.3} | victorias decisivas {}/{} | {}",
                comparison.incumbent_workers,
                comparison.challenger_workers,
                comparison.median_ratio,
                comparison.decisive_challenger_wins,
                comparison.pairs,
                if comparison.promoted {
                    "PROMUEVE"
                } else {
                    "MANTIENE"
                },
            );
        }
    }

    println!("------------------------------------------------------------");
    println!("Seleccion inicial: {} worker(s)", report.initial_selected_workers);
    println!(
        "Seleccion agregada: {} worker(s)",
        report.aggregate_selected_workers
    );
    println!("Seleccion       : {} worker(s)", report.profile.workers);
    println!(
        "Rendimiento     : {:.1} MiB/s",
        report.profile.median_mib_per_second
    );
    println!(
        "Margen al mejor : {:.2}%",
        report.profile.selected_slowdown_vs_fastest * 100.0
    );
    println!(
        "MAD seleccionado: {:.2}%",
        report.profile.relative_mad * 100.0
    );
    println!(
        "MAD decision    : {:.2}%",
        report.profile.decision_relative_mad * 100.0
    );
    println!(
        "MAD global      : {:.2}% (diagnostico)",
        report.profile.global_relative_mad * 100.0
    );
    println!(
        "Estabilidad     : {}",
        report.profile.stability.label_es()
    );

    if report.profile.stability == CalibrationStability::Invalid {
        println!("Perfil          : NO GUARDADO");
    } else {
        println!("Perfil          : GUARDADO");
        println!("Archivo         : {}", store_path.display());
    }

    println!("============================================================");

    Ok(())
}


#[cfg(not(test))]
#[derive(Debug)]
struct ParsedArgs {
    path: Option<PathBuf>,
    tuning_store_path: Option<PathBuf>,
}

#[cfg(not(test))]
fn parse_args(args: Vec<OsString>) -> Result<ParsedArgs, Box<dyn std::error::Error>> {
    let mut path = None;
    let mut tuning_store_path = None;
    let mut index = 0_usize;

    while index < args.len() {
        let value = args[index].to_string_lossy();
        match value.as_ref() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--tuning-store" => {
                index += 1;
                let raw = args
                    .get(index)
                    .ok_or("--tuning-store necesita una ruta de archivo")?;
                tuning_store_path = Some(PathBuf::from(raw));
            }
            option if option.starts_with('-') => {
                return Err(format!("opcion desconocida: {option}").into());
            }
            _ => {
                if path.is_some() {
                    return Err("solo puedes indicar una ruta de almacenamiento".into());
                }
                path = Some(PathBuf::from(&args[index]));
            }
        }
        index += 1;
    }

    Ok(ParsedArgs {
        path,
        tuning_store_path,
    })
}

#[cfg(not(test))]
fn print_usage() {
    println!(
        "Uso:\n\
         \n\
         krokiet-verifier-tune [opciones] [carpeta]\n\
         \n\
         Opciones:\n\
           --tuning-store <ruta>   Fuerza el archivo de perfiles\n\
           -h, --help              Muestra esta ayuda\n"
    );
}
