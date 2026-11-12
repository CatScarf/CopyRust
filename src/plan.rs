use crate::{CopyJob, filter::Filter, io::Paths, state::State};
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

pub(crate) struct Entry {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub job: usize,
    pub source_metadata: fs::Metadata,
    pub destination_metadata: Option<fs::Metadata>,
}

pub(crate) struct Removal {
    pub path: PathBuf,
    pub directory: bool,
}

struct Tree {
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
            .num_threads(workers.min(jobs.len()).max(1))
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
        let mut directories: Vec<_> = trees
            .iter()
            .flat_map(|tree| tree.directories.clone())
            .collect();
        directories.sort();
        directories.dedup();
        let mut files: Vec<_> = trees.into_iter().flat_map(|tree| tree.files).collect();
        Self::destination_snapshots(&mut files)
            .context("Failed to snapshot destination metadata")?;
        Ok(Self {
            jobs,
            files,
            directories,
            removals,
        })
    }

    /// Reuse directory enumeration metadata instead of issuing one stat per candidate.
    fn destination_snapshots(files: &mut [Entry]) -> Result<()> {
        let mut directories: HashMap<PathBuf, HashMap<String, usize>> = HashMap::new();
        for (index, entry) in files.iter().enumerate() {
            let parent = entry
                .destination
                .parent()
                .context("Destination has no parent")?;
            let name = entry
                .destination
                .file_name()
                .context("Destination has no filename")?;
            directories
                .entry(parent.into())
                .or_default()
                .insert(Paths::key(Path::new(name)), index);
        }
        for (directory, candidates) in directories {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).context("Failed to enumerate destination directory");
                }
            };
            for entry in entries {
                let entry = entry.context("Failed to read destination entry")?;
                if let Some(index) = candidates.get(&Paths::key(Path::new(&entry.file_name()))) {
                    files[*index].destination_metadata = Some(
                        entry
                            .metadata()
                            .context("Failed to read destination entry metadata")?,
                    );
                }
            }
        }
        Ok(())
    }

    /// Check shared destination ancestors once before any comparison or write.
    fn validate_parents(trees: &[Tree], destinations: &HashSet<String>) -> Result<()> {
        let mut inspected = HashSet::new();
        let paths = trees.iter().flat_map(|tree| {
            tree.files
                .iter()
                .filter_map(|entry| entry.destination.parent())
                .chain(tree.directories.iter().map(PathBuf::as_path))
        });
        for path in paths {
            let mut parent = Some(path);
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

    /// Walk one source with directory pruning and stable source-relative filters.
    fn scan(job: &CopyJob, index: usize, state: &State) -> Result<Tree> {
        let filter = Filter::new(&job.options).context("Failed to build source filters")?;
        let mut tree = Tree {
            files: Vec::new(),
            directories: Vec::new(),
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
            let metadata = entry.metadata().context("Failed to inspect source entry")?;
            if Paths::is_link(&metadata) {
                tree.links.push(relative.to_path_buf());
                state.update(|p| p.skipped_links += 1)?;
            } else if metadata.is_dir() || (metadata.is_file() && filter.file(relative)) {
                tree.present.insert(Paths::key(relative));
                if metadata.is_dir() && (job.options.recursive || entry.depth() == 0) {
                    tree.directories.push(job.destination.join(relative));
                }
                if metadata.is_file() {
                    tree.files.push(Entry {
                        source: entry.path().into(),
                        destination: job.destination.join(relative),
                        job: index,
                        source_metadata: metadata,
                        destination_metadata: None,
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
