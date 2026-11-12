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
#[command(version, about)]
pub struct Cli {
    #[arg(long, num_args = 2, value_names = ["SOURCE", "DESTINATION"], action = clap::ArgAction::Append)]
    pair: Vec<PathBuf>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, value_parser = ["mtime", "hash", "auto", "always"])]
    compare: Option<String>,
    #[arg(long)]
    exclude_dir: Vec<String>,
    #[arg(long)]
    exclude_file: Vec<String>,
    #[arg(long)]
    include_file: Vec<String>,
    #[arg(long, conflicts_with = "no_recursive")]
    recursive: bool,
    #[arg(long)]
    no_recursive: bool,
    #[arg(long, conflicts_with = "no_preserve_mtime")]
    preserve_mtime: bool,
    #[arg(long)]
    no_preserve_mtime: bool,
    #[arg(long)]
    case_sensitive: bool,
    #[arg(long)]
    purge: bool,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    scan_workers: Option<usize>,
    #[arg(long)]
    hash_workers: Option<usize>,
    #[arg(long)]
    copy_workers: Option<usize>,
    #[arg(long)]
    retries: Option<usize>,
    #[arg(long)]
    progress_interval: Option<String>,
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
                    failed = event.failed_files, elapsed_seconds = format_args!("{:.3}", event.elapsed_seconds),
                    "copy progress"
                )
            });
        }
        Ok(())
    }
}
