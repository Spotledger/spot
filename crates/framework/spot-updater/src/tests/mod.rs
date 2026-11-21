//! Unit tests for the update engine, its guards and the real filesystem. `#[cfg(test)]` so the
//! in-memory fakes in `crate::testing` are available without turning them on for a real build.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod crash;
mod engine;
mod exhaustive;
mod filesystem;
mod fixtures;
mod guards;
mod recovery;
