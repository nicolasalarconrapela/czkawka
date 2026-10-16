use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use czkawka_core::common::model::{CheckingMethod, HashType};
use czkawka_core::common::tool_data::CommonData;
use czkawka_core::common::traits::Search;
use czkawka_core::tools::duplicate::{DuplicateFinder, DuplicateFinderParameters};

use super::types::{DuplicateEngine, DuplicateEngineError, DuplicateFile, DuplicateGroup, DuplicateScanRequest, DuplicateScanResult};

// This adapter is intentionally NOT marked for removal. The temporary code is
// the direct `DuplicateFinder` orchestration that still lives in
// `connect_scan/duplicate.rs`. Keeping this engine gives us a known reference
// implementation and a fallback while the Fast Engine is being validated.

pub(crate) struct CzkawkaEngine;

impl DuplicateEngine for CzkawkaEngine {
    fn name(&self) -> &'static str {
        "czkawka"
    }

    fn scan(&self, request: &DuplicateScanRequest) -> Result<DuplicateScanResult, DuplicateEngineError> {
        request.validate()?;
        let started = Instant::now();

        // Keep this adapter intentionally conservative. It mirrors the current
        // content-based duplicate mode, but does not replace the GUI scan yet.
        let params = DuplicateFinderParameters::new(
            CheckingMethod::Hash,
            HashType::Blake3,
            true,
            0,
            0,
            true,
        );
        let mut finder = DuplicateFinder::new(params);
        finder.set_included_paths(request.paths.clone());
        finder.set_recursive_search(request.recursive);
        finder.set_minimal_file_size(request.min_size);
        finder.set_maximal_file_size(request.max_size.unwrap_or(u64::MAX));
        finder.set_exclude_other_filesystems(request.one_file_system);
        finder.set_hide_hard_links(true);
        finder.set_use_cache(request.use_cache);

        let stop_flag = Arc::new(AtomicBool::new(false));
        finder.search(&stop_flag, None);

        let groups = finder
            .get_files_sorted_by_hash()
            .values()
            .flatten()
            .map(|entries| {
                DuplicateGroup::new(
                    entries
                        .iter()
                        .map(|entry| DuplicateFile {
                            path: entry.path.clone(),
                            size: entry.size,
                            modified_date: entry.modified_date,
                        })
                        .collect(),
                )
            })
            .collect();

        Ok(DuplicateScanResult {
            engine: self.name(),
            groups,
            elapsed: started.elapsed(),
        })
    }
}
