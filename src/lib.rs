use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use config::EffectiveWatchConfig;

pub mod anchor;
pub mod config;
pub mod dispatch;
pub mod index;
pub mod ingest;
pub mod query;
pub mod store;
pub mod tape;

#[derive(Debug)]
pub struct CliError {
    pub code: &'static str,
    pub message: String,
}

impl CliError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn io(code: &'static str, err: io::Error) -> Self {
        Self::new(code, err.to_string())
    }
}

impl From<rusqlite::Error> for CliError {
    fn from(value: rusqlite::Error) -> Self {
        Self::new("sqlite_error", value.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(value: serde_json::Error) -> Self {
        Self::new("json_error", value.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct RepoPaths {
    pub root: PathBuf,
    pub tapes: PathBuf,
    pub objects: PathBuf,
    pub cursors: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RuntimeContext {
    pub config_path: PathBuf,
    pub db_path: PathBuf,
    pub tapes_dir: PathBuf,
    pub tape_lookup_dirs: Vec<PathBuf>,
    pub additional_stores: Vec<PathBuf>,
    pub explain_default_limit: usize,
    pub peek_default_lines: usize,
    pub peek_default_before: usize,
    pub peek_default_after: usize,
    pub peek_grep_context: usize,
    pub metrics_enabled: bool,
    pub metrics_log: PathBuf,
    pub watch: Option<EffectiveWatchConfig>,
}

pub fn ensure_db_parent(db_path: &Path) -> Result<(), CliError> {
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).map_err(|err| CliError::io("mkdir_error", err))?;
    }
    Ok(())
}

pub fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn home_dir() -> Result<PathBuf, CliError> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| CliError::new("home_error", "HOME environment variable is not set"))
}
