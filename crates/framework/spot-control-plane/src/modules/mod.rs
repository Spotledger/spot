//! The control plane's modules. Each module is one file in this directory that declares
//! `pub const ORDER: u32` and `pub fn register(ctx: &mut CpCtx) -> Result<(), CpError>`; the build
//! script lists it in [`REGISTRY`]. Modules contribute operator routes, host routes, background
//! tasks and readiness checks through the context and edit no shared file.

use crate::registry::Registration;

include!(concat!(env!("OUT_DIR"), "/modules_registry.rs"));
