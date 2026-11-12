use crate::{Cancellation, CopyEngine, CopyJob, RunOptions};
use anyhow::{Context, Result, ensure};
use std::{fs, path::Path};

struct Fixture {
    root: tempfile::TempDir,
}
impl Fixture {
    /// Create isolated roots for directory index regression coverage.
    fn new() -> Result<Self> {
        Ok(Self {
            root: tempfile::tempdir().context("Failed to create index fixture")?,
        })
    }
    /// Populate a nested source input without hiding path failures.
    fn put(root: &Path, path: &str, bytes: &[u8]) -> Result<()> {
        let file = root.join(path);
        fs::create_dir_all(file.parent().context("Fixture input has no parent")?)
            .context("Failed to create fixture directory")?;
        fs::write(file, bytes).context("Failed to write index fixture")
    }
}

/// Shared target directories retain every file's global metadata and copy index.
#[test]
fn shared_directory_indexes_merge_without_losing_offsets() -> Result<()> {
    let fixture = Fixture::new()?;
    let root = fixture.root.path();
    let destination = root.join("out");
    let jobs = [
        CopyJob::new(root.join("first"), &destination),
        CopyJob::new(root.join("second"), &destination),
    ];
    Fixture::put(&jobs[0].source, "a", b"first")?;
    Fixture::put(&jobs[0].source, "nested/Unicode 中文", b"nested first")?;
    Fixture::put(&jobs[1].source, "b", b"second")?;
    Fixture::put(&jobs[1].source, "nested/other", b"nested second")?;
    let first = CopyEngine::run(
        &jobs,
        &RunOptions::default(),
        Cancellation::default(),
        |_| Ok(()),
    )
    .context("Failed to run merged directory copy")?;
    ensure!(first.success() && first.progress.copied_files == 4);
    ensure!(fs::read(destination.join("a")).context("Failed to read first output")? == b"first");
    ensure!(fs::read(destination.join("b")).context("Failed to read second output")? == b"second");
    let mut hash_jobs = jobs;
    for job in &mut hash_jobs {
        job.options.compare = crate::Compare::Hash;
    }
    let repeated = CopyEngine::run(
        &hash_jobs,
        &RunOptions::default(),
        Cancellation::default(),
        |_| Ok(()),
    )
    .context("Failed to compare merged directory metadata")?;
    ensure!(
        repeated.success()
            && repeated.progress.skipped_hash == 4
            && repeated.progress.copied_files == 0
    );
    Ok(())
}

/// Unicode case folding detects Windows aliases across differently cased target groups.
#[cfg(windows)]
#[test]
fn case_alias_groups_reject_duplicate_names_before_writes() -> Result<()> {
    let fixture = Fixture::new()?;
    let root = fixture.root.path();
    let jobs = [
        CopyJob::new(root.join("first"), root.join("out/Mixed")),
        CopyJob::new(root.join("second"), root.join("out/mixed")),
    ];
    Fixture::put(&jobs[0].source, "ÉTUDE", b"first")?;
    Fixture::put(&jobs[1].source, "étude", b"second")?;
    let result = CopyEngine::run(
        &jobs,
        &RunOptions::default(),
        Cancellation::default(),
        |_| Ok(()),
    )
    .context("Failed to reject aliased destination groups")?;
    ensure!(!result.success() && !root.join("out").exists());
    Ok(())
}

/// Native scans refill batches and retain pre-epoch dates and exact 100-nanosecond mtimes.
#[cfg(windows)]
#[test]
fn native_batch_boundaries_preserve_content_and_timestamps() -> Result<()> {
    let fixture = Fixture::new()?;
    let root = fixture.root.path();
    let mut job = CopyJob::new(root.join("source"), root.join("target"));
    for index in 0u32..1024 {
        Fixture::put(
            &job.source,
            &format!("data 中文-{index:04}-tail.bin"),
            &index.to_le_bytes(),
        )?;
    }
    for (index, seconds) in [(0, -12345), (1, 12345)] {
        filetime::set_file_mtime(
            job.source.join(format!("data 中文-{index:04}-tail.bin")),
            filetime::FileTime::from_unix_time(seconds, 100),
        )
        .context("Failed to set exact native fixture mtime")?;
    }
    let first = CopyEngine::run(
        std::slice::from_ref(&job),
        &RunOptions::default(),
        Cancellation::default(),
        |_| Ok(()),
    )?;
    ensure!(first.success() && first.progress.copied_files == 1024);
    for index in 0u32..1024 {
        let file = job
            .destination
            .join(format!("data 中文-{index:04}-tail.bin"));
        ensure!(
            fs::read(file).context("Failed to read native batch output")? == index.to_le_bytes()
        );
    }
    let repeat = CopyEngine::run(
        std::slice::from_ref(&job),
        &RunOptions::default(),
        Cancellation::default(),
        |_| Ok(()),
    )?;
    ensure!(repeat.success() && repeat.progress.skipped_mtime == 1024);
    job.options.compare = crate::Compare::Hash;
    let hashed = CopyEngine::run(
        &[job],
        &RunOptions::default(),
        Cancellation::default(),
        |_| Ok(()),
    )?;
    ensure!(hashed.success() && hashed.progress.skipped_hash == 1024);
    Ok(())
}
