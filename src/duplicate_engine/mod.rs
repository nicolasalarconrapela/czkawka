//! Experimental duplicate-engine abstraction.
//!
//! Phase 1 deliberately does not replace the GUI's current `DuplicateFinder` path.
//! It gives us two independently testable engines with one result format, so we can
//! prove correctness before switching anything user-visible.
//!
//! Migration plan:
//! 1. Keep `connect_scan/duplicate.rs` as the production path while tests mature.
//! 2. Reach settings/progress/cancellation parity in this abstraction.
//! 3. Route the GUI scan through `DuplicateEngine`.
//! 4. Remove only the duplicated direct `DuplicateFinder` plumbing from the GUI
//!    layer; keep `CzkawkaEngine` as a reference/fallback for comparison.

#[cfg(feature = "fast_duplicates")]
mod compare;
#[cfg(feature = "fast_duplicates")]
mod czkawka;
#[cfg(feature = "fast_duplicates")]
mod fclones;
mod types;
mod verify;

#[cfg(feature = "fast_duplicates")]
pub(crate) use compare::{DuplicateComparison, compare_results};
#[cfg(feature = "fast_duplicates")]
pub(crate) use czkawka::CzkawkaEngine;
#[cfg(feature = "fast_duplicates")]
pub(crate) use fclones::FclonesEngine;
pub(crate) use types::{DuplicateFile, DuplicateGroup};
#[cfg(feature = "fast_duplicates")]
pub(crate) use types::{DuplicateEngine, DuplicateScanRequest, DuplicateScanResult};
pub(crate) use verify::verify_group;
#[cfg(feature = "fast_duplicates")]
pub(crate) use verify::verify_result;

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(feature = "fast_duplicates")]
    use std::io::Write;
    use std::path::Path;
    #[cfg(feature = "fast_duplicates")]
    use std::path::PathBuf;
    #[cfg(feature = "fast_duplicates")]
    use std::sync::Once;
    #[cfg(feature = "fast_duplicates")]
    use std::time::{Duration, Instant};

    #[cfg(feature = "fast_duplicates")]
    use czkawka_core::common::config_cache_path::set_config_cache_path;
    use tempfile::tempdir;

    use super::*;

    fn write_file(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).expect("no se pudo crear el archivo de prueba");
    }

    fn titulo(texto: &str) {
        println!();
        println!("============================================================");
        println!(" KROKIET - {texto}");
        println!("============================================================");
    }

    fn paso(numero: usize, total: usize, texto: &str) {
        println!();
        println!("[{numero}/{total}] {texto}");
    }

    fn estado(ok: bool) -> &'static str {
        if ok { "[OK]" } else { "[ERROR]" }
    }

    #[cfg(feature = "fast_duplicates")]
    fn mostrar_resultado_motor(result: &DuplicateScanResult) {
        let nombre = match result.engine {
            "czkawka" => "Czkawka",
            "fclones" => "Fast Engine",
            other => other,
        };

        println!(
            "      {nombre:<12} | tiempo: {:>9.3?} | grupos: {:>4} | archivos duplicados: {:>5}",
            result.elapsed,
            result.groups.len(),
            result.file_count()
        );
    }

    #[cfg(feature = "fast_duplicates")]
    fn mostrar_diferencias(comparison: &DuplicateComparison) {
        const MAX_GROUPS: usize = 5;
        const MAX_PATHS: usize = 8;

        if !comparison.only_left.is_empty() {
            println!();
            println!("      Grupos encontrados solo por Czkawka:");
            for (group_idx, group) in comparison.only_left.iter().take(MAX_GROUPS).enumerate() {
                println!("        Grupo {}:", group_idx + 1);
                for path in group.iter().take(MAX_PATHS) {
                    println!("          - {}", path.display());
                }
                if group.len() > MAX_PATHS {
                    println!("          ... y {} archivo(s) mas", group.len() - MAX_PATHS);
                }
            }
            if comparison.only_left.len() > MAX_GROUPS {
                println!("        ... y {} grupo(s) mas", comparison.only_left.len() - MAX_GROUPS);
            }
        }

        if !comparison.only_right.is_empty() {
            println!();
            println!("      Grupos encontrados solo por Fast Engine:");
            for (group_idx, group) in comparison.only_right.iter().take(MAX_GROUPS).enumerate() {
                println!("        Grupo {}:", group_idx + 1);
                for path in group.iter().take(MAX_PATHS) {
                    println!("          - {}", path.display());
                }
                if group.len() > MAX_PATHS {
                    println!("          ... y {} archivo(s) mas", group.len() - MAX_PATHS);
                }
            }
            if comparison.only_right.len() > MAX_GROUPS {
                println!("        ... y {} grupo(s) mas", comparison.only_right.len() - MAX_GROUPS);
            }
        }
    }

    /// `DuplicateFinder` puede acceder a la cache de Czkawka durante las
    /// fases de hash/prehash. El `main()` normal de Krokiet inicializa estas
    /// rutas, pero el ejecutable de tests de Rust no ejecuta `main()`.
    #[cfg(feature = "fast_duplicates")]
    fn init_test_config_cache() {
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            let _ = set_config_cache_path("KrokietTest", "KrokietTest");
        });
    }

    #[test]
    fn exact_verifier_accepts_identical_files() {
        titulo("VERIFICACION EXACTA - ARCHIVOS IDENTICOS");
        println!("Objetivo: comprobar que dos archivos realmente identicos");
        println!("          superan la comparacion byte a byte.");

        let dir = tempdir().expect("no se pudo crear el directorio temporal");
        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.bin");
        let data = vec![0x5A; 128 * 1024];
        write_file(&a, &data);
        write_file(&b, &data);

        paso(1, 2, "Dataset creado: 2 archivos identicos de 128 KiB");
        println!("      {}", estado(true));

        let mut group = DuplicateGroup::new(vec![
            DuplicateFile::from_path(a).expect("no se pudo leer metadata de a.bin"),
            DuplicateFile::from_path(b).expect("no se pudo leer metadata de b.bin"),
        ]);

        let identical = verify_group(&mut group).expect("fallo la verificacion exacta");

        paso(2, 2, "Comparacion byte a byte");
        println!("      Archivos identicos : {}", estado(identical));
        println!("      Grupo verificado    : {}", estado(group.verified));

        assert!(identical);
        assert!(group.verified);

        println!();
        println!("RESULTADO FINAL: CORRECTO");
    }

    #[test]
    fn exact_verifier_rejects_same_edges_but_different_middle() {
        titulo("VERIFICACION EXACTA - FALSO POSITIVO");
        println!("Objetivo: comprobar que dos archivos con el mismo tamano");
        println!("          y bordes parecidos NO pasan si cambia el contenido central.");

        let dir = tempdir().expect("no se pudo crear el directorio temporal");
        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.bin");

        let mut left = vec![b'A'; 64 * 1024];
        let mut right = left.clone();
        left[32 * 1024] = b'B';
        right[32 * 1024] = b'X';
        write_file(&a, &left);
        write_file(&b, &right);

        paso(1, 2, "Dataset trampa creado");
        println!("      Mismo tamano y contenido casi igual, pero el centro difiere.");
        println!("      {}", estado(true));

        let mut group = DuplicateGroup::new(vec![
            DuplicateFile::from_path(a).expect("no se pudo leer metadata de a.bin"),
            DuplicateFile::from_path(b).expect("no se pudo leer metadata de b.bin"),
        ]);

        let identical = verify_group(&mut group).expect("fallo la verificacion exacta");
        let rejected = !identical && !group.verified;

        paso(2, 2, "Comparacion byte a byte");
        println!("      Falso positivo rechazado : {}", estado(rejected));
        println!("      Grupo NO verificado      : {}", estado(!group.verified));

        assert!(!identical);
        assert!(!group.verified);

        println!();
        println!("RESULTADO FINAL: CORRECTO");
    }

    #[cfg(feature = "fast_duplicates")]
    #[test]
    fn czkawka_and_fclones_agree_on_basic_dataset() {
        titulo("PRUEBA DE CORRECCION DEL FAST ENGINE");
        println!("Objetivo:");
        println!("  Comprobar que Czkawka y Fast Engine (fclones) encuentran");
        println!("  exactamente los mismos duplicados y que el resultado del");
        println!("  Fast Engine supera despues una verificacion byte a byte.");
        println!();
        println!("Dataset esperado:");
        println!("  - 2 archivos realmente duplicados de 96 KiB.");
        println!("  - 2 archivos de 80 KiB con igual tamano pero distinto contenido.");
        println!("  - 2 archivos de 128 KiB con bordes iguales pero centro diferente.");
        println!("  - Resultado correcto: 1 grupo con 2 archivos duplicados.");

        init_test_config_cache();

        let dir = tempdir().expect("no se pudo crear el directorio temporal");

        let duplicate = vec![0x42; 96 * 1024];
        write_file(&dir.path().join("same-a.bin"), &duplicate);
        write_file(&dir.path().join("same-b.bin"), &duplicate);

        write_file(&dir.path().join("different-a.bin"), &vec![0x11; 80 * 1024]);
        write_file(&dir.path().join("different-b.bin"), &vec![0x22; 80 * 1024]);

        let mut middle_a = vec![0x33; 128 * 1024];
        let mut middle_b = middle_a.clone();
        middle_a[64 * 1024] = 0x44;
        middle_b[64 * 1024] = 0x55;
        write_file(&dir.path().join("middle-a.bin"), &middle_a);
        write_file(&dir.path().join("middle-b.bin"), &middle_b);

        paso(1, 5, "Dataset de prueba preparado");
        println!("      Directorio temporal: {}", dir.path().display());
        println!("      Archivos creados    : 6");
        println!("      {}", estado(true));

        let request = DuplicateScanRequest::for_paths([dir.path().to_path_buf()]);

        paso(2, 5, "Ejecutando motor de referencia: Czkawka");
        println!("      Nota: el diagnostico interno de czkawka_core puede aparecer");
        println!("      en ingles entre estas lineas; el resumen final esta en espanol.");
        let old = CzkawkaEngine.scan(&request).expect("fallo el escaneo de Czkawka");
        mostrar_resultado_motor(&old);
        let old_expected = old.groups.len() == 1 && old.file_count() == 2;
        println!("      Resultado esperado  : {}", estado(old_expected));

        paso(3, 5, "Ejecutando Fast Engine: fclones");
        let mut fast = FclonesEngine.scan(&request).expect("fallo el escaneo de fclones");
        mostrar_resultado_motor(&fast);
        let fast_expected = fast.groups.len() == 1 && fast.file_count() == 2;
        println!("      Resultado esperado  : {}", estado(fast_expected));

        paso(4, 5, "Comparando los grupos encontrados por ambos motores");
        let comparison = compare_results(&old, &fast);
        let same_results = comparison.identical();
        println!("      Mismos grupos       : {}", estado(same_results));
        println!("      Solo Czkawka        : {} grupo(s)", comparison.only_left.len());
        println!("      Solo Fast Engine    : {} grupo(s)", comparison.only_right.len());

        if !same_results {
            mostrar_diferencias(&comparison);
        }

        paso(5, 5, "Verificacion exacta byte a byte del resultado Fast Engine");
        let verify_started = Instant::now();
        let verified_groups = verify_result(&mut fast).expect("fallo la verificacion exacta");
        let verify_elapsed = verify_started.elapsed();
        let exact_ok = verified_groups == fast.groups.len();

        println!("      Grupos recibidos    : {}", fast.groups.len());
        println!("      Grupos verificados  : {}", verified_groups);
        println!("      Tiempo verificacion : {:.3?}", verify_elapsed);
        println!("      Seguridad exacta    : {}", estado(exact_ok));

        let everything_ok = old_expected && fast_expected && same_results && exact_ok;

        println!();
        println!("------------------------------------------------------------");
        println!(" RESUMEN");
        println!("------------------------------------------------------------");
        mostrar_resultado_motor(&old);
        mostrar_resultado_motor(&fast);
        println!("      Paridad de resultados : {}", estado(same_results));
        println!("      Verificacion exacta   : {}", estado(exact_ok));
        println!();
        println!("RESULTADO FINAL: {}", if everything_ok { "CORRECTO" } else { "ERROR" });
        println!("============================================================");

        assert!(same_results, "los motores devolvieron grupos diferentes: {comparison:#?}");
        assert_eq!(old.groups.len(), 1, "numero inesperado de grupos de Czkawka: {old:#?}");
        assert_eq!(old.file_count(), 2, "numero inesperado de archivos duplicados de Czkawka: {old:#?}");
        assert_eq!(fast.groups.len(), 1, "numero inesperado de grupos de Fast Engine: {fast:#?}");
        assert_eq!(fast.file_count(), 2, "numero inesperado de archivos duplicados de Fast Engine: {fast:#?}");
        assert_eq!(
            verified_groups,
            fast.groups.len(),
            "al menos un grupo de Fast Engine no supero la verificacion byte a byte"
        );
    }

    // ============================================================
    // FASE 1.3 - BENCHMARK REPRODUCIBLE
    // ============================================================

    #[cfg(feature = "fast_duplicates")]
    #[derive(Clone, Debug)]
    struct GeneratedDatasetInfo {
        root: PathBuf,
        total_files: usize,
        expected_groups: usize,
        expected_duplicate_files: usize,
        total_bytes: u64,
    }

    #[cfg(feature = "fast_duplicates")]
    #[derive(Clone, Debug)]
    struct BenchmarkSample {
        czkawka: Duration,
        fast: Duration,
        verify: Duration,
        fast_safe: Duration,
        groups: usize,
        duplicate_files: usize,
    }

    #[cfg(feature = "fast_duplicates")]
    fn env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
        match std::env::var(name) {
            Ok(raw) => {
                let parsed = raw.parse::<usize>().unwrap_or_else(|_| panic!("{name} debe ser un numero entero; recibido: {raw}"));
                assert!((min..=max).contains(&parsed), "{name} debe estar entre {min} y {max}; recibido: {parsed}");
                parsed
            }
            Err(_) => default,
        }
    }

    #[cfg(feature = "fast_duplicates")]
    fn human_bytes(bytes: u64) -> String {
        const KIB: f64 = 1024.0;
        const MIB: f64 = 1024.0 * KIB;
        const GIB: f64 = 1024.0 * MIB;
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

    #[cfg(feature = "fast_duplicates")]
    fn deterministic_bytes(size: usize, seed: u64) -> Vec<u8> {
        let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
        let mut data = vec![0_u8; size];
        for byte in &mut data {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = (state >> 24) as u8;
        }
        data
    }

    #[cfg(feature = "fast_duplicates")]
    fn write_generated_file(path: &Path, size: usize, seed: u64) -> u64 {
        let data = deterministic_bytes(size, seed);
        let mut file = fs::File::create(path).unwrap_or_else(|error| panic!("no se pudo crear {}: {error}", path.display()));
        file.write_all(&data).unwrap_or_else(|error| panic!("no se pudo escribir {}: {error}", path.display()));
        size as u64
    }

    #[cfg(feature = "fast_duplicates")]
    fn write_middle_trap_pair(left: &Path, right: &Path, size: usize, seed: u64) -> u64 {
        let mut left_data = deterministic_bytes(size, seed);
        let mut right_data = left_data.clone();
        let middle = size / 2;
        left_data[middle] = left_data[middle].wrapping_add(1);
        right_data[middle] = right_data[middle].wrapping_add(2);
        write_file(left, &left_data);
        write_file(right, &right_data);
        (size as u64) * 2
    }

    #[cfg(feature = "fast_duplicates")]
    fn create_generated_benchmark_dataset(root: &Path, scale: usize) -> GeneratedDatasetInfo {
        let duplicate_groups = 40 * scale;
        let unique_files = 120 * scale;
        let trap_pairs = 24 * scale;

        let duplicates_dir = root.join("duplicados_reales");
        let unique_dir = root.join("unicos");
        let traps_dir = root.join("falsos_candidatos");
        fs::create_dir_all(&duplicates_dir).expect("no se pudo crear duplicados_reales");
        fs::create_dir_all(&unique_dir).expect("no se pudo crear unicos");
        fs::create_dir_all(&traps_dir).expect("no se pudo crear falsos_candidatos");

        let mut total_files = 0usize;
        let mut duplicate_files = 0usize;
        let mut total_bytes = 0u64;

        // 40 grupos por escala. Dos de cada cinco grupos tienen 3 copias;
        // los demas tienen 2. En escala 1: 40 grupos y 96 archivos duplicados.
        for group in 0..duplicate_groups {
            let copies = if group % 5 < 2 { 3 } else { 2 };
            let size = 64 * 1024 + (group % 24) * 8 * 1024;
            let seed = 0xD001_0000_u64 + group as u64;
            let data = deterministic_bytes(size, seed);

            for copy in 0..copies {
                let path = duplicates_dir.join(format!("grupo_{group:05}_copia_{copy}.bin"));
                write_file(&path, &data);
                total_files += 1;
                duplicate_files += 1;
                total_bytes += size as u64;
            }
        }

        // Archivos unicos: obligan a enumerar y filtrar sin aumentar el numero
        // real de grupos duplicados.
        for index in 0..unique_files {
            let size = 32 * 1024 + (index % 64) * 4 * 1024;
            let path = unique_dir.join(format!("unico_{index:06}.bin"));
            total_bytes += write_generated_file(&path, size, 0xA110_0000_u64 + index as u64);
            total_files += 1;
        }

        // Pares con el mismo tamano y los mismos bordes, pero un byte distinto
        // en el centro. Son candidatos deliberadamente falsos.
        for pair in 0..trap_pairs {
            let size = 256 * 1024 + (pair % 8) * 4 * 1024;
            let left = traps_dir.join(format!("trampa_{pair:05}_a.bin"));
            let right = traps_dir.join(format!("trampa_{pair:05}_b.bin"));
            total_bytes += write_middle_trap_pair(&left, &right, size, 0xFA15_0000_u64 + pair as u64);
            total_files += 2;
        }

        GeneratedDatasetInfo {
            root: root.to_path_buf(),
            total_files,
            expected_groups: duplicate_groups,
            expected_duplicate_files: duplicate_files,
            total_bytes,
        }
    }

    #[cfg(feature = "fast_duplicates")]
    fn scan_fast_verified(request: &DuplicateScanRequest) -> (DuplicateScanResult, Duration, usize) {
        let mut fast = FclonesEngine.scan(request).expect("fallo el escaneo de Fast Engine");
        let verify_started = Instant::now();
        let verified_groups = verify_result(&mut fast).expect("fallo la verificacion exacta del Fast Engine");
        let verify_elapsed = verify_started.elapsed();
        (fast, verify_elapsed, verified_groups)
    }

    #[cfg(feature = "fast_duplicates")]
    fn validate_round(
        old: &DuplicateScanResult,
        fast: &DuplicateScanResult,
        verified_groups: usize,
        expected: Option<(usize, usize)>,
    ) {
        let comparison = compare_results(old, fast);
        if !comparison.identical() {
            mostrar_diferencias(&comparison);
        }

        assert!(comparison.identical(), "los motores devolvieron grupos diferentes: {comparison:#?}");
        assert_eq!(
            verified_groups,
            fast.groups.len(),
            "al menos un grupo de Fast Engine no supero la verificacion byte a byte"
        );

        if let Some((expected_groups, expected_files)) = expected {
            assert_eq!(old.groups.len(), expected_groups, "Czkawka devolvio un numero inesperado de grupos");
            assert_eq!(fast.groups.len(), expected_groups, "Fast Engine devolvio un numero inesperado de grupos");
            assert_eq!(old.file_count(), expected_files, "Czkawka devolvio un numero inesperado de archivos duplicados");
            assert_eq!(fast.file_count(), expected_files, "Fast Engine devolvio un numero inesperado de archivos duplicados");
        }
    }

    #[cfg(feature = "fast_duplicates")]
    fn warm_up(request: &DuplicateScanRequest, expected: Option<(usize, usize)>) {
        println!();
        println!("CALENTAMIENTO (NO CONTABILIZADO)");
        println!("  Se ejecuta una pasada completa para estabilizar carga de codigo");
        println!("  y cache del sistema operativo antes de medir.");

        let old = CzkawkaEngine.scan(request).expect("fallo Czkawka durante el calentamiento");
        let (fast, _verify_elapsed, verified_groups) = scan_fast_verified(request);
        validate_round(&old, &fast, verified_groups, expected);

        println!("  Paridad y verificacion : [OK]");
    }

    #[cfg(feature = "fast_duplicates")]
    fn run_benchmark_round(
        round: usize,
        runs: usize,
        request: &DuplicateScanRequest,
        expected: Option<(usize, usize)>,
    ) -> BenchmarkSample {
        let czkawka_first = round % 2 == 0;
        let order = if czkawka_first {
            "Czkawka -> Fast Engine + verificacion"
        } else {
            "Fast Engine + verificacion -> Czkawka"
        };

        println!();
        println!("RONDA {}/{}", round + 1, runs);
        println!("  Orden                   : {order}");

        let (old, fast, verify_elapsed, verified_groups) = if czkawka_first {
            let old = CzkawkaEngine.scan(request).expect("fallo el escaneo de Czkawka");
            let (fast, verify_elapsed, verified_groups) = scan_fast_verified(request);
            (old, fast, verify_elapsed, verified_groups)
        } else {
            let (fast, verify_elapsed, verified_groups) = scan_fast_verified(request);
            let old = CzkawkaEngine.scan(request).expect("fallo el escaneo de Czkawka");
            (old, fast, verify_elapsed, verified_groups)
        };

        let comparison = compare_results(&old, &fast);
        let same_results = comparison.identical();
        let exact_ok = verified_groups == fast.groups.len();

        if !same_results {
            mostrar_diferencias(&comparison);
        }

        if let Some((expected_groups, expected_files)) = expected {
            println!("  Grupos esperados        : {expected_groups}");
            println!("  Duplicados esperados    : {expected_files}");
        }
        println!("  Czkawka                 : {:.3?}", old.elapsed);
        println!("  Fast Engine             : {:.3?}", fast.elapsed);
        println!("  Verificacion exacta     : {:.3?}", verify_elapsed);
        println!("  Fast seguro             : {:.3?}", fast.elapsed + verify_elapsed);
        println!("  Grupos encontrados      : {}", fast.groups.len());
        println!("  Archivos duplicados     : {}", fast.file_count());
        println!("  Mismos resultados       : {}", estado(same_results));
        println!("  Seguridad exacta        : {}", estado(exact_ok));

        validate_round(&old, &fast, verified_groups, expected);

        BenchmarkSample {
            czkawka: old.elapsed,
            fast: fast.elapsed,
            verify: verify_elapsed,
            fast_safe: fast.elapsed + verify_elapsed,
            groups: fast.groups.len(),
            duplicate_files: fast.file_count(),
        }
    }

    #[cfg(feature = "fast_duplicates")]
    fn median_duration(samples: impl IntoIterator<Item = Duration>) -> Duration {
        let mut values = samples.into_iter().collect::<Vec<_>>();
        assert!(!values.is_empty(), "no hay muestras para calcular la mediana");
        values.sort_unstable();
        let middle = values.len() / 2;
        if values.len() % 2 == 1 {
            values[middle]
        } else {
            Duration::from_secs_f64((values[middle - 1].as_secs_f64() + values[middle].as_secs_f64()) / 2.0)
        }
    }

    #[cfg(feature = "fast_duplicates")]
    fn print_benchmark_summary(samples: &[BenchmarkSample]) {
        let czkawka = median_duration(samples.iter().map(|sample| sample.czkawka));
        let fast = median_duration(samples.iter().map(|sample| sample.fast));
        let verify = median_duration(samples.iter().map(|sample| sample.verify));
        let fast_safe = median_duration(samples.iter().map(|sample| sample.fast_safe));
        let groups = samples.first().map_or(0, |sample| sample.groups);
        let duplicate_files = samples.first().map_or(0, |sample| sample.duplicate_files);

        println!();
        println!("------------------------------------------------------------");
        println!(" RESULTADO FINAL - MEDIANA DE {} RONDA(S)", samples.len());
        println!("------------------------------------------------------------");
        println!(" Czkawka                  : {:.3?}", czkawka);
        println!(" Fast Engine              : {:.3?}", fast);
        println!(" Verificacion exacta      : {:.3?}", verify);
        println!(" Fast seguro              : {:.3?}", fast_safe);
        println!(" Grupos duplicados        : {groups}");
        println!(" Archivos duplicados      : {duplicate_files}");
        println!(" Paridad en todas rondas  : [OK]");
        println!(" Seguridad en todas       : [OK]");

        if fast_safe.as_secs_f64() > 0.0 {
            let ratio = czkawka.as_secs_f64() / fast_safe.as_secs_f64();
            if ratio >= 1.0 {
                println!(" Fast seguro / Czkawka    : {:.2}x mas rapido", ratio);
            } else {
                println!(" Fast seguro / Czkawka    : {:.2}x el tiempo de Czkawka", 1.0 / ratio);
            }
        }
        println!("============================================================");
    }

    /// Benchmark reproducible con un dataset generado automaticamente.
    ///
    /// Variables:
    /// - KROKIET_DUP_BENCH_SCALE: 1..16 (por defecto 1)
    /// - KROKIET_DUP_BENCH_RUNS:  1..9  (por defecto 3)
    #[cfg(feature = "fast_duplicates")]
    #[test]
    #[ignore = "benchmark manual reproducible"]
    fn visual_duplicate_engine_benchmark() {
        init_test_config_cache();

        let scale = env_usize("KROKIET_DUP_BENCH_SCALE", 1, 1, 16);
        let runs = env_usize("KROKIET_DUP_BENCH_RUNS", 3, 1, 9);
        let dir = tempdir().expect("no se pudo crear el directorio temporal del benchmark");

        titulo("BENCHMARK REPRODUCIBLE DEL FAST ENGINE");
        println!("Preparando dataset generado. Este tiempo NO se contabiliza.");
        let dataset_started = Instant::now();
        let dataset = create_generated_benchmark_dataset(dir.path(), scale);
        let dataset_elapsed = dataset_started.elapsed();

        println!();
        println!("Dataset");
        println!("  Escala                  : {scale}");
        println!("  Archivos totales        : {}", dataset.total_files);
        println!("  Grupos duplicados       : {}", dataset.expected_groups);
        println!("  Archivos duplicados     : {}", dataset.expected_duplicate_files);
        println!("  Datos escritos          : {}", human_bytes(dataset.total_bytes));
        println!("  Tiempo de preparacion   : {:.3?} (fuera de benchmark)", dataset_elapsed);
        println!("  Rondas medidas          : {runs}");
        println!("  Cache interna motores   : desactivada");
        println!("  Verificacion Fast       : byte a byte en cada ronda");

        let request = DuplicateScanRequest::for_paths([dataset.root.clone()]);
        let expected = Some((dataset.expected_groups, dataset.expected_duplicate_files));

        warm_up(&request, expected);

        let mut samples = Vec::with_capacity(runs);
        for round in 0..runs {
            samples.push(run_benchmark_round(round, runs, &request, expected));
        }

        print_benchmark_summary(&samples);
    }

    /// Benchmark multirronda sobre un dataset real. No modifica ni elimina
    /// ningun archivo de la carpeta indicada.
    #[cfg(feature = "fast_duplicates")]
    #[test]
    #[ignore = "benchmark manual; definir KROKIET_DUP_BENCH_PATH"]
    fn duplicate_engine_real_dataset_benchmark() {
        init_test_config_cache();

        let path = std::env::var_os("KROKIET_DUP_BENCH_PATH")
            .expect("debes definir KROKIET_DUP_BENCH_PATH antes de ejecutar el benchmark");
        let path = PathBuf::from(path);
        let runs = env_usize("KROKIET_DUP_BENCH_RUNS", 3, 1, 9);

        assert!(path.is_dir(), "la ruta del benchmark no es una carpeta: {}", path.display());

        titulo("BENCHMARK REAL DE MOTORES DE DUPLICADOS");
        println!("Carpeta analizada         : {}", path.display());
        println!("Modo                      : SOLO LECTURA");
        println!("Rondas medidas            : {runs}");
        println!("Orden                     : alternado entre rondas");
        println!("Fast seguro               : fclones + verificacion byte a byte");
        println!("Cache interna motores     : desactivada");

        let request = DuplicateScanRequest::for_paths([path]);

        warm_up(&request, None);

        let mut samples = Vec::with_capacity(runs);
        for round in 0..runs {
            samples.push(run_benchmark_round(round, runs, &request, None));
        }

        print_benchmark_summary(&samples);
    }
}
