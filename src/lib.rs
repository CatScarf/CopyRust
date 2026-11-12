//! Content-aware batch copying with bounded parallelism and observable progress.

mod engine;
mod entry;
mod filesystem;
mod filter;
mod hashing;
mod index;
mod io;
mod model;
mod plan;
mod state;
#[cfg(windows)]
mod windows;

pub use engine::CopyEngine;
pub use model::{Compare, HashAlgorithm};
pub use model::{CopyJob, CopyOptions, Progress, RunOptions, Summary};
pub use state::Cancellation;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod index_tests;
