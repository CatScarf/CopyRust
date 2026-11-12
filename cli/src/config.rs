use super::Cli;
use anyhow::{Context, Result, bail};
use copyrust::{Compare, CopyJob, CopyOptions, HashAlgorithm, RunOptions};
use serde::Deserialize;
use std::{fs, path::PathBuf};

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Configuration {
    defaults: CopyOptions,
    run: RunOptions,
    jobs: Vec<Job>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    source: PathBuf,
    destination: PathBuf,
    compare: Option<Compare>,
    hash_algorithm: Option<HashAlgorithm>,
    recursive: Option<bool>,
    preserve_mtime: Option<bool>,
    purge: Option<bool>,
    case_sensitive: Option<bool>,
    #[serde(default)]
    exclude_dirs: Vec<String>,
    #[serde(default)]
    exclude_files: Vec<String>,
    include_files: Option<Vec<String>>,
}

impl Job {
    /// Merge per-job overrides with shared defaults without losing global exclusions.
    fn resolve(self, defaults: &CopyOptions) -> CopyJob {
        let mut options = defaults.clone();
        if let Some(value) = self.compare {
            options.compare = value;
        }
        if let Some(value) = self.hash_algorithm {
            options.hash_algorithm = value;
        }
        if let Some(value) = self.recursive {
            options.recursive = value;
        }
        if let Some(value) = self.preserve_mtime {
            options.preserve_mtime = value;
        }
        if let Some(value) = self.purge {
            options.purge = value;
        }
        if let Some(value) = self.case_sensitive {
            options.case_sensitive = value;
        }
        options.exclude_dirs.extend(self.exclude_dirs);
        options.exclude_files.extend(self.exclude_files);
        if let Some(value) = self.include_files {
            options.include_files = value;
        }
        CopyJob {
            source: self.source,
            destination: self.destination,
            options,
        }
    }
}

impl Configuration {
    /// Load strict TOML and apply explicit CLI settings to every selected mapping.
    pub(super) fn load(cli: &Cli) -> Result<(Vec<CopyJob>, RunOptions)> {
        let configuration: Self = if let Some(path) = &cli.config {
            toml::from_str(&fs::read_to_string(path).context("Failed to read copy configuration")?)
                .context("Failed to parse copy configuration")?
        } else {
            Self::default()
        };
        let mut jobs: Vec<_> = configuration
            .jobs
            .into_iter()
            .map(|job| job.resolve(&configuration.defaults))
            .collect();
        for pair in cli.pair.chunks_exact(2) {
            jobs.push(CopyJob {
                source: pair[0].clone(),
                destination: pair[1].clone(),
                options: configuration.defaults.clone(),
            });
        }
        for job in &mut jobs {
            Self::overrides(&mut job.options, cli)?;
        }
        let mut options = configuration.run;
        if let Some(value) = cli.scan_workers {
            options.scan_workers = value;
        }
        if let Some(value) = cli.hash_workers {
            options.hash_workers = value;
        }
        if let Some(value) = cli.copy_workers {
            options.copy_workers = value;
        }
        if let Some(value) = cli.retries {
            options.retries = value;
        }
        options.dry_run |= cli.dry_run;
        if let Some(interval) = &cli.progress_interval {
            options.progress_interval_ms = Self::interval(interval)?;
        }
        Ok((jobs, options))
    }

    /// Give explicit command-line selection precedence over TOML options.
    fn overrides(options: &mut CopyOptions, cli: &Cli) -> Result<()> {
        if let Some(algorithm) = &cli.hash_algorithm {
            options.hash_algorithm = match algorithm.as_str() {
                "xxh3" => HashAlgorithm::Xxh3,
                "xxh64" => HashAlgorithm::Xxh64,
                "blake3" => HashAlgorithm::Blake3,
                _ => bail!("Unknown hash algorithm: {algorithm}"),
            };
        }
        if let Some(compare) = &cli.compare {
            options.compare = match compare.as_str() {
                "mtime" => Compare::Mtime,
                "hash" => Compare::Hash,
                "auto" => Compare::Auto,
                "always" => Compare::Always,
                _ => bail!("Unknown comparison mode: {compare}"),
            };
        }
        if cli.recursive {
            options.recursive = true;
        }
        if cli.no_recursive {
            options.recursive = false;
        }
        if cli.preserve_mtime {
            options.preserve_mtime = true;
        }
        if cli.no_preserve_mtime {
            options.preserve_mtime = false;
        }
        options.purge |= cli.purge;
        options.case_sensitive |= cli.case_sensitive;
        options.exclude_dirs.extend(cli.exclude_dir.clone());
        options.exclude_files.extend(cli.exclude_file.clone());
        if !cli.include_file.is_empty() {
            options.include_files.clone_from(&cli.include_file);
        }
        Ok(())
    }

    /// Parse a positive integer duration without silently guessing unsupported units.
    fn interval(text: &str) -> Result<u64> {
        let (number, multiplier) = if let Some(number) = text.strip_suffix("ms") {
            (number, 1)
        } else if let Some(number) = text.strip_suffix('s') {
            (number, 1000)
        } else {
            (text, 1)
        };
        let value: u64 = number
            .parse()
            .context("Failed to parse progress interval")?;
        value
            .checked_mul(multiplier)
            .filter(|value| *value > 0)
            .context("Progress interval must be a positive millisecond duration")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Repeated pairs retain both mappings and every source/destination spelling.
    #[test]
    fn repeated_pairs_are_one_batch() -> Result<()> {
        let cli = Cli::try_parse_from([
            "copyrust",
            "--pair",
            "a",
            "b",
            "--pair",
            "c",
            "d",
            "--compare",
            "hash",
        ])?;
        let (jobs, _) = Configuration::load(&cli)?;
        anyhow::ensure!(
            jobs.len() == 2 && jobs.iter().all(|job| job.options.compare == Compare::Hash)
        );
        anyhow::ensure!(jobs[1].destination == PathBuf::from("d"));
        Ok(())
    }

    /// Defaults, job overrides, and explicit CLI flags have predictable precedence.
    #[test]
    fn configuration_overrides_are_explicit() -> Result<()> {
        let root = tempfile::tempdir().context("Failed to create config fixture")?;
        let config = root.path().join("copy.toml");
        fs::write(&config, "[defaults]\ncompare='hash'\nexclude_dirs=['cache']\n[run]\nprogress_interval_ms=900\n[[jobs]]\nsource='a'\ndestination='b'\nrecursive=false\nexclude_dirs=['build']\n").context("Failed to write config fixture")?;
        let cli = Cli::try_parse_from([
            std::ffi::OsString::from("copyrust"),
            "--config".into(),
            config.into_os_string(),
            "--compare".into(),
            "auto".into(),
        ])?;
        let (jobs, options) = Configuration::load(&cli)?;
        anyhow::ensure!(jobs[0].options.compare == Compare::Auto && !jobs[0].options.recursive);
        anyhow::ensure!(jobs[0].options.exclude_dirs == ["cache", "build"]);
        anyhow::ensure!(options.progress_interval_ms == 900);
        Ok(())
    }
}
