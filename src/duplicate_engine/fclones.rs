use std::fs;
use std::time::Instant;

use fclones::config::GroupConfig;
use fclones::log::StdLog;
use fclones::{FileLen, Path as FclonesPath, group_files};

use super::types::{
    DuplicateEngine, DuplicateEngineError, DuplicateFile, DuplicateGroup, DuplicateScanRequest, DuplicateScanResult, modified_unix_seconds,
};

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
                        let modified_date = fs::metadata(&path).map(|metadata| modified_unix_seconds(&metadata)).unwrap_or(0);
                        DuplicateFile {
                            path,
                            size: file.len.0,
                            modified_date,
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
}
