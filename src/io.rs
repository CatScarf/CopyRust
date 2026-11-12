use crate::{Compare, CopyOptions, plan::Entry, state::State};
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File, Metadata},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::Ordering,
};

pub(crate) struct Paths;

impl Paths {
    /// Atomically replace existing files and restore read-only flags after a failed move.
    pub(crate) fn replace(temporary: tempfile::NamedTempFile, destination: &Path) -> Result<()> {
        #[cfg(windows)]
        let previous = match fs::metadata(destination) {
            Ok(metadata) if metadata.permissions().readonly() => {
                let original = metadata.permissions();
                let mut writable = original.clone();
                #[allow(
                    clippy::permissions_set_readonly_false,
                    reason = "This branch only clears the Windows read-only attribute."
                )]
                writable.set_readonly(false);
                fs::set_permissions(destination, writable)
                    .context("Failed to make destination writable")?;
                Some(original)
            }
            Ok(_) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("Failed to inspect destination permissions"),
        };
        let result = temporary.persist(destination);
        #[cfg(windows)]
        if result.is_err() {
            if let Some(original) = previous {
                fs::set_permissions(destination, original).context(
                    "Failed to restore destination permissions after failed replacement",
                )?;
            }
        }
        result
            .map(|_| ())
            .map_err(|error| error.error)
            .context("Failed to replace destination atomically")
    }

    /// Produce filesystem comparison keys without changing actual path spellings.
    pub(crate) fn key(path: &Path) -> String {
        let value = path.to_string_lossy().replace('\\', "/");
        if cfg!(windows) {
            value.to_lowercase()
        } else {
            value
        }
    }

    /// Compare complete path components rather than textual prefixes.
    pub(crate) fn within(path: &Path, root: &Path) -> bool {
        Path::new(&Self::key(path)).starts_with(Self::key(root))
    }

    /// Recognize symlinks and Windows junctions without following them.
    pub(crate) fn is_link(metadata: &Metadata) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & 0x400 != 0
        }
        #[cfg(not(windows))]
        {
            metadata.file_type().is_symlink()
        }
    }

    /// Resolve the nearest existing ancestor and append validated missing components.
    pub(crate) fn resolve(path: &Path) -> Result<PathBuf> {
        let absolute = std::path::absolute(path).context("Failed to make path absolute")?;
        let mut normalized = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::ParentDir => {
                    normalized.pop();
                }
                Component::CurDir => {}
                _ => normalized.push(component),
            }
        }
        match fs::symlink_metadata(&normalized) {
            Ok(metadata) if Self::is_link(&metadata) => bail!(
                "Destination root must not be a link: {}",
                normalized.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Failed to inspect destination root"),
        }
        let mut ancestor = normalized.clone();
        let mut missing = Vec::new();
        while !ancestor
            .try_exists()
            .context("Failed to inspect path ancestor")?
        {
            missing.push(
                ancestor
                    .file_name()
                    .context("Failed to find existing path ancestor")?
                    .to_owned(),
            );
            if !ancestor.pop() {
                bail!("Path has no existing ancestor: {}", path.display());
            }
        }
        let mut result =
            fs::canonicalize(ancestor).context("Failed to canonicalize path ancestor")?;
        for component in missing.into_iter().rev() {
            result.push(component);
        }
        Ok(result)
    }

    /// Reject link traversal along every existing destination component.
    pub(crate) fn guard(path: &Path) -> Result<()> {
        let absolute = std::path::absolute(path).context("Failed to inspect absolute path")?;
        let mut current = PathBuf::new();
        for component in absolute.components() {
            current.push(component);
            if matches!(component, Component::Prefix(_)) || !current.has_root() {
                continue;
            }
            match fs::symlink_metadata(&current) {
                Ok(metadata) if Self::is_link(&metadata) => {
                    bail!("Link traversal is disabled: {}", current.display())
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to inspect {}", current.display()));
                }
            }
        }
        Ok(())
    }
}

pub(crate) struct Content;

impl Content {
    /// Read a complete BLAKE3 digest while checking cancellation and source stability.
    fn digest(path: &Path, state: &State) -> Result<blake3::Hash> {
        let before = fs::metadata(path).context("Failed to inspect hash input")?;
        let mut file = File::open(path)
            .with_context(|| format!("Failed to open hash input {}", path.display()))?;
        let mut hash = blake3::Hasher::new();
        let mut buffer = vec![0u8; 512 * 1024];
        loop {
            state
                .cancellation
                .check()
                .context("Failed to continue hashing")?;
            let count = file
                .read(&mut buffer)
                .context("Failed to read hash input")?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            state.hashed.fetch_add(count as u64, Ordering::Relaxed);
        }
        Self::unchanged(path, &before).context("Hash input changed during comparison")?;
        Ok(hash.finalize())
    }

    /// Reject observed mutations instead of silently accepting a partial file.
    fn unchanged(path: &Path, before: &Metadata) -> Result<()> {
        let after = fs::metadata(path).context("Failed to recheck file metadata")?;
        if before.len() != after.len()
            || before.modified().context("Failed to read initial mtime")?
                != after.modified().context("Failed to read final mtime")?
        {
            bail!("File changed during operation: {}", path.display());
        }
        Ok(())
    }

    /// Classify files using size, optional timestamp shortcuts, and strict full digests.
    pub(crate) fn needs_copy(entry: &Entry, options: &CopyOptions, state: &State) -> Result<bool> {
        state
            .cancellation
            .check()
            .context("Failed to continue comparison")?;
        let source = fs::metadata(&entry.source).context("Failed to inspect copy source")?;
        let destination = match fs::symlink_metadata(&entry.destination) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error).context("Failed to inspect copy destination"),
        };
        if Paths::is_link(&destination) {
            bail!(
                "Destination link traversal is disabled: {}",
                entry.destination.display()
            );
        }
        if !destination.is_file() {
            bail!("Destination is not a file: {}", entry.destination.display());
        }
        if same_file::is_same_file(&entry.source, &entry.destination)
            .context("Failed to compare file identities")?
        {
            state.update(|p| p.skipped_same_file += 1)?;
            return Ok(false);
        }
        if options.compare == Compare::Always || source.len() != destination.len() {
            return Ok(true);
        }
        let same_time = source.modified().context("Failed to read source mtime")?
            == destination
                .modified()
                .context("Failed to read destination mtime")?;
        if matches!(options.compare, Compare::Mtime | Compare::Auto) && same_time {
            state.update(|p| p.skipped_mtime += 1)?;
            return Ok(false);
        }
        if options.compare == Compare::Mtime {
            return Ok(true);
        }
        if Self::digest(&entry.source, state)? == Self::digest(&entry.destination, state)? {
            state.update(|p| p.skipped_hash += 1)?;
            return Ok(false);
        }
        Ok(true)
    }

    /// Copy into a sibling temporary file and replace the destination only after success.
    pub(crate) fn copy(entry: &Entry, options: &CopyOptions, state: &State) -> Result<()> {
        Paths::guard(&entry.destination)?;
        Paths::guard(&entry.source)?;
        let before = fs::metadata(&entry.source).context("Failed to inspect copy input")?;
        let parent = entry
            .destination
            .parent()
            .context("Destination has no parent")?;
        fs::create_dir_all(parent).context("Failed to create destination parent")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .context("Failed to create temporary destination")?;
        let mut source = File::open(&entry.source).context("Failed to open copy source")?;
        let mut buffer = vec![0u8; 512 * 1024];
        loop {
            state
                .cancellation
                .check()
                .context("Failed to continue copy")?;
            let count = source
                .read(&mut buffer)
                .context("Failed to read copy source")?;
            if count == 0 {
                break;
            }
            temporary
                .write_all(&buffer[..count])
                .context("Failed to write temporary destination")?;
            state.copied.fetch_add(count as u64, Ordering::Relaxed);
        }
        temporary
            .flush()
            .context("Failed to flush temporary destination")?;
        Self::unchanged(&entry.source, &before).context("Failed to validate copied source")?;
        if options.preserve_mtime {
            filetime::set_file_mtime(
                temporary.path(),
                filetime::FileTime::from_last_modification_time(&before),
            )
            .context("Failed to preserve modification time")?;
        }
        Paths::guard(&entry.destination)?;
        Paths::replace(temporary, &entry.destination)?;
        fs::set_permissions(&entry.destination, before.permissions())
            .context("Failed to preserve destination permissions")?;
        Ok(())
    }
}
