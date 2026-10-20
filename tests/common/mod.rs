#![allow(dead_code)]
use std::{collections::BTreeSet, fs, path::PathBuf};
use vr_test_mcp::{config::Paths, setup::VRSETTINGS_KEYS};

/// What a round trip must give back. SteamVR rewrites the rest of steamvr.vrsettings, so only
/// VRSETTINGS_KEYS are compared; openvrpaths.vrpath is compared parsed, as ALVR rewrites it.
#[derive(Debug, PartialEq)]
pub struct Snap {
    pub openvrpaths: serde_json::Value,
    pub vrsettings: Vec<Option<serde_json::Value>>,
    pub session: Option<String>,
    pub vrcompositor_target: Option<PathBuf>,
    pub vrcompositor_real_exists: bool,
    pub config_dir_files: BTreeSet<PathBuf>,
}

impl Snap {
    pub fn take(paths: &Paths) -> Snap {
        let vrsettings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(paths.vrsettings_file().unwrap()).unwrap()).unwrap();
        let session_file = paths.session_file().unwrap();
        let compositor = paths.vrcompositor().unwrap();
        Snap {
            openvrpaths: alvr_server_io::load_openvr_paths_json().unwrap(),
            vrsettings: VRSETTINGS_KEYS.iter().map(|(s, k)| vrsettings.get(s).and_then(|s| s.get(k)).cloned()).collect(),
            session: fs::read_to_string(&session_file).ok(),
            vrcompositor_target: fs::read_link(&compositor).ok(),
            vrcompositor_real_exists: compositor.with_file_name("vrcompositor.real").exists(),
            config_dir_files: fs::read_dir(session_file.parent().unwrap())
                .map(|d| d.flatten().map(|e| e.path()).collect())
                .unwrap_or_default(),
        }
    }
}
