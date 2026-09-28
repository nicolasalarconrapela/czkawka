use std::fs;
use std::time::Instant;

use fclones::config::GroupConfig;
use fclones::log::StdLog;
use fclones::{FileLen, Path as FclonesPath, group_files};

use super::types::{
    DuplicateEngine, DuplicateEngineError, DuplicateFile, DuplicateGroup, DuplicateScanRequest, DuplicateScanResult, modified_unix_seconds,
};

pub(crate) struct FclonesEngine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataMode {
    /// Preserve the old adapter behaviour for controlled A/B benchmarks.
    Eager,
    /// Defer path metadata until the exact verifier already has the file open.
    Deferred,
}

impl FclonesEngine {
    fn scan_with_metadata_mode(
        &self,
        request: &DuplicateScanRequest,
        metadata_mode: MetadataMode,
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
                    .map(|file| {
                        let path = file.path.to_path_buf();
                        let size = file.len.0;

                        match metadata_mode {
                            MetadataMode::Deferred => DuplicateFile::from_scanned_size(path, size),
                            MetadataMode::Eager => {
                                let modified_date = fs::metadata(&path).map(|metadata| modified_unix_seconds(&metadata)).unwrap_or(0);
                                DuplicateFile {
                                    path,
                                    size,
                                    modified_date,
                                }
                            }
                        }
                    })
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

    /// Test-only baseline that reproduces the adapter behaviour used before
    /// Phase 2.1. It lets the benchmark compare the metadata handoff without
    /// changing the fclones algorithm or the exact verifier.
    #[cfg(test)]
    pub(crate) fn scan_with_eager_metadata(
        &self,
        request: &DuplicateScanRequest,
    ) -> Result<DuplicateScanResult, DuplicateEngineError> {
        self.scan_with_metadata_mode(request, MetadataMode::Eager)
    }
}

impl DuplicateEngine for FclonesEngine {
    fn name(&self) -> &'static str {
        "fclones"
    }

    fn scan(&self, request: &DuplicateScanRequest) -> Result<DuplicateScanResult, DuplicateEngineError> {
        self.scan_with_metadata_mode(request, MetadataMode::Deferred)
    }
}
