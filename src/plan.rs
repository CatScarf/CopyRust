use crate::{CopyJob, filter::Filter, io::Paths, state::State};
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

pub(crate) struct Entry {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub job: usize,
}

pub(crate) struct Removal {
    pub path: PathBuf,
    pub directory: bool,
}

struct Tree {
    files: Vec<Entry>,
    present: HashSet<String>,
    links: Vec<PathBuf>,
}

pub(crate) struct Plan {
    pub jobs: Vec<CopyJob>,
    pub files: Vec<Entry>,
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
        let mut destinations = HashSet::new();
        for tree in &trees {
            for entry in &tree.files {
                if !destinations.insert(Paths::key(&entry.destination)) {
                    bail!("Multiple sources map to {}", entry.destination.display());
                }
            }
        }
        Self::validate_parents(&trees, &destinations)?;
        let mut removals = Vec::new();
        for (index, tree) in trees.iter().enumerate() {
            removals.extend(
                Self::purge(&jobs, index, tree, state)
                    .context("Failed to plan destination cleanup")?,
            );
        }
        removals.sort_by_key(|entry| std::cmp::Reverse(entry.path.components().count()));
        let files = trees.into_iter().flat_map(|tree| tree.files).collect();
        Ok(Self {
            jobs,
            files,
            removals,
        })
    }

    /// Check shared destination ancestors once before any comparison or write.
    fn validate_parents(trees: &[Tree], destinations: &HashSet<String>) -> Result<()> {
        let mut inspected = HashSet::new();
        for entry in trees.iter().flat_map(|tree| &tree.files) {
            let mut parent = entry.destination.parent();
            while let Some(path) = parent {
                let key = Paths::key(path);
                if destinations.contains(&key) {
                    bail!(
                        "Destination file conflicts with another mapping's directory: {}",
                        path.display()
                    );
                }
                if !inspected.insert(key) {
                    break;
                }
                Paths::guard(path).context("Failed to inspect destination ancestor")?;
                if path
                    .try_exists()
                    .context("Failed to inspect destination ancestor existence")?
                    && !path.is_dir()
                {
                    bail!(
                        "Destination ancestor is not a directory: {}",
                        path.display()
                    );
                }
                parent = path.parent();
            }
        }
        Ok(())
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

    /// Walk one source with directory pruning and stable source-relative filters.
    fn scan(job: &CopyJob, index: usize, state: &State) -> Result<Tree> {
        let filter = Filter::new(&job.options).context("Failed to build source filters")?;
        let mut tree = Tree {
            files: Vec::new(),
            present: HashSet::new(),
            links: Vec::new(),
        };
        let depth = if job.options.recursive { usize::MAX } else { 1 };
        let entries = WalkDir::new(&job.source)
            .follow_links(false)
            .max_depth(depth)
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !entry.file_type().is_dir()
                    || entry
                        .path()
                        .strip_prefix(&job.source)
                        .is_ok_and(|relative| filter.directory(relative))
            });
        for entry in entries {
            state
                .cancellation
                .check()
                .context("Failed to continue source scan")?;
            let entry = entry.context("Failed to enumerate source entry")?;
            let relative = entry
                .path()
                .strip_prefix(&job.source)
                .context("Failed to resolve relative source path")?;
            let metadata =
                fs::symlink_metadata(entry.path()).context("Failed to inspect source entry")?;
            if Paths::is_link(&metadata) {
                tree.links.push(relative.to_path_buf());
                state.update(|p| p.skipped_links += 1)?;
            } else if metadata.is_dir() || (metadata.is_file() && filter.file(relative)) {
                tree.present.insert(Paths::key(relative));
                if metadata.is_file() {
                    tree.files.push(Entry {
                        source: entry.path().into(),
                        destination: job.destination.join(relative),
                        job: index,
                    });
                    state.update(|p| p.scanned_files += 1)?;
                }
            }
        }
        Ok(tree)
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
            let metadata =
                fs::symlink_metadata(entry.path()).context("Failed to inspect cleanup entry")?;
            if Paths::is_link(&metadata) || tree.present.contains(&Paths::key(relative)) {
                continue;
            }
            if metadata.is_dir() || (metadata.is_file() && filter.file(relative)) {
                result.push(Removal {
                    path: entry.path().into(),
                    directory: metadata.is_dir(),
                });
            }
        }
        Ok(result)
    }
}
