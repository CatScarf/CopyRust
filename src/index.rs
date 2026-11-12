use crate::{io::Paths, plan::Entry, state::State};
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::{
    collections::HashMap,
    ffi::OsStr,
    path::{Path, PathBuf},
};

pub(crate) struct Candidates<'a> {
    pub directory: &'a Path,
    pub offset: usize,
    pub entries: &'a [Entry],
}

#[derive(Clone, Copy)]
struct Slot {
    hash: u64,
    index: usize,
}
struct Pending {
    path: PathBuf,
    rows: Vec<Slot>,
}
struct Directory {
    path: PathBuf,
    slots: Vec<Slot>,
    count: usize,
}
pub(crate) struct DirectoryIndex {
    directories: HashMap<String, Directory>,
}

impl DirectoryIndex {
    /// Build immutable directory-local tables without allocating a string per filename.
    pub(crate) fn build(groups: Vec<Candidates<'_>>, files: &[&Entry]) -> Result<Self> {
        let batches: Vec<_> = groups
            .into_par_iter()
            .map(Self::pending)
            .collect::<Result<_>>()
            .context("Failed to index source directory groups")?;
        let mut pending: HashMap<String, Pending> = HashMap::new();
        for (key, batch) in batches {
            if let Some(existing) = pending.get_mut(&key) {
                existing.rows.extend(batch.rows);
            } else {
                pending.insert(key, batch);
            }
        }
        let directories = pending
            .into_par_iter()
            .map(|(key, batch)| Ok((key, Directory::new(batch, files)?)))
            .collect::<Result<_>>()
            .context("Failed to build directory lookup tables")?;
        Ok(Self { directories })
    }

    /// Hash short normalized names once while retaining their original names in entries.
    fn pending(group: Candidates<'_>) -> Result<(String, Pending)> {
        let mut rows = Vec::with_capacity(group.entries.len());
        for (local, entry) in group.entries.iter().enumerate() {
            rows.push(Slot {
                hash: Directory::hash(entry.file_name()?),
                index: group.offset + local,
            });
        }
        Ok((
            Paths::key(group.directory),
            Pending {
                path: group.directory.into(),
                rows,
            },
        ))
    }

    /// Detect file-versus-directory collisions without retaining full file path keys.
    fn has_file(&self, path: &Path, files: &[&Entry]) -> Result<bool> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Ok(false);
        };
        match self.directories.get(&Paths::key(parent)) {
            Some(directory) => Ok(directory.find(name, files)?.is_some()),
            None => Ok(false),
        }
    }

    /// Check shared ancestors in the bounded pool before retaining existing directories.
    pub(crate) fn validate(&self, state: &State, files: &[&Entry]) -> Result<()> {
        let mut paths = HashMap::new();
        for directory in self.directories.values() {
            let mut parent = Some(directory.path.as_path());
            while let Some(path) = parent {
                if paths.insert(Paths::key(path), path.to_path_buf()).is_some() {
                    break;
                }
                if self.has_file(path, files)? {
                    bail!(
                        "Destination file conflicts with another mapping's directory: {}",
                        path.display()
                    );
                }
                parent = path.parent();
            }
        }
        let existing: Vec<Option<PathBuf>> = paths
            .into_par_iter()
            .map(|(_, path)| {
                state
                    .cancellation
                    .check()
                    .context("Cancelled during destination validation")?;
                let Some(metadata) =
                    Paths::leaf(&path).context("Failed to inspect destination ancestor")?
                else {
                    return Ok(None);
                };
                if !metadata.is_dir() {
                    bail!(
                        "Destination ancestor is not a directory: {}",
                        path.display()
                    );
                }
                Ok(Some(path))
            })
            .collect::<Result<_>>()
            .context("Failed to validate destination directories")?;
        state
            .remember_parents(existing.into_iter().flatten().collect())
            .context("Failed to retain validated destination directories")
    }

    /// Reuse the collision tables to match cached target metadata to source entries.
    pub(crate) fn snapshot(
        &self,
        files: &[&Entry],
    ) -> Result<Vec<Vec<(usize, crate::filesystem::Snapshot)>>> {
        self.directories
            .par_iter()
            .map(|(_, directory)| directory.snapshot(files))
            .collect::<Result<_>>()
            .context("Failed to read destination snapshots")
    }
}

impl Directory {
    /// Reserve a half-full static table so every probe terminates at an empty slot.
    fn new(pending: Pending, files: &[&Entry]) -> Result<Self> {
        let count = pending.rows.len();
        let capacity = count
            .max(1)
            .checked_mul(2)
            .and_then(usize::checked_next_power_of_two)
            .context("Too many files for a directory index")?;
        let mut directory = Self {
            path: pending.path,
            count,
            slots: vec![
                Slot {
                    hash: 0,
                    index: usize::MAX
                };
                capacity
            ],
        };
        for slot in pending.rows {
            directory.insert(slot, files)?;
        }
        Ok(directory)
    }

    /// Hash normalized names while preserving exact comparisons after hash matches.
    fn hash(name: &OsStr) -> u64 {
        twox_hash::XxHash64::oneshot(0, Paths::name_key(name).as_bytes())
    }

    /// Reject actual duplicate names while allowing distinct names with equal hashes.
    fn insert(&mut self, slot: Slot, files: &[&Entry]) -> Result<()> {
        let mask = self.slots.len() - 1;
        let mut position = slot.hash as usize & mask;
        while self.slots[position].index != usize::MAX {
            let previous = self.slots[position];
            if previous.hash == slot.hash
                && Paths::name_key(files[previous.index].file_name()?)
                    == Paths::name_key(files[slot.index].file_name()?)
            {
                bail!(
                    "Multiple sources map to {}",
                    files[slot.index].destination().display()
                );
            }
            position = (position + 1) & mask;
        }
        self.slots[position] = slot;
        Ok(())
    }

    /// Match fingerprints and original names without per-lookup allocation.
    fn find(&self, name: &OsStr, files: &[&Entry]) -> Result<Option<usize>> {
        self.find_hash(Self::hash(name), name, files)
    }

    /// Probe collisions using full normalized names so hash collisions cannot misroute files.
    fn find_hash(&self, hash: u64, name: &OsStr, files: &[&Entry]) -> Result<Option<usize>> {
        let mask = self.slots.len() - 1;
        let mut position = hash as usize & mask;
        while self.slots[position].index != usize::MAX {
            let candidate = self.slots[position];
            if candidate.hash == hash
                && Paths::name_key(files[candidate.index].file_name()?) == Paths::name_key(name)
            {
                return Ok(Some(candidate.index));
            }
            position = (position + 1) & mask;
        }
        Ok(None)
    }

    /// Capture enumeration metadata only for selected target files.
    fn snapshot(&self, files: &[&Entry]) -> Result<Vec<(usize, crate::filesystem::Snapshot)>> {
        if self.count == 0 {
            return Ok(Vec::new());
        }
        let entries = match crate::filesystem::Directory::read(&self.path) {
            Ok(entries) => entries,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error).context("Failed to enumerate destination directory"),
        };
        let mut snapshots = Vec::with_capacity(self.count);
        for entry in entries {
            let entry = entry.context("Failed to read destination entry")?;
            if let Some(index) = self.find(&entry.name, files)? {
                snapshots.push((index, entry.metadata));
            }
        }
        Ok(snapshots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    /// Forced equal fingerprints preserve distinct names and reject only exact duplicates.
    #[test]
    fn fingerprints_never_replace_exact_name_checks() -> Result<()> {
        let root = tempfile::tempdir().context("Failed to create collision fixture")?;
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::write(&first, b"a").context("Failed to write first collision input")?;
        fs::write(&second, b"b").context("Failed to write second collision input")?;
        let a = Entry::known(
            first.clone(),
            first.clone(),
            0,
            fs::metadata(&first).context("Failed to inspect first input")?,
            None,
        );
        let b = Entry::known(
            second.clone(),
            second.clone(),
            0,
            fs::metadata(&second).context("Failed to inspect second input")?,
            None,
        );
        let files = [&a, &b];
        let mut directory = Directory::new(
            Pending {
                path: root.path().into(),
                rows: vec![Slot { hash: 0, index: 0 }, Slot { hash: 0, index: 1 }],
            },
            &files,
        )?;
        assert_eq!(
            directory.find_hash(0, OsStr::new("first"), &files)?,
            Some(0)
        );
        assert_eq!(
            directory.find_hash(0, OsStr::new("second"), &files)?,
            Some(1)
        );
        assert_eq!(directory.find_hash(0, OsStr::new("missing"), &files)?, None);
        assert!(
            directory
                .insert(Slot { hash: 0, index: 0 }, &files)
                .is_err()
        );
        Ok(())
    }
}
