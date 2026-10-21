mod common;

use alvr_common::glam::{Quat, Vec3};
use common::Snap;
use std::{fs, panic, path::Path, sync::{Arc, OnceLock}, thread, time::{Duration, Instant}};
use vr_test_mcp::{
    checks::startup_checks,
    config::{Config, Paths},
    headset::Eye,
    rig::{AnimateArgs, PoseArgs, RigAction},
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

/// The shared stack of the live window: starts it when the headset is not streaming and leaves it running.
fn ensure_streaming() -> Arc<Stack> {
    static STACK: OnceLock<Arc<Stack>> = OnceLock::new();
    let stack = STACK.get_or_init(|| {
        let cfg = Config::load().unwrap();
        Stack::new(cfg.clone(), Paths::detect(&cfg))
    });
    if !stack.status().headset.is_some_and(|h| h.streaming) {
        stack.start().unwrap();
    }
    Arc::clone(stack)
}

/// Proves the screenshot path (spec "Screenshots" decoder choice) on a real ALVR 21 VAAPI stream;
/// part of done-when 2. Needs Steam closed or its GPU supported, and SteamVR closed or streaming.
#[test]
#[ignore]
fn live_screenshot_png() {
    let s = ensure_streaming();
    let [view_w, view_h] = s.status().headset.unwrap().view_resolution;
    let decode = |png: &[u8]| {
        let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().unwrap();
        let mut rgb = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut rgb).unwrap();
        (info.width, info.height, rgb)
    };
    for (eye, width) in [(Eye::Both, 2 * view_w), (Eye::Left, view_w)] {
        let started = Instant::now();
        let shot = s.screenshot(eye).unwrap();
        let took = started.elapsed();
        println!("{eye:?}: {}x{} in {took:?}, frame at {} ms", shot.width, shot.height, shot.frame_timestamp_ms);
        assert!(took < Duration::from_secs(3), "{eye:?} took {took:?}");
        let (w, h, rgb) = decode(&shot.png);
        assert_eq!((w, h, shot.width, shot.height), (width, view_h, width, view_h), "{eye:?}");
        let mean = rgb.iter().map(|&b| b as f64).sum::<f64>() / rgb.len() as f64;
        let stddev = (rgb.iter().map(|&b| (b as f64 - mean).powi(2)).sum::<f64>() / rgb.len() as f64).sqrt();
        println!("{eye:?}: pixel stddev {stddev:.2}");
        assert!(stddev > 0.0, "{eye:?} image is uniform");
    }
}

/// Proves early risk 6 (the host-side OpenVR client reaches vrserver in its container) and the
/// get_tracked_state half of done-when 3. Needs Steam closed or its GPU supported, and SteamVR closed or streaming.
#[test]
#[ignore]
fn live_tracked_head_matches() {
    let s = ensure_streaming();
    let state = s.tracked_state(false).unwrap();
    println!("{state:#?}\nscene app: {:?}", s.scene_app());
    let hmds: Vec<_> = state.devices.iter().filter(|d| d.class == "hmd").collect();
    assert_eq!(hmds.len(), 1, "{hmds:?}");
    let [x, y, z] = hmds[0].position;
    assert!(hmds[0].pose_valid, "{:?}", hmds[0]);
    assert!(x.abs() < 0.01 && (y - 1.6).abs() < 0.01 && z.abs() < 0.01, "{:?}", hmds[0]);
    let mut roles: Vec<_> = state.devices.iter().filter(|d| d.class == "controller").map(|d| d.role.as_str()).collect();
    roles.sort();
    // pose_valid may stay false until Task 6 sends controller poses
    assert_eq!(roles, ["left", "right"], "{:?}", state.devices);
}

/// Proves done-when 3: head moves show in get_tracked_state and in the screenshot (and the
/// deferred Task 4 check that the screenshot changes between two head poses). Needs Steam closed
/// or its GPU supported, and SteamVR closed or streaming.
#[test]
#[ignore]
fn live_head_move_tracked() {
    let s = ensure_streaming();
    // later tests in this process expect the default head pose, also after a failed assertion
    struct Reset(Arc<Stack>);
    impl Drop for Reset {
        fn drop(&mut self) {
            let default = PoseArgs { position: [0.0, 1.6, 0.0], ..Default::default() };
            self.0.apply(RigAction::SetHeadPose(default)).ok(); // also stops an animation
        }
    }
    let _reset = Reset(Arc::clone(&s));
    let set = |position, yaw| s.apply(RigAction::SetHeadPose(PoseArgs { position, yaw, ..Default::default() })).unwrap();
    let hmd = || s.tracked_state(false).unwrap().devices.into_iter().find(|d| d.class == "hmd").expect("no hmd");
    let pixels = |eye| {
        let png = s.screenshot(eye).unwrap().png;
        let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().unwrap();
        let mut rgb = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut rgb).unwrap();
        rgb
    };

    set([0.0, 1.6, 0.0], None);
    thread::sleep(Duration::from_millis(300));
    let a = pixels(Eye::Left);
    set([0.3, 1.7, -0.2], Some(90.0));
    thread::sleep(Duration::from_millis(300));
    let d = hmd();
    let forward = Quat::from_array(d.quat) * Vec3::NEG_Z;
    let yaw = (-forward.x).atan2(-forward.z).to_degrees();
    println!("hmd {:?} yaw {yaw:.2}", d.position);
    assert!(Vec3::from_array(d.position).distance(Vec3::new(0.3, 1.7, -0.2)) < 0.01, "{d:?}");
    assert!((yaw - 90.0).abs() < 1.0, "yaw {yaw}: {d:?}");
    let b = pixels(Eye::Left);
    assert_eq!(a.len(), b.len());
    let diff = a.iter().zip(&b).map(|(&x, &y)| (x as f64 - y as f64).abs()).sum::<f64>() / a.len() as f64;
    println!("mean absolute pixel difference {diff:.2}");
    // 7.1 measured on the dark SteamVR Home between yaw 0 and yaw 90
    assert!(diff > 4.0, "the screenshot did not change with the head pose: mean abs diff {diff:.2}, want > 4");

    let waypoints = vec![
        PoseArgs { position: [0.0, 1.6, -0.5], ..Default::default() },
        PoseArgs { position: [0.3, 1.6, 0.0], yaw: Some(-45.0), ..Default::default() },
    ];
    let started = Instant::now();
    let done_after = s.apply(RigAction::AnimateHead(AnimateArgs { waypoints, duration_s: 2.0 })).unwrap();
    assert_eq!(done_after, Duration::from_secs(2));
    for at in [0.5, 1.5] {
        thread::sleep(Duration::from_secs_f32(at).saturating_sub(started.elapsed()));
        let d = hmd();
        println!("at {at} s: {d:?}");
        assert!(Vec3::from_array(d.linear_velocity).length() > 0.05, "no velocity mid-animation: {d:?}");
    }
    thread::sleep(done_after.saturating_sub(started.elapsed()) + Duration::from_millis(300));
    let d = hmd();
    assert!(Vec3::from_array(d.position).distance(Vec3::new(0.3, 1.6, 0.0)) < 0.01, "{d:?}");
}

/// Proves early risk 5 (Beat Saber runs on the integrated GPU at the MCP's default resolution). Needs env
/// VR_TEST_GAME_ID: the Beat Saber Steam app id or its non-Steam shortcut game id.
#[test]
#[ignore]
fn live_beat_saber_menu_fps() {
    let id = std::env::var("VR_TEST_GAME_ID").expect("set VR_TEST_GAME_ID");
    let s = ensure_streaming();
    steamvr::launch(&Config::load().unwrap(), &format!("steam://rungameid/{id}")).unwrap();
    let running = steamvr::wait_for(Duration::from_secs(180), || s.scene_app().is_some_and(|a| a.state == "running"));
    let app = s.scene_app();
    assert!(running, "scene app not running after 180 s: {app:?}");
    thread::sleep(Duration::from_secs(20));
    let f0 = s.frame_stats().expect("frame stats");
    thread::sleep(Duration::from_secs(30));
    let f1 = s.frame_stats().expect("frame stats");
    steamvr::terminate(app.unwrap().pid, libc::SIGTERM);
    let presents = f1.presents - f0.presents;
    let bad = (f1.dropped - f0.dropped) + (f1.reprojected - f0.reprojected);
    let ratio = bad as f64 / presents.max(1) as f64;
    println!("{f0:?} -> {f1:?}: {bad} dropped or reprojected of {presents} presents ({ratio:.3})");
    assert!(presents > 0 && ratio < 0.10, "ratio {ratio:.3}");
}
