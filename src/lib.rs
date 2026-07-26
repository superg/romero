extern crate self as romero;

mod cache;
mod config;
mod cue;
mod dat;
mod engine;
mod error;
mod filesystem;
mod model;
mod ordering;
#[cfg(test)]
mod presentation;
mod reconcile;

pub use config::{ConfigValues, ResolvedConfig};
pub use engine::{
    CacheCommitReason, ExecutionSummary, LeftoverDetail, LeftoverMatch, LeftoverStatus,
    ProgressEvent, ProgressMoveKind, ProgressRemovalKind, run, run_with_progress,
};
pub use error::{Result, RomeroError};
