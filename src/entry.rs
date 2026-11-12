use anyhow::{Context, Result};
use std::{ffi, fs, path, sync};

pub(crate) struct Directory {
    pub source: path::PathBuf,
    pub destination: path::PathBuf,
}

enum Paths {
    Known {
        source: path::PathBuf,
        destination: path::PathBuf,
    },
    Deferred {
        directory: sync::Arc<Directory>,
        name: ffi::OsString,
        source: sync::OnceLock<path::PathBuf>,
        destination: sync::OnceLock<path::PathBuf>,
    },
}

pub(crate) struct Entry {
    paths: Paths,
    pub job: usize,
    pub source_metadata: crate::filesystem::Snapshot,
    pub destination_metadata: Option<crate::filesystem::Snapshot>,
}

impl Entry {
    /// Retain existing full paths when relative filters or purge require them.
    pub(crate) fn known(
        source: path::PathBuf,
        destination: path::PathBuf,
        job: usize,
        source_metadata: impl Into<crate::filesystem::Snapshot>,
        destination_metadata: Option<fs::Metadata>,
    ) -> Self {
        Self {
            paths: Paths::Known {
                source,
                destination,
            },
            job,
            source_metadata: source_metadata.into(),
            destination_metadata: destination_metadata.map(Into::into),
        }
    }
    /// Share immutable directory paths and materialize full filenames only when needed.
    pub(crate) fn deferred(
        directory: sync::Arc<Directory>,
        name: ffi::OsString,
        job: usize,
        source_metadata: impl Into<crate::filesystem::Snapshot>,
    ) -> Self {
        Self {
            paths: Paths::Deferred {
                directory,
                name,
                source: sync::OnceLock::new(),
                destination: sync::OnceLock::new(),
            },
            job,
            source_metadata: source_metadata.into(),
            destination_metadata: None,
        }
    }
    /// Resolve a source filename lazily for copying, hashing, or contextual errors.
    pub(crate) fn source(&self) -> &path::Path {
        match &self.paths {
            Paths::Known { source, .. } => source,
            Paths::Deferred {
                directory,
                name,
                source,
                ..
            } => source.get_or_init(|| directory.source.join(name)),
        }
    }
    /// Resolve a target filename lazily without allocating during metadata-only skips.
    pub(crate) fn destination(&self) -> &path::Path {
        match &self.paths {
            Paths::Known { destination, .. } => destination,
            Paths::Deferred {
                directory,
                name,
                destination,
                ..
            } => destination.get_or_init(|| directory.destination.join(name)),
        }
    }
    /// Return a filename index key without forcing full path construction.
    pub(crate) fn file_name(&self) -> Result<&ffi::OsStr> {
        match &self.paths {
            Paths::Known { destination, .. } => destination
                .file_name()
                .context("Destination has no filename"),
            Paths::Deferred { name, .. } => Ok(name),
        }
    }
}
