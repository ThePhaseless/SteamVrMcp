mod common;

use common::Snap;
use serde_json::{Value, json};
use std::{fs, os::unix::fs::symlink, path::PathBuf, sync::Mutex};
use vr_test_mcp::{
    config::{Config, Paths},
    setup::{prepare, restore, session_values},
};

// The tests set XDG_CONFIG_HOME, which is process-wide.
static ENV: Mutex<()> = Mutex::new(());
const HOST: &str = "1234.client.local.";

struct Fixture {
    _guard: std::sync::MutexGuard<'static, ()>,
    root: PathBuf,
    paths: Paths,
}

fn fixture(name: &str) -> Fixture {
    let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let root = std::env::temp_dir().join(format!("vr-test-mcp-setup-{}-{name}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    let xdg = root.join("xdg");
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };

    fs::create_dir_all(xdg.join("openvr")).unwrap();
    let v20 = root.join("alvr20/lib64/alvr");
    fs::write(
        xdg.join("openvr/openvrpaths.vrpath"),
        json!({"external_drivers": [v20], "jsonid": "vrpathreg", "version": 1}).to_string(),
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
    let f = fixture("roundtrip");
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
    let f = fixture("twice");
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
    let f = fixture("fileonly");
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
