use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::thread;

use sysinfo::{DiskExt, DiskKind, System, SystemExt};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KrokietStorageClass {
    Ssd,
    Hdd,
    Removable,
    Unknown,
}

impl KrokietStorageClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ssd => "SSD",
            Self::Hdd => "HDD",
            Self::Removable => "removable",
            Self::Unknown => "unknown",
        }
    }
}

fn main() {
    let requested_paths = env::args_os().skip(1).map(PathBuf::from).collect::<Vec<_>>();

    if requested_paths.is_empty() {
        eprintln!("Uso:");
        eprintln!("  krokiet-storage-probe.exe <ruta> [ruta ...]");
        eprintln!();
        eprintln!("Ejemplo:");
        eprintln!(r#"  krokiet-storage-probe.exe "C:\" "Q:\KrokietBenchmark" "E:\""#);
        std::process::exit(2);
    }

    let logical_cpus = thread::available_parallelism().map_or(1, usize::from).max(1);

    let mut system = System::new();
    system.refresh_disks_list();

    println!("============================================================");
    println!(" KROKIET - STORAGE PROBE 3.1.3");
    println!("============================================================");
    println!("CPU logicos     : {logical_cpus}");
    println!("Discos sysinfo  : {}", system.disks().len());
    println!();
    println!("Esta herramienta NO cambia el tuning ni guarda perfiles.");
    println!("Solo comprueba como clasifica el almacenamiento la misma API");
    println!("(sysinfo 0.29) que usa fclones 0.35.0.");

    for requested in requested_paths {
        println!();
        println!("------------------------------------------------------------");
        println!("Ruta solicitada : {}", requested.display());

        let absolute = absolute_path(&requested);
        println!("Ruta evaluada   : {}", absolute.display());

        let Some(disk) = best_matching_disk(system.disks(), &absolute) else {
            println!("Resultado       : no se encontro un disco/mount point");
            println!("Clase Krokiet   : unknown");
            println!("Fallback prop.  : 1 worker");
            continue;
        };

        let class = classify_storage(disk.kind(), disk.is_removable());
        let proposed_workers = proposed_exact_verifier_fallback(class, logical_cpus);
        let (fclones_random, fclones_sequential) = fclones_035_default_parallelism(disk.kind(), logical_cpus);

        println!("Mount point     : {}", disk.mount_point().display());
        println!("Dispositivo     : {}", display_os(disk.name()));
        println!("Filesystem      : {}", String::from_utf8_lossy(disk.file_system()));
        println!("DiskKind        : {}", disk_kind_label(disk.kind()));
        println!("Removable       : {}", yes_no(disk.is_removable()));
        println!("Clase Krokiet   : {}", class.as_str());
        println!("Espacio total   : {}", format_bytes(disk.total_space()));
        println!("Espacio libre   : {}", format_bytes(disk.available_space()));
        let required = default_calibration_required_space(logical_cpus);
        println!("Min. tuning     : {}", format_bytes(required));
        println!(
            "Preflight tuning: {}",
            if disk.available_space() >= required {
                "PERMITIDO"
            } else {
                "OMITIDO (espacio insuficiente)"
            }
        );
        println!();
        println!("fclones 0.35.0  : random={fclones_random}, sequential={fclones_sequential}");
        println!("Fallback prop.  : {proposed_workers} worker(s) para verifier exacto");

        if class == KrokietStorageClass::Ssd {
            println!("Nota            : este valor es conservador; un perfil calibrado");
            println!("                  seguira teniendo prioridad sobre el fallback.");
        }
    }

    println!();
    println!("============================================================");
    println!(" FIN - no se ha modificado ningun perfil");
    println!("============================================================");
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    env::current_dir()
        .map(|current| current.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn best_matching_disk<'a>(disks: &'a [sysinfo::Disk], path: &Path) -> Option<&'a sysinfo::Disk> {
    disks
        .iter()
        .filter(|disk| mount_matches(path, disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().components().count())
}

#[cfg(windows)]
fn mount_matches(path: &Path, mount_point: &Path) -> bool {
    fn normalize(value: &Path) -> String {
        value
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    }

    let path = normalize(path);
    let mount = normalize(mount_point);

    path == mount
        || path
            .strip_prefix(&mount)
            .is_some_and(|rest| rest.starts_with('\\'))
}

#[cfg(not(windows))]
fn mount_matches(path: &Path, mount_point: &Path) -> bool {
    path.starts_with(mount_point)
}

fn classify_storage(kind: DiskKind, removable: bool) -> KrokietStorageClass {
    if removable {
        return KrokietStorageClass::Removable;
    }

    match kind {
        DiskKind::SSD => KrokietStorageClass::Ssd,
        DiskKind::HDD => KrokietStorageClass::Hdd,
        DiskKind::Unknown(_) => KrokietStorageClass::Unknown,
    }
}

fn proposed_exact_verifier_fallback(class: KrokietStorageClass, logical_cpus: usize) -> usize {
    match class {
        // Deliberately much more conservative than fclones' hash pipeline.
        // This is only a fallback for our sequential full-file byte verifier.
        KrokietStorageClass::Ssd => logical_cpus.clamp(1, 4),
        KrokietStorageClass::Hdd | KrokietStorageClass::Removable | KrokietStorageClass::Unknown => 1,
    }
}

fn fclones_035_default_parallelism(kind: DiskKind, logical_cpus: usize) -> (usize, usize) {
    match kind {
        DiskKind::SSD => {
            let workers = logical_cpus.saturating_mul(4).max(1);
            (workers, workers)
        }
        DiskKind::HDD => (1, 1),
        DiskKind::Unknown(_) => (logical_cpus.saturating_mul(4).max(1), 1),
    }
}

fn disk_kind_label(kind: DiskKind) -> &'static str {
    match kind {
        DiskKind::SSD => "SSD",
        DiskKind::HDD => "HDD",
        DiskKind::Unknown(_) => "Unknown",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "si" } else { "no" }
}

fn display_os(value: &OsStr) -> String {
    let text = value.to_string_lossy();
    if text.is_empty() {
        "(sin nombre)".to_string()
    } else {
        text.into_owned()
    }
}

fn default_calibration_required_space(logical_cpus: usize) -> u64 {
    const FILE_SIZE: u64 = 4 * 1024 * 1024;
    const MIN_GROUPS: usize = 8;
    const MAX_WORKERS: usize = 16;
    const SAFETY_MARGIN: u64 = 128 * 1024 * 1024;

    let largest_candidate = logical_cpus.max(1).min(MAX_WORKERS);
    let groups = MIN_GROUPS.max(largest_candidate.saturating_mul(2));
    FILE_SIZE
        .saturating_mul(groups as u64)
        .saturating_mul(2)
        .saturating_add(SAFETY_MARGIN)
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const TIB: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0;

    let bytes_f = bytes as f64;
    if bytes_f >= TIB {
        format!("{:.2} TiB", bytes_f / TIB)
    } else if bytes_f >= GIB {
        format!("{:.2} GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.2} MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.2} KiB", bytes_f / KIB)
    } else {
        format!("{bytes} B")
    }
}
