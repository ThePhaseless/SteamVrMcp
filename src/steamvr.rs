use crate::{alvr_api::Api, config::{Config, Paths}, setup};
use alvr_common::{
    anyhow::{Context, Result, bail},
    error,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub const STEAMVR_URL: &str = "steam://rungameid/250820";

/// SteamVR's processes, as `/proc/<pid>/comm` names them (pinned from a live SteamVR 2.x run).
/// vrmonitor is last: killing it first makes SteamVR's safe mode block the ALVR driver.
pub const VR_PROCESS_NAMES: &[&str] = &[
    "vrserver", "vrcompositor", "vrdashboard", "vrwebhelper", "vrstartup", "vrstartup.sh", "steamtours", "steamtours.sh",
    "vrmonitor",
];

pub fn pids_named(name: &str) -> Vec<u32> {
    let Ok(dir) = fs::read_dir("/proc") else { return vec![] };
    dir.flatten()
        .filter_map(|e| {
            let pid = e.file_name().to_str()?.parse().ok()?;
            (comm(pid)? == name).then_some(pid)
        })
        .collect()
}

/// `/proc/<pid>/comm`: the process name.
pub fn comm(pid: u32) -> Option<String> {
    Some(fs::read_to_string(format!("/proc/{pid}/comm")).ok()?.trim_end().to_owned())
}

pub fn exe_of(pid: u32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

pub fn is_steamvr_running() -> bool {
    !pids_named("vrserver").is_empty()
}

pub fn is_steam_running() -> bool {
    !pids_named("steam").is_empty()
}

/// Polls `done` every 100 ms until it holds or `timeout` passes.
pub fn wait_for(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

pub fn wait_gone(names: &[&str], timeout: Duration) -> bool {
    wait_for(timeout, || names.iter().all(|n| pids_named(n).is_empty()))
}

/// `pid` as kill() takes it, when that names one process: 0 and anything above i32::MAX would
/// signal a process group or every process.
pub fn signalable(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|&p| p > 0)
}

/// Sends `sig` to `pid`; does nothing for a pid that is not a single process.
pub fn terminate(pid: u32, sig: i32) {
    if let Some(pid) = signalable(pid) {
        unsafe { libc::kill(pid, sig) };
    }
}

/// SIGTERM, then SIGKILL, to every SteamVR process (vrmonitor last), 5 s apart. True when none remain.
pub fn kill_vr_processes() -> bool {
    for sig in [libc::SIGTERM, libc::SIGKILL] {
        for name in VR_PROCESS_NAMES {
            pids_named(name).into_iter().for_each(|pid| terminate(pid, sig));
        }
        if wait_gone(VR_PROCESS_NAMES, Duration::from_secs(5)) {
            return true;
        }
    }
    false
}

/// True when `now` holds a vrserver pid that `old` does not: SteamVR really relaunched.
pub fn is_new_vrserver(old: &[u32], now: &[u32]) -> bool {
    now.iter().any(|pid| !old.contains(pid))
}

/// True when SteamVR processes outlive vrserver: Steam then refuses to launch SteamVR again
/// (console_log: WaitingPrevProcess, AppError_16), so start() kills them first.
pub fn has_leftovers(alive: &[&str]) -> bool {
    !alive.is_empty() && !alive.contains(&"vrserver")
}

/// The VR_PROCESS_NAMES with a running process.
pub fn alive_vr_processes() -> Vec<&'static str> {
    VR_PROCESS_NAMES.iter().copied().filter(|n| !pids_named(n).is_empty()).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppKind {
    Steam,
    Shortcut,
}

#[derive(Clone, Debug, Serialize)]
pub struct AppInfo {
    pub app_id: u32,
    /// What steam://rungameid/ takes.
    pub game_id: u64,
    pub kind: AppKind,
    pub name: String,
    /// Steam's CompatToolMapping entry, e.g. "proton_experimental"; none for native games and Steam's default.
    pub compat_tool: Option<String>,
}

/// Steam's game id for a non-Steam shortcut.
pub fn shortcut_game_id(appid: u32) -> u64 {
    (appid as u64) << 32 | 0x0200_0000
}

// shortcut: line-oriented VDF scan without escape handling; switch to a parser if Valve changes the format or a value holds a quote
/// The quoted tokens of a text VDF line.
fn tokens(line: &str) -> Vec<&str> {
    line.split('"').skip(1).step_by(2).collect()
}

/// The first `"key" "value"` pair's value with this key.
fn vdf_value<'a>(vdf: &'a str, key: &str) -> Option<&'a str> {
    vdf.lines().find_map(|l| match tokens(l)[..] {
        [k, v] if k == key => Some(v),
        _ => None,
    })
}

/// Every library's `path` in libraryfolders.vdf.
pub fn library_dirs(libraryfolders_vdf: &str) -> Vec<PathBuf> {
    libraryfolders_vdf
        .lines()
        .filter_map(|l| match tokens(l)[..] {
            ["path", v] => Some(PathBuf::from(v)),
            _ => None,
        })
        .collect()
}

/// The tool name under CompatToolMapping / `<app_id>` in config.vdf.
pub fn compat_tool(config_vdf: &str, app_id: u32) -> Option<String> {
    let id = app_id.to_string();
    let lines = config_vdf.lines().skip_while(|l| tokens(l) != ["CompatToolMapping"]).skip(1);
    let (mut depth, mut in_app) = (0, false);
    for line in lines {
        match (line.trim(), &tokens(line)[..]) {
            ("{", _) => depth += 1,
            ("}", _) if depth == 1 => return None,
            ("}", _) => (depth, in_app) = (depth - 1, false),
            (_, [k]) if depth == 1 => in_app = *k == id,
            (_, ["name", v]) if depth == 2 && in_app => return Some(v.to_string()),
            _ => (),
        }
    }
    None
}

/// The (appid, AppName) of each entry in Steam's binary shortcuts.vdf; a truncated file yields what came before.
pub fn parse_shortcuts(bytes: &[u8]) -> Vec<(u32, String)> {
    fn walk(b: &[u8], out: &mut Vec<(u32, String)>) -> Option<()> {
        let (mut i, mut id, mut name) = (0, None, None);
        let cstr = |i: &mut usize| {
            let len = b.get(*i..)?.iter().position(|&c| c == 0)?;
            *i += len + 1;
            Some(String::from_utf8_lossy(&b[*i - len - 1..*i - 1]).into_owned())
        };
        while let Some(&kind) = b.get(i) {
            i += 1;
            if kind == 0x08 {
                // the end of a map; an entry has both fields once its first nested map (tags) ends
                if let (Some(id), Some(name)) = (id.take(), name.take()) {
                    out.push((id, name));
                }
                continue;
            }
            let key = cstr(&mut i)?;
            match kind {
                0x00 => (),
                0x01 => name = Some(cstr(&mut i)?).filter(|_| key.eq_ignore_ascii_case("appname")).or(name),
                0x02 => {
                    let v = i32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?);
                    i += 4;
                    if key.eq_ignore_ascii_case("appid") {
                        id = Some(v as u32);
                    }
                }
                _ => return None, // shortcuts.vdf uses only maps, strings and int32
            }
        }
        Some(())
    }
    let mut out = vec![];
    walk(bytes, &mut out);
    out
}

/// `id`: a Steam app id, a 32-bit shortcut app id or a 64-bit shortcut game id.
pub fn find_app(steam_dir: &Path, id: u64) -> Result<AppInfo> {
    let app_id = if id > u32::MAX as u64 { (id >> 32) as u32 } else { id as u32 };
    let compat_tool = || compat_tool(&fs::read_to_string(steam_dir.join("config/config.vdf")).unwrap_or_default(), app_id);
    if id <= u32::MAX as u64 {
        let libraries = fs::read_to_string(steam_dir.join("steamapps/libraryfolders.vdf")).unwrap_or_default();
        for lib in [steam_dir.to_path_buf()].into_iter().chain(library_dirs(&libraries)) {
            if let Ok(manifest) = fs::read_to_string(lib.join(format!("steamapps/appmanifest_{app_id}.acf"))) {
                let name = vdf_value(&manifest, "name").unwrap_or_default().to_owned();
                return Ok(AppInfo { app_id, game_id: id, kind: AppKind::Steam, name, compat_tool: compat_tool() });
            }
        }
    }
    let game_id = shortcut_game_id(app_id);
    if id == app_id as u64 || id == game_id {
        for user in fs::read_dir(steam_dir.join("userdata")).into_iter().flatten().flatten() {
            let shortcuts = fs::read(user.path().join("config/shortcuts.vdf")).unwrap_or_default();
            if let Some((_, name)) = parse_shortcuts(&shortcuts).into_iter().find(|(a, _)| *a == app_id) {
                return Ok(AppInfo { app_id, game_id, kind: AppKind::Shortcut, name, compat_tool: compat_tool() });
            }
        }
    }
    bail!("no Steam app or non-Steam shortcut with id {id}")
}

fn console_log(steam_dir: &Path) -> PathBuf {
    steam_dir.join("logs/console_log.txt")
}

/// Where Steam's console log ends now; read from here after a launch.
pub fn console_log_len(steam_dir: &Path) -> u64 {
    fs::metadata(console_log(steam_dir)).map_or(0, |m| m.len())
}

/// What Steam wrote to its console log after `from`; all of it when the log is now shorter (rotated).
pub fn console_log_since(steam_dir: &Path, from: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let mut bytes = vec![];
    if let Ok(mut file) = fs::File::open(console_log(steam_dir)) {
        let len = file.metadata().map_or(0, |m| m.len());
        let from = if len < from { 0 } else { from };
        file.seek(SeekFrom::Start(from)).and_then(|_| file.read_to_end(&mut bytes)).ok();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Launch steps at which Steam can wait on a desktop dialog.
const DIALOG_STEPS: [&str; 2] = ["ShowInterstitials", "ShowLaunchOption"];

/// The AppID and step of a launch Steam holds at a desktop dialog: a wait at a DIALOG_STEPS step with
/// no "continues" after it. Steam logs a ShowInterstitials wait for every launch and continues at once when no dialog shows.
pub fn pending_interstitial(console_log: &str) -> Option<(u32, &'static str)> {
    let mut pending: Vec<(&str, &'static str)> = vec![];
    for line in console_log.lines() {
        let Some((action, rest)) = line.split_once("GameAction [").and_then(|(_, r)| r.split_once(']')) else { continue };
        for step in DIALOG_STEPS {
            if rest.contains(&format!("LaunchApp waiting for user response to {step} ")) {
                pending.push((action, step));
            } else if rest.contains(&format!("LaunchApp continues with user response \"{step}\"")) {
                pending.retain(|p| *p != (action, step));
            }
        }
    }
    let (action, step) = pending.last()?;
    Some((action.strip_prefix("AppID ")?.split(',').next()?.parse().ok()?, step))
}

/// How long a dialog-step wait may last before it counts as a dialog: seen live, Steam continued after up to 5 s.
const DIALOG_GRACE: Duration = Duration::from_secs(10);

/// wait_for for a launch through Steam, which fails fast once Steam waits on a desktop dialog
/// (console log lines after `log_from`).
pub fn wait_launched(steam_dir: Option<&Path>, log_from: u64, timeout: Duration, mut done: impl FnMut() -> bool) -> Result<bool> {
    let (mut since, mut blocked) = (None, None);
    let finished = wait_for(timeout, || {
        if done() {
            return true;
        }
        let pending = steam_dir.and_then(|d| pending_interstitial(&console_log_since(d, log_from)));
        let started = *since.get_or_insert_with(Instant::now);
        if pending.is_none() {
            since = None;
        }
        blocked = pending.filter(|_| started.elapsed() >= DIALOG_GRACE);
        blocked.is_some()
    });
    if let Some((id, step)) = blocked {
        bail!("Steam is waiting for a dialog on the desktop ({step} for AppID {id}); dismiss it and retry");
    }
    Ok(finished)
}

/// Host variables a desktop Steam needs; the rest of the MCP's environment (API keys and other
/// secrets of the agent host) never reaches Steam or the games it starts.
const DESKTOP_ENV: [&str; 12] = [
    "HOME", "USER", "LOGNAME", "SHELL", "PATH", "LANG", "LANGUAGE", "DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS", "PULSE_SERVER",
];

/// Steam's environment: the desktop-session variables of `host`, then `extra` (steam.env) over them.
pub fn steam_env(host: impl IntoIterator<Item = (OsString, OsString)>, extra: &BTreeMap<String, String>) -> Vec<(OsString, OsString)> {
    let desktop = |k: &str| DESKTOP_ENV.contains(&k) || k.starts_with("LC_") || k.starts_with("XDG_");
    let mut env: BTreeMap<OsString, OsString> = host.into_iter().filter(|(k, _)| k.to_str().is_some_and(desktop)).collect();
    env.extend(extra.iter().map(|(k, v)| (k.into(), v.into())));
    env.into_iter().collect()
}

/// Runs `steam <url>` with only the desktop-session environment. Only when Steam is not running yet
/// does it get `steam.env` and `steam.args`, because a running Steam just forwards the URL. True if this started Steam.
pub fn launch(cfg: &Config, url: &str) -> Result<bool> {
    let start_steam = !is_steam_running();
    let no_extra = BTreeMap::new();
    let mut command = Command::new("steam");
    command.env_clear().envs(steam_env(std::env::vars_os(), if start_steam { &cfg.steam.env } else { &no_extra }));
    if start_steam {
        command.args(&cfg.steam.args);
    }
    let mut child = command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("running steam")?;
    // port of the dashboard's spawn_and_reap: an unreaped zombie blocks vrmonitor
    thread::spawn(move || child.wait());
    Ok(start_steam)
}

/// The last `lines` lines of SteamVR's own log `<steam>/logs/<name>.txt`; none when it is unreadable.
pub fn log_tail(steam_dir: &Path, name: &str, lines: usize) -> Vec<String> {
    let Ok(bytes) = fs::read(steam_dir.join("logs").join(format!("{name}.txt"))) else { return vec![] };
    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].iter().map(|l| l.to_string()).collect()
}

/// Starts this binary as `alvr_dashboard` through a link in `runtime_dir`. ALVR creates the
/// headset at SteamVR start only while a process of that name exists.
pub fn spawn_dashboard_child(runtime_dir: &Path) -> Result<Child> {
    fs::create_dir_all(runtime_dir)?;
    let link = runtime_dir.join(alvr_filesystem::dashboard_fname());
    match fs::remove_file(&link) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e).context(link.display().to_string()),
        _ => (),
    }
    std::os::unix::fs::symlink(std::env::current_exe()?, &link)?;
    Command::new(&link)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning the alvr_dashboard child")
}

/// The dashboard child's whole life: exist until the parent process goes away. Not
/// PR_SET_PDEATHSIG, which fires when the spawning thread exits.
pub fn run_dashboard_child() -> ! {
    let parent = unsafe { libc::getppid() };
    while parent != 1 && unsafe { libc::getppid() } == parent {
        thread::sleep(Duration::from_secs(1));
    }
    std::process::exit(0)
}

/// The dashboard's restart: ALVR shuts SteamVR down cleanly, then SteamVR is launched again.
pub fn restart_steamvr(cfg: &Config, paths: &Paths, api: &Api) -> Result<()> {
    let vrsettings = paths.vrsettings_file().context("Steam not found; set steam_dir in config.toml")?;
    let old = pids_named("vrserver");
    // on failure the wait and signals below still stop SteamVR, so it is always relaunched
    if let Err(e) = api.post("/api/steamvr/restart") {
        error!("{e:#}");
    }
    // as in Stack::stop(): ALVR's shutdown hangs on a stream thread spinning on its client's EOF
    // (sockets/src/stream_socket/tcp.rs:137-147), so escalate to signals. Every SteamVR process
    // must be gone: a steamtours that outlives vrserver keeps Steam's AppID 250820 running, and
    // Steam then refuses the relaunch (console_log: WaitingPrevProcess, AppError_16)
    if !wait_gone(VR_PROCESS_NAMES, Duration::from_secs(30)) {
        if !kill_vr_processes() {
            let left: Vec<_> = VR_PROCESS_NAMES.iter().filter(|n| !pids_named(n).is_empty()).collect();
            bail!("SteamVR processes still running after SIGKILL: {left:?}");
        }
        // SteamVR has exited, so it no longer rewrites the file
        setup::undo_safe_mode_blocks(paths)?;
    }
    setup::unblock_safe_mode(&vrsettings)?;
    let log_from = paths.steam_dir.as_deref().map_or(0, console_log_len);
    launch(cfg, STEAMVR_URL)?;
    // the old server can still answer ping while it hangs in shutdown
    let relaunched = || is_new_vrserver(&old, &pids_named("vrserver")) && api.ping();
    if !wait_launched(paths.steam_dir.as_deref(), log_from, Duration::from_secs(cfg.timeouts.streaming_s), relaunched)? {
        bail!("ALVR did not come back within {} s after the SteamVR restart", cfg.timeouts.streaming_s);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Binary VDF as Steam writes shortcuts.vdf: one entry per (appid, AppName), plus a nested tags map.
    fn shortcuts_vdf(entries: &[(u32, &str)]) -> Vec<u8> {
        let mut b = b"\x00shortcuts\x00".to_vec();
        for (i, (id, name)) in entries.iter().enumerate() {
            b.extend([&[0x00][..], i.to_string().as_bytes(), b"\x00\x02appid\x00"].concat());
            b.extend((*id as i32).to_le_bytes());
            b.extend([b"\x01AppName\x00", name.as_bytes(), b"\x00\x01Exe\x00\"/x.exe\"\x00\x00tags\x00\x01", b"0\x00VR\x00\x08\x08"].concat());
        }
        b.extend(b"\x08\x08");
        b
    }

    #[test]
    fn shortcut_game_id_matches_steam() {
        assert_eq!(shortcut_game_id(3000000000), 12884901888033554432);
        assert_eq!(shortcut_game_id(3_000_000_000), (3_000_000_000u64 << 32) | 0x0200_0000);
    }

    #[test]
    fn parse_shortcuts_reads_appid_and_name() {
        assert_eq!(parse_shortcuts(&shortcuts_vdf(&[(3000000000, "Probe")])), [(3000000000, "Probe".to_owned())]);
        let two = parse_shortcuts(&shortcuts_vdf(&[(1, "A"), (2, "B")]));
        assert_eq!(two, [(1, "A".to_owned()), (2, "B".to_owned())]);
        assert!(parse_shortcuts(b"\x00shortcuts").is_empty(), "truncated");
    }

    #[test]
    fn library_dirs_from_vdf() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/a\"\n\t\t\"label\"\t\t\"\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"/b\"\n\t}\n}\n";
        assert_eq!(library_dirs(vdf), [PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    const CONFIG_VDF: &str = "\"InstallConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"CompatToolMapping\"\n\t\t{\n\t\t\t\"620980\"\n\t\t\t{\n\t\t\t\t\"name\"\t\t\"proton_experimental\"\n\t\t\t\t\"config\"\t\t\"\"\n\t\t\t}\n\t\t\t\"0\"\n\t\t\t{\n\t\t\t\t\"name\"\t\t\"proton_9\"\n\t\t\t}\n\t\t}\n\t\t\"Other\"\n\t\t{\n\t\t\t\"7\"\n\t\t\t{\n\t\t\t\t\"name\"\t\t\"not_a_tool\"\n\t\t\t}\n\t\t}\n\t}\n}\n";

    #[test]
    fn compat_tool_lookup() {
        assert_eq!(compat_tool(CONFIG_VDF, 620980).as_deref(), Some("proton_experimental"));
        assert_eq!(compat_tool(CONFIG_VDF, 1), None);
        assert_eq!(compat_tool(CONFIG_VDF, 7), None, "outside CompatToolMapping");
    }

    fn fake_steam(name: &str) -> PathBuf {
        let steam = std::env::temp_dir().join(format!("vr-test-mcp-{name}-{}", std::process::id()));
        fs::remove_dir_all(&steam).ok();
        fs::create_dir_all(steam.join("steamapps")).unwrap();
        steam
    }

    #[test]
    fn unknown_app_is_rejected() {
        let steam = fake_steam("unknown-app");
        let e = find_app(&steam, 1).unwrap_err();
        assert_eq!(e.to_string(), "no Steam app or non-Steam shortcut with id 1");
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn find_app_happy_paths() {
        let steam = fake_steam("find-app");
        let lib = steam.join("lib2");
        fs::create_dir_all(lib.join("steamapps")).unwrap();
        fs::create_dir_all(steam.join("config")).unwrap();
        fs::create_dir_all(steam.join("userdata/42/config")).unwrap();
        let libraries = format!("\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n", lib.display());
        fs::write(steam.join("steamapps/libraryfolders.vdf"), libraries).unwrap();
        fs::write(lib.join("steamapps/appmanifest_620980.acf"), "\"AppState\"\n{\n\t\"appid\"\t\t\"620980\"\n\t\"name\"\t\t\"Beat Saber\"\n}\n").unwrap();
        fs::write(steam.join("config/config.vdf"), CONFIG_VDF).unwrap();
        fs::write(steam.join("userdata/42/config/shortcuts.vdf"), shortcuts_vdf(&[(3000000000, "Probe")])).unwrap();

        let steam_app = find_app(&steam, 620980).unwrap();
        assert_eq!((steam_app.app_id, steam_app.game_id, steam_app.kind), (620980, 620980, AppKind::Steam));
        assert_eq!((steam_app.name.as_str(), steam_app.compat_tool.as_deref()), ("Beat Saber", Some("proton_experimental")));
        for id in [3000000000, 12884901888033554432] {
            let shortcut = find_app(&steam, id).unwrap();
            assert_eq!((shortcut.app_id, shortcut.game_id, shortcut.kind), (3000000000, 12884901888033554432, AppKind::Shortcut), "{id}");
            assert_eq!((shortcut.name.as_str(), shortcut.compat_tool.as_deref()), ("Probe", None));
        }
        assert!(find_app(&steam, (620980u64 << 32) | 1).is_err(), "a 64-bit id must be a shortcut game id");
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn interstitial_pending_until_steam_continues() {
        let waiting = "[2026-10-21 07:04:37] GameAction [AppID 620980, ActionID 70] : LaunchApp waiting for user response to ShowInterstitials \"\"\n";
        let continues = "[2026-10-21 07:04:37] GameAction [AppID 620980, ActionID 70] : LaunchApp continues with user response \"ShowInterstitials\"\n";
        let other = "[2026-10-21 07:04:38] GameAction [AppID 250820, ActionID 71] : LaunchApp waiting for user response to CreatingProcess \"\"\n";
        assert_eq!(pending_interstitial(waiting), Some((620980, "ShowInterstitials")));
        let option = "[2026-10-17 09:17:28] GameAction [AppID 1574820, ActionID 4] : LaunchApp waiting for user response to ShowLaunchOption \"\"\n";
        assert_eq!(pending_interstitial(&[waiting, continues, option].concat()), Some((1574820, "ShowLaunchOption")));
        assert_eq!(pending_interstitial(&[waiting, continues].concat()), None, "Steam logs every launch's interstitial step, and continues at once");
        assert_eq!(pending_interstitial(other), None);
        assert_eq!(pending_interstitial(""), None);
    }

    #[test]
    fn console_log_since_reads_only_new_lines() {
        let steam = fake_steam("console-log");
        fs::create_dir_all(steam.join("logs")).unwrap();
        fs::write(steam.join("logs/console_log.txt"), "old\n").unwrap();
        let from = console_log_len(&steam);
        fs::write(steam.join("logs/console_log.txt"), "old\nnew\n").unwrap();
        assert_eq!(console_log_since(&steam, from), "new\n");
        fs::write(steam.join("logs/console_log.txt"), "x\n").unwrap();
        assert_eq!(console_log_since(&steam, from), "x\n", "a rotated log is read from its start");
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn leftover_vr_processes_need_cleanup() {
        assert!(has_leftovers(&["steamtours", "vrmonitor"]), "Steam refuses the relaunch: WaitingPrevProcess");
        assert!(!has_leftovers(&[]));
        assert!(!has_leftovers(&["vrserver", "steamtours"]), "a running SteamVR is adopted, not killed");
    }

    #[test]
    fn log_tail_keeps_the_last_lines() {
        let steam = std::env::temp_dir().join(format!("vr-test-mcp-log-tail-{}", std::process::id()));
        fs::create_dir_all(steam.join("logs")).unwrap();
        fs::write(steam.join("logs/vrserver.txt"), "a\nb\nc\n").unwrap();
        assert_eq!(log_tail(&steam, "vrserver", 2), ["b", "c"]);
        assert_eq!(log_tail(&steam, "vrserver", 9), ["a", "b", "c"]);
        assert!(log_tail(&steam, "vrmonitor", 2).is_empty(), "missing file");
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn relaunch_needs_a_new_vrserver() {
        assert!(!is_new_vrserver(&[10], &[10]), "the old, wedged server still answers");
        assert!(!is_new_vrserver(&[10], &[]));
        assert!(is_new_vrserver(&[10], &[11]));
        assert!(is_new_vrserver(&[], &[11]));
    }

    #[test]
    fn steam_gets_only_desktop_session_env() {
        let host = [("HOME", "/h"), ("LC_ALL", "C"), ("XDG_RUNTIME_DIR", "/r"), ("DISPLAY", ":0"), ("ANTHROPIC_API_KEY", "secret"), ("GH_TOKEN", "t"), ("PATH", "/bin")]
            .map(|(k, v)| (k.into(), v.into()));
        let extra = [("MESA_VK_DEVICE_SELECT".to_owned(), "x!".to_owned()), ("PATH".to_owned(), "/steam".to_owned())].into();
        let env: BTreeMap<_, _> = steam_env(host, &extra).into_iter().collect();
        let want = [("DISPLAY", ":0"), ("HOME", "/h"), ("LC_ALL", "C"), ("MESA_VK_DEVICE_SELECT", "x!"), ("PATH", "/steam"), ("XDG_RUNTIME_DIR", "/r")];
        assert_eq!(env, want.map(|(k, v)| (k.into(), v.into())).into(), "steam.env wins over the host");
    }

    #[test]
    fn only_single_processes_are_signalled() {
        assert_eq!(signalable(0), None, "0 is our own process group");
        assert_eq!(signalable(i32::MAX as u32 + 1), None, "negative as i32: a process group");
        assert_eq!(signalable(u32::MAX), None, "-1: every process");
        assert_eq!(signalable(4242), Some(4242));
    }

    #[test]
    fn pids_named_finds_self() {
        let comm = std::fs::read_to_string("/proc/self/comm").unwrap();
        assert!(pids_named(comm.trim()).contains(&std::process::id()));
    }
}
