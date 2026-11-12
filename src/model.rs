use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Select the evidence needed before an existing destination is skipped.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Compare {
    #[default]
    Mtime,
    Hash,
    Auto,
    Always,
}

/// Configure one directory tree's selection and comparison behavior.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CopyOptions {
    pub compare: Compare,
    pub recursive: bool,
    pub preserve_mtime: bool,
    pub purge: bool,
    pub case_sensitive: bool,
    pub exclude_dirs: Vec<String>,
    pub exclude_files: Vec<String>,
    pub include_files: Vec<String>,
}

impl Default for CopyOptions {
    /// Use portable timestamp comparison without destructive cleanup.
    fn default() -> Self {
        Self {
            compare: Compare::Mtime,
            recursive: true,
            preserve_mtime: true,
            purge: false,
            case_sensitive: !cfg!(windows),
            exclude_dirs: Vec::new(),
            exclude_files: Vec::new(),
            include_files: Vec::new(),
        }
    }
}

/// Map a source directory's contents into a destination directory.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CopyJob {
    pub source: PathBuf,
    pub destination: PathBuf,
    #[serde(default)]
    pub options: CopyOptions,
}

impl CopyJob {
    /// Construct a mapping using the standard copy defaults.
    pub fn new(source: impl Into<PathBuf>, destination: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            destination: destination.into(),
            options: CopyOptions::default(),
        }
    }
}

/// Bound worker pools and configure reporting, retries, and preview mode.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunOptions {
    pub scan_workers: usize,
    pub hash_workers: usize,
    pub copy_workers: usize,
    pub retries: usize,
    pub progress_interval_ms: u64,
    pub dry_run: bool,
}

impl Default for RunOptions {
    /// Keep disk concurrency bounded on both workstations and build agents.
    fn default() -> Self {
        Self {
            scan_workers: 8,
            hash_workers: 4,
            copy_workers: 4,
            retries: 2,
            progress_interval_ms: 250,
            dry_run: false,
        }
    }
}

/// Report cumulative counters; totals are unknown until their stage finishes planning.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Progress {
    pub phase: String,
    pub elapsed_seconds: f64,
    pub scanned_files: u64,
    pub total_files: Option<u64>,
    pub compared_files: u64,
    pub hashed_bytes: u64,
    pub copied_files: u64,
    pub copied_bytes: u64,
    pub total_copy_bytes: Option<u64>,
    pub skipped_mtime: u64,
    pub skipped_hash: u64,
    pub skipped_same_file: u64,
    pub skipped_links: u64,
    pub deleted_files: u64,
    pub failed_files: u64,
    pub planned_copy_files: u64,
    pub planned_delete_files: u64,
    pub dry_run: bool,
    pub complete: bool,
}

/// Return final counters together with every recoverable file failure.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Summary {
    pub progress: Progress,
    pub errors: Vec<String>,
}

impl Summary {
    /// Report success only after all selected files and cleanup complete.
    pub fn success(&self) -> bool {
        self.errors.is_empty() && self.progress.complete
    }
}
