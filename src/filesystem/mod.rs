#[cfg(windows)]
mod native;
mod snapshot;

use anyhow::{Context, Result};
pub(crate) use snapshot::Snapshot;
use std::{ffi::OsString, path::Path};

pub(crate) struct Entry {
    pub name: OsString,
    pub metadata: Snapshot,
}
pub(crate) struct Directory;
impl Directory {
    /// Enumerate Windows directory metadata in bounded NT batches.
    #[cfg(windows)]
    pub(crate) fn read(path: &Path) -> Result<impl Iterator<Item = Result<Entry>>> {
        native::Entries::open(path).context("Failed to open native directory iterator")
    }
    /// Retain portable enumeration metadata without following source links.
    #[cfg(not(windows))]
    pub(crate) fn read(path: &Path) -> Result<impl Iterator<Item = Result<Entry>>> {
        Ok(std::fs::read_dir(path)
            .context("Failed to open directory")?
            .map(|entry| {
                let entry = entry.context("Failed to enumerate directory entry")?;
                let metadata = entry
                    .metadata()
                    .context("Failed to inspect directory entry")?;
                Ok(Entry {
                    name: entry.file_name(),
                    metadata: metadata.into(),
                })
            }))
    }
}
