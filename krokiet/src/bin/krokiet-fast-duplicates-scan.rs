#![cfg_attr(test, allow(dead_code, unused_imports))]

#[cfg(all(not(test), feature = "fast_duplicates"))]
use std::env;
#[cfg(all(not(test), feature = "fast_duplicates"))]
use std::ffi::OsString;
#[cfg(all(not(test), feature = "fast_duplicates"))]
use std::io::{self, Write};
#[cfg(all(not(test), feature = "fast_duplicates"))]
use std::path::PathBuf;
#[cfg(all(not(test), feature = "fast_duplicates"))]
use std::process::{Command, ExitCode};
#[cfg(all(not(test), feature = "fast_duplicates"))]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(all(not(test), feature = "fast_duplicates"))]
use czkawka_core::common::config_cache_path::{get_config_cache_path, set_config_cache_path};

#[cfg(all(not(test), feature = "fast_duplicates"))]
#[path = "../duplicate_engine/mod.rs"]
mod duplicate_engine;

#[cfg(all(not(test), feature = "fast_duplicates"))]
use duplicate_engine::{
    DuplicateScanExecution, DuplicateScanRequest, DuplicateScanServiceOptions,
    DuplicateServiceEngine, MissingProfileDecision, MissingStorageProfile, ProfileFlowAction,
    ProfileFlowExitReason, RequestProfileStatus, ScanAttemptContext, StorageTuningTrace,
    TuningFallbackReason, TuningSource, decide_profile_flow, inspect_request_profiles,
    run_duplicate_scan,
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

    let _ = set_config_cache_path("Czkawka", "Krokiet");

    let mut request = DuplicateScanRequest::for_paths(parsed.paths.clone());
    request.recursive = parsed.recursive;
    request.min_size = parsed.min_size;
    request.max_size = parsed.max_size;

    let options = match parsed.engine {
        DuplicateServiceEngine::Czkawka => DuplicateScanServiceOptions::czkawka(),
        DuplicateServiceEngine::Fast => {
            let tuning_store_path = if let Some(path) = parsed.tuning_store_path.clone() {
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
    println!(" KROKIET - DUPLICATE SCAN SERVICE 3.3.0");
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

    let execution = if options.engine == DuplicateServiceEngine::Fast {
        let Some(execution) = run_fast_profile_flow(
            &request,
            &options,
            parsed.missing_profile_mode,
        )? else {
            return Ok(());
        };
        execution
    } else {
        run_duplicate_scan(&request, &options)?
    };

    println!();
    println!("============================================================");
    println!(" RESULTADO");
    println!("============================================================");
    println!("Detector        : {}", execution.result.engine);
    println!("Grupos          : {}", execution.result.groups.len());
    println!("Archivos        : {}", execution.result.file_count());
    println!("Tiempo detector : {:.3?}", execution.result.elapsed);
    println!("Tiempo perfil   : {:.3?}", execution.profile_lookup_elapsed);
    println!("Tiempo verifier : {:.3?}", execution.verification_elapsed);
    println!("Tiempo total    : {:.3?}", execution.total_elapsed);

    match execution.verifier_workers {
        Some(workers) => println!("Workers verifier: {workers}"),
        None => println!("Workers verifier: no aplica"),
    }

    if !execution.storage_tuning.is_empty() {
        println!("------------------------------------------------------------");
        println!(" PERFIL POR ALMACENAMIENTO");
        println!("------------------------------------------------------------");
        for trace in &execution.storage_tuning {
            print_tuning_trace(trace);
        }
    } else if options.engine == DuplicateServiceEngine::Fast && execution.result.groups.is_empty() {
        println!("Perfil          : omitido (sin candidatos que verificar)");
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
fn run_fast_profile_flow(
    request: &DuplicateScanRequest,
    options: &DuplicateScanServiceOptions,
    mode: MissingProfileMode,
) -> Result<Option<DuplicateScanExecution>, Box<dyn std::error::Error>> {
    let attempt_id = new_attempt_id();
    let mut context = ScanAttemptContext::new(attempt_id);

    loop {
        let profile_status = match inspect_request_profiles(request, options) {
            Ok(status) => status,
            Err(error) => {
                eprintln!(
                    "AVISO: no se pudo comprobar el perfil antes del scan: {error}.\n\
                     Se continua con el fallback universal de 1 worker; no se inicia calibracion."
                );
                return Ok(Some(run_duplicate_scan(request, options)?));
            }
        };

        match decide_profile_flow(context, &profile_status, None) {
            ProfileFlowAction::StartScanWithProfile => {
                if context.profile_reentries > 0 {
                    println!(
                        "Perfil encontrado tras {} reentrada(s). Reanudando el flujo de analisis.",
                        context.profile_reentries
                    );
                }
                return Ok(Some(run_duplicate_scan(request, options)?));
            }
            ProfileFlowAction::AskUser => {
                let decision = choose_missing_profile_action(mode, &profile_status)?;
                match decide_profile_flow(context, &profile_status, Some(decision)) {
                    ProfileFlowAction::StartScanSafe => {
                        println!(
                            "Continuando en modo seguro: 1 worker. No se ejecutara calibracion."
                        );
                        return Ok(Some(run_duplicate_scan(request, options)?));
                    }
                    ProfileFlowAction::CalibrateAndReenter { next_context } => {
                        let RequestProfileStatus::Missing(missing) = &profile_status else {
                            unreachable!("calibration is only requested for missing profiles");
                        };

                        println!(
                            "Deteniendo el flujo de analisis. Calibracion externa para {} almacenamiento(s).",
                            missing.len()
                        );
                        if let Err(error) = run_independent_calibrator(missing, options) {
                            eprintln!("La calibracion independiente fallo: {error}");
                            print_flow_exit(context, ProfileFlowExitReason::CalibrationFailed);
                            return Ok(None);
                        }

                        context = next_context;
                        println!(
                            "Calibracion finalizada. Reentrada {}/{} del mismo intento {}.",
                            context.profile_reentries,
                            duplicate_engine::MAX_PROFILE_REENTRIES,
                            context.attempt_id
                        );
                        continue;
                    }
                    ProfileFlowAction::Exit(reason) => {
                        print_flow_exit(context, reason);
                        return Ok(None);
                    }
                    ProfileFlowAction::AskUser | ProfileFlowAction::StartScanWithProfile => {
                        unreachable!("decision must resolve the missing-profile prompt");
                    }
                }
            }
            ProfileFlowAction::Exit(reason) => {
                print_flow_exit(context, reason);
                return Ok(None);
            }
            ProfileFlowAction::CalibrateAndReenter { .. }
            | ProfileFlowAction::StartScanSafe => {
                unreachable!("no user decision was supplied yet");
            }
        }
    }
}

#[cfg(all(not(test), feature = "fast_duplicates"))]
fn choose_missing_profile_action(
    mode: MissingProfileMode,
    status: &RequestProfileStatus,
) -> Result<MissingProfileDecision, Box<dyn std::error::Error>> {
    let RequestProfileStatus::Missing(missing) = status else {
        return Ok(MissingProfileDecision::ContinueSafe);
    };

    println!();
    println!("============================================================");
    println!(" PERFIL DE VERIFICACION NO DISPONIBLE");
    println!("============================================================");
    for item in missing {
        println!("Storage key     : {}", item.storage_key);
        println!("Ruta referencia : {}", item.representative_path.display());
    }
    println!();
    println!("La calibracion NO forma parte del scan.");
    println!("Si eliges calibrar, el flujo de analisis se detiene, se ejecuta");
    println!("krokiet-verifier-tune y despues se vuelve a entrar por la comprobacion de perfil.");

    match mode {
        MissingProfileMode::Calibrate => Ok(MissingProfileDecision::Calibrate),
        MissingProfileMode::Safe => Ok(MissingProfileDecision::ContinueSafe),
        MissingProfileMode::Exit => Ok(MissingProfileDecision::Exit),
        MissingProfileMode::Ask => {
            print!("\n¿Calibrar ahora? [s] si / [n] modo seguro (1 worker) / [x] salir: ");
            io::stdout().flush()?;
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            match answer.trim().to_ascii_lowercase().as_str() {
                "s" | "si" | "sí" | "y" | "yes" => Ok(MissingProfileDecision::Calibrate),
                "x" | "exit" | "salir" | "q" => Ok(MissingProfileDecision::Exit),
                _ => Ok(MissingProfileDecision::ContinueSafe),
            }
        }
    }
}

#[cfg(all(not(test), feature = "fast_duplicates"))]
fn run_independent_calibrator(
    missing: &[MissingStorageProfile],
    options: &DuplicateScanServiceOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let store_path = options
        .tuning_store_path
        .as_deref()
        .ok_or("fast duplicate service requires a verifier tuning store path")?;
    let tuner = verifier_tuner_executable()?;

    for item in missing {
        println!();
        println!("------------------------------------------------------------");
        println!(" CALIBRADOR EXTERNO");
        println!("------------------------------------------------------------");
        println!("Ejecutable      : {}", tuner.display());
        println!("Storage key     : {}", item.storage_key);
        println!("Ruta            : {}", item.representative_path.display());

        let status = Command::new(&tuner)
            .arg("--tuning-store")
            .arg(store_path)
            .arg(&item.representative_path)
            .status()?;

        if !status.success() {
            return Err(format!(
                "krokiet-verifier-tune termino con codigo {:?} para {}",
                status.code(),
                item.representative_path.display()
            )
            .into());
        }
    }

    Ok(())
}

#[cfg(all(not(test), feature = "fast_duplicates"))]
fn verifier_tuner_executable() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let current = env::current_exe()?;
    let file_name = if cfg!(windows) {
        "krokiet-verifier-tune.exe"
    } else {
        "krokiet-verifier-tune"
    };
    let tuner = current.with_file_name(file_name);
    if !tuner.is_file() {
        return Err(format!(
            "no se encontro el calibrador junto al scan: {}",
            tuner.display()
        )
        .into());
    }
    Ok(tuner)
}

#[cfg(all(not(test), feature = "fast_duplicates"))]
fn new_attempt_id() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    (nanos as u64) ^ ((std::process::id() as u64) << 32)
}

#[cfg(all(not(test), feature = "fast_duplicates"))]
fn print_flow_exit(context: ScanAttemptContext, reason: ProfileFlowExitReason) {
    let reason = match reason {
        ProfileFlowExitReason::UserCancelled => "cancelado por el usuario",
        ProfileFlowExitReason::ReentryLimitReached => {
            "limite de 3 reentradas alcanzado sin obtener un perfil valido"
        }
        ProfileFlowExitReason::CalibrationFailed => "fallo de calibracion",
    };
    println!();
    println!("EXIT            : {reason}");
    println!("Intento         : {}", context.attempt_id);
    println!("Reentradas      : {}", context.profile_reentries);
    println!("Scan ejecutado  : no");
    println!("============================================================");
}

#[cfg(all(not(test), feature = "fast_duplicates"))]
fn print_tuning_trace(trace: &StorageTuningTrace) {
    let source = match trace.source {
        TuningSource::ReusedProfile => "perfil guardado",
        TuningSource::Fallback => "fallback seguro (scan sin auto-calibracion)",
    };

    println!("Storage key     : {}", trace.storage_key);
    println!("Origen perfil   : {source}");
    println!("Workers         : {}", trace.workers);
    println!(
        "Estabilidad     : {}",
        trace
            .stability
            .map(|stability| stability.label_es())
            .unwrap_or("no disponible")
    );

    if let Some(reason) = trace.fallback_reason {
        let reason_text = match reason {
            TuningFallbackReason::NoStoredProfile => {
                "no existe perfil calibrado; el scan no calibra automaticamente"
            }
            TuningFallbackReason::NoWritableCalibrationDirectory => {
                "sin directorio escribible para calibrar"
            }
            TuningFallbackReason::InsufficientFreeSpace => {
                "espacio insuficiente para calibracion"
            }
            TuningFallbackReason::CalibrationInvalid => "calibracion estadisticamente invalida",
            TuningFallbackReason::CalibrationExecutionFailed => "error ejecutando la calibracion",
            TuningFallbackReason::ProfilePersistenceFailed => {
                "perfil no persistido; se mantiene la medicion para este scan"
            }
            TuningFallbackReason::TuningUnavailable => "tuning no disponible",
            TuningFallbackReason::StorageIdentificationFailed => {
                "no se pudo identificar el almacenamiento"
            }
        };
        println!("Motivo fallback : {reason_text}");
        if reason == TuningFallbackReason::NoStoredProfile {
            println!("Accion          : ejecutar krokiet-verifier-tune fuera del analisis");
        }

    }

    if let Some(detail) = trace.diagnostic_detail.as_deref() {
        println!("Detalle         : {detail}");
    }
    println!();
}

#[cfg(all(not(test), feature = "fast_duplicates"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MissingProfileMode {
    Ask,
    Calibrate,
    Safe,
    Exit,
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
    missing_profile_mode: MissingProfileMode,
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
    let mut missing_profile_mode = MissingProfileMode::Ask;

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
            "--calibrate-missing" => missing_profile_mode = MissingProfileMode::Calibrate,
            "--safe-fallback" => missing_profile_mode = MissingProfileMode::Safe,
            "--exit-on-missing" => missing_profile_mode = MissingProfileMode::Exit,
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
        missing_profile_mode,
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
           --calibrate-missing     Calibra fuera del scan y reentra al flujo\n\
           --safe-fallback         Continua directamente con 1 worker\n\
           --exit-on-missing       Sale si no existe un perfil valido\n\
           -h, --help              Muestra esta ayuda\n"
    );
}
