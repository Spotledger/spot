//! The Linux handler set: the `os.*` operations of `spot-agent-linux` (packages, services,
//! users, firewall) and `spot-agent-linux-sys` (sysctl, MAC, network, time, certificates,
//! storage). Registered only on Linux and only when `allowed_modules` lists `os`.

use std::sync::Arc;

use spot_agent_core::HandlerRegistry;
use spot_agent_linux::LinuxContext;
use spot_agent_linux_sys::LinuxSysHandler;

use super::HandlerCtx;

/// The operation namespace.
pub const NAMESPACE: &str = "os";

/// Registers the Linux handlers (a no-op on other platforms).
pub fn register(reg: &mut HandlerRegistry, ctx: &HandlerCtx) {
    if cfg!(target_os = "linux") {
        register_linux(reg, ctx);
    }
}

/// The unconditional registration, used directly by tests on any host.
pub fn register_linux(reg: &mut HandlerRegistry, ctx: &HandlerCtx) {
    let context = Arc::new(LinuxContext::new(
        ctx.linux_runner.clone(),
        ctx.linux_fs.clone(),
    ));
    spot_agent_linux::register_handlers(reg, &context);
    let sys = Arc::new(
        LinuxSysHandler::new(
            ctx.sys_runner.clone(),
            ctx.sys_fs.clone(),
            ctx.clock.clone(),
        )
        .with_state_dir(ctx.state_dir.join("network")),
    );
    LinuxSysHandler::register(&sys, reg);
}

/// The operations of the Linux modules (the Windows file declares the same `os.*` ids, so
/// each platform builds only its own catalogue).
pub fn register_catalogue(builder: &mut spot_mgmt_ops::CatalogueBuilder) {
    use spot_mgmt_ops::OperationModule as _;
    if cfg!(target_os = "linux") {
        spot_agent_linux::LinuxModule.register(builder);
        spot_agent_linux_sys::LinuxSysModule.register(builder);
    }
}
