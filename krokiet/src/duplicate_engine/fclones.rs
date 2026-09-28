use std::time::Instant;

use fclones::config::GroupConfig;
use fclones::log::StdLog;
use fclones::{FileLen, Path as FclonesPath, group_files};

use super::types::{DuplicateEngine, DuplicateEngineError, DuplicateFile, DuplicateGroup, DuplicateScanRequest, DuplicateScanResult};

pub(crate) struct FclonesEngine;

impl FclonesEngine {
    fn scan_with_content_hash(
        &self,
        request: &DuplicateScanRequest,
        skip_content_hash: bool,
    ) -> Result<DuplicateScanResult, DuplicateEngineError> {
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
        config.match_links = false;
        // Production still keeps fclones' full-content hash in Phase 2.2. The
        // test-only candidate path skips only this final stage, preserving the
        // size + prefix + suffix filters before our independent exact refiner.
        config.skip_content_hash = skip_content_hash;

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

    /// Experimental Phase 2.2 candidate scan.
    ///
    /// fclones still performs its size, prefix and suffix stages, but does not
    /// perform the final full-content hash. The returned groups are therefore
    /// candidates only and MUST pass `refine_result_exact` before they can be
    /// treated as duplicates.
    #[cfg(test)]
    pub(crate) fn scan_prefix_suffix_candidates(
        &self,
        request: &DuplicateScanRequest,
    ) -> Result<DuplicateScanResult, DuplicateEngineError> {
        self.scan_with_content_hash(request, true)
    }
}

impl DuplicateEngine for FclonesEngine {
    fn name(&self) -> &'static str {
        "fclones"
    }

    fn scan(&self, request: &DuplicateScanRequest) -> Result<DuplicateScanResult, DuplicateEngineError> {
        self.scan_with_content_hash(request, false)
    }
}
