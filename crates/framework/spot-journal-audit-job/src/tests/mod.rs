//! Tests of the scheduled audit. The fixture builds a real journal per tenant (sealed segments
//! and a signed manifest in a memory store); the live side is an in-memory side built from the
//! same script, so a clean run must match it exactly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cadence_tests;
mod fixture;
mod handler_tests;
mod run_tests;
