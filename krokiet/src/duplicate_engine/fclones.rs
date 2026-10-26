use std::path::Path;
use std::time::Instant;

use czkawka_core::common::items::{DEFAULT_EXCLUDED_ITEMS, TRASH_EXCLUDED_ITEMS};
use fclones::config::GroupConfig;
use fclones::log::StdLog;
use fclones::{FileLen, Path as FclonesPath, group_files};

use super::types::{DuplicateEngine, DuplicateEngineError, DuplicateFile, DuplicateGroup, DuplicateScanRequest, DuplicateScanResult};

pub(crate) struct FclonesEngine;

impl DuplicateEngine for FclonesEngine {
    fn name(&self) -> &'static str {
        "fclones"
    }

    fn scan(&self, request: &DuplicateScanRequest) -> Result<DuplicateScanResult, DuplicateEngineError> {
        request.validate()?;
        let started = Instant::now();

        let mut config = GroupConfig::default();
        config.paths = request.paths.iter().map(FclonesPath::from).collect();
        config.depth = if request.recursive { None } else { Some(1) };
        config.hidden = true;
        // Krokiet does not implicitly consume .gitignore/.fdignore files, so disable
        // that fclones behaviour for a fair side-by-side comparison.
        config.no_ignore = true;
        config.min_size = FileLen(request.min_size);
        config.max_size = request.max_size.map(FileLen);
        config.cache = request.use_cache;
        config.one_fs = request.one_file_system;
        config.match_links = !request.hide_hard_links;

        // Krokiet wildcard exclusions allow `*` to span directory separators.
        // fclones supports regex selection directly, so translate those patterns
        // instead of depending on subtly different glob semantics.
        config.regex = true;
        config.ignore_case = cfg!(target_family = "windows");
        config.exclude_patterns = build_exclude_patterns(request);

        // Keep fclones' full-content hash as the production candidate filter.
        // The independent ExactVerifier still performs the final byte-for-byte
        // confirmation, so correctness never depends on hash equality alone.
        config.skip_content_hash = false;

        let mut log = StdLog::new();
        log.no_progress = true;

        let groups = group_files(&config, &log).map_err(|error| DuplicateEngineError::engine(self.name(), error))?;
        let groups = groups
            .into_iter()
            .map(|group| {
                let files = group
                    .files
                    .into_iter()
                    .map(|file| DuplicateFile::from_scanned_size(file.path.to_path_buf(), file.len.0))
                    .collect();
                DuplicateGroup::new(files)
            })
            .collect();

        Ok(DuplicateScanResult {
            engine: self.name(),
            groups,
            elapsed: started.elapsed(),
        })
    }
}

fn build_exclude_patterns(request: &DuplicateScanRequest) -> Vec<String> {
    let mut patterns = Vec::new();

    patterns.extend(
        request
            .excluded_paths
            .iter()
            .map(|path| excluded_path_regex(path)),
    );

    for item in expand_excluded_items(&request.excluded_items) {
        patterns.push(krokiet_wildcard_to_regex(&item));
    }

    patterns
}

fn expand_excluded_items(items: &[String]) -> Vec<String> {
    let mut expanded = Vec::new();

    for raw in items {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }

        match item {
            "DEFAULT" => {
                expanded.extend(
                    DEFAULT_EXCLUDED_ITEMS
                        .split(',')
                        .map(str::trim)
                        .filter(|item| !item.is_empty())
                        .map(str::to_string),
                );
            }
            "$TRASH" => {
                expanded.extend(
                    TRASH_EXCLUDED_ITEMS
                        .split(',')
                        .map(str::trim)
                        .filter(|item| !item.is_empty())
                        .map(str::to_string),
                );
            }
            _ if item.contains('*') => expanded.push(item.to_string()),
            // Czkawka ignores wildcard-free excluded-item expressions and emits
            // a warning. The Fast Engine mirrors the filtering behavior here;
            // GUI message parity can be added when the production route is wired.
            _ => {}
        }
    }

    expanded
}

fn excluded_path_regex(path: &Path) -> String {
    let path = path.to_string_lossy();
    let fragment = literal_path_regex_fragment(&path);

    // fclones 0.35.0 rebases regex path patterns against its `base_dir`
    // unless the compiled pattern is considered absolute. For regexes,
    // PathSelector recognizes a leading `.*` as absolute. Starting with `^`
    // therefore makes an absolute Krokiet excluded path behave like a relative
    // fclones pattern and the exclusion never matches.
    //
    // The leading `.*` also tolerates Windows' optional `\\?\` canonical-path
    // prefix. The path itself is still followed by either a separator or end of
    // string, so excluding `C:\foo\bar` cannot accidentally exclude
    // `C:\foo\bar-other`.
    if cfg!(target_family = "windows") {
        format!(r".*{}(?:[\\/].*)?$", fragment)
    } else {
        format!(r".*{}(?:/.*)?$", fragment)
    }
}

fn krokiet_wildcard_to_regex(pattern: &str) -> String {
    let mut output = String::from("^");
    let mut literal = String::new();

    for ch in pattern.chars() {
        if ch == '*' {
            flush_literal_path_fragment(&mut output, &mut literal);
            output.push_str(".*");
        } else {
            literal.push(ch);
        }
    }

    flush_literal_path_fragment(&mut output, &mut literal);
    output.push('$');
    output
}

fn flush_literal_path_fragment(output: &mut String, literal: &mut String) {
    if literal.is_empty() {
        return;
    }
    output.push_str(&literal_path_regex_fragment(literal));
    literal.clear();
}

fn literal_path_regex_fragment(value: &str) -> String {
    let mut output = String::new();
    let mut normal = String::new();

    for ch in value.chars() {
        if ch == '/' || ch == '\\' {
            if !normal.is_empty() {
                output.push_str(&regex::escape(&normal));
                normal.clear();
            }
            // Accept either representation. This is especially useful on Windows,
            // where user settings commonly use backslashes but libraries may
            // normalize paths internally.
            output.push_str(r"[\\/]");
        } else {
            normal.push(ch);
        }
    }

    if !normal.is_empty() {
        output.push_str(&regex::escape(&normal));
    }

    output
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;

    fn write_pair(directory: &Path, prefix: &str, byte: u8) {
        fs::create_dir_all(directory).expect("create directory");
        let bytes = vec![byte; 32 * 1024];
        fs::write(directory.join(format!("{prefix}-a.bin")), &bytes).expect("write a");
        fs::write(directory.join(format!("{prefix}-b.bin")), &bytes).expect("write b");
    }

    #[test]
    fn wildcard_translation_matches_across_path_separators() {
        let pattern = krokiet_wildcard_to_regex(r"*\node_modules\*");
        let regex = regex::RegexBuilder::new(&pattern)
            .case_insensitive(true)
            .build()
            .expect("regex");

        assert!(regex.is_match(r"C:\repo\node_modules\package\index.js"));
        assert!(regex.is_match("C:/repo/node_modules/package/index.js"));
        assert!(!regex.is_match(r"C:\repo\src\index.js"));
    }

    #[test]
    fn excluded_path_is_removed_before_grouping() {
        let dir = tempdir().expect("tempdir");
        let keep = dir.path().join("keep");
        let excluded = dir.path().join("excluded");

        write_pair(&keep, "keep", 0x51);
        write_pair(&excluded, "skip", 0x51);

        let mut request = DuplicateScanRequest::for_paths([dir.path().to_path_buf()]);
        request.excluded_paths.push(excluded.clone());

        let result = FclonesEngine.scan(&request).expect("fclones scan");
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.file_count(), 2);
        assert!(result.groups[0].files.iter().all(|file| file.path.starts_with(&keep)));
        assert!(result.groups[0].files.iter().all(|file| !file.path.starts_with(&excluded)));
    }

    #[test]
    fn excluded_item_wildcard_is_removed_before_grouping() {
        let dir = tempdir().expect("tempdir");
        let keep = dir.path().join("keep");
        let excluded = dir.path().join("node_modules");

        write_pair(&keep, "keep", 0x61);
        write_pair(&excluded, "skip", 0x61);

        let mut request = DuplicateScanRequest::for_paths([dir.path().to_path_buf()]);
        request
            .excluded_items
            .push(format!("*{}node_modules{}*", std::path::MAIN_SEPARATOR, std::path::MAIN_SEPARATOR));

        let result = FclonesEngine.scan(&request).expect("fclones scan");
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.file_count(), 2);
        assert!(result.groups[0].files.iter().all(|file| file.path.starts_with(&keep)));
    }

    #[test]
    fn default_and_trash_presets_expand_to_real_patterns() {
        let expanded = expand_excluded_items(&["DEFAULT".to_string(), "$TRASH".to_string()]);
        assert!(expanded.iter().any(|item| item.contains(".git")));
        assert!(expanded.len() >= 2);
    }
}
