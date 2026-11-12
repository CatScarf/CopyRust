use crate::{
    CopyJob, Progress, RunOptions, Summary,
    io::{Content, Paths},
    plan::{Entry, Plan},
    state::{Cancellation, State},
};
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::{
    fs,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

/// Execute a validated batch using shared worker pools and one progress reporter.
pub struct CopyEngine;

impl CopyEngine {
    /// Run a batch with serialized progress callbacks and cooperative cancellation.
    pub fn run(
        jobs: &[CopyJob],
        options: &RunOptions,
        cancellation: Cancellation,
        on_progress: impl Fn(&Progress) -> Result<()> + Sync,
    ) -> Result<Summary> {
        Self::validate(options)?;
        let state = State::new(options, cancellation);
        let done = AtomicBool::new(false);
        let result = thread::scope(|scope| -> Result<()> {
            let reporter = scope.spawn(|| Self::report(&state, options, &done, &on_progress));
            let result = Self::execute(jobs, options, &state);
            done.store(true, Ordering::Release);
            reporter.thread().unpark();
            reporter
                .join()
                .map_err(|_| anyhow::anyhow!("Progress reporter panicked"))??;
            result
        });
        if let Err(error) = result {
            state
                .failure(error)
                .context("Failed to retain batch error")?;
        }
        let errors = state.errors()?;
        state.update(|p| {
            p.complete = errors.is_empty();
            p.phase = if p.complete { "finished" } else { "failed" }.into();
        })?;
        let progress = state.snapshot()?;
        on_progress(&progress).context("Final progress callback failed")?;
        Ok(Summary { progress, errors })
    }

    /// Reject unreasonable concurrency and progress settings before spawning threads.
    fn validate(options: &RunOptions) -> Result<()> {
        for count in [
            options.scan_workers,
            options.hash_workers,
            options.copy_workers,
        ] {
            if !(1..=256).contains(&count) {
                bail!("Worker counts must be between 1 and 256");
            }
        }
        if options.progress_interval_ms == 0 || options.retries > 100 {
            bail!("Progress interval must be positive and retries at most 100");
        }
        Ok(())
    }

    /// Emit bounded-frequency snapshots from one thread and propagate callback errors.
    fn report(
        state: &State,
        options: &RunOptions,
        done: &AtomicBool,
        callback: &(impl Fn(&Progress) -> Result<()> + Sync),
    ) -> Result<()> {
        while !done.load(Ordering::Acquire) {
            if let Err(error) = callback(&state.snapshot()?) {
                state.cancellation.cancel();
                return Err(error).context("Progress callback failed");
            }
            thread::park_timeout(Duration::from_millis(options.progress_interval_ms));
        }
        Ok(())
    }

    /// Complete planning and comparison before any writes, then purge only on success.
    fn execute(jobs: &[CopyJob], options: &RunOptions, state: &State) -> Result<()> {
        let plan = Plan::build(jobs, options.scan_workers, state)?;
        let selections = Self::compare(&plan, options, state)?;
        let selected: Vec<_> = plan
            .files
            .iter()
            .zip(selections)
            .filter_map(|(entry, copy)| copy.then_some(entry))
            .collect();
        let bytes = selected
            .iter()
            .try_fold(0u64, |total, entry| -> Result<u64> {
                Ok(total
                    + fs::metadata(&entry.source)
                        .context("Failed to count selected copy bytes")?
                        .len())
            })?;
        state.update(|p| {
            p.phase = "copying".into();
            p.total_copy_bytes = Some(bytes);
            p.planned_copy_files = selected.len() as u64;
            p.planned_delete_files = plan.removals.len() as u64;
        })?;
        if options.dry_run {
            return Ok(());
        }
        Self::copy_selected(&selected, &plan, options, state)?;
        state
            .cancellation
            .check()
            .context("Copy cancelled before cleanup")?;
        if state.errors()?.is_empty() {
            Self::purge(&plan, state)?;
        }
        Ok(())
    }

    /// Compare selected files in bounded parallel workers without mutating outputs.
    fn compare(plan: &Plan, options: &RunOptions, state: &State) -> Result<Vec<bool>> {
        state.update(|p| {
            p.phase = "comparing".into();
            p.total_files = Some(plan.files.len() as u64);
        })?;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(options.hash_workers)
            .build()
            .context("Failed to start hash workers")?;
        pool.install(|| {
            plan.files
                .par_iter()
                .map(|entry| {
                    let result = Content::needs_copy(entry, &plan.jobs[entry.job].options, state)
                        .with_context(|| format!("Failed to compare {}", entry.source.display()));
                    state.update(|p| p.compared_files += 1)?;
                    result
                })
                .collect::<Result<_>>()
        })
        .context("Failed to compare batch inputs")
    }

    /// Copy planned differences using the batch-wide copy worker limit.
    fn copy_selected(
        selected: &[&Entry],
        plan: &Plan,
        options: &RunOptions,
        state: &State,
    ) -> Result<()> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(options.copy_workers)
            .build()
            .context("Failed to start copy workers")?;
        pool.install(|| {
            selected.par_iter().try_for_each(|entry| {
                match Self::retry(options.retries, state, || {
                    Content::copy(entry, &plan.jobs[entry.job].options, state)
                }) {
                    Ok(()) => state.update(|p| p.copied_files += 1),
                    Err(error) => state.failure(
                        error.context(format!("Failed to copy {}", entry.source.display())),
                    ),
                }
            })
        })
        .context("Failed to execute selected copies")
    }

    /// Retry a failed file operation while preserving the last contextual error.
    fn retry(attempts: usize, state: &State, action: impl Fn() -> Result<()>) -> Result<()> {
        for attempt in 0..=attempts {
            state
                .cancellation
                .check()
                .context("Failed to start copy attempt")?;
            match action() {
                Ok(()) => return Ok(()),
                Err(error) if attempt == attempts => {
                    return Err(error).context("Copy attempts exhausted");
                }
                Err(_) => thread::sleep(Duration::from_millis(100 * (attempt as u64 + 1))),
            }
        }
        bail!("Copy retry loop did not execute")
    }

    /// Delete planned extras after successful copying without following links.
    fn purge(plan: &Plan, state: &State) -> Result<()> {
        state.update(|p| p.phase = "deleting".into())?;
        for removal in &plan.removals {
            state
                .cancellation
                .check()
                .context("Failed to continue cleanup")?;
            Paths::guard(&removal.path)?;
            if removal.directory {
                if fs::read_dir(&removal.path)
                    .context("Failed to inspect extra directory")?
                    .next()
                    .transpose()
                    .context("Failed to read extra directory")?
                    .is_none()
                {
                    fs::remove_dir(&removal.path).context("Failed to remove extra directory")?;
                    state.update(|p| p.deleted_files += 1)?;
                }
            } else {
                fs::remove_file(&removal.path).with_context(|| {
                    format!("Failed to remove extra file {}", removal.path.display())
                })?;
                state.update(|p| p.deleted_files += 1)?;
            }
        }
        Ok(())
    }
}
