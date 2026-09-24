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

mod compare;
mod czkawka;
#[cfg(feature = "fast_duplicates")]
mod fclones;
mod types;
mod verify;

pub(crate) use compare::{DuplicateComparison, compare_results};
pub(crate) use czkawka::CzkawkaEngine;
#[cfg(feature = "fast_duplicates")]
pub(crate) use fclones::FclonesEngine;
pub(crate) use types::{DuplicateEngine, DuplicateFile, DuplicateGroup, DuplicateScanRequest, DuplicateScanResult};
pub(crate) use verify::{verify_group, verify_result};

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Once;

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

    fn mostrar_resultado_motor(result: &DuplicateScanResult) {
        let nombre = match result.engine {
            "czkawka" => "Czkawka",
            "fclones" => "Fast Engine",
            other => other,
        };

        println!(
            "      {nombre:<12} | tiempo: {:>9.3?} | grupos: {:>3} | archivos duplicados: {:>3}",
            result.elapsed,
            result.groups.len(),
            result.file_count()
        );
    }

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
                println!(
                    "        ... y {} grupo(s) mas",
                    comparison.only_left.len() - MAX_GROUPS
                );
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
                println!(
                    "        ... y {} grupo(s) mas",
                    comparison.only_right.len() - MAX_GROUPS
                );
            }
        }
    }

    /// `DuplicateFinder` puede acceder a la cache de Czkawka durante las
    /// fases de hash/prehash. El `main()` normal de Krokiet inicializa estas
    /// rutas, pero el ejecutable de tests de Rust no ejecuta `main()`.
    ///
    /// `czkawka_core` guarda estas rutas en un OnceCell global, asi que la
    /// inicializacion debe realizarse una sola vez por proceso.
    fn init_test_config_cache() {
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            // Usamos la API publica porque las funciones #[cfg(test)] de
            // czkawka_core no estan disponibles al compilarlo como dependencia.
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
        let verify_started = std::time::Instant::now();
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
        println!(
            "RESULTADO FINAL: {}",
            if everything_ok { "CORRECTO" } else { "ERROR" }
        );
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

    /// Benchmark manual sobre un dataset real.
    ///
    /// Ejemplo PowerShell:
    /// $env:KROKIET_DUP_BENCH_PATH = 'D:\\KrokietBenchmark\\mixed'
    /// cargo test -p krokiet --release --features fast_duplicates -j 1 \
    ///     duplicate_engine_real_dataset_benchmark -- --ignored --nocapture --test-threads=1
    #[cfg(feature = "fast_duplicates")]
    #[test]
    #[ignore = "benchmark manual; definir KROKIET_DUP_BENCH_PATH"]
    fn duplicate_engine_real_dataset_benchmark() {
        init_test_config_cache();

        titulo("BENCHMARK REAL DE MOTORES DE DUPLICADOS");

        let path = std::env::var_os("KROKIET_DUP_BENCH_PATH")
            .expect("debes definir KROKIET_DUP_BENCH_PATH antes de ejecutar el benchmark");
        let path = std::path::PathBuf::from(path);

        println!("Carpeta analizada : {}", path.display());
        println!("Modo               : solo lectura");
        println!("Comparacion        : Czkawka vs Fast Engine + verificacion exacta");

        let request = DuplicateScanRequest::for_paths([path]);

        paso(1, 4, "Ejecutando Czkawka");
        let old = CzkawkaEngine.scan(&request).expect("fallo el escaneo de Czkawka");
        mostrar_resultado_motor(&old);

        paso(2, 4, "Ejecutando Fast Engine (fclones)");
        let mut fast = FclonesEngine.scan(&request).expect("fallo el escaneo de fclones");
        mostrar_resultado_motor(&fast);

        paso(3, 4, "Comparando resultados");
        let comparison = compare_results(&old, &fast);
        let same_results = comparison.identical();
        println!("      Mismos grupos       : {}", estado(same_results));
        println!("      Solo Czkawka        : {} grupo(s)", comparison.only_left.len());
        println!("      Solo Fast Engine    : {} grupo(s)", comparison.only_right.len());

        if !same_results {
            mostrar_diferencias(&comparison);
        }

        paso(4, 4, "Verificacion exacta byte a byte");
        let verify_started = std::time::Instant::now();
        let verified_groups = verify_result(&mut fast).expect("fallo la verificacion exacta");
        let verify_elapsed = verify_started.elapsed();
        let exact_ok = verified_groups == fast.groups.len();
        let safe_total = fast.elapsed + verify_elapsed;

        println!("      Grupos verificados  : {verified_groups}/{}", fast.groups.len());
        println!("      Tiempo verificacion : {:.3?}", verify_elapsed);
        println!("      Seguridad exacta    : {}", estado(exact_ok));

        println!();
        println!("------------------------------------------------------------");
        println!(" RESUMEN DEL BENCHMARK");
        println!("------------------------------------------------------------");
        println!(" Czkawka");
        println!("   Tiempo de escaneo      : {:.3?}", old.elapsed);
        println!("   Grupos                  : {}", old.groups.len());
        println!("   Archivos duplicados     : {}", old.file_count());
        println!();
        println!(" Fast Engine");
        println!("   Tiempo de escaneo      : {:.3?}", fast.elapsed);
        println!("   Verificacion exacta    : {:.3?}", verify_elapsed);
        println!("   Tiempo seguro total    : {:.3?}", safe_total);
        println!("   Grupos                 : {}", fast.groups.len());
        println!("   Archivos duplicados    : {}", fast.file_count());
        println!();
        println!(" Correccion");
        println!("   Mismos resultados      : {}", estado(same_results));
        println!("   Verificacion byte-byte : {}", estado(exact_ok));

        if safe_total.as_secs_f64() > 0.0 {
            let ratio = old.elapsed.as_secs_f64() / safe_total.as_secs_f64();
            println!();
            println!(" Rendimiento");
            if ratio >= 1.0 {
                println!("   Fast Engine seguro     : {:.2}x mas rapido", ratio);
            } else {
                println!("   Fast Engine seguro     : {:.2}x el tiempo de Czkawka", 1.0 / ratio);
            }
        }

        println!("============================================================");

        assert!(
            same_results,
            "los motores devolvieron grupos diferentes: {comparison:#?}"
        );
        assert_eq!(
            verified_groups,
            fast.groups.len(),
            "al menos un grupo de Fast Engine no supero la verificacion byte a byte"
        );
    }
}
