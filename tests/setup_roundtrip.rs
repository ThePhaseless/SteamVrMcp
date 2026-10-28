mod common;

use common::Snap;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    sync::Mutex,
};
use vr_test_mcp::{
    config::{Config, Paths},
    setup::{prepare, refresh_stale_snapshot, restore, session_values, snapshot},
};

// The tests set XDG_CONFIG_HOME, which is process-wide.
static ENV: Mutex<()> = Mutex::new(());
const HOST: &str = "1234.client.local.";

struct Fixture {
    _guard: std::sync::MutexGuard<'static, ()>,
    root: PathBuf,
    paths: Paths,
}

fn fixture(name: &str, external_drivers: impl FnOnce(&PathBuf) -> Value) -> Fixture {
    let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let root = std::env::temp_dir().join(format!("vr-test-mcp-setup-{}-{name}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    let xdg = root.join("xdg");
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };

    fs::create_dir_all(xdg.join("openvr")).unwrap();
    let v20 = root.join("alvr20/lib64/alvr");
    fs::write(
        xdg.join("openvr/openvrpaths.vrpath"),
        json!({"external_drivers": external_drivers(&v20), "jsonid": "vrpathreg", "version": 1}).to_string(),
    )
    .unwrap();

    fs::create_dir_all(root.join("steam/config")).unwrap();
    fs::write(root.join("steam/config/steamvr.vrsettings"), r#"{"steamvr":{"x":1}}"#).unwrap();

    let bin = root.join("steamvr/bin/linux64");
    fs::create_dir_all(&bin).unwrap();
    fs::write(root.join("wrapper"), "wrapper").unwrap();
    symlink(root.join("wrapper"), bin.join("vrcompositor")).unwrap();
    fs::write(bin.join("vrcompositor.real"), "real").unwrap();

    let driver = root.join("alvr21/lib64/alvr");
    fs::create_dir_all(&driver).unwrap();
    fs::write(driver.join("driver.vrdrivermanifest"), r#"{"name":"alvr_server"}"#).unwrap();

    fs::create_dir_all(xdg.join("alvr")).unwrap();
    fs::write(xdg.join("alvr/session.json"), "{}").unwrap();

    let paths = Paths {
        steam_dir: Some(root.join("steam")),
        flatpak_steam: false,
        steamvr_dir: Some(root.join("steamvr")),
        streamer_dir: Some(root.join("alvr21")),
        state_dir: root.join("state"),
        runtime_dir: root.join("run"),
    };
    Fixture { _guard: guard, root, paths }
}

#[test]
fn prepare_then_restore_round_trips() {
    let f = fixture("roundtrip", |v20| json!([v20]));
    let before = Snap::take(&f.paths);
    let values = session_values(&Config::default()).unwrap();
    let report = prepare(&f.paths, &values, HOST).unwrap();
    assert!(report.snapshot_written && report.unwrapped_vrcompositor);

    let after = Snap::take(&f.paths);
    assert_eq!(after.openvrpaths["external_drivers"], json!([f.paths.driver_dir().unwrap()]));
    assert!(after.vrcompositor_target.is_none() && !after.vrcompositor_real_exists);
    assert!(f.paths.vrcompositor().unwrap().is_file());
    assert_eq!(after.vrsettings[0], Some(json!("alvr_server")));
    let session: Value = serde_json::from_str(after.session.as_deref().unwrap()).unwrap();
    assert_eq!(session["client_connections"][HOST]["trusted"], true);
    assert_eq!(session["client_connections"][HOST]["manual_ips"], json!(["127.0.0.1"]));
    assert!(f.paths.restore_file().exists());

    restore(&f.paths).unwrap();
    assert_eq!(Snap::take(&f.paths), before);
    assert!(!f.paths.restore_file().exists());
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn prepare_twice_keeps_first_snapshot() {
    let f = fixture("twice", |v20| json!([v20]));
    let values = session_values(&Config::default()).unwrap();
    prepare(&f.paths, &values, HOST).unwrap();
    let first = fs::read(f.paths.restore_file()).unwrap();
    let report = prepare(&f.paths, &values, HOST).unwrap();
    assert!(!report.snapshot_written && !report.unwrapped_vrcompositor);
    assert_eq!(fs::read(f.paths.restore_file()).unwrap(), first);
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn restore_from_file_only() {
    let f = fixture("fileonly", |v20| json!([v20]));
    let before = Snap::take(&f.paths);
    let values = session_values(&Config::default()).unwrap();
    prepare(&f.paths, &values, HOST).unwrap();
    // v20 session `{}` gets extrapolated, so ALVR leaves session_old.json behind
    let created = f.paths.session_file().unwrap().with_file_name("session_old.json");
    assert!(created.exists());
    drop(values);

    let fresh = Paths { ..f.paths.clone() };
    restore(&fresh).unwrap();
    assert_eq!(Snap::take(&fresh), before);
    assert!(!created.exists() && !fresh.restore_file().exists());
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn restore_deletes_files_created_after_an_earlier_snapshot() {
    let f = fixture("snapfirst", |v20| json!([v20]));
    let before = Snap::take(&f.paths);
    // a cold vr_start snapshots before prepare, so it can clear safe-mode blocks in between
    snapshot(&f.paths).unwrap();
    prepare(&f.paths, &session_values(&Config::default()).unwrap(), HOST).unwrap();
    let created = f.paths.session_file().unwrap().with_file_name("session_old.json");
    assert!(created.exists());

    restore(&f.paths).unwrap();
    assert_eq!(Snap::take(&f.paths), before);
    assert!(!created.exists());
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn null_external_drivers_round_trips() {
    let f = fixture("nulldrivers", |_| Value::Null);
    let before = Snap::take(&f.paths);
    assert_eq!(before.openvrpaths["external_drivers"], Value::Null);
    let values = session_values(&Config::default()).unwrap();
    prepare(&f.paths, &values, HOST).unwrap();
    assert_eq!(Snap::take(&f.paths).openvrpaths["external_drivers"], json!([f.paths.driver_dir().unwrap()]));
    restore(&f.paths).unwrap();
    assert_eq!(Snap::take(&f.paths), before);
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn restore_puts_back_whole_vrsettings() {
    let f = fixture("wholefile", |v20| json!([v20]));
    let file = f.paths.vrsettings_file().unwrap();
    let original = fs::read(&file).unwrap();
    let values = session_values(&Config::default()).unwrap();
    prepare(&f.paths, &values, HOST).unwrap();
    // what SteamVR does on a signalled exit
    let mut v: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    v["driver_gamepad"] = json!({"blocked_by_safe_mode": true});
    v["steamvr"]["ipd"] = json!(0.0634);
    fs::write(&file, v.to_string()).unwrap();

    restore(&f.paths).unwrap();
    assert_eq!(fs::read(&file).unwrap(), original);
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn absent_vrsettings_is_deleted_on_restore() {
    let f = fixture("absentfile", |v20| json!([v20]));
    let file = f.paths.vrsettings_file().unwrap();
    let values = session_values(&Config::default()).unwrap();
    prepare(&f.paths, &values, HOST).unwrap();
    let mut state: Value = serde_json::from_slice(&fs::read(f.paths.restore_file()).unwrap()).unwrap();
    state["vrsettings_file"] = json!("Absent");
    fs::write(f.paths.restore_file(), state.to_string()).unwrap();
    restore(&f.paths).unwrap();
    assert!(!file.exists());
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn old_restore_json_still_restores() {
    let f = fixture("oldjson", |v20| json!([v20]));
    let before = Snap::take(&f.paths);
    let values = session_values(&Config::default()).unwrap();
    prepare(&f.paths, &values, HOST).unwrap();
    // a restore.json from before vrsettings_file existed: per-key map only
    let mut state: Value = serde_json::from_slice(&fs::read(f.paths.restore_file()).unwrap()).unwrap();
    let obj = state.as_object_mut().unwrap();
    obj.remove("vrsettings_file");
    obj.remove("paths");
    obj.remove("session_backups");
    obj.insert(
        "vrsettings".into(),
        json!({"steamvr/forcedDriver": null, "driver_alvr_server/blocked_by_safe_mode": null}),
    );
    fs::write(f.paths.restore_file(), state.to_string()).unwrap();

    restore(&f.paths).unwrap();
    assert_eq!(Snap::take(&f.paths), before);
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn missing_owner_file_is_named() {
    let f = fixture("missing", |v20| json!([v20]));
    let vrsettings = f.paths.vrsettings_file().unwrap();
    fs::remove_file(&vrsettings).unwrap();
    let e = format!("{:#}", prepare(&f.paths, &session_values(&Config::default()).unwrap(), HOST).unwrap_err());
    assert!(e.contains(&vrsettings.display().to_string()), "{e}");
    fs::remove_dir_all(&f.root).ok();
}

fn prep(paths: &Paths) {
    prepare(paths, &session_values(&Config::default()).unwrap(), HOST).unwrap();
}

#[test]
fn torn_restore_json_stops_prepare() {
    let f = fixture("torn", |v20| json!([v20]));
    let before = Snap::take(&f.paths);
    fs::create_dir_all(&f.paths.state_dir).unwrap();
    fs::write(f.paths.restore_file(), r#"{"external_drivers": ["#).unwrap();
    let e = format!("{:#}", prepare(&f.paths, &session_values(&Config::default()).unwrap(), HOST).unwrap_err());
    assert!(e.contains(&f.paths.restore_file().display().to_string()) && e.contains("delete"), "{e}");
    assert_eq!(Snap::take(&f.paths), before, "prepare changed nothing");
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn restore_uses_the_paths_recorded_at_snapshot() {
    let f = fixture("recorded", |v20| json!([v20]));
    let before = Snap::take(&f.paths);
    prep(&f.paths);
    // a partial restore unregistered ALVR, or the config changed since: the current Paths know nothing
    let lost = Paths { steam_dir: None, steamvr_dir: None, streamer_dir: None, ..f.paths.clone() };
    restore(&lost).unwrap();
    assert_eq!(Snap::take(&f.paths), before);
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn prepare_refuses_when_the_paths_changed_since_the_snapshot() {
    let f = fixture("moved", |v20| json!([v20]));
    prep(&f.paths);
    let other = f.root.join("steam2");
    fs::create_dir_all(other.join("config")).unwrap();
    fs::write(other.join("config/steamvr.vrsettings"), "{}").unwrap();
    let moved = Paths { steam_dir: Some(other.clone()), ..f.paths.clone() };
    let e = format!("{:#}", prepare(&moved, &session_values(&Config::default()).unwrap(), HOST).unwrap_err());
    assert!(e.contains("vr_restore") && e.contains(&other.display().to_string()), "{e}");
    assert_eq!(fs::read_to_string(other.join("config/steamvr.vrsettings")).unwrap(), "{}");
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn restore_finishes_after_a_crash_between_compositor_steps() {
    let f = fixture("crashmid", |v20| json!([v20]));
    let before = Snap::take(&f.paths);
    prep(&f.paths);
    // a restore killed after moving Valve's binary back, before the symlink
    let compositor = f.paths.vrcompositor().unwrap();
    fs::rename(&compositor, compositor.with_file_name("vrcompositor.real")).unwrap();
    restore(&f.paths).unwrap();
    assert_eq!(Snap::take(&f.paths), before);
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn restore_retried_after_a_failure_finishes_and_is_not_stale() {
    let f = fixture("retry", |v20| json!([v20]));
    let before = Snap::take(&f.paths);
    prep(&f.paths);
    let bin = f.paths.vrcompositor().unwrap().parent().unwrap().to_owned();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o555)).unwrap();
    let failed = restore(&f.paths);
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed.is_err(), "the vrcompositor step fails");
    // the driver list and forcedDriver are back, but the setup is half restored
    assert!(!refresh_stale_snapshot(&f.paths).unwrap(), "a partial restore is not stale");
    restore(&f.paths).unwrap();
    assert_eq!(Snap::take(&f.paths), before);
    assert!(!f.paths.restore_file().exists());
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn restore_skips_the_rewrap_when_the_wrapper_is_gone() {
    let f = fixture("nowrapper", |v20| json!([v20]));
    prep(&f.paths);
    fs::remove_file(f.root.join("wrapper")).unwrap();
    let notes = restore(&f.paths).unwrap();
    let compositor = f.paths.vrcompositor().unwrap();
    assert!(fs::symlink_metadata(&compositor).unwrap().is_file(), "Valve's binary stays in place");
    assert!(notes.iter().any(|n| n.contains("vrcompositor")), "{notes:?}");
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn restore_puts_back_the_session_old_json_alvr_overwrites() {
    let f = fixture("sessionold", |v20| json!([v20]));
    let config = f.paths.session_file().unwrap().parent().unwrap().to_owned();
    fs::write(config.join("session_old.json"), "mine").unwrap();
    prep(&f.paths); // `{}` is extrapolated, so ALVR overwrites session_old.json
    assert_ne!(fs::read_to_string(config.join("session_old.json")).unwrap(), "mine");
    restore(&f.paths).unwrap();
    assert_eq!(fs::read_to_string(config.join("session_old.json")).unwrap(), "mine");
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn restore_puts_back_the_session_invalid_json_alvr_overwrites() {
    let f = fixture("sessioninvalid", |v20| json!([v20]));
    let config = f.paths.session_file().unwrap().parent().unwrap().to_owned();
    fs::write(config.join("session.json"), "not json").unwrap();
    fs::write(config.join("session_invalid.json"), "mine").unwrap();
    prep(&f.paths);
    assert_eq!(fs::read_to_string(config.join("session_invalid.json")).unwrap(), "not json");
    restore(&f.paths).unwrap();
    assert_eq!(fs::read_to_string(config.join("session_invalid.json")).unwrap(), "mine");
    assert_eq!(fs::read_to_string(config.join("session.json")).unwrap(), "not json");
    fs::remove_dir_all(&f.root).ok();
}

#[test]
fn a_stale_snapshot_is_replaced() {
    let f = fixture("stale", |v20| json!([v20]));
    prep(&f.paths);
    assert!(!refresh_stale_snapshot(&f.paths).unwrap(), "the MCP's changes are in place");
    // the session ended without vr_restore; the user put SteamVR back by hand and kept using it
    let vrpath = f.root.join("xdg/openvr/openvrpaths.vrpath");
    let mut openvr: Value = serde_json::from_str(&fs::read_to_string(&vrpath).unwrap()).unwrap();
    openvr["external_drivers"] = json!([f.root.join("alvr20/lib64/alvr")]);
    fs::write(&vrpath, openvr.to_string()).unwrap();
    let vrsettings = f.paths.vrsettings_file().unwrap();
    fs::write(&vrsettings, r#"{"steamvr":{"x":2}}"#).unwrap();
    let now = Snap::take(&f.paths);

    assert!(refresh_stale_snapshot(&f.paths).unwrap());
    restore(&f.paths).unwrap();
    assert_eq!(fs::read_to_string(&vrsettings).unwrap(), r#"{"steamvr":{"x":2}}"#, "the user's changes survive");
    assert_eq!(Snap::take(&f.paths), now);
    fs::remove_dir_all(&f.root).ok();
}
