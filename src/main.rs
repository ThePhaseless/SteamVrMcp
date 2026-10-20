use alvr_common::anyhow::Result;
use std::{io::Read, path::Path, sync::mpsc, thread, time::Duration};
use vr_test_mcp::{
    config::{Config, Paths},
    stack::Stack,
    steamvr,
};

fn main() -> Result<()> {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    if Path::new(&argv0).file_name() == Some(alvr_filesystem::dashboard_fname().as_ref()) {
        steamvr::run_dashboard_child();
    }
    alvr_client_core::init_logging();

    // Headless until Task 3: start, print status to stderr every 5 s until stdin closes, stop.
    let cfg = Config::load()?;
    let stack = Stack::new(cfg.clone(), Paths::detect(&cfg));
    match stack.start() {
        Ok(report) => eprintln!("{}", serde_json::to_string(&report)?),
        Err(e) => eprintln!("start failed: {e:#}"),
    }
    let (eof_tx, eof_rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        std::io::stdin().read_to_end(&mut vec![]).ok();
        eof_tx.send(()).ok();
    });
    while eof_rx.recv_timeout(Duration::from_secs(5)) == Err(mpsc::RecvTimeoutError::Timeout) {
        eprintln!("{}", serde_json::to_string(&stack.status())?);
    }
    eprintln!("{}", serde_json::to_string(&stack.stop()?)?);
    Ok(())
}
