//! wtree — policy-driven git worktree management.
//!
//! Library surface used by the `wtree` binary and by integration tests.

pub mod config;
pub mod judge;
pub mod repo;
pub mod settings;
pub mod state;
pub mod verbs;

/// Test-support fixtures (real git repos in temp dirs). Not part of the
/// tool's API — exposed so integration tests can reuse them.
#[doc(hidden)]
pub mod testutil;
