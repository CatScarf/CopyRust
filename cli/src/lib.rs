//! Command-line frontend shared by the standalone binary and Python console entry point.

mod config;

use anyhow::{Context, Result};
use clap::Parser;
use copyrust::{Cancellation, CopyEngine, Progress};
use std::{
    ffi::OsString,
    io::{IsTerminal, Write},
    path::PathBuf,
};

/// Copy multiple directory trees using timestamps or complete content hashes.
#[derive(Parser, Debug)]
#[command(name = "copyrust", version, about, arg_required_else_help = true)]
pub struct Cli {
    /// Map source directory contents into a destination; repeat for batch copying.
    #[arg(long, num_args = 2, value_names = ["SOURCE", "DESTINATION"], action = clap::ArgAction::Append)]
    pair: Vec<PathBuf>,
    /// Load shared defaults and per-mapping overrides from strict TOML.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Select timestamp, strict hash, metadata-then-hash, or unconditional copying.
    #[arg(long, value_parser = ["mtime", "hash", "auto", "always"])]
    compare: Option<String>,
    /// Select XXH3-64 (default), traditional XXH64, or BLAKE3 for content comparison.
    #[arg(long, value_parser = ["xxh3", "xxh64", "blake3"])]
    hash_algorithm: Option<String>,
    /// Prune matching directory names or source-relative globs; repeat as needed.
    #[arg(long)]
    exclude_dir: Vec<String>,
    /// Protect matching files from both copying and purge; repeat as needed.
    #[arg(long)]
    exclude_file: Vec<String>,
    /// Select matching files only; patterns are relative to each source root.
    #[arg(long)]
    include_file: Vec<String>,
    /// Recursively copy directories (the default).
    #[arg(long, conflicts_with = "no_recursive")]
    recursive: bool,
    /// Limit each mapping to files directly under its source root.
    #[arg(long)]
    no_recursive: bool,
    /// Preserve modification times after copying (the default).
    #[arg(long, conflicts_with = "no_preserve_mtime")]
    preserve_mtime: bool,
    /// Leave newly copied files with their write time instead of source mtime.
    #[arg(long)]
    no_preserve_mtime: bool,
    /// Stage each file and atomically replace its destination after successful validation.
    #[arg(long, conflicts_with = "no_atomic")]
    atomic: bool,
    /// Write directly to destinations (the default); overrides TOML atomic mode.
    #[arg(long)]
    no_atomic: bool,
    /// Match filter patterns case-sensitively, including on Windows.
    #[arg(long)]
    case_sensitive: bool,
    /// Remove selected destination extras after successful copies; off by default.
    #[arg(long)]
    purge: bool,
    /// Compare and plan all operations without modifying destination files.
    #[arg(long)]
    dry_run: bool,
    /// Bound parallel source scans (default 8).
    #[arg(long)]
    scan_workers: Option<usize>,
    /// Bound concurrent complete-content comparisons (default 4).
    #[arg(long)]
    hash_workers: Option<usize>,
    /// Bound concurrent copies; zero selects up to 32 available CPUs (default).
    #[arg(long)]
    copy_workers: Option<usize>,
    /// Retry each failed copy this many times (default 2).
    #[arg(long)]
    retries: Option<usize>,
    /// Emit progress at this interval, such as 250ms or 1s (default 250ms).
    #[arg(long)]
    progress_interval: Option<String>,
    /// Select timestamped text, newline-delimited JSON, or final-summary-only output.
    #[arg(long, default_value = "auto", value_parser = ["auto", "plain", "json", "off"])]
    progress: String,
}

impl Cli {
    /// Parse arguments, run the batch, and distinguish usage errors from copy failures.
    pub fn run(args: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> i32 {
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let cli = match Self::try_parse_from(args) {
                Ok(cli) => cli,
                Err(error) => {
                    let code = error.exit_code();
                    if let Err(print_error) = error.print() {
                        tracing::error!("Failed to print command help: {print_error}");
                    }
                    return code;
                }
            };
            match cli.execute() {
                Ok(true) => 0,
                Ok(false) => 1,
                Err(error) => {
                    tracing::error!("{error:#}");
                    1
                }
            }
        })
    }

    /// Run native workers and keep progress rendering outside the copy engine.
    fn execute(self) -> Result<bool> {
        let (jobs, options) = config::Configuration::load(&self)?;
        let cancellation = Cancellation::default();
        let signal = cancellation.clone();
        ctrlc::set_handler(move || signal.cancel())
            .context("Failed to install cancellation handler")?;
        let summary = CopyEngine::run(&jobs, &options, cancellation, |event| self.render(event))?;
        for error in &summary.errors {
            tracing::error!("{error}");
        }
        Ok(summary.success())
    }

    /// Emit JSON events to stdout or timestamped progress lines to stderr.
    fn render(&self, event: &Progress) -> Result<()> {
        if self.progress == "json" {
            let mut output = std::io::stdout().lock();
            serde_json::to_writer(&mut output, event).context("Failed to serialize progress")?;
            writeln!(output).context("Failed to write progress event")?;
        } else if self.progress != "off" || event.complete || event.phase == "failed" {
            let subscriber = tracing_subscriber::fmt()
                .with_target(false)
                .with_writer(std::io::stderr)
                .with_ansi(self.progress == "auto" && std::io::stderr().is_terminal())
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                tracing::info!(
                    phase = %event.phase, scanned = event.scanned_files, compared = event.compared_files,
                    copied_files = event.copied_files, copied_bytes = event.copied_bytes,
                    hashed_bytes = event.hashed_bytes, skipped_mtime = event.skipped_mtime,
                    skipped_hash = event.skipped_hash, deleted = event.deleted_files,
                total_files = ?event.total_files, total_copy_bytes = ?event.total_copy_bytes,
                planned_copy_files = event.planned_copy_files, planned_delete_files = event.planned_delete_files,
                average_mib_per_second = format_args!("{:.2}", event.copied_bytes as f64 / event.elapsed_seconds.max(0.001) / 1048576.0),
                    failed = event.failed_files, elapsed_seconds = format_args!("{:.3}", event.elapsed_seconds),
                    "copy progress"
                )
            });
        }
        Ok(())
    }
}
