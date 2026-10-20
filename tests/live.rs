mod common;

use common::Snap;
use vr_test_mcp::{
    config::{Config, Paths},
    setup::{prepare, restore, session_values},
};

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
