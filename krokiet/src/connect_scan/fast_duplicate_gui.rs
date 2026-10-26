use czkawka_core::common::model::CheckingMethod;
use czkawka_core::tools::duplicate::DuplicateEntry;

use crate::duplicate_engine::{DuplicateScanRequest, DuplicateScanResult};
use crate::settings::model::SettingsCustom;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FastGuiUnsupportedSetting {
    NonHashCheckingMethod,
    ReferencePaths,
    AllowedExtensions,
    ExcludedExtensions,
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

pub(crate) fn plan_fast_duplicate_scan(
    checking_method: CheckingMethod,
    settings: &SettingsCustom,
) -> FastGuiScanPlan {
    if checking_method != CheckingMethod::Hash {
        return FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::NonHashCheckingMethod);
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
        let group_len = entries.len() + if reference.is_some() { 1 } else { 0 };
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
    use std::path::PathBuf;
    use std::time::Duration;

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
            plan_fast_duplicate_scan(CheckingMethod::Hash, &settings)
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
            plan_fast_duplicate_scan(CheckingMethod::Name, &settings),
            FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::NonHashCheckingMethod)
        ));
    }

    #[test]
    fn reference_paths_stay_on_legacy_engine() {
        let mut settings = base_settings();
        settings.included_paths_referenced = vec![PathBuf::from("reference")];

        assert!(matches!(
            plan_fast_duplicate_scan(CheckingMethod::Hash, &settings),
            FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::ReferencePaths)
        ));
    }

    #[test]
    fn extension_filters_stay_on_legacy_engine_for_now() {
        let mut settings = base_settings();
        settings.allowed_extensions = "jpg,png".to_string();

        assert!(matches!(
            plan_fast_duplicate_scan(CheckingMethod::Hash, &settings),
            FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::AllowedExtensions)
        ));

        settings.allowed_extensions.clear();
        settings.excluded_extensions = "tmp".to_string();

        assert!(matches!(
            plan_fast_duplicate_scan(CheckingMethod::Hash, &settings),
            FastGuiScanPlan::Legacy(FastGuiUnsupportedSetting::ExcludedExtensions)
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
}
