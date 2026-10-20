use crate::config::{Config, Paths};
use alvr_common::anyhow::{Context, Result, bail};
use alvr_packets::{ClientConnectionsAction, PathValuePair, parse_path};
use alvr_server_io::ServerSessionManager;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
};

/// (section, key) pairs in steamvr.vrsettings that prepare changes and restore puts back.
pub const VRSETTINGS_KEYS: [(&str, &str); 2] =
    [("steamvr", "forcedDriver"), ("driver_alvr_server", "blocked_by_safe_mode")];

const NO_STREAMER: &str = "ALVR streamer not found; set streamer_dir in config.toml";

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
#[serde(default)]
pub struct RestoreState {
    /// The original `external_drivers` value, written back verbatim (null included).
    pub external_drivers: Value,
    /// "section/key" -> original (None = absent)
    pub vrsettings: BTreeMap<String, Option<Value>>,
    pub session: Option<FileState>,
    pub created_files: Vec<PathBuf>,
    /// Some only if it was wrapped
    pub vrcompositor: Option<VrcompositorWrap>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum FileState {
    Absent,
    Contents(String),
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct VrcompositorWrap {
    pub link_target: PathBuf,
    pub real_path: PathBuf,
}

#[derive(Debug)]
pub struct PrepareReport {
    pub snapshot_written: bool,
    pub unblocked_safe_mode: bool,
    pub unwrapped_vrcompositor: bool,
}

/// Built-in defaults, then `cfg.alvr_settings` in key order. Paths start at the session JSON root.
pub fn session_values(cfg: &Config) -> Result<Vec<PathValuePair>> {
    let defaults = [
        ("session_settings.video.preferred_codec.variant", json!("H264")),
        ("session_settings.video.encoder_config.h264_profile.variant", json!("Baseline")),
        ("session_settings.video.foveated_encoding.enabled", json!(false)),
        ("session_settings.video.transcoding_view_resolution.variant", json!("Scale")),
        ("session_settings.video.transcoding_view_resolution.Scale", json!(1.0)),
        ("session_settings.video.emulated_headset_view_resolution.variant", json!("Scale")),
        ("session_settings.video.emulated_headset_view_resolution.Scale", json!(1.0)),
        ("session_settings.video.preferred_fps", json!(72.0)),
        ("session_settings.audio.game_audio.enabled", json!(false)),
        ("session_settings.audio.microphone.enabled", json!(false)),
        ("session_settings.headset.recentering_mode.variant", json!("Stage")),
        ("session_settings.headset.controllers.enabled", json!(true)),
        ("session_settings.headset.controllers.content.tracked", json!(true)),
        ("session_settings.headset.controllers.content.emulation_mode.variant", json!("ValveIndex")),
        ("session_settings.headset.controllers.content.hand_skeleton.enabled", json!(true)),
        ("session_settings.headset.controllers.content.hand_skeleton.content.steamvr_input_2_0", json!(false)),
        ("session_settings.headset.controllers.content.hand_tracking_interaction.enabled", json!(true)),
        ("session_settings.connection.client_discovery.enabled", json!(false)),
    ];
    defaults
        .into_iter()
        .chain(cfg.alvr_settings.iter().map(|(k, v)| (k.as_str(), v.clone())))
        .map(|(key, value)| Ok(PathValuePair { path: parse_path(key)?, value }))
        .collect()
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_str(&fs::read_to_string(path).with_context(|| path.display().to_string())?)
        .with_context(|| path.display().to_string())
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    fs::write(path, serde_json::to_string_pretty(value)?).with_context(|| path.display().to_string())
}

/// Sets `section.key`, or removes it (and its section once empty) for None.
fn put(settings: &mut Value, section: &str, key: &str, value: Option<Value>) {
    match value {
        Some(v) => settings[section][key] = v,
        None => {
            let Some(obj) = settings.get_mut(section).and_then(Value::as_object_mut) else { return };
            obj.remove(key);
            if obj.is_empty() {
                settings.as_object_mut().unwrap().remove(section);
            }
        }
    }
}

/// Port of the ALVR dashboard's `unblock_alvr_driver_within_vrsettings`. True if it was blocked.
pub fn unblock_safe_mode(vrsettings: &Path) -> Result<bool> {
    let (section, key) = VRSETTINGS_KEYS[1];
    let mut settings = read_json(vrsettings)?;
    if settings[section][key] != true {
        return Ok(false);
    }
    put(&mut settings, section, key, Some(json!(false)));
    write_json(vrsettings, &settings)?;
    Ok(true)
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e).with_context(|| path.display().to_string()),
        _ => Ok(()),
    }
}

/// Port of the dashboard's `maybe_unwrap_vrcompositor_launcher` check: Some if vrcompositor is
/// ALVR's wrapper symlink and Valve's binary is found.
fn wrapped_vrcompositor(compositor: &Path) -> Result<Option<VrcompositorWrap>> {
    let Ok(link_target) = fs::read_link(compositor) else { return Ok(None) };
    let bin_dir = compositor.parent().context("vrcompositor has no parent")?;
    let candidates = [alvr_filesystem::original_vrcompositor_path(bin_dir), bin_dir.join("vrcompositor.real")];
    let Some(real_path) = candidates.into_iter().find(|p| p.is_file()) else {
        bail!("vrcompositor is a symlink but Valve's binary was not found next to it; verify SteamVR files in Steam");
    };
    Ok(Some(VrcompositorWrap { link_target, real_path }))
}

fn write_state(paths: &Paths, state: &RestoreState) -> Result<()> {
    fs::create_dir_all(&paths.state_dir)?;
    fs::write(paths.restore_file(), serde_json::to_string_pretty(state)?).context("writing restore.json")
}

fn list_dir(dir: &Path) -> BTreeSet<PathBuf> {
    fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default()
}

/// Switches SteamVR to this streamer's ALVR driver. Writes restore.json first, only if absent.
pub fn prepare(paths: &Paths, values: &[PathValuePair], hostname: &str) -> Result<PrepareReport> {
    let driver_dir = paths.driver_dir().context(NO_STREAMER)?;
    let session_file = paths.session_file().context(NO_STREAMER)?;
    let vrsettings = paths.vrsettings_file().context("Steam not found; set steam_dir in config.toml")?;
    let compositor = paths.vrcompositor().context("SteamVR not found; set steamvr_dir in config.toml")?;
    let config_dir = session_file.parent().context("session file has no parent")?;

    // 1. snapshot
    let snapshot_written = !paths.restore_file().exists();
    let mut state = RestoreState::default();
    if snapshot_written {
        let settings = read_json(&vrsettings)?;
        state = RestoreState {
            external_drivers: alvr_server_io::load_openvr_paths_json()?
                .get("external_drivers")
                .cloned()
                .unwrap_or(Value::Null),
            vrsettings: VRSETTINGS_KEYS
                .iter()
                .map(|(s, k)| (format!("{s}/{k}"), settings.get(s).and_then(|s| s.get(k)).cloned()))
                .collect(),
            session: Some(match session_file.exists() {
                true => FileState::Contents(fs::read_to_string(&session_file)?),
                false => FileState::Absent,
            }),
            created_files: vec![],
            vrcompositor: wrapped_vrcompositor(&compositor)?,
        };
        write_state(paths, &state)?;
    }

    // 2. driver registration, as the dashboard's launch_steamvr does it
    let others: Vec<_> = alvr_server_io::get_registered_drivers()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.to_string_lossy().to_lowercase().contains("alvr") && *p != driver_dir)
        .collect();
    alvr_server_io::driver_registration(&others, false)?;
    alvr_server_io::driver_registration(&[driver_dir], true)?;

    // 3. vrcompositor wrapper
    let wrap = wrapped_vrcompositor(&compositor)?;
    if let Some(wrap) = &wrap {
        fs::remove_file(&compositor)?;
        fs::rename(&wrap.real_path, &compositor)?;
    }

    // 4. vrsettings
    let (section, key) = VRSETTINGS_KEYS[0];
    let mut settings = read_json(&vrsettings)?;
    put(&mut settings, section, key, Some(json!("alvr_server")));
    write_json(&vrsettings, &settings)?;
    let unblocked_safe_mode = unblock_safe_mode(&vrsettings)?;

    // 5. session
    let before = list_dir(config_dir);
    let mut session = ServerSessionManager::new(Some(session_file.clone()));
    session.set_session_values(values.to_vec())?;
    let loopback = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    for action in [
        ClientConnectionsAction::AddIfMissing { trusted: true, manual_ips: loopback.clone() },
        ClientConnectionsAction::Trust,
        ClientConnectionsAction::SetManualIps(loopback),
    ] {
        session.update_client_connections(hostname.into(), action);
    }
    if snapshot_written {
        // ALVR writes session_old.json when it extrapolates an older session
        state.created_files = list_dir(config_dir).difference(&before).cloned().collect();
        write_state(paths, &state)?;
    }

    Ok(PrepareReport { snapshot_written, unblocked_safe_mode, unwrapped_vrcompositor: wrap.is_some() })
}

/// Puts back everything restore.json recorded, then deletes it.
pub fn restore(paths: &Paths) -> Result<()> {
    let state: RestoreState = serde_json::from_str(
        &fs::read_to_string(paths.restore_file()).context("no restore.json: nothing to restore")?,
    )
    .context("parsing restore.json")?;

    let mut json = alvr_server_io::load_openvr_paths_json()?;
    json.as_object_mut()
        .context("openvrpaths.vrpath is not an object")?
        .insert("external_drivers".into(), state.external_drivers.clone());
    alvr_server_io::save_openvr_paths_json(&json)?;

    if !state.vrsettings.is_empty() {
        let path = paths.vrsettings_file().context("Steam not found; set steam_dir in config.toml")?;
        let mut settings = read_json(&path)?;
        for (name, original) in &state.vrsettings {
            let (section, key) = name.split_once('/').context("bad vrsettings key in restore.json")?;
            put(&mut settings, section, key, original.clone());
        }
        write_json(&path, &settings)?;
    }

    if let Some(original) = &state.session {
        let path = paths.session_file().context(NO_STREAMER)?;
        match original {
            FileState::Contents(text) => fs::write(&path, text)?,
            FileState::Absent => remove_file(&path)?,
        }
    }
    for file in &state.created_files {
        remove_file(file)?;
    }

    if let Some(wrap) = &state.vrcompositor {
        let compositor = paths.vrcompositor().context("SteamVR not found; set steamvr_dir in config.toml")?;
        if fs::read_link(&compositor).is_err() {
            fs::rename(&compositor, &wrap.real_path)?;
            std::os::unix::fs::symlink(&wrap.link_target, &compositor)?;
        }
    }

    remove_file(&paths.restore_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alvr_server_io::ServerSessionManager;
    use alvr_session::{
        CodecType, ControllersEmulationMode, H264Profile, RecenteringMode, settings_schema::Switch,
    };

    fn applied(cfg: &Config) -> alvr_common::anyhow::Result<ServerSessionManager> {
        let mut m = ServerSessionManager::new(None);
        m.set_session_values(session_values(cfg)?)?;
        Ok(m)
    }

    #[test]
    fn default_session_values_apply_with_alvr_code() {
        let m = applied(&Config::default()).unwrap();
        let s = m.settings();
        assert!(matches!(s.video.preferred_codec, CodecType::H264));
        assert!(matches!(s.video.encoder_config.h264_profile, H264Profile::Baseline));
        let Switch::Enabled(c) = &s.headset.controllers else { panic!("controllers disabled") };
        assert!(matches!(c.emulation_mode, ControllersEmulationMode::ValveIndex));
        let Switch::Enabled(h) = &c.hand_skeleton else { panic!("hand_skeleton disabled") };
        assert!(!h.steamvr_input_2_0);
        assert!(matches!(c.hand_tracking_interaction, Switch::Enabled(_)));
        assert!(matches!(s.headset.recentering_mode, RecenteringMode::Stage));
        assert!(matches!(s.video.foveated_encoding, Switch::Disabled));
        assert!(matches!(s.audio.game_audio, Switch::Disabled));
        assert!(matches!(s.audio.microphone, Switch::Disabled));
        assert!(matches!(s.connection.client_discovery, Switch::Disabled));
        assert_eq!(s.video.preferred_fps, 72.0);
    }

    #[test]
    fn overrides_follow_defaults() {
        let mut cfg = Config::default();
        cfg.alvr_settings.insert("session_settings.video.preferred_fps".into(), 90.0.into());
        assert_eq!(applied(&cfg).unwrap().settings().video.preferred_fps, 90.0);
    }

    #[test]
    fn overrides_reject_bad_path() {
        let mut cfg = Config::default();
        cfg.alvr_settings.insert("a..b".into(), 1.into());
        let err = session_values(&cfg).unwrap_err();
        assert!(err.to_string().contains("invalid path"), "{err}");
    }

    #[test]
    fn unblock_safe_mode_sets_false_and_reports() {
        let path = std::env::temp_dir().join(format!("vr-test-mcp-vrsettings-{}", std::process::id()));
        fs::write(&path, r#"{"driver_alvr_server":{"blocked_by_safe_mode":true}}"#).unwrap();
        assert!(unblock_safe_mode(&path).unwrap());
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["driver_alvr_server"]["blocked_by_safe_mode"], false);
        assert!(!unblock_safe_mode(&path).unwrap());
        fs::remove_file(&path).unwrap();
    }
}
