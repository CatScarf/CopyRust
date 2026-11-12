use crate::{Progress, RunOptions};
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Instant;

/// Request cooperative cancellation during scanning, hashing, and copying.
#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    /// Request cancellation from another thread or a progress callback.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Read whether a cancellation request is pending.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Stop the current operation with a contextual cancellation error.
    pub(crate) fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("Copy operation was cancelled");
        }
        Ok(())
    }
}

pub(crate) struct ScanCounter<'a> {
    target: &'a AtomicU64,
    pending: u64,
}

impl ScanCounter<'_> {
    /// Amortize shared cache-line updates while preserving bounded progress latency.
    pub(crate) fn increment(&mut self) {
        self.pending += 1;
        if self.pending == 64 {
            self.flush();
        }
    }
    /// Publish pending counts without locks or fallible cleanup.
    fn flush(&mut self) {
        if self.pending != 0 {
            self.target.fetch_add(self.pending, Ordering::Relaxed);
            self.pending = 0;
        }
    }
}

impl Drop for ScanCounter<'_> {
    /// Preserve every selected file count on success, cancellation, or early failure.
    fn drop(&mut self) {
        self.flush();
    }
}

pub(crate) struct State {
    pub progress: Mutex<Progress>,
    pub scanned: AtomicU64,
    pub hashed: AtomicU64,
    pub copied: AtomicU64,
    pub errors: Mutex<Vec<String>>,
    pub cancellation: Cancellation,
    parents: Mutex<HashSet<PathBuf>>,
    started: Instant,
}

impl State {
    /// Start counters before enumeration so long scans remain observable.
    pub(crate) fn new(options: &RunOptions, cancellation: Cancellation) -> Self {
        Self {
            progress: Mutex::new(Progress {
                phase: "scanning".into(),
                dry_run: options.dry_run,
                ..Progress::default()
            }),
            scanned: AtomicU64::new(0),
            hashed: AtomicU64::new(0),
            copied: AtomicU64::new(0),
            errors: Mutex::new(Vec::new()),
            cancellation,
            parents: Mutex::new(HashSet::new()),
            started: Instant::now(),
        }
    }

    /// Create one thread-local counting buffer for a directory scan.
    pub(crate) fn scan_counter(&self) -> ScanCounter<'_> {
        ScanCounter {
            target: &self.scanned,
            pending: 0,
        }
    }

    /// Reuse existing destination directories only after all ancestors pass planning checks.
    pub(crate) fn remember_parents(&self, paths: Vec<PathBuf>) -> Result<()> {
        self.parents
            .lock()
            .map_err(|e| anyhow::anyhow!("Failed to lock validated directories: {e}"))?
            .extend(paths);
        Ok(())
    }

    /// Validate shared directory topology once per batch and create missing copy parents.
    pub(crate) fn parent(&self, path: &Path, create: bool) -> Result<()> {
        let mut parents = self
            .parents
            .lock()
            .map_err(|error| anyhow::anyhow!("Failed to lock validated directories: {error}"))?;
        if parents.contains(path) {
            return Ok(());
        }
        crate::io::Paths::guard(path).context("Failed to validate copy parent")?;
        if create {
            std::fs::create_dir_all(path).context("Failed to create copy parent")?;
        }
        crate::io::Paths::guard(path).context("Failed to validate prepared copy parent")?;
        parents.insert(path.into());
        Ok(())
    }

    /// Apply one short counter update without exposing synchronization primitives.
    pub(crate) fn update(&self, change: impl FnOnce(&mut Progress)) -> Result<()> {
        let mut progress = self
            .progress
            .lock()
            .map_err(|e| anyhow::anyhow!("Failed to lock progress: {e}"))?;
        change(&mut progress);
        Ok(())
    }

    /// Return a coherent progress snapshot with byte counters and elapsed time.
    pub(crate) fn snapshot(&self) -> Result<Progress> {
        let mut progress = self
            .progress
            .lock()
            .map_err(|e| anyhow::anyhow!("Failed to read progress: {e}"))?
            .clone();
        progress.elapsed_seconds = self.started.elapsed().as_secs_f64();
        progress.scanned_files = self.scanned.load(Ordering::Relaxed);
        progress.hashed_bytes = self.hashed.load(Ordering::Relaxed);
        progress.copied_bytes = self.copied.load(Ordering::Relaxed);
        Ok(progress)
    }

    /// Retain each file error and count it as a failed file.
    pub(crate) fn failure(&self, error: anyhow::Error) -> Result<()> {
        self.errors
            .lock()
            .map_err(|e| anyhow::anyhow!("Failed to lock file errors: {e}"))?
            .push(format!("{error:#}"));
        self.update(|p| p.failed_files += 1)
            .context("Failed to record file failure")
    }

    /// Copy the complete error list for the final report.
    pub(crate) fn errors(&self) -> Result<Vec<String>> {
        Ok(self
            .errors
            .lock()
            .map_err(|e| anyhow::anyhow!("Failed to read file errors: {e}"))?
            .clone())
    }
}
