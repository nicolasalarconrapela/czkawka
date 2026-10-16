#![cfg_attr(test, allow(dead_code, unused_imports))]

// Standalone validation utility for Krokiet's normal-scan tuning path.
//
// It deliberately does NOT calibrate. It verifies that a normal duplicate scan
// can reuse an already persisted verifier profile, that the selected worker
// count is the one stored for the target storage, and that resolving the profile
// does not modify the tuning store or create calibration directories.

#[cfg(not(test))]
use std::env;
#[cfg(not(test))]
use std::ffi::OsString;
#[cfg(not(test))]
use std::fs;
#[cfg(not(test))]
use std::path::{Path, PathBuf};
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
use tuning::{VerifierScanResolution, resolve_scan_tuning_for_path};

#[cfg(not(test))]
const CALIBRATION_TEMP_PREFIX: &str = ".krokiet-verifier-tuning-";

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
    let args = parse_args(env::args_os().skip(1).collect())?;

    if !args.path.is_dir() {
        return Err(format!(
            "la ruta no existe o no es un directorio: {}",
            args.path.display()
        )
        .into());
    }
    if !args.tuning_store.is_file() {
        return Err(format!(
            "el perfil de tuning no existe: {}",
            args.tuning_store.display()
        )
        .into());
    }

    let canonical_target = fs::canonicalize(&args.path)?;
    let store_before = fs::read(&args.tuning_store)?;
    let temp_dirs_before = calibration_dirs(&canonical_target)?;

    println!("============================================================");
    println!(" KROKIET - PROFILE REUSE CHECK 3.4.5");
    println!("============================================================");
    println!("Ruta solicitada : {}", args.path.display());
    println!("Ruta canonica   : {}", canonical_target.display());
    println!("Perfil          : {}", args.tuning_store.display());
    println!("Operacion       : resolver de scan normal (solo lectura)");
    println!();

    let resolution = resolve_scan_tuning_for_path(&canonical_target, &args.tuning_store)?;

    let (storage_key, workers, stability) = match resolution {
        VerifierScanResolution::ReusedProfile {
            storage_key,
            workers,
            stability,
        } => (storage_key, workers, stability),
        VerifierScanResolution::Fallback {
            storage_key,
            workers,
            reason,
            detail,
        } => {
            return Err(format!(
                "el scan no reutilizo un perfil: storage={storage_key}, fallback_workers={workers}, reason={reason:?}, detail={detail:?}"
            )
            .into());
        }
    };

    let store_after = fs::read(&args.tuning_store)?;
    let temp_dirs_after = calibration_dirs(&canonical_target)?;

    let store_unchanged = store_before == store_after;
    let temp_dirs_unchanged = temp_dirs_before == temp_dirs_after;

    println!("Storage key     : {storage_key}");
    println!("Resolucion      : PERFIL REUTILIZADO");
    println!("Workers         : {workers}");
    println!("Estabilidad     : {}", stability.label_es());
    println!("Store modificado: {}", if store_unchanged { "NO" } else { "SI" });
    println!(
        "Calibracion nueva: {}",
        if temp_dirs_unchanged { "NO" } else { "SI" }
    );

    if let Some(expected) = args.expected_workers {
        println!("Workers esperados: {expected}");
        if workers != expected {
            return Err(format!(
                "el perfil activo usa {workers} worker(s), pero se esperaban {expected}"
            )
            .into());
        }
    }

    if !store_unchanged {
        return Err("el resolver de scan modifico el tuning store".into());
    }
    if !temp_dirs_unchanged {
        return Err("el resolver de scan creo o elimino directorios de calibracion".into());
    }

    println!("------------------------------------------------------------");
    println!(" RESULTADO FINAL: REUTILIZACION CORRECTA");
    println!("============================================================");
    Ok(())
}

#[cfg(not(test))]
fn calibration_dirs(path: &Path) -> Result<Vec<OsString>, Box<dyn std::error::Error>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(CALIBRATION_TEMP_PREFIX) {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(not(test))]
#[derive(Debug)]
struct Args {
    path: PathBuf,
    tuning_store: PathBuf,
    expected_workers: Option<usize>,
}

#[cfg(not(test))]
fn parse_args(args: Vec<OsString>) -> Result<Args, Box<dyn std::error::Error>> {
    let mut path = None;
    let mut tuning_store = None;
    let mut expected_workers = None;
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];
        let arg_text = arg.to_string_lossy();
        if arg_text == "--tuning-store" {
            index += 1;
            let value = args.get(index).ok_or("falta valor para --tuning-store")?;
            tuning_store = Some(PathBuf::from(value));
        } else if arg_text == "--expect-workers" {
            index += 1;
            let value = args.get(index).ok_or("falta valor para --expect-workers")?;
            let text = value.to_string_lossy();
            let parsed = text.parse::<usize>()?;
            if parsed == 0 {
                return Err("--expect-workers debe ser mayor que cero".into());
            }
            expected_workers = Some(parsed);
        } else if arg_text == "--help" || arg_text == "-h" {
            print_help();
            std::process::exit(0);
        } else if arg_text.starts_with('-') {
            return Err(format!("opcion desconocida: {arg_text}").into());
        } else if path.replace(PathBuf::from(arg.clone())).is_some() {
            return Err("solo se admite una ruta objetivo".into());
        }
        index += 1;
    }

    Ok(Args {
        path: path.ok_or("falta la ruta objetivo")?,
        tuning_store: tuning_store.ok_or("falta --tuning-store <archivo>")?,
        expected_workers,
    })
}

#[cfg(not(test))]
fn print_help() {
    println!("Krokiet profile reuse check 3.4.5");
    println!();
    println!("Uso:");
    println!("  krokiet-profile-reuse-check --tuning-store <json> [--expect-workers N] <ruta>");
}
