//! Generated Rust client for the Spotledger native API contract
//! (`contracts/native-api/openapi.yaml`, WP-CON-03, ADR-055).
//!
//! The code below is generated at build time by `build.rs` (progenitor) from
//! `spec/openapi-3.0.json`, which `tools/contract-clients/convert.py to-30`
//! produces from the contract. It is written to `OUT_DIR` and never committed;
//! run `tools/contract-clients/check.sh` to build everything. Decimal values
//! are kept as strings in this client: money never passes through a float
//! (ADR-12), and `tools/contract-clients/check.sh` enforces that on every run.
#![allow(warnings)]

include!(concat!(env!("OUT_DIR"), "/codegen.rs"));