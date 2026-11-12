//! Content-aware batch copying with bounded parallelism and observable progress.

mod engine;
mod filter;
mod io;
mod model;
mod plan;
mod state;

pub use engine::CopyEngine;
pub use model::{Compare, CopyJob, CopyOptions, Progress, RunOptions, Summary};
pub use state::Cancellation;

#[cfg(test)]
mod tests;
