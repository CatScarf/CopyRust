use crate::CopyOptions;
use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::path::Path;

pub(crate) struct Patterns {
    names: GlobSet,
    paths: GlobSet,
    empty: bool,
}

impl Patterns {
    /// Separate basename globs from source-root-relative path globs.
    fn new(patterns: &[String], sensitive: bool) -> Result<Self> {
        let mut names = GlobSetBuilder::new();
        let mut paths = GlobSetBuilder::new();
        for pattern in patterns {
            let pattern = pattern.replace('\\', "/");
            let glob = GlobBuilder::new(&pattern)
                .literal_separator(true)
                .backslash_escape(false)
                .case_insensitive(!sensitive)
                .build()
                .with_context(|| format!("Failed to compile pattern {pattern}"))?;
            if pattern.contains('/') {
                paths.add(glob);
            } else {
                names.add(glob);
            }
        }
        Ok(Self {
            names: names.build().context("Failed to compile name filters")?,
            paths: paths.build().context("Failed to compile path filters")?,
            empty: patterns.is_empty(),
        })
    }

    /// Match names anywhere and paths relative to the mapping's source root.
    pub(crate) fn matches(&self, path: &Path) -> bool {
        if self.empty {
            return false;
        }
        path.file_name()
            .is_some_and(|name| self.names.is_match(Path::new(name)))
            || self.paths.is_match(path)
    }
}

pub(crate) struct Filter {
    dirs: Patterns,
    excluded: Patterns,
    included: Patterns,
}

impl Filter {
    /// Compile each mapping's filters before walking or changing files.
    pub(crate) fn new(options: &CopyOptions) -> Result<Self> {
        Ok(Self {
            dirs: Patterns::new(&options.exclude_dirs, options.case_sensitive)?,
            excluded: Patterns::new(&options.exclude_files, options.case_sensitive)?,
            included: Patterns::new(&options.include_files, options.case_sensitive)?,
        })
    }

    /// Prune ignored directories without enumerating their descendants.
    pub(crate) fn directory(&self, relative: &Path) -> bool {
        !self.dirs.matches(relative)
    }

    /// Evaluate basename-only filters without constructing complete relative paths.
    pub(crate) fn file_name(&self, name: &std::ffi::OsStr) -> Option<bool> {
        if !self.excluded.paths.is_empty() || !self.included.paths.is_empty() {
            None
        } else {
            Some(self.file(Path::new(name)))
        }
    }

    /// Select files using exclusions followed by an optional inclusion set.
    pub(crate) fn file(&self, relative: &Path) -> bool {
        !self.excluded.matches(relative) && (self.included.empty || self.included.matches(relative))
    }
}
