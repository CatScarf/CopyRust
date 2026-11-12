use crate::{CopyJob, filter::Filter, index, io::Paths, state::State};
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::sync::Arc;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

pub(crate) use crate::entry::Entry;

pub(crate) struct Removal {
    pub path: PathBuf,
    pub directory: bool,
}

struct Group {
    destination: PathBuf,
    files: std::ops::Range<usize>,
}

struct Tree {
    groups: Vec<Group>,
    files: Vec<Entry>,
    directories: Vec<PathBuf>,
    present: HashSet<String>,
    links: Vec<PathBuf>,
}

pub(crate) struct Plan {
    pub jobs: Vec<CopyJob>,
    pub files: Vec<Entry>,
    pub directories: Vec<PathBuf>,
    pub removals: Vec<Removal>,
}

impl Plan {
    /// Resolve mappings, reject dangerous relationships, and enumerate without mutations.
    pub(crate) fn build(jobs: &[CopyJob], workers: usize, state: &State) -> Result<Self> {
        let jobs = Self::normalize(jobs).context("Failed to validate copy mappings")?;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .context("Failed to start scanning workers")?;

        let trees: Vec<Tree> = pool.install(|| {
            jobs.par_iter()
                .enumerate()
                .map(|(index, job)| Self::scan(job, index, state))
                .collect::<Result<_>>()
        })?;

        let references: Vec<_> = trees.iter().flat_map(|tree| tree.files.iter()).collect();
        let index = pool.install(|| Self::index(&trees, &references))?;
        pool.install(|| index.validate(state, &references))?;
        let mut removals = Vec::new();
        for (index, tree) in trees.iter().enumerate() {
            removals.extend(
                Self::purge(&jobs, index, tree, state)
                    .context("Failed to plan destination cleanup")?,
            );
        }
        removals.sort_by_key(|entry| std::cmp::Reverse(entry.path.components().count()));
        let mut directories: Vec<_> = trees
            .iter()
            .flat_map(|tree| tree.directories.clone())
            .collect();
        directories.sort();
        directories.dedup();
        let snapshots = pool
            .install(|| index.snapshot(&references))
            .context("Failed to snapshot destination metadata")?;
        drop(references);
        let mut files: Vec<_> = trees.into_iter().flat_map(|tree| tree.files).collect();
        for (index, metadata) in snapshots.into_iter().flatten() {
            files[index].destination_metadata = Some(metadata);
        }
        Ok(Self {
            jobs,
            files,
            directories,
            removals,
        })
    }

    /// Retain directory group ranges while assigning globally stable file indexes.
    fn index(trees: &[Tree], files: &[&Entry]) -> Result<index::DirectoryIndex> {
        let mut groups = Vec::new();
        let mut offset = 0;
        for tree in trees {
            for group in &tree.groups {
                groups.push(index::Candidates {
                    directory: &group.destination,
                    offset: offset + group.files.start,
                    entries: &tree.files[group.files.clone()],
                });
            }
            offset += tree.files.len();
        }
        index::DirectoryIndex::build(groups, files).context("Failed to build destination index")
    }

    /// Canonicalize existing parents and protect all sources from destination writes.
    fn normalize(jobs: &[CopyJob]) -> Result<Vec<CopyJob>> {
        if jobs.is_empty() {
            bail!("At least one source/destination mapping is required");
        }
        let mut result = Vec::new();
        for job in jobs {
            let metadata =
                fs::symlink_metadata(&job.source).context("Failed to inspect source root")?;
            if Paths::is_link(&metadata) {
                bail!("Source root must not be a link: {}", job.source.display());
            }
            let mut job = job.clone();
            job.source = fs::canonicalize(&job.source)
                .with_context(|| format!("Failed to resolve source {}", job.source.display()))?;
            Paths::guard(&job.source).context("Failed to validate resolved source path")?;
            if !job.source.is_dir() {
                bail!("Source is not a directory: {}", job.source.display());
            }
            job.destination =
                Paths::resolve(&job.destination).context("Failed to resolve destination")?;
            if job
                .destination
                .try_exists()
                .context("Failed to inspect destination root")?
                && !job.destination.is_dir()
            {
                bail!(
                    "Destination root is not a directory: {}",
                    job.destination.display()
                );
            }
            Paths::guard(&job.destination).context("Failed to validate destination path")?;
            result.push(job);
        }
        for job in &result {
            for other in &result {
                if Paths::within(&job.destination, &other.source)
                    || Paths::within(&other.source, &job.destination)
                {
                    bail!(
                        "Source and destination trees overlap: {} and {}",
                        other.source.display(),
                        job.destination.display()
                    );
                }
            }
        }
        Ok(result)
    }

    /// Scan each directory frontier in the shared bounded pool without following links.
    fn scan(job: &CopyJob, index: usize, state: &State) -> Result<Tree> {
        let filter = Filter::new(&job.options).context("Failed to build source filters")?;
        let mut tree = Tree {
            groups: Vec::new(),
            files: Vec::new(),
            directories: vec![job.destination.clone()],
            present: HashSet::from([String::new()]),
            links: Vec::new(),
        };
        let mut pending = vec![job.source.clone()];
        while !pending.is_empty() {
            let results: Vec<_> = pending
                .par_iter()
                .map(|path| Self::scan_directory(job, index, path, &filter, state))
                .collect::<Result<_>>()
                .context("Failed to scan source directory frontier")?;
            let mut next = Vec::new();
            for ((part, children), directory) in results.into_iter().zip(&pending) {
                let relative = directory
                    .strip_prefix(&job.source)
                    .context("Failed to resolve group directory")?;
                let destination = if relative.as_os_str().is_empty() {
                    job.destination.clone()
                } else {
                    job.destination.join(relative)
                };
                let start = tree.files.len();
                tree.groups.push(Group {
                    destination,
                    files: start..start + part.files.len(),
                });
                tree.files.extend(part.files);
                tree.directories.extend(part.directories);
                tree.present.extend(part.present);
                tree.links.extend(part.links);
                next.extend(children);
            }
            pending = next;
        }
        Ok(tree)
    }

    /// Merge locally processed entries after each directory has been enumerated.
    fn scan_directory(
        job: &CopyJob,
        index: usize,
        directory: &Path,
        filter: &Filter,
        state: &State,
    ) -> Result<(Tree, Vec<PathBuf>)> {
        let mut tree = Tree {
            groups: Vec::new(),
            files: Vec::new(),
            directories: Vec::new(),
            present: HashSet::new(),
            links: Vec::new(),
        };
        let mut scanner = Scanner::new(job, index, directory, filter, state)?;
        let mut children = Vec::new();
        for entry in crate::filesystem::Directory::read(directory)
            .with_context(|| format!("Failed to open source directory {}", directory.display()))?
        {
            state
                .cancellation
                .check()
                .context("Failed to continue source scan")?;
            let entry = entry.context("Failed to enumerate source entry")?;
            if let Some(child) = scanner.entry(entry, &mut tree)? {
                children.push(child);
            }
        }
        Ok((tree, children))
    }

    /// Plan only selected extras while protecting ignored paths and nested mappings.
    fn purge(jobs: &[CopyJob], index: usize, tree: &Tree, state: &State) -> Result<Vec<Removal>> {
        let job = &jobs[index];
        if !job.options.purge || !job.destination.exists() {
            return Ok(Vec::new());
        }
        let filter = Filter::new(&job.options).context("Failed to build purge filters")?;
        let protected: Vec<&Path> = jobs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .map(|(_, other)| other.destination.as_path())
            .collect();
        let depth = if job.options.recursive { usize::MAX } else { 1 };
        let entries = WalkDir::new(&job.destination)
            .follow_links(false)
            .max_depth(depth)
            .into_iter()
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                let Ok(relative) = entry.path().strip_prefix(&job.destination) else {
                    return false;
                };
                !protected
                    .iter()
                    .any(|root| Paths::within(entry.path(), root))
                    && !tree.links.iter().any(|link| relative.starts_with(link))
                    && (!entry.file_type().is_dir() || filter.directory(relative))
            });
        let mut result = Vec::new();
        for entry in entries {
            state
                .cancellation
                .check()
                .context("Failed to continue cleanup scan")?;
            let entry = entry.context("Failed to enumerate cleanup entry")?;
            if entry.depth() == 0 {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&job.destination)
                .context("Failed to resolve cleanup path")?;
            let metadata = entry
                .metadata()
                .context("Failed to inspect cleanup entry")?;
            if Paths::is_link(&metadata) || tree.present.contains(&Paths::key(relative)) {
                continue;
            }
            if (metadata.is_dir() && job.options.recursive)
                || (metadata.is_file() && filter.file(relative))
            {
                result.push(Removal {
                    path: entry.path().into(),
                    directory: metadata.is_dir(),
                });
            }
        }
        Ok(result)
    }
}

struct Scanner<'a> {
    job: &'a CopyJob,
    index: usize,
    filter: &'a Filter,
    state: &'a State,
    directory: Arc<crate::entry::Directory>,
    scanned: crate::state::ScanCounter<'a>,
}

impl<'a> Scanner<'a> {
    /// Share immutable directory prefixes and retain a local progress counter.
    fn new(
        job: &'a CopyJob,
        index: usize,
        directory: &Path,
        filter: &'a Filter,
        state: &'a State,
    ) -> Result<Self> {
        let relative = directory
            .strip_prefix(&job.source)
            .context("Failed to resolve source directory")?;
        let destination = if relative.as_os_str().is_empty() {
            job.destination.clone()
        } else {
            job.destination.join(relative)
        };
        Ok(Self {
            job,
            index,
            filter,
            state,
            directory: Arc::new(crate::entry::Directory {
                source: directory.into(),
                destination,
            }),
            scanned: state.scan_counter(),
        })
    }
    /// Classify cached metadata without following reparse points or ignored directories.
    fn entry(
        &mut self,
        entry: crate::filesystem::Entry,
        tree: &mut Tree,
    ) -> Result<Option<PathBuf>> {
        let crate::filesystem::Entry { name, metadata } = entry;
        if metadata.is_file() && !metadata.is_link() && !self.job.options.purge {
            if let Some(selected) = self.filter.file_name(&name) {
                if selected {
                    tree.files.push(Entry::deferred(
                        Arc::clone(&self.directory),
                        name,
                        self.index,
                        metadata,
                    ));
                    self.scanned.increment();
                }
                return Ok(None);
            }
        }
        self.relative_entry(self.directory.source.join(&name), metadata, tree)
    }

    /// Resolve relative names only when filters or purge require full paths.
    fn relative_entry(
        &mut self,
        path: PathBuf,
        metadata: crate::filesystem::Snapshot,
        tree: &mut Tree,
    ) -> Result<Option<PathBuf>> {
        let relative = path
            .strip_prefix(&self.job.source)
            .context("Failed to resolve relative source path")?;
        if metadata.is_link() {
            tree.links.push(relative.to_path_buf());
            self.state.update(|p| p.skipped_links += 1)?;
        } else if metadata.is_dir() {
            if !self.filter.directory(relative) {
                return Ok(None);
            }
            if self.job.options.purge {
                tree.present.insert(Paths::key(relative));
            }
            if self.job.options.recursive {
                tree.directories.push(self.job.destination.join(relative));
                return Ok(Some(path));
            }
        } else if metadata.is_file() && self.filter.file(relative) {
            if self.job.options.purge {
                tree.present.insert(Paths::key(relative));
            }
            let destination = self.job.destination.join(relative);
            tree.files
                .push(Entry::known(path, destination, self.index, metadata, None));
            self.scanned.increment();
        }
        Ok(None)
    }
}
