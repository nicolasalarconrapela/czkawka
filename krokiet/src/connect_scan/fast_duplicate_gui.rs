use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use czkawka_core::common::model::{CheckingMethod, HashType};
use czkawka_core::tools::duplicate::DuplicateEntry;
use humansize::{BINARY, format_size};

use crate::duplicate_engine::{
    CalibrationMode, CalibrationOptions, CalibrationStability, DuplicateScanExecution,
    DuplicateScanRequest, DuplicateScanResult, DuplicateScanServiceOptions, RequestProfileStatus,
    TuningSource, calibrate_and_store_mode, inspect_request_profiles,
};
use crate::settings::model::SettingsCustom;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DuplicateEnginePreference {
    Auto,
    Czkawka,
    Fast,
}

impl DuplicateEnginePreference {
    pub(crate) fn from_config(value: &str) -> Self {
        match value {
            "czkawka" => Self::Czkawka,
            "fast" => Self::Fast,
            _ => Self::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FastGuiUnsupportedSetting {
    NonHashCheckingMethod,
    NonBlake3Hash,
    CustomLegacyHashTuning,
    ReferencePaths,
    AllowedExtensions,
    ExcludedExtensions,
}

impl FastGuiUnsupportedSetting {
    pub(crate) fn label_es(self) -> &'static str {
        match self {
            Self::NonHashCheckingMethod => "Fast Engine requiere el metodo Hash",
            Self::NonBlake3Hash => "Fast Engine requiere Blake3",
            Self::CustomLegacyHashTuning => "Fast Engine no admite los ajustes hash avanzados de Czkawka",
            Self::ReferencePaths => "Fast Engine no admite carpetas de referencia",
            Self::AllowedExtensions => "Fast Engine no admite todavia el filtro de extensiones permitidas",
            Self::ExcludedExtensions => "Fast Engine no admite todavia el filtro de extensiones excluidas",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum FastGuiScanPlan {
    Fast(DuplicateScanRequest),
    Legacy(FastGuiUnsupportedSetting),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FastGuiConversionError {
    UnverifiedGroup(usize),
}

/// Result state retained after a Fast Engine scan.
///
/// Krokiet's existing `SharedModels` keeps the original `DuplicateFinder` so the
/// Save action can serialize the last scan. A Fast Engine scan has no
/// `DuplicateFinder`, therefore this compact state stores only the already
/// verified Hash groups plus the original search paths needed by the text report.
#[derive(Clone, Debug)]
pub(crate) struct FastDuplicateState {
    included_paths: Vec<PathBuf>,
    excluded_paths: Vec<PathBuf>,
    excluded_items: Vec<String>,
    groups_by_size: BTreeMap<u64, Vec<Vec<DuplicateEntry>>>,
}

impl FastDuplicateState {
    pub(crate) fn from_gui_groups(
        groups: &[(Option<DuplicateEntry>, Vec<DuplicateEntry>)],
        request: &DuplicateScanRequest,
    ) -> Self {
        let mut groups_by_size = BTreeMap::<u64, Vec<Vec<DuplicateEntry>>>::new();

        for (reference, entries) in groups {
            let mut group = Vec::with_capacity(entries.len() + usize::from(reference.is_some()));
            if let Some(reference) = reference {
                group.push(reference.clone());
            }
            group.extend(entries.iter().cloned());

            if group.len() < 2 {
                continue;
            }

            let size = group[0].size;
            groups_by_size.entry(size).or_default().push(group);
        }

        Self {
            included_paths: request.paths.clone(),
            excluded_paths: request.excluded_paths.clone(),
            excluded_items: request.excluded_items.clone(),
            groups_by_size,
        }
    }

    pub(crate) fn save_all_in_one(&self, folder: &str) -> io::Result<()> {
        let folder = Path::new(folder);
        fs::create_dir_all(folder)?;

        let pretty = folder.join("results_duplicates_pretty.json");
        let compact = folder.join("results_duplicates_compact.json");
        let text = folder.join("results_duplicates.txt");

        self.save_json(&pretty, true)?;
        self.save_json(&compact, false)?;
        self.save_text(&text)?;
        Ok(())
    }

    fn save_json(&self, path: &Path, pretty: bool) -> io::Result<()> {
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        if pretty {
            serde_json::to_writer_pretty(&mut writer, &self.groups_by_size)?;
        } else {
            serde_json::to_writer(&mut writer, &self.groups_by_size)?;
        }
        writer.flush()
    }

    fn save_text(&self, path: &Path) -> io::Result<()> {
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);

        writeln!(
            writer,
            "Results of searching {:?} with excluded paths {:?} and excluded items {:?}",
            self.included_paths, self.excluded_paths, self.excluded_items
        )?;
        writeln!(
            writer,
            "(Fast Engine: fclones candidates confirmed by ExactVerifier)"
        )?;

        let (duplicate_files, groups, lost_space) = self.stats();
        if groups == 0 {
            writeln!(writer, "Not found any duplicates.")?;
            writer.flush()?;
            return Ok(());
        }

        writeln!(
            writer,
            "-------------------------------------------------Files with same hashes-------------------------------------------------"
        )?;
        writeln!(
            writer,
            "Found {duplicate_files} duplicated files which in {groups} groups which takes {}.",
            format_size(lost_space, BINARY)
        )?;

        for (size, groups) in self.groups_by_size.iter().rev() {
            for group in groups {
                writeln!(
                    writer,
                    "\n---- Size {} ({size}) - {} files",
                    format_size(*size, BINARY),
                    group.len()
                )?;
                for entry in group {
                    writeln!(writer, "\"{}\"", entry.path.to_string_lossy())?;
                }
            }
        }

        writer.flush()
    }

    fn stats(&self) -> (usize, usize, u64) {
        let mut duplicate_files = 0_usize;
        let mut groups = 0_usize;
        let mut lost_space = 0_u64;

        for (size, size_groups) in &self.groups_by_size {
            for group in size_groups {
                if group.len() < 2 {
                    continue;
                }
                groups += 1;
                duplicate_files += group.len() - 1;
                lost_space = lost_space.saturating_add(size.saturating_mul((group.len() - 1) as u64));
            }
        }

        (duplicate_files, groups, lost_space)
    }
}

pub(crate) fn plan_fast_duplicate_scan(
    checking_method: CheckingMethod,
    hash_type: HashType,
    settings: &SettingsCustom,
) -> FastGuiScanPlan {
    if checking_method != CheckingMethod::Hash {
        return FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::NonHashCheckingMethod);
    }

    if hash_type != HashType::Blake3 {
        return FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::NonBlake3Hash);
    }

    let defaults = SettingsCustom::default();
    if settings.duplicate_use_prehash != defaults.duplicate_use_prehash
        || settings.duplicate_minimal_hash_cache_size != defaults.duplicate_minimal_hash_cache_size
        || settings.duplicate_minimal_prehash_cache_size != defaults.duplicate_minimal_prehash_cache_size
    {
        return FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::CustomLegacyHashTuning);
    }

    if !settings.included_paths_referenced.is_empty() {
        return FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::ReferencePaths);
    }

    if !settings.allowed_extensions.trim().is_empty() {
        return FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::AllowedExtensions);
    }

    if !settings.excluded_extensions.trim().is_empty() {
        return FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::ExcludedExtensions);
    }

    let mut request = DuplicateScanRequest::for_paths(settings.included_paths.clone());
    request.excluded_paths = settings.excluded_paths.clone();
    request.excluded_items = split_csv(&settings.excluded_items);
    request.recursive = settings.recursive_search;
    request.min_size = kib_to_bytes(settings.minimum_file_size);
    request.max_size = Some(kib_to_bytes(settings.maximum_file_size));
    request.use_cache = settings.use_cache;
    request.one_file_system = settings.ignore_other_file_systems;
    request.hide_hard_links = settings.hide_hard_links;

    FastGuiScanPlan::Fast(request)
}

/// Ensures that every storage referenced by this request gets one autonomous
/// calibration attempt before the normal scan.
///
/// Calibration is deliberately outside `run_duplicate_scan`, so it is not part
/// of detector/verifier timing. If calibration fails or remains statistically
/// invalid, the scan service will later resolve that storage to the universal
/// one-worker safe fallback.
pub(crate) fn prepare_fast_profiles(
    request: &DuplicateScanRequest,
    tuning_store_path: &Path,
) -> Vec<String> {
    let options = DuplicateScanServiceOptions::fast(tuning_store_path.to_path_buf());
    let status = match inspect_request_profiles(request, &options) {
        Ok(status) => status,
        Err(error) => {
            return vec![format!(
                "Fast Engine: no se pudo inspeccionar el perfil ({error}); el scan usara fallback seguro si es necesario."
            )];
        }
    };

    let RequestProfileStatus::Missing(missing) = status else {
        return Vec::new();
    };

    let mut notes = Vec::with_capacity(missing.len());

    for item in missing {
        let calibration_path = calibration_directory_for(&item.representative_path);
        match calibrate_and_store_mode(
            &calibration_path,
            tuning_store_path,
            CalibrationOptions::default(),
            CalibrationMode::Auto,
        ) {
            Ok(run) if run.report.profile.stability != CalibrationStability::Invalid => {
                notes.push(format!(
                    "Fast Engine: almacenamiento {} calibrado de forma autonoma: {} worker(s), estabilidad {}.",
                    item.storage_key,
                    run.report.profile.workers,
                    run.report.profile.stability.label_es()
                ));
            }
            Ok(_) => {
                notes.push(format!(
                    "Fast Engine: la calibracion de {} fue inestable; el scan continuara con fallback seguro.",
                    item.storage_key
                ));
            }
            Err(error) => {
                notes.push(format!(
                    "Fast Engine: no se pudo calibrar {} ({error}); el scan continuara con fallback seguro.",
                    item.storage_key
                ));
            }
        }
    }

    notes
}

pub(crate) fn fast_execution_messages(
    execution: &DuplicateScanExecution,
    calibration_notes: &[String],
) -> String {
    let mut lines = calibration_notes.to_vec();

    lines.push(format!(
        "Fast Engine: fclones + ExactVerifier; detector {:.3?}, verificacion {:.3?}.",
        execution.result.elapsed, execution.verification_elapsed
    ));

    if let Some(workers) = execution.verifier_workers {
        lines.push(format!("ExactVerifier: {workers} worker(s)."));
    }

    for trace in &execution.storage_tuning {
        match trace.source {
            TuningSource::ReusedProfile => lines.push(format!(
                "Perfil {} reutilizado: {} worker(s).",
                trace.storage_key, trace.workers
            )),
            TuningSource::Fallback => lines.push(format!(
                "Perfil {}: fallback seguro de {} worker(s) ({:?}).",
                trace.storage_key, trace.workers, trace.fallback_reason
            )),
        }
    }

    lines.join("\n")
}

pub(crate) fn duplicate_result_into_gui_groups(
    result: DuplicateScanResult,
) -> Result<Vec<(Option<DuplicateEntry>, Vec<DuplicateEntry>)>, FastGuiConversionError> {
    let mut gui_groups = Vec::with_capacity(result.groups.len());

    for (group_index, group) in result.groups.into_iter().enumerate() {
        if !group.verified {
            return Err(FastGuiConversionError::UnverifiedGroup(group_index));
        }

        let entries = group
            .files
            .into_iter()
            .map(|file| DuplicateEntry {
                path: file.path,
                modified_date: file.modified_date,
                size: file.size,
                // fclones' digest is only a candidate-grouping detail. The final
                // correctness guarantee is ExactVerifier, so do not pretend this
                // field contains Czkawka's configured hash.
                hash: String::new(),
            })
            .collect();

        gui_groups.push((None, entries));
    }

    Ok(gui_groups)
}

pub(crate) fn gui_group_stats(
    groups: &[(Option<DuplicateEntry>, Vec<DuplicateEntry>)],
) -> (usize, usize, u64) {
    let mut duplicate_files = 0_usize;
    let mut lost_space = 0_u64;

    for (reference, entries) in groups {
        let group_len = entries.len() + usize::from(reference.is_some());
        if group_len < 2 {
            continue;
        }

        duplicate_files += group_len - 1;
        let size = reference
            .as_ref()
            .map_or_else(|| entries.first().map_or(0, |entry| entry.size), |entry| entry.size);
        lost_space = lost_space.saturating_add(size.saturating_mul((group_len - 1) as u64));
    }

    (duplicate_files, groups.len(), lost_space)
}

fn calibration_directory_for(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf())
    }
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn kib_to_bytes(value: i32) -> u64 {
    (value.max(0) as u64).saturating_mul(1024)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::tempdir;

    use crate::duplicate_engine::{DuplicateFile, DuplicateGroup};

    use super::*;

    fn base_settings() -> SettingsCustom {
        let mut settings = SettingsCustom::default();
        settings.included_paths = vec![PathBuf::from("test-data")];
        settings.included_paths_referenced.clear();
        settings.allowed_extensions.clear();
        settings.excluded_extensions.clear();
        settings
    }

    #[test]
    fn engine_preference_parses_persisted_values() {
        assert_eq!(DuplicateEnginePreference::from_config("auto"), DuplicateEnginePreference::Auto);
        assert_eq!(DuplicateEnginePreference::from_config("czkawka"), DuplicateEnginePreference::Czkawka);
        assert_eq!(DuplicateEnginePreference::from_config("fast"), DuplicateEnginePreference::Fast);
        assert_eq!(DuplicateEnginePreference::from_config("unknown"), DuplicateEnginePreference::Auto);
    }

    #[test]
    fn unsupported_fast_reason_has_user_message() {
        assert!(FastGuiUnsupportedSetting::NonBlake3Hash.label_es().contains("Blake3"));
    }

    #[test]
    fn hash_scan_with_normal_exclusions_uses_fast_engine() {
        let mut settings = base_settings();
        settings.excluded_paths = vec![PathBuf::from("excluded-root")];
        settings.excluded_items = "*/.git/*,*/node_modules/*".to_string();
        settings.minimum_file_size = 16;
        settings.maximum_file_size = 4096;
        settings.recursive_search = false;
        settings.use_cache = true;
        settings.ignore_other_file_systems = true;
        settings.hide_hard_links = false;

        let FastGuiScanPlan::Fast(request) =
            plan_fast_duplicate_scan(CheckingMethod::Hash, HashType::Blake3, &settings)
        else {
            panic!("normal Hash settings should use Fast Engine");
        };

        assert_eq!(request.paths, settings.included_paths);
        assert_eq!(request.excluded_paths, settings.excluded_paths);
        assert_eq!(request.excluded_items, vec!["*/.git/*", "*/node_modules/*"]);
        assert_eq!(request.min_size, 16 * 1024);
        assert_eq!(request.max_size, Some(4096 * 1024));
        assert!(!request.recursive);
        assert!(request.use_cache);
        assert!(request.one_file_system);
        assert!(!request.hide_hard_links);
    }

    #[test]
    fn non_hash_mode_stays_on_legacy_engine() {
        let settings = base_settings();
        assert!(matches!(
            plan_fast_duplicate_scan(CheckingMethod::Name, HashType::Blake3, &settings),
            FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::NonHashCheckingMethod)
        ));
    }

    #[test]
    fn reference_paths_stay_on_legacy_engine() {
        let mut settings = base_settings();
        settings.included_paths_referenced = vec![PathBuf::from("reference")];

        assert!(matches!(
            plan_fast_duplicate_scan(CheckingMethod::Hash, HashType::Blake3, &settings),
            FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::ReferencePaths)
        ));
    }

    #[test]
    fn extension_filters_stay_on_legacy_engine_for_now() {
        let mut settings = base_settings();
        settings.allowed_extensions = "jpg,png".to_string();

        assert!(matches!(
            plan_fast_duplicate_scan(CheckingMethod::Hash, HashType::Blake3, &settings),
            FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::AllowedExtensions)
        ));

        settings.allowed_extensions.clear();
        settings.excluded_extensions = "tmp".to_string();

        assert!(matches!(
            plan_fast_duplicate_scan(CheckingMethod::Hash, HashType::Blake3, &settings),
            FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::ExcludedExtensions)
        ));
    }

    #[test]
    fn non_default_hash_type_stays_on_legacy_engine() {
        let settings = base_settings();
        assert!(matches!(
            plan_fast_duplicate_scan(CheckingMethod::Hash, HashType::Crc32, &settings),
            FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::NonBlake3Hash)
        ));
        assert!(matches!(
            plan_fast_duplicate_scan(CheckingMethod::Hash, HashType::Xxh3, &settings),
            FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::NonBlake3Hash)
        ));
    }

    #[test]
    fn custom_czkawka_hash_tuning_stays_on_legacy_engine() {
        let mut settings = base_settings();
        settings.duplicate_use_prehash = !SettingsCustom::default().duplicate_use_prehash;

        assert!(matches!(
            plan_fast_duplicate_scan(CheckingMethod::Hash, HashType::Blake3, &settings),
            FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::CustomLegacyHashTuning)
        ));
    }

    #[test]
    fn verified_fast_result_converts_to_gui_entries() {
        let mut group = DuplicateGroup::new(vec![
            DuplicateFile {
                path: PathBuf::from("a.bin"),
                size: 123,
                modified_date: 10,
            },
            DuplicateFile {
                path: PathBuf::from("b.bin"),
                size: 123,
                modified_date: 11,
            },
        ]);
        group.verified = true;

        let result = DuplicateScanResult {
            engine: "fclones",
            groups: vec![group],
            elapsed: Duration::from_millis(1),
        };

        let groups = duplicate_result_into_gui_groups(result).expect("conversion");
        assert_eq!(groups.len(), 1);
        assert!(groups[0].0.is_none());
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[0].1[0].path, PathBuf::from("a.bin"));
        assert_eq!(groups[0].1[1].path, PathBuf::from("b.bin"));

        let (duplicates, group_count, lost_space) = gui_group_stats(&groups);
        assert_eq!(duplicates, 1);
        assert_eq!(group_count, 1);
        assert_eq!(lost_space, 123);
    }

    #[test]
    fn unverified_fast_result_is_never_exposed_to_gui() {
        let group = DuplicateGroup::new(vec![
            DuplicateFile {
                path: PathBuf::from("a.bin"),
                size: 123,
                modified_date: 10,
            },
            DuplicateFile {
                path: PathBuf::from("b.bin"),
                size: 123,
                modified_date: 11,
            },
        ]);

        let result = DuplicateScanResult {
            engine: "fclones",
            groups: vec![group],
            elapsed: Duration::from_millis(1),
        };

        assert!(matches!(
            duplicate_result_into_gui_groups(result),
            Err(FastGuiConversionError::UnverifiedGroup(0))
        ));
    }

    #[test]
    fn fast_state_saves_text_and_both_json_formats() {
        let dir = tempdir().expect("tempdir");
        let request = DuplicateScanRequest::for_paths([PathBuf::from("input")]);
        let groups = vec![(
            None,
            vec![
                DuplicateEntry {
                    path: PathBuf::from("a.bin"),
                    modified_date: 10,
                    size: 123,
                    hash: String::new(),
                },
                DuplicateEntry {
                    path: PathBuf::from("b.bin"),
                    modified_date: 11,
                    size: 123,
                    hash: String::new(),
                },
            ],
        )];

        let state = FastDuplicateState::from_gui_groups(&groups, &request);
        state
            .save_all_in_one(dir.path().to_string_lossy().as_ref())
            .expect("save");

        assert!(dir.path().join("results_duplicates.txt").is_file());
        assert!(dir.path().join("results_duplicates_pretty.json").is_file());
        assert!(dir.path().join("results_duplicates_compact.json").is_file());

        let text = fs::read_to_string(dir.path().join("results_duplicates.txt")).expect("read text");
        assert!(text.contains("a.bin"));
        assert!(text.contains("b.bin"));
        assert!(text.contains("ExactVerifier"));
    }

    #[test]
    fn calibration_target_uses_parent_for_file_paths() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("file.bin");
        fs::write(&file, b"x").expect("write");
        assert_eq!(calibration_directory_for(&file), dir.path());
        assert_eq!(calibration_directory_for(dir.path()), dir.path());
    }
}
