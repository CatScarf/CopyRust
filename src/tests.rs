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

/// A file-to-directory collision across mappings must fail before writes.
#[test]
fn prefix_collisions_fail_before_mutations() -> Result<()> {
    let fixture = Fixture::new()?;
    Fixture::put(&fixture.job.source, "nested", b"file")?;
    let other_source = fixture._root.path().join("other");
    Fixture::put(&other_source, "file", b"nested")?;
    let other = CopyJob::new(other_source, fixture.job.destination.join("nested"));
    let summary = CopyEngine::run(
        &[fixture.job.clone(), other],
        &RunOptions::default(),
        Cancellation::default(),
        |_| Ok(()),
    )?;
    ensure!(!summary.success() && !fixture.job.destination.join("nested").exists());
    Ok(())
}

/// Read-only destination files can be replaced without losing full source contents.
#[test]
fn readonly_destination_can_be_replaced() -> Result<()> {
    let fixture = Fixture::new()?;
    Fixture::put(&fixture.job.source, "file", b"new complete contents")?;
    Fixture::put(&fixture.job.destination, "file", b"old")?;
    let path = fixture.job.destination.join("file");
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions)?;
    let summary = fixture.run()?;
    ensure!(summary.success(), "Copy failed: {:?}", summary.errors);
    ensure!(fs::read(path)? == b"new complete contents");
    Ok(())
}

/// Identical hard-link identities are skipped even in forced-copy mode.
#[test]
fn same_file_identity_is_not_overwritten() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.job.options.compare = Compare::Always;
    Fixture::put(&fixture.job.source, "file", b"contents")?;
    fs::hard_link(
        fixture.job.source.join("file"),
        fixture.job.destination.join("file"),
    )?;
    let summary = fixture.run()?;
    ensure!(summary.success() && summary.progress.skipped_same_file == 1);
    ensure!(fs::read(fixture.job.source.join("file"))? == b"contents");
    Ok(())
}

/// A planning failure prevents both ordinary copies and destructive cleanup.
#[test]
fn failed_batch_never_purges() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.job.options.purge = true;
    Fixture::put(&fixture.job.source, "file", b"new")?;
    Fixture::put(&fixture.job.destination, "extra", b"retain")?;
    let summary = CopyEngine::run(
        &[fixture.job.clone(), fixture.job.clone()],
        &RunOptions::default(),
        Cancellation::default(),
        |_| Ok(()),
    )?;
    ensure!(!summary.success());
    ensure!(fixture.job.destination.join("extra").exists());
    Ok(())
}

/// Every supported digest detects changes and skips timestamp-only rewrites.
#[test]
fn digest_algorithms_preserve_copy_semantics() -> Result<()> {
    for algorithm in [
        crate::HashAlgorithm::Xxh3,
        crate::HashAlgorithm::Xxh64,
        crate::HashAlgorithm::Blake3,
    ] {
        let mut fixture = Fixture::new()?;
        fixture.job.options.compare = Compare::Hash;
        fixture.job.options.hash_algorithm = algorithm;
        Fixture::put(&fixture.job.source, "file", b"AAAA")?;
        Fixture::put(&fixture.job.destination, "file", b"BBBB")?;
        Fixture::time(&fixture.job.source.join("file"), 100)?;
        Fixture::time(&fixture.job.destination.join("file"), 100)?;
        let first = fixture.run()?;
        ensure!(first.success() && first.progress.copied_files == 1);
        ensure!(fs::read(fixture.job.destination.join("file"))? == b"AAAA");
        Fixture::time(&fixture.job.destination.join("file"), 200)?;
        let second = fixture.run()?;
        ensure!(second.success() && second.progress.skipped_hash == 1);
    }
    Ok(())
}

/// Successful transfers preserve exact timestamps for the next incremental run.
#[test]
fn actual_copies_preserve_timestamps() -> Result<()> {
    let fixture = Fixture::new()?;
    Fixture::put(&fixture.job.source, "space 中文/file.bin", b"contents")?;
    Fixture::time(&fixture.job.source.join("space 中文/file.bin"), 12345)?;
    ensure!(fixture.run()?.success());
    let repeated = fixture.run()?;
    ensure!(repeated.success() && repeated.progress.skipped_mtime == 1);
    Ok(())
}

/// Recursive copying retains empty directories and creates an empty destination root.
#[test]
fn empty_directories_are_preserved() -> Result<()> {
    let fixture = Fixture::new()?;
    fs::create_dir_all(fixture.job.source.join("empty/child"))?;
    fs::remove_dir(&fixture.job.destination)?;
    ensure!(fixture.run()?.success());
    ensure!(fixture.job.destination.join("empty/child").is_dir());
    Ok(())
}

/// Cancellation before an atomic transfer preserves the original destination.
#[test]
fn cancelled_transfer_keeps_original_file() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.job.options.atomic = true;
    Fixture::put(&fixture.job.source, "file", &vec![1u8; 1024 * 1024])?;
    Fixture::put(&fixture.job.destination, "file", b"original")?;
    let entry = crate::plan::Entry::known(
        fixture.job.source.join("file"),
        fixture.job.destination.join("file"),
        0,
        fs::metadata(fixture.job.source.join("file"))?,
        Some(fs::metadata(fixture.job.destination.join("file"))?),
    );
    let cancellation = Cancellation::default();
    cancellation.cancel();
    let state = crate::state::State::new(&RunOptions::default(), cancellation);
    ensure!(crate::io::Content::copy(&entry, &fixture.job.options, &state).is_err());
    ensure!(fs::read(fixture.job.destination.join("file"))? == b"original");
    ensure!(fs::read_dir(&fixture.job.destination)?.count() == 1);
    Ok(())
}

/// Source links are skipped and destination directory links are rejected.
#[cfg(unix)]
#[test]
fn links_do_not_escape_roots() -> Result<()> {
    let fixture = Fixture::new()?;
    let external = fixture._root.path().join("external");
    Fixture::put(&external, "file", b"retain")?;
    std::os::unix::fs::symlink(&external, fixture.job.source.join("source-link"))?;
    let summary = fixture.run()?;
    ensure!(summary.success() && summary.progress.skipped_links == 1);
    Fixture::put(&fixture.job.source, "nested/file", b"replace")?;
    std::os::unix::fs::symlink(&external, fixture.job.destination.join("nested"))?;
    ensure!(!fixture.run()?.success());
    ensure!(fs::read(external.join("file"))? == b"retain");
    Ok(())
}

/// Direct and atomic modes copy, truncate, and preserve timestamps consistently.
#[test]
fn transfer_modes_replace_longer_destinations() -> Result<()> {
    for atomic in [false, true] {
        let mut fixture = Fixture::new()?;
        fixture.job.options.atomic = atomic;
        Fixture::put(&fixture.job.source, "file", b"new")?;
        Fixture::put(&fixture.job.destination, "file", b"long original contents")?;
        Fixture::time(&fixture.job.source.join("file"), 12345)?;
        ensure!(fixture.run()?.success());
        ensure!(fs::read(fixture.job.destination.join("file"))? == b"new");
        ensure!(fixture.run()?.progress.skipped_mtime == 1);
        ensure!(fs::read_dir(&fixture.job.destination)?.count() == 1);
    }
    Ok(())
}

/// One directory mapping produces identical filtered inventories at every scan concurrency.
#[test]
fn single_tree_parallel_scans_preserve_selection() -> Result<()> {
    for workers in [1, 2, 8, 16] {
        let mut fixture = Fixture::new()?;
        fixture.job.options.exclude_dirs = vec!["ignored".into()];
        fixture.job.options.exclude_files = vec!["*.tmp".into()];
        for index in 0..24 {
            Fixture::put(
                &fixture.job.source,
                &format!("dir-{index}/deep/file"),
                b"parallel",
            )?;
            Fixture::put(
                &fixture.job.source,
                &format!("dir-{index}/omit.tmp"),
                b"omit",
            )?;
        }
        Fixture::put(&fixture.job.source, "ignored/file", b"omit")?;
        Fixture::put(&fixture.job.source, ".hidden/file", b"retain")?;
        let options = RunOptions {
            scan_workers: workers,
            retries: 0,
            ..RunOptions::default()
        };
        let first = CopyEngine::run(
            std::slice::from_ref(&fixture.job),
            &options,
            Cancellation::default(),
            |_| Ok(()),
        )?;
        ensure!(
            first.success()
                && first.progress.copied_files == 25
                && first.progress.scanned_files == 25
        );
        ensure!(!fixture.job.destination.join("ignored").exists());
        let second = CopyEngine::run(
            std::slice::from_ref(&fixture.job),
            &options,
            Cancellation::default(),
            |_| Ok(()),
        )?;
        ensure!(
            second.success()
                && second.progress.skipped_mtime == 25
                && second.progress.scanned_files == 25
        );
        ensure!(second.progress.copied_files == 0 && second.progress.hashed_bytes == 0);
    }
    Ok(())
}
