use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct DuplicateFile {
    pub path: PathBuf,
    pub size: u64,
    pub modified_date: u64,
}

impl DuplicateFile {
    pub(crate) fn from_path(path: PathBuf) -> io::Result<Self> {
        let metadata = fs::metadata(&path)?;
        Ok(Self {
            size: metadata.len(),
            modified_date: modified_unix_seconds(&metadata),
            path,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DuplicateGroup {
    pub files: Vec<DuplicateFile>,
    /// `true` only after an exact byte-for-byte verification completed successfully.
    pub verified: bool,
}

impl DuplicateGroup {
    pub(crate) fn new(files: Vec<DuplicateFile>) -> Self {
        Self { files, verified: false }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DuplicateScanRequest {
    pub paths: Vec<PathBuf>,
    pub recursive: bool,
    pub min_size: u64,
    pub max_size: Option<u64>,
    pub use_cache: bool,
    pub one_file_system: bool,
}

impl DuplicateScanRequest {
    pub(crate) fn for_paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            paths: paths.into_iter().collect(),
            recursive: true,
            // fclones defaults to 1 byte and this also avoids treating every empty
            // file as a duplicate in the initial engine-comparison tests.
            min_size: 1,
            max_size: None,
            use_cache: false,
            one_file_system: false,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), DuplicateEngineError> {
        if self.paths.is_empty() {
            return Err(DuplicateEngineError::Configuration("no input paths supplied".to_string()));
        }
        if let Some(max_size) = self.max_size
            && max_size < self.min_size
        {
            return Err(DuplicateEngineError::Configuration(format!(
                "max_size ({max_size}) is smaller than min_size ({})",
                self.min_size
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DuplicateScanResult {
    pub engine: &'static str,
    pub groups: Vec<DuplicateGroup>,
    pub elapsed: Duration,
}

impl DuplicateScanResult {
    pub(crate) fn file_count(&self) -> usize {
        self.groups.iter().map(|group| group.files.len()).sum()
    }
}

pub(crate) trait DuplicateEngine {
    fn name(&self) -> &'static str;
    fn scan(&self, request: &DuplicateScanRequest) -> Result<DuplicateScanResult, DuplicateEngineError>;
}

#[derive(Debug)]
pub(crate) enum DuplicateEngineError {
    Configuration(String),
    Io(io::Error),
    Engine { engine: &'static str, message: String },
}

impl DuplicateEngineError {
    pub(crate) fn engine(engine: &'static str, error: impl Display) -> Self {
        Self::Engine {
            engine,
            message: error.to_string(),
        }
    }
}

impl Display for DuplicateEngineError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Configuration(message) => write!(f, "invalid duplicate-engine configuration: {message}"),
            Self::Io(error) => write!(f, "duplicate-engine I/O error: {error}"),
            Self::Engine { engine, message } => write!(f, "{engine} duplicate engine failed: {message}"),
        }
    }
}

impl Error for DuplicateEngineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Configuration(_) | Self::Engine { .. } => None,
        }
    }
}

impl From<io::Error> for DuplicateEngineError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub(crate) fn modified_unix_seconds(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs())
}

pub(crate) fn metadata_fingerprint(path: &Path) -> io::Result<(u64, Option<SystemTime>)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.len(), metadata.modified().ok()))
}
