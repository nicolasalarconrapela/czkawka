#![cfg_attr(test, allow(dead_code, unused_imports))]

// Phase 3.1 diagnostic command for Krokiet's duplicate scan integration service.
//
// This binary deliberately reuses the real `duplicate_engine` module sources.
// It does not copy the scanning/tuning/verifier logic. The GUI is still untouched.

#[cfg(all(not(test), feature = "fast_duplicates"))]
use std::env;
#[cfg(all(not(test), feature = "fast_duplicates"))]
use std::ffi::OsString;
#[cfg(all(not(test), feature = "fast_duplicates"))]
use std::path::PathBuf;
#[cfg(all(not(test), feature = "fast_duplicates"))]
use std::process::ExitCode;

#[cfg(all(not(test), feature = "fast_duplicates"))]
use czkawka_core::common::config_cache_path::{get_config_cache_path, set_config_cache_path};

#[cfg(all(not(test), feature = "fast_duplicates"))]
#[path = "../duplicate_engine/mod.rs"]
mod duplicate_engine;

#[cfg(all(not(test), feature = "fast_duplicates"))]
use duplicate_engine::{
    DuplicateScanRequest, DuplicateScanServiceOptions, DuplicateServiceEngine,
    StorageTuningTrace, TuningFallbackReason, TuningSource, run_duplicate_scan,
};

#[cfg(all(not(test), feature = "fast_duplicates"))]
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ERROR: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(all(not(test), not(feature = "fast_duplicates")))]
fn main() {
    eprintln!(
        "Este comando necesita la feature `fast_duplicates`.\n\
         Ejecutalo con --features \"fast_duplicates,winit_femtovg\"."
    );
    std::process::exit(2);
}

#[cfg(all(not(test), feature = "fast_duplicates"))]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let parsed = parse_args(env::args_os().skip(1).collect())?;

    for path in &parsed.paths {
        if !path.is_dir() {
            return Err(format!(
                "la ruta no existe o no es un directorio: {}",
                path.display()
            )
            .into());
        }
    }

    // Match Krokiet's normal application identity so the diagnostic command uses
    // the same config/cache root family as the GUI.
    let _ = set_config_cache_path("Czkawka", "Krokiet");

    let mut request = DuplicateScanRequest::for_paths(parsed.paths.clone());
    request.recursive = parsed.recursive;
    request.min_size = parsed.min_size;
    request.max_size = parsed.max_size;

    let options = match parsed.engine {
        DuplicateServiceEngine::Czkawka => DuplicateScanServiceOptions::czkawka(),
        DuplicateServiceEngine::Fast => {
            let tuning_store_path = if let Some(path) = parsed.tuning_store_path {
                path
            } else {
                let config = get_config_cache_path().ok_or(
                    "Krokiet no pudo obtener su directorio de cache/configuracion",
                )?;
                config.cache_folder.join("verifier-tuning.json")
            };

            DuplicateScanServiceOptions::fast(tuning_store_path)
        }
    };

    println!("============================================================");
    println!(" KROKIET - DUPLICATE SCAN SERVICE 3.1.2");
    println!("============================================================");
    println!(
        "Motor           : {}",
        match options.engine {
            DuplicateServiceEngine::Czkawka => "Czkawka",
            DuplicateServiceEngine::Fast => "Fast (fclones + exact verifier)",
        }
    );
    println!("Recursivo       : {}", if request.recursive { "si" } else { "no" });
    println!("Tamano minimo   : {} bytes", request.min_size);
    println!(
        "Tamano maximo   : {}",
        request
            .max_size
            .map(|value| format!("{value} bytes"))
            .unwrap_or_else(|| "sin limite".to_string())
    );
    println!("Rutas           :");
    for path in &request.paths {
        println!("  - {}", path.display());
    }

    if let Some(path) = options.tuning_store_path.as_ref() {
        println!("Perfil tuning   : {}", path.display());
    }

    println!("------------------------------------------------------------");
    println!(" Ejecutando...");
    println!("------------------------------------------------------------");

    let execution = run_duplicate_scan(&request, &options)?;

    println!();
    println!("============================================================");
    println!(" RESULTADO");
    println!("============================================================");
    println!("Detector        : {}", execution.result.engine);
    println!("Grupos          : {}", execution.result.groups.len());
    println!("Archivos        : {}", execution.result.file_count());
    println!("Tiempo detector : {:.3?}", execution.result.elapsed);
    println!("Tiempo tuning   : {:.3?}", execution.tuning_elapsed);
    println!("Tiempo verifier : {:.3?}", execution.verification_elapsed);
    println!("Tiempo total    : {:.3?}", execution.total_elapsed);

    match execution.verifier_workers {
        Some(workers) => println!("Workers verifier: {workers}"),
        None => println!("Workers verifier: no aplica"),
    }

    if !execution.storage_tuning.is_empty() {
        println!("------------------------------------------------------------");
        println!(" TUNING POR ALMACENAMIENTO");
        println!("------------------------------------------------------------");
        for trace in &execution.storage_tuning {
            print_tuning_trace(trace);
        }
    } else if options.engine == DuplicateServiceEngine::Fast && execution.result.groups.is_empty() {
        println!("Tuning          : omitido (sin candidatos que verificar)");
    }

    let all_verified = execution.result.groups.iter().all(|group| group.verified);
    println!("------------------------------------------------------------");
    println!(
        "Verificacion    : {}",
        if options.engine == DuplicateServiceEngine::Fast {
            if execution.result.groups.is_empty() {
                "no necesaria (sin candidatos)"
            } else if all_verified {
                "EXACTA [OK]"
            } else {
                "ERROR"
            }
        } else {
            "gestionada por Czkawka"
        }
    );
    println!("============================================================");

    Ok(())
}

#[cfg(all(not(test), feature = "fast_duplicates"))]
fn print_tuning_trace(trace: &StorageTuningTrace) {
    let source = match trace.source {
        TuningSource::ReusedProfile => "perfil guardado",
        TuningSource::Calibrated => "calibracion nueva",
        TuningSource::Fallback => "fallback seguro",
    };

    println!("Storage key     : {}", trace.storage_key);
    println!("Origen tuning   : {source}");
    println!("Workers         : {}", trace.workers);
    println!(
        "Estabilidad     : {}",
        trace
            .stability
            .map(|stability| stability.label_es())
            .unwrap_or("no disponible")
    );
    if let Some(reason) = trace.fallback_reason {
        let reason = match reason {
            TuningFallbackReason::NoWritableCalibrationDirectory => {
                "sin directorio escribible para calibrar"
            }
            TuningFallbackReason::CalibrationInvalid => "calibracion estadisticamente invalida",
            TuningFallbackReason::CalibrationExecutionFailed => "error ejecutando la calibracion",
            TuningFallbackReason::ProfilePersistenceFailed => "error guardando el perfil de tuning",
            TuningFallbackReason::TuningUnavailable => "tuning no disponible",
            TuningFallbackReason::StorageIdentificationFailed => {
                "no se pudo identificar el almacenamiento"
            }
        };
        println!("Motivo fallback : {reason}");
    }
    if let Some(detail) = trace.diagnostic_detail.as_deref() {
        println!("Detalle         : {detail}");
    }
    println!();
}

#[cfg(all(not(test), feature = "fast_duplicates"))]
#[derive(Debug)]
struct ParsedArgs {
    engine: DuplicateServiceEngine,
    paths: Vec<PathBuf>,
    recursive: bool,
    min_size: u64,
    max_size: Option<u64>,
    tuning_store_path: Option<PathBuf>,
}

#[cfg(all(not(test), feature = "fast_duplicates"))]
fn parse_args(args: Vec<OsString>) -> Result<ParsedArgs, Box<dyn std::error::Error>> {
    if args.is_empty() {
        print_usage();
        return Err("debes indicar al menos una carpeta".into());
    }

    let mut engine = DuplicateServiceEngine::Fast;
    let mut paths = Vec::new();
    let mut recursive = true;
    let mut min_size = 1_u64;
    let mut max_size = None;
    let mut tuning_store_path = None;

    let mut index = 0_usize;
    while index < args.len() {
        let value = args[index].to_string_lossy();

        match value.as_ref() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--fast" => engine = DuplicateServiceEngine::Fast,
            "--czkawka" => engine = DuplicateServiceEngine::Czkawka,
            "--no-recursive" => recursive = false,
            "--min-size" => {
                index += 1;
                let raw = args.get(index).ok_or("--min-size necesita un numero de bytes")?;
                min_size = raw
                    .to_string_lossy()
                    .parse::<u64>()
                    .map_err(|_| "--min-size debe ser un numero entero de bytes")?;
            }
            "--max-size" => {
                index += 1;
                let raw = args.get(index).ok_or("--max-size necesita un numero de bytes")?;
                max_size = Some(
                    raw.to_string_lossy()
                        .parse::<u64>()
                        .map_err(|_| "--max-size debe ser un numero entero de bytes")?,
                );
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
            _ => paths.push(PathBuf::from(&args[index])),
        }

        index += 1;
    }

    if paths.is_empty() {
        return Err("debes indicar al menos una carpeta".into());
    }

    if let Some(max) = max_size
        && max < min_size
    {
        return Err(format!(
            "--max-size ({max}) no puede ser menor que --min-size ({min_size})"
        )
        .into());
    }

    Ok(ParsedArgs {
        engine,
        paths,
        recursive,
        min_size,
        max_size,
        tuning_store_path,
    })
}

#[cfg(all(not(test), feature = "fast_duplicates"))]
fn print_usage() {
    println!(
        "Uso:\n\
         \n\
         krokiet-fast-duplicates-scan [opciones] <carpeta> [carpeta...]\n\
         \n\
         Opciones:\n\
           --fast                  Usa Fast Engine (por defecto)\n\
           --czkawka               Usa el motor Czkawka de referencia\n\
           --no-recursive          No entra en subcarpetas\n\
           --min-size <bytes>      Tamano minimo (por defecto: 1)\n\
           --max-size <bytes>      Tamano maximo\n\
           --tuning-store <ruta>   Fuerza el archivo de perfiles de tuning\n\
           -h, --help              Muestra esta ayuda\n"
    );
}
