mod common;

use common::Snap;
use std::{fs, panic, path::Path, thread, time::Duration};
use vr_test_mcp::{
    checks::startup_checks,
    config::{Config, Paths},
    setup::{prepare, restore, session_values},
    stack::Stack,
    steamvr,
};

/// Not a check. Stack re-executes the running binary as its `alvr_dashboard` child; in this test
/// binary that runs libtest, which runs only the non-ignored tests, so this one plays the child.
#[test]
fn dashboard_child_role() {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    if Path::new(&argv0).file_name() == Some("alvr_dashboard".as_ref()) {
        steamvr::run_dashboard_child();
    }
}

/// Proves done-when 8 (files) early, and switches the owner's v20 setup to v21 and back.
/// Touches the real SteamVR and ALVR files; needs ~/.config/vr-test-mcp/config.toml with streamer_dir.
#[test]
#[ignore]
fn live_prepare_restore_round_trip() {
    let vrserver_running = std::fs::read_dir("/proc").unwrap().flatten().any(|e| {
        std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == "vrserver")
    });
    assert!(!vrserver_running, "vrserver is running; close SteamVR before this test");

    let cfg = Config::load().unwrap();
    let paths = Paths::detect(&cfg);
    println!("{paths:#?}\nsession file: {:?}", paths.session_file());
    let before = Snap::take(&paths);
    let values = session_values(&cfg).unwrap();

    let prepared = prepare(&paths, &values, "1234.client.local.");
    let mut problems = vec![];
    match &prepared {
        Ok(_) => {
            let drivers = Snap::take(&paths).openvrpaths["external_drivers"].clone();
            if drivers != serde_json::json!([paths.driver_dir().unwrap()]) {
                problems.push(format!("external_drivers is {drivers}, expected only the v21 driver dir"));
            }
            if !paths.vrcompositor().unwrap().is_file() {
                problems.push("vrcompositor is not a regular file".into());
            }
        }
        Err(e) => problems.push(format!("prepare failed: {e:#}")),
    }

    // restore even when a check above failed, so the machine is never left on v21
    let restored = restore(&paths);
    assert!(problems.is_empty(), "{problems:?}");
    restored.unwrap();
    assert_eq!(Snap::take(&paths), before);
    assert!(!paths.restore_file().exists());
}

/// Proves done-when 1 and 8 (processes), early risk 1 (the `alvr_dashboard` child gives early
/// HMD init: the first connect restarts SteamVR exactly once, then the client streams with no
/// further restart and ALVR never misses the dashboard) and early risk 4 (the manual 127.0.0.1
/// entry connects with client discovery off). The no-LAN-address case of risk 4 is not simulated.
/// Limitation: a headset reconnect without a SteamVR restart is not proven; on loopback TCP the
/// ALVR EOF spin (sockets/src/stream_socket/tcp.rs:137-147) leaves the server stuck Disconnecting,
/// so the MCP restarts SteamVR on every reconnect.
/// Needs Steam closed and ~/.config/vr-test-mcp/config.toml with streamer_dir.
#[test]
#[ignore]
fn live_start_streams_and_stop_cleans() {
    assert!(!steamvr::is_steam_running(), "close Steam first");
    start_streams_and_stop_cleans(false);
}

/// The same proofs with the user's Steam already running: the MCP must neither start nor close it.
/// Needs Steam running (on a GPU that ALVR 21's direct mode supports) and SteamVR closed.
#[test]
#[ignore]
fn live_start_streams_and_stop_cleans_with_steam_running() {
    assert!(steamvr::is_steam_running() && !steamvr::is_steamvr_running(), "start Steam and close SteamVR first");
    start_streams_and_stop_cleans(true);
    assert!(steamvr::is_steam_running(), "the user's Steam was closed");
}

fn start_streams_and_stop_cleans(steam_was_running: bool) {
    let cfg = Config::load().unwrap();
    let paths = Paths::detect(&cfg);
    let before = Snap::take(&paths);
    // ALVR logs errors to crash_log.txt (always on, appended); the session log is off in release
    // builds and truncated at each SteamVR start
    let crash_log = alvr_filesystem::filesystem_layout_from_openvr_driver_root_dir(&paths.driver_dir().unwrap())
        .unwrap()
        .crash_log();
    let log_start = fs::metadata(&crash_log).map_or(0, |m| m.len() as usize);
    let new_errors = || {
        let bytes = fs::read(&crash_log).unwrap_or_default();
        String::from_utf8_lossy(bytes.get(log_start..).unwrap_or(&bytes)).into_owned()
    };
    let s = Stack::new(cfg, paths.clone());

    let checks = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let r = s.start().unwrap();
        println!("start: {r:?}");
        assert!(r.started_steam != steam_was_running && !r.already_streaming && !r.adopted, "{r:?}");
        assert_eq!(r.restarts, 1, "a fresh session (hash 0) restarts SteamVR exactly once: {r:?}");
        assert_eq!(r.codec.as_deref(), Some("H264"));
        assert!(!steamvr::pids_named("alvr_dashboard").is_empty(), "no alvr_dashboard child");
        // the HUD trigger restarts SteamVR even when ALVR missed the dashboard, so check ALVR's own view
        let errors = new_errors();
        assert!(!errors.contains("No dashboard process found"), "risk 1 failed, ALVR saw no dashboard:\n{errors}");

        let frames = || s.status().headset.unwrap().frames_received;
        let f0 = frames();
        thread::sleep(Duration::from_secs(2));
        assert!(frames() > f0, "no video frames in 2 s");
        assert!(s.start().unwrap().already_streaming);

        // no further restart while streaming: same vrserver, frames still growing
        let vrserver = steamvr::pids_named("vrserver");
        let f1 = frames();
        thread::sleep(Duration::from_secs(10));
        let h = s.status().headset.unwrap();
        assert!(h.streaming && h.frames_received > f1, "stream stalled: {h:?}");
        assert_eq!(steamvr::pids_named("vrserver"), vrserver, "vrserver restarted while streaming");
        println!("status: {:?}", s.status());
    }));

    // stop and restore even when a check failed, so the machine is never left on v21
    let stopped = s.stop();
    let restored = restore(&paths);
    if let Err(p) = checks {
        panic::resume_unwind(p);
    }
    let stop = stopped.unwrap();
    println!("stop: {stop:?}");
    assert!(stop.remaining_processes.is_empty() && stop.closed_steam != steam_was_running, "{stop:?}");
    restored.unwrap();
    assert_eq!(Snap::take(&paths), before);
    assert!(!paths.restore_file().exists());
}

/// Proves done-when 9 (second half): with no config file, the startup checks pass or say exactly
/// what to set; with the owner's config they all pass. Read-only.
#[test]
#[ignore]
fn live_checks_without_config() {
    let started = std::time::Instant::now();
    let bare = startup_checks(&Config::default(), &Paths::detect(&Config::default()));
    println!("without config ({:?}): {bare:#?}", started.elapsed());
    assert!(bare.iter().all(|c| c.ok || !c.fix.is_empty()), "{bare:#?}");
    let streamer = bare.iter().find(|c| c.name == "alvr_streamer").unwrap();
    assert!(!streamer.ok && streamer.fix.contains("streamer_dir"), "{streamer:?}");

    let cfg = Config::load().unwrap();
    let configured = startup_checks(&cfg, &Paths::detect(&cfg));
    println!("with config: {configured:#?}");
    assert!(configured.iter().all(|c| c.ok), "{configured:#?}");
}
