use crate::{alvr_api::Api, config::{Config, Paths}, setup};
use alvr_common::{
    anyhow::{Context, Result, bail},
    error,
};
use std::{
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
            (fs::read_to_string(e.path().join("comm")).ok()?.trim_end() == name).then_some(pid)
        })
        .collect()
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

pub fn terminate(pid: u32, sig: i32) {
    unsafe { libc::kill(pid as i32, sig) };
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

/// Runs `steam <url>`. Only when Steam is not running yet does it get `steam.env` and `steam.args`,
/// because a running Steam just forwards the URL. True if this started Steam.
pub fn launch(cfg: &Config, url: &str) -> Result<bool> {
    let start_steam = !is_steam_running();
    let mut command = Command::new("steam");
    if start_steam {
        command.envs(&cfg.steam.env).args(&cfg.steam.args);
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
    // (sockets/src/stream_socket/tcp.rs:137-147), so escalate to signals
    if !wait_gone(&["vrserver", "vrmonitor"], Duration::from_secs(30)) {
        if !kill_vr_processes() {
            let left: Vec<_> = VR_PROCESS_NAMES.iter().filter(|n| !pids_named(n).is_empty()).collect();
            bail!("SteamVR processes still running after SIGKILL: {left:?}");
        }
        // SteamVR has exited, so it no longer rewrites the file
        setup::undo_safe_mode_blocks(paths)?;
    }
    setup::unblock_safe_mode(&vrsettings)?;
    launch(cfg, STEAMVR_URL)?;
    // the old server can still answer ping while it hangs in shutdown
    let relaunched = || is_new_vrserver(&old, &pids_named("vrserver")) && api.ping();
    if !wait_for(Duration::from_secs(cfg.timeouts.streaming_s), relaunched) {
        bail!("ALVR did not come back within {} s after the SteamVR restart", cfg.timeouts.streaming_s);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn pids_named_finds_self() {
        let comm = std::fs::read_to_string("/proc/self/comm").unwrap();
        assert!(pids_named(comm.trim()).contains(&std::process::id()));
    }
}
