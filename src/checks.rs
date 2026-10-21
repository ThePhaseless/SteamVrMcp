use crate::{alvr_api, config::{Config, Paths}, steamvr};
use alvr_common::{ALVR_VERSION, semver::Version};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    ffi::{c_int, c_void},
    fs,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub reason: String,
    pub fix: String,
}

/// What the tools need, as of now: failed startup checks by name, plus runtime state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Readiness {
    pub failed: BTreeMap<&'static str, String>,
    pub restore_file: bool,
    pub steamvr_running: bool,
    pub alvr_api: bool,
    pub streaming: bool,
    /// A vr_start session, Steam started by the MCP, SteamVR with ALVR, or the MCP's headset.
    pub running: bool,
}

const CORE: [&str; 6] =
    ["steam_native", "steamvr_installed", "alvr_streamer", "alvr_dashboard_absent", "audio_input", "gpu_direct_mode"];
const AMD: u32 = 0x1002;
const INTEL: u32 = 0x8086;
const NVIDIA: u32 = 0x10de;

/// Ok, or the first missing need of `tool`.
pub fn tool_ready(tool: &str, r: &Readiness) -> Result<(), String> {
    let checks = |names: &[&str]| match names.iter().find_map(|n| Some((n, r.failed.get(n)?))) {
        Some((name, reason)) => Err(format!("check {name} failed: {reason}")),
        None => Ok(()),
    };
    let holds = |ok: bool, missing: &str| if ok { Ok(()) } else { Err(missing.to_owned()) };
    let streaming = || holds(r.streaming, "headset is not streaming");
    match tool {
        "vr_status" | "get_logs" => Ok(()),
        "vr_start" => checks(&CORE),
        "vr_stop" => holds(r.running, "nothing to stop: no SteamVR with ALVR, headset or MCP-started Steam"),
        "vr_restore" => holds(r.restore_file, "no restore.json: nothing to restore"),
        "set_alvr_settings" | "set_hand_tracking_flavour" => {
            checks(&CORE)?;
            holds(r.alvr_api, "the ALVR server is not running")
        }
        "set_head_pose" | "move_head" | "animate_head" | "set_controller_pose" | "move_controller"
        | "animate_controller" | "set_controller_state" | "press_button" | "release_button" | "touch_button"
        | "set_trigger" | "set_grip" | "set_thumbstick" | "set_trackpad" | "set_finger_curl" | "set_hand_mode"
        | "set_hand_skeleton" | "set_hand_gesture" | "run_sequence" | "set_playspace" | "recenter"
        | "get_haptics" => streaming(),
        "screenshot" => {
            streaming()?;
            checks(&["h264_encoder", "screenshot_decoder"])
        }
        "get_tracked_state" => {
            holds(r.steamvr_running, "SteamVR is not running")?;
            checks(&["openvr_client"])
        }
        "launch_app" | "stop_app" => {
            streaming()?;
            checks(&["openvr_client"])
        }
        _ => Err("unknown tool".to_owned()),
    }
}

fn check(name: &'static str, result: Result<String, String>, fix: impl Into<String>) -> Check {
    let ok = result.is_ok();
    let (Ok(reason) | Err(reason)) = result;
    Check { name, ok, reason, fix: fix.into() }
}

/// Quick prerequisite checks without side effects.
pub fn startup_checks(cfg: &Config, paths: &Paths) -> Vec<Check> {
    let steam = match (&paths.steam_dir, paths.flatpak_steam) {
        (_, true) => Err("Flatpak Steam is not supported in v1".to_owned()),
        (Some(dir), false) => Ok(dir.display().to_string()),
        (None, false) => Err("Steam not found".to_owned()),
    };
    let steamvr = match &paths.steamvr_dir {
        Some(dir) if dir.join("bin/linux64/vrserver").is_file() => Ok(dir.display().to_string()),
        Some(dir) => Err(format!("no bin/linux64/vrserver in {}", dir.display())),
        None => Err("SteamVR not found".to_owned()),
    };
    let audio = alvr_audio::new_input(None)
        .and_then(|device| alvr_audio::input_sample_rate(&device))
        .map(|rate| format!("default input at {rate} Hz"))
        .map_err(|e| format!("{e:#}"));
    let openvr = paths.steamvr_dir.as_ref().ok_or_else(|| "SteamVR not found".to_owned()).and_then(|dir| {
        let lib = dir.join("bin/linux64/libopenvr_api.so");
        unsafe { libloading::Library::new(&lib) }.map(|_| lib.display().to_string()).map_err(|e| e.to_string())
    });
    let decoder = openh264::decoder::Decoder::new().map(|_| "OpenH264 decoder".to_owned()).map_err(|e| e.to_string());
    let (gpu, vendor) = gpu_verdict(&gpu_vendors(), cfg.steam.env.is_empty());
    let encoder = vendor
        .ok_or_else(|| "no AMD or Intel GPU".to_owned())
        .and_then(|v| render_node(v).ok_or_else(|| format!("no render node for GPU vendor {v:#06x}")))
        .and_then(|node| va_h264(&node));
    vec![
        check("steam_native", steam, "install native Steam or set steam_dir in config.toml"),
        check("steamvr_installed", steamvr, "install SteamVR (app 250820) and run it once, or set steamvr_dir"),
        check(
            "alvr_streamer",
            streamer(paths),
            format!("install the ALVR {} streamer and set streamer_dir = \"<path to alvr_streamer_linux>\"", *ALVR_VERSION),
        ),
        dashboard_check(),
        check("audio_input", audio, "enable a default audio input; ALVR's client opens it at every connection"),
        check(
            "gpu_direct_mode",
            gpu,
            "run SteamVR on an AMD or Intel GPU; hybrid users set steam.env (e.g. MESA_VK_DEVICE_SELECT)",
        ),
        check("h264_encoder", encoder, "install VA-API drivers with H.264 encoding"),
        check("openvr_client", openvr, "reinstall SteamVR"),
        check("screenshot_decoder", decoder, "rebuild with the openh264 crate"),
    ]
}

fn streamer(paths: &Paths) -> Result<String, String> {
    let dir = paths.driver_dir().ok_or("ALVR streamer not found")?;
    let manifest: Option<serde_json::Value> =
        fs::read_to_string(dir.join("driver.vrdrivermanifest")).ok().and_then(|t| serde_json::from_str(&t).ok());
    if manifest.is_none_or(|m| m["name"] != "alvr_server") {
        return Err(format!("no ALVR driver.vrdrivermanifest in {}", dir.display()));
    }
    let so = dir.join("bin/linux64/driver_alvr_server.so");
    let bytes = fs::read(&so).map_err(|e| format!("{}: {e}", so.display()))?;
    // shortcut: version found by byte search in the driver; vr_start re-checks via /api/version, replace when ALVR 21 stable ships
    let found = driver_versions(&bytes);
    match (found.iter().find(|v| alvr_api::compatible(v)), found.is_empty()) {
        (Some(v), _) => Ok(format!("ALVR {v} in {}", dir.display())),
        (None, true) => Err(format!("{} is not an ALVR {} driver", so.display(), ALVR_VERSION.major)),
        (None, false) => Err(format!("{} is ALVR {}, not compatible with {}", so.display(), found.join(", "), *ALVR_VERSION)),
    }
}

/// Strings `M.m.p` or `M.m.p-pre` with our major in `bytes`, as ALVR's version sits in the driver's rodata.
fn driver_versions(bytes: &[u8]) -> Vec<String> {
    let major = format!("{}.", ALVR_VERSION.major);
    let mut found: Vec<String> = (0..bytes.len())
        .filter(|&i| bytes[i..].starts_with(major.as_bytes()) && (i == 0 || !bytes[i - 1].is_ascii_digit()))
        .filter_map(|i| {
            let run: String = bytes[i..]
                .iter()
                .map(|&b| b as char)
                .take_while(|c| c.is_ascii_alphanumeric() || ".-".contains(*c))
                .collect();
            let core = &run[..run.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(run.len())];
            let version = if run[core.len()..].starts_with('-') { &run } else { core };
            Version::parse(version).is_ok().then(|| version.to_owned())
        })
        .collect();
    found.dedup();
    found
}

/// The one static check cheap enough for every vr_status.
pub fn dashboard_check() -> Check {
    check("alvr_dashboard_absent", dashboard_absent(), "close the ALVR dashboard (or the other vr-test-mcp)")
}

fn parent_of(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(1)?.parse().ok()
}

/// Our own `alvr_dashboard` child is fine; any other one (the real dashboard, another vr-test-mcp) is not.
fn dashboard_absent() -> Result<String, String> {
    let others: Vec<String> = steamvr::pids_named("alvr_dashboard")
        .into_iter()
        .filter(|&pid| parent_of(pid) != Some(std::process::id()))
        .map(|pid| format!("pid {pid} ({})", steamvr::exe_of(pid).unwrap_or_default().display()))
        .collect();
    if others.is_empty() { Ok("none running".to_owned()) } else { Err(format!("alvr_dashboard running: {}", others.join(", "))) }
}

/// PCI vendor of each GPU (`card<N>`), from sysfs, so the check never wakes a sleeping dGPU.
fn gpu_vendors() -> Vec<u32> {
    let Ok(dir) = fs::read_dir("/sys/class/drm") else { return vec![] };
    let mut cards: Vec<String> = dir
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.strip_prefix("card").is_some_and(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit())))
        .collect();
    cards.sort();
    cards
        .iter()
        .filter_map(|c| fs::read_to_string(format!("/sys/class/drm/{c}/device/vendor")).ok())
        .filter_map(|v| u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok())
        .collect()
}

/// Port of ALVR's `linux_hardware_checks` over PCI vendors: the result, and the vendor to encode on.
fn gpu_verdict(vendors: &[u32], steam_env_empty: bool) -> (Result<String, String>, Option<u32>) {
    let name = |v: &u32| match *v {
        AMD => "AMD".to_owned(),
        INTEL => "Intel".to_owned(),
        NVIDIA => "NVIDIA".to_owned(),
        other => format!("{other:#06x}"),
    };
    let names = vendors.iter().map(name).collect::<Vec<_>>().join(", ");
    // shortcut: cannot see which GPU Steam will use; vr_start reports compositor errors
    let Some(supported) = vendors.iter().find(|v| [AMD, INTEL].contains(v)) else {
        return match vendors.contains(&NVIDIA) {
            true => (Err("ALVR 21 direct mode does not work on NVIDIA (ALVR #3421)".to_owned()), None),
            false => (Err(format!("no AMD or Intel GPU in /sys/class/drm, found [{names}]")), None),
        };
    };
    let reason = match (vendors.len(), steam_env_empty) {
        (1, _) => format!("{names} GPU"),
        (_, true) => format!("hybrid [{names}]: steam.env is empty, so SteamVR may not run on the {} GPU", name(supported)),
        (_, false) => format!("hybrid [{names}]: SteamVR must run on the {} GPU through steam.env", name(supported)),
    };
    (Ok(reason), Some(*supported))
}

fn render_node(vendor: u32) -> Option<PathBuf> {
    let mut nodes: Vec<String> = fs::read_dir("/sys/class/drm")
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("renderD"))
        .collect();
    nodes.sort();
    let matches = |n: &String| {
        fs::read_to_string(format!("/sys/class/drm/{n}/device/vendor")).is_ok_and(|v| v.trim() == format!("{vendor:#06x}"))
    };
    Some(Path::new("/dev/dri").join(nodes.into_iter().find(matches)?))
}

/// Whether libva on `node` offers an H.264 encoder ALVR can use.
fn va_h264(node: &Path) -> Result<String, String> {
    type Display = *mut c_void;
    const PROFILES: [c_int; 2] = [13, 6]; // VAProfileH264ConstrainedBaseline, VAProfileH264Main
    const ENTRYPOINTS: [c_int; 2] = [6, 8]; // VAEntrypointEncSlice, VAEntrypointEncSliceLP
    let file = fs::File::options().read(true).write(true).open(node).map_err(|e| format!("{}: {e}", node.display()))?;
    unsafe {
        let va = libloading::Library::new("libva.so.2").map_err(|e| e.to_string())?;
        let va_drm = libloading::Library::new("libva-drm.so.2").map_err(|e| e.to_string())?;
        let get_display = va_drm.get::<unsafe extern "C" fn(c_int) -> Display>(b"vaGetDisplayDRM").map_err(|e| e.to_string())?;
        let initialize = va.get::<unsafe extern "C" fn(Display, *mut c_int, *mut c_int) -> c_int>(b"vaInitialize").map_err(|e| e.to_string())?;
        let max_entrypoints = va.get::<unsafe extern "C" fn(Display) -> c_int>(b"vaMaxNumEntrypoints").map_err(|e| e.to_string())?;
        let query = va
            .get::<unsafe extern "C" fn(Display, c_int, *mut c_int, *mut c_int) -> c_int>(b"vaQueryConfigEntrypoints")
            .map_err(|e| e.to_string())?;
        let terminate = va.get::<unsafe extern "C" fn(Display) -> c_int>(b"vaTerminate").map_err(|e| e.to_string())?;

        let display = get_display(file.as_raw_fd());
        if display.is_null() {
            return Err(format!("vaGetDisplayDRM failed on {}", node.display()));
        }
        let (mut major, mut minor) = (0, 0);
        let status = initialize(display, &mut major, &mut minor);
        if status != 0 {
            return Err(format!("vaInitialize failed on {} (status {status})", node.display()));
        }
        let mut entrypoints = vec![0; max_entrypoints(display).max(0) as usize];
        let profile = PROFILES.into_iter().find(|&p| {
            let mut n = 0;
            query(display, p, entrypoints.as_mut_ptr(), &mut n) == 0
                && entrypoints[..n.clamp(0, entrypoints.len() as c_int) as usize].iter().any(|e| ENTRYPOINTS.contains(e))
        });
        terminate(display);
        match profile {
            Some(p) => Ok(format!("VA-API {major}.{minor} on {}: H.264 profile {p} encodes", node.display())),
            None => Err(format!("VA-API {major}.{minor} on {} has no H.264 encoder", node.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::{Config, Paths}, server::VrServer};

    const ALL_CHECKS: [&str; 9] = [
        "steam_native", "steamvr_installed", "alvr_streamer", "alvr_dashboard_absent", "audio_input",
        "gpu_direct_mode", "h264_encoder", "openvr_client", "screenshot_decoder",
    ];

    fn all_ready() -> Readiness {
        Readiness { restore_file: true, steamvr_running: true, alvr_api: true, streaming: true, running: true, ..Default::default() }
    }

    #[test]
    fn vr_status_always_ready() {
        let r = Readiness { failed: ALL_CHECKS.iter().map(|n| (*n, "broken".to_owned())).collect(), ..Default::default() };
        assert_eq!(tool_ready("vr_status", &r), Ok(()));
        assert_eq!(tool_ready("get_logs", &r), Ok(()));
    }

    #[test]
    fn start_needs_core_checks() {
        let r = Readiness { failed: [("gpu_direct_mode", "NVIDIA only".to_owned())].into(), ..all_ready() };
        let err = tool_ready("vr_start", &r).unwrap_err();
        assert!(err.contains("gpu_direct_mode") && err.contains("NVIDIA only"), "{err}");
        assert_eq!(tool_ready("vr_start", &all_ready()), Ok(()));
    }

    #[test]
    fn vr_stop_needs_only_something_running() {
        let r = Readiness { failed: ALL_CHECKS.iter().map(|n| (*n, "broken".to_owned())).collect(), running: true, ..Default::default() };
        assert_eq!(tool_ready("vr_stop", &r), Ok(()));
        assert!(tool_ready("vr_stop", &Readiness { running: false, ..all_ready() }).is_err());
    }

    #[test]
    fn rig_tools_refuse_when_not_streaming() {
        let r = Readiness { streaming: false, ..all_ready() };
        assert_eq!(tool_ready("set_head_pose", &r), Err("headset is not streaming".to_owned()));
        assert_eq!(tool_ready("set_head_pose", &all_ready()), Ok(()));
    }

    #[test]
    fn vr_restore_needs_restore_file() {
        assert!(tool_ready("vr_restore", &Readiness { restore_file: false, ..all_ready() }).is_err());
        assert_eq!(tool_ready("vr_restore", &Readiness { restore_file: true, ..Default::default() }), Ok(()));
    }

    #[test]
    fn every_tool_has_a_rule() {
        for tool in VrServer::tool_router().list_all() {
            assert_eq!(tool_ready(&tool.name, &all_ready()), Ok(()), "{}", tool.name);
        }
        assert_eq!(tool_ready("bogus", &all_ready()), Err("unknown tool".to_owned()));
    }

    #[test]
    fn startup_checks_report_missing_streamer() {
        let paths = Paths {
            steam_dir: None,
            flatpak_steam: false,
            steamvr_dir: None,
            streamer_dir: None,
            state_dir: "/nonexistent".into(),
            runtime_dir: "/nonexistent".into(),
        };
        let checks = startup_checks(&Config::default(), &paths);
        let streamer = checks.iter().find(|c| c.name == "alvr_streamer").unwrap();
        assert!(!streamer.ok && streamer.fix.contains("streamer_dir"), "{streamer:?}");
        assert!(checks.iter().all(|c| !c.fix.is_empty()));
        assert!(checks.iter().find(|c| c.name == "screenshot_decoder").unwrap().ok);
    }

    #[test]
    fn gpu_vendors_decide_direct_mode() {
        let nvidia_only = gpu_verdict(&[NVIDIA], true);
        assert_eq!(nvidia_only, (Err("ALVR 21 direct mode does not work on NVIDIA (ALVR #3421)".to_owned()), None));
        assert_eq!(gpu_verdict(&[], true).1, None);
        assert!(gpu_verdict(&[], true).0.is_err());
        assert_eq!(gpu_verdict(&[AMD], true).1, Some(AMD));
        let (hybrid, vendor) = gpu_verdict(&[NVIDIA, AMD], true);
        assert!(hybrid.unwrap().contains("steam.env"));
        assert_eq!(vendor, Some(AMD));
        assert_eq!(gpu_verdict(&[0x1234, INTEL], false).1, Some(INTEL));
    }

    #[test]
    fn driver_versions_follow_alvr_layout() {
        // as the nightly driver embeds it, between other rodata strings
        assert_eq!(driver_versions(b"struct DeviceMotion21.0.0-dev14+nightly.2026.10.19nightlyalvr_events"), ["21.0.0-dev14"]);
        assert_eq!(driver_versions(b"Motion21.3.0alvr_events 121.0.0 21.0"), ["21.3.0"]);
    }
}
