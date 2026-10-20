mod common;

use common::Snap;
use std::{panic, path::Path, thread, time::Duration};
use vr_test_mcp::{
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
/// HMD init, so a recreated headset connects without a vrserver restart) and early risk 4 (the
/// manual 127.0.0.1 entry connects with client discovery off). The no-LAN-address case of risk 4
/// is not simulated. Needs Steam closed and ~/.config/vr-test-mcp/config.toml with streamer_dir.
#[test]
#[ignore]
fn live_start_streams_and_stop_cleans() {
    assert!(!steamvr::is_steam_running(), "close Steam first");
    let cfg = Config::load().unwrap();
    let paths = Paths::detect(&cfg);
    let before = Snap::take(&paths);
    let s = Stack::new(cfg, paths.clone());

    let checks = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let r = s.start().unwrap();
        println!("start: {r:?}");
        assert!(r.started_steam && !r.already_streaming && !r.adopted, "{r:?}");
        assert!(r.restarts >= 1, "a fresh session (hash 0) restarts SteamVR once: {r:?}");
        assert_eq!(r.codec.as_deref(), Some("H264"));
        assert!(!steamvr::pids_named("alvr_dashboard").is_empty(), "no alvr_dashboard child");

        let frames = || s.status().headset.unwrap().frames_received;
        let f0 = frames();
        thread::sleep(Duration::from_secs(2));
        assert!(frames() > f0, "no video frames in 2 s");
        assert!(s.start().unwrap().already_streaming);

        let vrserver = steamvr::pids_named("vrserver");
        s.recreate_headset();
        let streams = steamvr::wait_for(Duration::from_secs(30), || s.status().headset.is_some_and(|h| h.streaming));
        assert!(streams, "recreated headset not streaming: {:?}", s.status());
        assert_eq!(steamvr::pids_named("vrserver"), vrserver, "vrserver restarted: risk 1 failed");
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
    assert!(stop.remaining_processes.is_empty() && stop.closed_steam, "{stop:?}");
    restored.unwrap();
    assert_eq!(Snap::take(&paths), before);
    assert!(!paths.restore_file().exists());
}
