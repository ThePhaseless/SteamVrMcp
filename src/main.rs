use alvr_common::anyhow::Result;
use std::path::Path;
use vr_test_mcp::{
    config::{Config, Paths},
    server,
    stack::Stack,
    steamvr,
};

fn main() -> Result<()> {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    if Path::new(&argv0).file_name() == Some(alvr_filesystem::dashboard_fname().as_ref()) {
        steamvr::run_dashboard_child();
    }
    alvr_client_core::init_logging();

    // a malformed config.toml fails the `config` check instead, so vr_status stays available
    let cfg = Config::load().unwrap_or_else(|e| {
        alvr_common::error!("{e:#}");
        Config::default()
    });
    std::sync::LazyLock::force(&vr_test_mcp::clock::T0);
    let stack = Stack::new(cfg.clone(), Paths::detect(&cfg));
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let served = runtime.block_on(server::serve(stack));
    runtime.shutdown_background(); // a vr_start still running must not hold the exit
    served
}
