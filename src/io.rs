use crate::{Compare, CopyOptions, HashAlgorithm};
use crate::{hashing, plan, state};
use anyhow::{Context, Result, bail};
use std::fs::{self, File, Metadata};
use std::io::Read;
#[cfg(not(windows))]
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;

pub(crate) struct Paths;

impl Paths {
    /// Inspect a leaf without following links after directory preflight has completed.
    pub(crate) fn leaf(path: &Path) -> Result<Option<Metadata>> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if Self::is_link(&metadata) => {
                bail!("Link traversal is disabled: {}", path.display())
            }
            Ok(metadata) => Ok(Some(metadata)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("Failed to inspect file {}", path.display()))
            }
        }
    }

    /// Atomically replace existing files and restore read-only flags after a failed move.
    pub(crate) fn replace(temporary: tempfile::TempPath, destination: &Path) -> Result<()> {
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
    /// Read a complete digest while checking cancellation and source stability.
    fn digest(
        path: &Path,
        before: &Metadata,
        algorithm: HashAlgorithm,
        state: &state::State,
    ) -> Result<hashing::Digest> {
        let mut file = File::open(path)
            .with_context(|| format!("Failed to open hash input {}", path.display()))?;
        let mut hash = hashing::ContentHash::new(algorithm);
        let mut buffer = vec![0u8; before.len().clamp(1, 512 * 1024) as usize];
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
        Self::unchanged(path, before).context("Hash input changed during comparison")?;
        Ok(hash.finish())
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
    pub(crate) fn needs_copy(
        entry: &plan::Entry,
        options: &CopyOptions,
        state: &state::State,
    ) -> Result<bool> {
        state
            .cancellation
            .check()
            .context("Failed to continue comparison")?;
        let source = &entry.source_metadata;
        let Some(destination) = &entry.destination_metadata else {
            return Ok(true);
        };
        if Paths::is_link(destination) {
            bail!(
                "Destination link traversal is disabled: {}",
                entry.destination.display()
            );
        }
        if !destination.is_file() {
            bail!("Destination is not a file: {}", entry.destination.display());
        }
        if options.compare == Compare::Always {
            if same_file::is_same_file(&entry.source, &entry.destination)
                .context("Failed to compare file identities")?
            {
                state.update(|p| p.skipped_same_file += 1)?;
                return Ok(false);
            }
            return Ok(true);
        }
        if source.len() != destination.len() {
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
        if Self::digest(&entry.source, source, options.hash_algorithm, state)?
            == Self::digest(
                &entry.destination,
                destination,
                options.hash_algorithm,
                state,
            )?
        {
            state.update(|p| p.skipped_hash += 1)?;
            return Ok(false);
        }
        Ok(true)
    }

    /// Copy directly by default or stage each file before atomic replacement.
    pub(crate) fn copy(
        entry: &plan::Entry,
        options: &CopyOptions,
        state: &state::State,
    ) -> Result<()> {
        state.parent(
            entry.source.parent().context("Source has no parent")?,
            false,
        )?;
        let before = Paths::leaf(&entry.source)?.context("Source disappeared before copying")?;
        let parent = entry
            .destination
            .parent()
            .context("Destination has no parent")?;
        state.parent(parent, true)?;
        Paths::leaf(&entry.destination)?;
        state
            .cancellation
            .check()
            .context("Failed to begin file transfer")?;
        let temporary = if options.atomic {
            Some(
                tempfile::NamedTempFile::new_in(parent)
                    .context("Failed to create temporary destination")?
                    .into_temp_path(),
            )
        } else {
            None
        };
        let output = temporary.as_deref().unwrap_or(&entry.destination);
        #[cfg(unix)]
        if !options.atomic {
            use std::os::unix::fs::PermissionsExt;
            if let Some(metadata) = &entry.destination_metadata {
                if metadata.permissions().readonly() {
                    let mode = metadata.permissions().mode() | 0o200;
                    fs::set_permissions(output, fs::Permissions::from_mode(mode))
                        .context("Failed to make direct destination writable")?;
                }
            }
        }
        #[cfg(windows)]
        if !options.atomic {
            crate::windows::NativeCopy::make_writable(output)?;
        }
        Self::transfer(&entry.source, output, before.len(), state)
            .context("Failed to transfer file contents")?;
        Self::finish(entry, options, &before, temporary)
    }

    /// Validate completed transfers, preserve metadata, and commit optional staging files.
    fn finish(
        entry: &plan::Entry,
        options: &CopyOptions,
        before: &Metadata,
        temporary: Option<tempfile::TempPath>,
    ) -> Result<()> {
        let output = temporary.as_deref().unwrap_or(&entry.destination);
        Self::unchanged(&entry.source, before).context("Failed to validate copied source")?;
        #[cfg(not(windows))]
        if options.preserve_mtime {
            filetime::set_file_mtime(
                output,
                filetime::FileTime::from_last_modification_time(before),
            )
            .context("Failed to preserve modification time")?;
        }
        #[cfg(windows)]
        if !options.preserve_mtime {
            filetime::set_file_mtime(output, filetime::FileTime::now())
                .context("Failed to set copied file write time")?;
        }
        Paths::leaf(&entry.destination)?;
        if let Some(temporary) = temporary {
            Paths::replace(temporary, &entry.destination)?;
        }
        #[cfg(windows)]
        if before.permissions().readonly() {
            fs::set_permissions(&entry.destination, before.permissions())
                .context("Failed to preserve source read-only attribute")?;
        }
        #[cfg(not(windows))]
        fs::set_permissions(&entry.destination, before.permissions())
            .context("Failed to preserve destination permissions")?;
        Ok(())
    }

    /// Delegate Windows transfers to the native progress-capable copy engine.
    #[cfg(windows)]
    fn transfer(source: &Path, destination: &Path, _size: u64, state: &state::State) -> Result<()> {
        crate::windows::NativeCopy::run(source, destination, state)
    }

    /// Stream portable transfers with buffers proportional to small-file sizes.
    #[cfg(not(windows))]
    fn transfer(source: &Path, destination: &Path, size: u64, state: &state::State) -> Result<()> {
        let mut input = File::open(source).context("Failed to open copy source")?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(destination)
            .context("Failed to open copy destination")?;
        let mut buffer = vec![0u8; size.clamp(1, 512 * 1024) as usize];
        loop {
            state
                .cancellation
                .check()
                .context("Failed to continue portable transfer")?;
            let count = input
                .read(&mut buffer)
                .context("Failed to read portable copy source")?;
            if count == 0 {
                break;
            }
            output
                .write_all(&buffer[..count])
                .context("Failed to write portable copy destination")?;
            state.copied.fetch_add(count as u64, Ordering::Relaxed);
        }
        output
            .flush()
            .context("Failed to flush portable copy destination")
    }
}
