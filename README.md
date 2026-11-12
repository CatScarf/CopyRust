# CopyRust

Parallel, content-aware batch copying for Rust, Python, and the command line.
Copy directory **contents** into explicit destinations, skip unchanged files by
mtime or full BLAKE3 digest, and observe separate scan, hash, and copy counters.

```console
cargo install copyrust-cli
pip install copyrust
copyrust --help
```

## Command line

```console
copyrust --pair source/Engine output/Engine --pair source/Game output/Game --compare hash --exclude-dir Intermediate --exclude-dir Saved --exclude-file "*.tmp" --progress plain
copyrust --config copy.toml --dry-run --progress json
```

| Mode | Skip condition for existing regular files |
| --- | --- |
| `mtime` (default) | Equal size and exact modification time |
| `hash` | Equal size and equal complete BLAKE3 content digest; timestamps never bypass hashing |
| `auto` | Equal size and mtime, otherwise equal full digest |
| `always` | Never skip distinct files |

Different sizes copy without hashing. Identical file identities are always skipped.
`auto` and `mtime` cannot detect equal-size content changes with preserved timestamps.
There is no persistent digest cache in version 0.1.

```toml
[defaults]
compare = "hash"
exclude_dirs = ["Intermediate", "Saved", "DerivedDataCache"]
exclude_files = ["*.tmp", "*.log"]

[run]
scan_workers = 8
hash_workers = 4
copy_workers = 4
retries = 2

[[jobs]]
source = "source/Engine"
destination = "output/Engine"
purge = true

[[jobs]]
source = "source/Game"
destination = "output/Game"
recursive = false
include_files = ["*.uproject", "*.bat"]
```

CLI options override each TOML job; job exclusions append to defaults. Job include
patterns replace default includes. Relative paths resolve against the working
directory. Globs without `/` match basenames anywhere. Globs with `/` match paths
relative to each source root; `*` does not cross separators and `**` can. Both
slash spellings normalize to `/`. Windows matching ignores case by default;
`--case-sensitive` opts in. Excluded directories are pruned, and excluded files
remain protected from purge. Inclusion filters also constrain purge candidates.

`--progress json` emits newline-delimited JSON to stdout; plain progress and errors
go to stderr with timestamps. Scanning totals remain null until enumeration ends.
`hashed_bytes` counts actual digest input across source and destination; copying
and retries count separately in `copied_bytes`. The final event includes planned
counts in dry-run mode. Progress callbacks run serially, at bounded intervals.
Exit codes: 0 success (including an empty change set), 1 operation failure,
2 command-line syntax error. Ctrl+C requests cooperative cancellation.

## Python

```python
from copyrust import CopyJob, CopyOptions, RunOptions, copy

result = copy(
    [CopyJob("source", "output", options=CopyOptions(compare="hash"))],
    options=RunOptions(copy_workers=4),
)
if not result.success:
    raise RuntimeError("; ".join(result.errors))
print(result.progress.copied_files, result.progress.skipped_hash)
```

Pass `on_progress` a callable accepting an immutable `Progress` event. Native
workers release the interpreter while copying; callbacks reacquire it. Operational
file failures return a failed `Summary` with contextual errors; malformed arguments
and callback failures may raise. Type declarations ship with the wheel.
`python -m copyrust` and the pip-installed `copyrust` command use the same CLI.

## Rust

```rust,no_run
use copyrust::{Cancellation, Compare, CopyEngine, CopyJob, RunOptions};

# fn main() -> anyhow::Result<()> {
let mut job = CopyJob::new("source", "output");
job.options.compare = Compare::Hash;
let summary = CopyEngine::run(&[job], &RunOptions::default(), Cancellation::default(), |_| Ok(()))?;
anyhow::ensure!(summary.success(), "Copy failed: {:?}", summary.errors);
# Ok(())
# }
```

## Boundaries

All mappings are scanned and compared before writes begin. Each file is copied to
a sibling temporary file, checked for observed source size/mtime changes, then
replaced. Modification times and basic permission bits are preserved by default.
This is not a filesystem snapshot: writers must not modify inputs concurrently,
especially while deliberately preserving their size and timestamps.

Source/destination tree overlap and duplicate destination files are rejected.
Symlinks and Windows reparse points are not followed; source links are skipped and
destination link traversal is rejected. Existing hard links to the same input are
skipped. Purge runs only after successful copies, respects filters, protects nested
mapping destinations, and removes selected extra files and empty directories.
It is not a transactional operation across the entire batch.

Version 0.1 targets regular build artifacts. ACLs, ownership, alternate data streams,
sparse layout, extended attributes, resumable transfers, and network snapshot
consistency are not replicated. Hash comparison must read equal-size candidates
on both sides; performance depends on workload, storage, and filesystem cache.

## Development

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
python -m maturin build --release --manifest-path python/Cargo.toml
```

The release workflow builds six 64-bit native CLI/wheel targets and publishes the
Rust crates, GitHub binaries, and Python wheels from version tags.
