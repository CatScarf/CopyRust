use crate::{Cancellation, Compare, CopyEngine, CopyJob, RunOptions, Summary};
use anyhow::{Context, Result, ensure};
use std::{fs, path::Path};

struct Fixture {
    _root: tempfile::TempDir,
    job: CopyJob,
}

impl Fixture {
    /// Create isolated source and destination trees for behavior checks.
    fn new() -> Result<Self> {
        let root = tempfile::tempdir().context("Failed to create test directory")?;
        let job = CopyJob::new(root.path().join("source"), root.path().join("destination"));
        fs::create_dir(&job.source).context("Failed to create fixture source")?;
        fs::create_dir(&job.destination).context("Failed to create fixture destination")?;
        Ok(Self { _root: root, job })
    }

    /// Write one nested fixture file without hiding filesystem errors.
    fn put(root: &Path, relative: &str, contents: &[u8]) -> Result<()> {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().context("Fixture file has no parent")?)
            .context("Failed to create fixture parent")?;
        fs::write(path, contents).context("Failed to write fixture file")
    }

    /// Run a fixture with bounded workers and no destructive retries.
    fn run(&self) -> Result<Summary> {
        CopyEngine::run(
            std::slice::from_ref(&self.job),
            &RunOptions {
                retries: 0,
                ..RunOptions::default()
            },
            Cancellation::default(),
            |_| Ok(()),
        )
    }

    /// Set exact file timestamps to exercise both metadata and content decisions.
    fn time(path: &Path, seconds: i64) -> Result<()> {
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(seconds, 0))
            .context("Failed to set fixture timestamp")
    }
}

/// Strict hashing detects changed contents even when size and timestamps match.
#[test]
fn hash_detects_preserved_metadata_changes() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.job.options.compare = Compare::Hash;
    Fixture::put(&fixture.job.source, "binary", b"AAAA")?;
    Fixture::put(&fixture.job.destination, "binary", b"BBBB")?;
    Fixture::time(&fixture.job.source.join("binary"), 12345)?;
    Fixture::time(&fixture.job.destination.join("binary"), 12345)?;
    let summary = fixture.run()?;
    ensure!(summary.success(), "Copy failed: {:?}", summary.errors);
    ensure!(summary.progress.copied_files == 1 && summary.progress.hashed_bytes == 8);
    ensure!(fs::read(fixture.job.destination.join("binary"))? == b"AAAA");
    Ok(())
}

/// Hash mode skips identical contents whose timestamps were rewritten by a build cache.
#[test]
fn hash_skips_timestamp_only_changes() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.job.options.compare = Compare::Hash;
    Fixture::put(&fixture.job.source, "binary", b"same")?;
    Fixture::put(&fixture.job.destination, "binary", b"same")?;
    Fixture::time(&fixture.job.source.join("binary"), 100)?;
    Fixture::time(&fixture.job.destination.join("binary"), 200)?;
    let summary = fixture.run()?;
    ensure!(
        summary.success()
            && summary.progress.skipped_hash == 1
            && summary.progress.copied_files == 0
    );
    ensure!(
        filetime::FileTime::from_last_modification_time(&fs::metadata(
            fixture.job.destination.join("binary")
        )?)
        .unix_seconds()
            == 200
    );
    Ok(())
}

/// Timestamp and auto modes intentionally shortcut equal-size equal-time contents.
#[test]
fn metadata_modes_have_explicit_shortcuts() -> Result<()> {
    for compare in [Compare::Mtime, Compare::Auto] {
        let mut fixture = Fixture::new()?;
        fixture.job.options.compare = compare;
        Fixture::put(&fixture.job.source, "file", b"AAAA")?;
        Fixture::put(&fixture.job.destination, "file", b"BBBB")?;
        Fixture::time(&fixture.job.source.join("file"), 100)?;
        Fixture::time(&fixture.job.destination.join("file"), 100)?;
        let summary = fixture.run()?;
        ensure!(
            summary.success()
                && summary.progress.skipped_mtime == 1
                && summary.progress.hashed_bytes == 0
        );
    }
    Ok(())
}

/// Different-sized files are copied without reading either side for hashing.
#[test]
fn size_changes_do_not_need_digests() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.job.options.compare = Compare::Hash;
    Fixture::put(&fixture.job.source, "file", b"new contents")?;
    Fixture::put(&fixture.job.destination, "file", b"old")?;
    let summary = fixture.run()?;
    ensure!(
        summary.success()
            && summary.progress.copied_files == 1
            && summary.progress.hashed_bytes == 0
    );
    Ok(())
}

/// Purge removes selected extras while retaining ignored and out-of-scope files.
#[test]
fn purge_respects_filters() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.job.options.purge = true;
    fixture.job.options.exclude_dirs = vec!["cache".into()];
    fixture.job.options.exclude_files = vec!["*.log".into()];
    fixture.job.options.include_files = vec!["*.bin".into()];
    Fixture::put(&fixture.job.source, "keep.bin", b"keep")?;
    for path in ["extra.bin", "cache/extra.bin", "keep.log", "keep.txt"] {
        Fixture::put(&fixture.job.destination, path, b"old")?;
    }
    let summary = fixture.run()?;
    ensure!(summary.success(), "Copy failed: {:?}", summary.errors);
    ensure!(!fixture.job.destination.join("extra.bin").exists());
    for path in ["cache/extra.bin", "keep.log", "keep.txt"] {
        ensure!(fixture.job.destination.join(path).exists());
    }
    Ok(())
}

/// Preview performs comparison and cleanup planning without creating a destination.
#[test]
fn preview_has_no_writes() -> Result<()> {
    let fixture = Fixture::new()?;
    Fixture::put(&fixture.job.source, "new/file", b"contents")?;
    fs::remove_dir(&fixture.job.destination)?;
    let summary = CopyEngine::run(
        std::slice::from_ref(&fixture.job),
        &RunOptions {
            dry_run: true,
            ..RunOptions::default()
        },
        Cancellation::default(),
        |_| Ok(()),
    )?;
    ensure!(summary.success() && summary.progress.planned_copy_files == 1);
    ensure!(!fixture.job.destination.exists());
    Ok(())
}

/// An outer mapping's purge must not delete another mapping's nested destination.
#[test]
fn batch_protects_nested_mappings() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.job.options.purge = true;
    let nested_source = fixture._root.path().join("other");
    Fixture::put(&nested_source, "new", b"nested")?;
    Fixture::put(&fixture.job.destination, "nested/old", b"retain")?;
    let nested = CopyJob::new(nested_source, fixture.job.destination.join("nested"));
    let summary = CopyEngine::run(
        &[fixture.job, nested.clone()],
        &RunOptions::default(),
        Cancellation::default(),
        |_| Ok(()),
    )?;
    ensure!(summary.success(), "Copy failed: {:?}", summary.errors);
    ensure!(nested.destination.join("new").exists() && nested.destination.join("old").exists());
    Ok(())
}

/// Duplicate destination ownership fails before any files are written.
#[test]
fn collisions_fail_before_mutations() -> Result<()> {
    let fixture = Fixture::new()?;
    Fixture::put(&fixture.job.source, "file", b"new")?;
    let summary = CopyEngine::run(
        &[fixture.job.clone(), fixture.job.clone()],
        &RunOptions::default(),
        Cancellation::default(),
        |_| Ok(()),
    )?;
    ensure!(!summary.success() && !fixture.job.destination.join("file").exists());
    Ok(())
}

/// Source/destination overlap is rejected before recursive enumeration.
#[test]
fn overlapping_source_is_rejected() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.job.destination = fixture.job.source.join("output");
    ensure!(!fixture.run()?.success());
    ensure!(!fixture.job.destination.exists());
    Ok(())
}

/// A pre-cancelled batch cannot copy or purge any files.
#[test]
fn cancellation_prevents_writes() -> Result<()> {
    let fixture = Fixture::new()?;
    Fixture::put(&fixture.job.source, "file", b"new")?;
    let cancellation = Cancellation::default();
    cancellation.cancel();
    let summary = CopyEngine::run(
        std::slice::from_ref(&fixture.job),
        &RunOptions::default(),
        cancellation,
        |_| Ok(()),
    )?;
    ensure!(!summary.success() && !fixture.job.destination.join("file").exists());
    Ok(())
}

/// Callback errors propagate into a failed result rather than silently disappearing.
#[test]
fn callback_failure_is_observable() -> Result<()> {
    let fixture = Fixture::new()?;
    let result = CopyEngine::run(
        &[fixture.job],
        &RunOptions::default(),
        Cancellation::default(),
        |_| anyhow::bail!("callback failed"),
    );
    ensure!(result.is_err());
    Ok(())
}

/// Nonrecursive mappings select only files directly inside their source root.
#[test]
fn nonrecursive_selection() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.job.options.recursive = false;
    Fixture::put(&fixture.job.source, "root", b"root")?;
    Fixture::put(&fixture.job.source, "nested/file", b"nested")?;
    let summary = fixture.run()?;
    ensure!(summary.success() && summary.progress.copied_files == 1);
    ensure!(!fixture.job.destination.join("nested").exists());
    Ok(())
}
