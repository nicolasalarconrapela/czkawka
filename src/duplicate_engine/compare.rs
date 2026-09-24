use std::collections::BTreeSet;
use std::path::PathBuf;

use super::types::DuplicateScanResult;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DuplicateComparison {
    pub only_left: Vec<Vec<PathBuf>>,
    pub only_right: Vec<Vec<PathBuf>>,
}

impl DuplicateComparison {
    pub(crate) fn identical(&self) -> bool {
        self.only_left.is_empty() && self.only_right.is_empty()
    }
}

pub(crate) fn compare_results(left: &DuplicateScanResult, right: &DuplicateScanResult) -> DuplicateComparison {
    let left = normalized_groups(left);
    let right = normalized_groups(right);

    DuplicateComparison {
        only_left: left.difference(&right).cloned().collect(),
        only_right: right.difference(&left).cloned().collect(),
    }
}

fn normalized_groups(result: &DuplicateScanResult) -> BTreeSet<Vec<PathBuf>> {
    result
        .groups
        .iter()
        .map(|group| {
            let mut paths = group
                .files
                .iter()
                .map(|file| std::fs::canonicalize(&file.path).unwrap_or_else(|_| file.path.clone()))
                .collect::<Vec<_>>();
            paths.sort_unstable();
            paths
        })
        .collect()
}
