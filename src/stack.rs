use crate::{
    alvr_api::{self, Api},
    checks::{self, Check, Readiness},
    config::{Config, Paths},
    headset::{self, Eye, HapticEvent, Headset, HeadsetStatus, Screenshot},
    openvr::{self, OpenVr},
    rig::{Rig, RigAction},
    setup,
    steamvr::{self, STEAMVR_URL, VR_PROCESS_NAMES},
};
use alvr_common::{
    ConnectionState, LogEntry, LogSeverity,
    anyhow::{Context, Result, anyhow, bail},
    error,
    parking_lot::Mutex,
    warn,
};
use alvr_events::{Event, EventType};
use alvr_packets::{PathSegment, PathValuePair, parse_path, path_to_string};
use alvr_session::{SessionConfig, Settings, settings_schema::SchemaNode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::VecDeque,
    process::Child,
    sync::{Arc, Weak},
    thread,
    time::{Duration, Instant},
};

// client_core/src/connection.rs SERVER_RESTART_MESSAGE, shown after ServerControlPacket::Restarting
const SERVER_RESTART_MESSAGE: &str = "The streamer is restarting\nPlease wait...";

/// client_core's set_hud_message puts version, hostname and IP before the message.
fn is_restart_hud(hud: &str) -> bool {
    hud.ends_with(SERVER_RESTART_MESSAGE)
}

const WATCHDOG: Duration = Duration::from_secs(30);

const LEAKED: &str = "headset leaked; restart the MCP to free a CPU core";

const HEADSET_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on each OpenVR call, connect and disconnect: a stale connection must not hang a tool.
const OPENVR_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs `f` on its own thread and returns its result, or None if it is still running after
/// `timeout`; the thread is then left running (detached).
fn within<T: Send + 'static>(timeout: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || tx.send(f()).ok());
    rx.recv_timeout(timeout).ok()
}

/// True when the session lists `hostname` in any state but Disconnected: ALVR's handshake loop
/// connects only to Disconnected entries (server_core/src/connection.rs:395-405).
fn client_stuck(session: &SessionConfig, hostname: &str) -> bool {
    session.client_connections.get(hostname).is_some_and(|c| c.connection_state != ConnectionState::Disconnected)
}

#[derive(Debug, Default, Serialize)]
pub struct StartReport {
    pub already_streaming: bool,
    pub adopted: bool,
    pub started_steam: bool,
    pub restarts: u32,
    pub unblocked_safe_mode: bool,
    pub view_resolution: [u32; 2],
    pub fps: f32,
    pub codec: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct StopReport {
    pub remaining_processes: Vec<String>,
    pub closed_steam: bool,
    pub used_signals: bool,
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub steam_running: bool,
    pub steamvr_running: bool,
    pub alvr_version: Option<String>,
    pub headset: Option<HeadsetStatus>,
    /// Only while the OpenVR reader is connected, so vr_status never connects one.
    pub scene_app: Option<openvr::SceneApp>,
    pub warning: Option<&'static str>,
}

/// One ALVR session value to set.
#[derive(Clone, Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SettingValue {
    /// ALVR's path syntax from the session JSON root, e.g. "session_settings.video.preferred_fps" or
    /// "session_settings.headset.controllers.content.gestures[2].joystick_range".
    pub path: String,
    pub value: Value,
}

#[derive(Debug, Serialize)]
pub struct AppReport {
    pub app: steamvr::AppInfo,
    pub scene_pid: u32,
}

#[derive(Debug, Default, Serialize)]
pub struct SettingsReport {
    /// SteamVR restarted at least once.
    pub restarted: bool,
    /// How many SteamVR restarts happened: ALVR may ask for one more once the headset negotiates.
    pub restarts: u32,
    pub streaming: bool,
}

/// ALVR's `steamvr_input_2_0`: on, a bare hand is a separate skeletal hand-tracker device.
pub const STEAMVR_INPUT_2_0: &str = "session_settings.headset.controllers.content.hand_skeleton.content.steamvr_input_2_0";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LogLine {
    /// Local time of day, HH:MM:SS.mmm.
    pub t: String,
    /// "alvr" | "vrserver" | "vrcompositor" | "vrmonitor"
    pub source: String,
    /// "error" | "warning" | "info" | "debug"
    pub level: String,
    pub text: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct LogQuery {
    /// "alvr", "vrserver", "vrcompositor", "vrmonitor" or "all" (default).
    pub source: Option<String>,
    /// Keep only lines whose text contains this, ignoring case.
    pub contains: Option<String>,
    /// "error", "warning", "info" or "debug": the lowest level kept for ALVR events. SteamVR lines are not filtered by level.
    pub min_level: Option<String>,
    /// How many of the newest matching lines to return, 1 to 1000 (default 100).
    pub limit: Option<usize>,
}

const LOG_SOURCES: [&str; 5] = ["alvr", "vrserver", "vrcompositor", "vrmonitor", "all"];
const STEAMVR_LOGS: [&str; 3] = ["vrserver", "vrcompositor", "vrmonitor"];
/// ALVR events kept, and SteamVR lines read per log.
const LOGS_KEPT: usize = 1000;

fn level_name(severity: LogSeverity) -> String {
    format!("{severity:?}").to_lowercase()
}

fn severity(level: &str) -> Option<LogSeverity> {
    [LogSeverity::Error, LogSeverity::Warning, LogSeverity::Info, LogSeverity::Debug].into_iter().find(|s| level_name(*s) == level)
}

/// The values in ALVR's own types; a bad path fails here, before any request.
pub fn parse_values(values: Vec<SettingValue>) -> Result<Vec<PathValuePair>> {
    values.into_iter().map(|v| Ok(PathValuePair { path: parse_path(&v.path)?, value: v.value })).collect()
}

/// Ok when `session` (ALVR's session as JSON) holds every value.
pub fn verify_applied(session: &Value, values: &[PathValuePair]) -> Result<(), String> {
    for v in values {
        let path = path_to_string(&v.path);
        let found = v.path.iter().try_fold(session, |node, segment| match segment {
            PathSegment::Name(name) => node.get(name),
            PathSegment::Index(index) => node.get(index),
        });
        match found {
            None => return Err(format!("{path} is not in ALVR's session")),
            Some(found) if !json_eq(found, &v.value) => return Err(format!("{path} is {found} in ALVR's session, not {}", v.value)),
            Some(_) => (),
        }
    }
    Ok(())
}

/// Equality with numbers compared at f32 precision: ALVR stores most numbers as f32.
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap_or(f64::NAN), y.as_f64().unwrap_or(f64::NAN));
            (x - y).abs() <= f32::EPSILON as f64 * x.abs().max(1.0)
        }
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| json_eq(x, y)),
        (Value::Object(x), Value::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w))),
        _ => a == b,
    }
}

/// Whether ALVR's settings schema flags `path`, or an entry above it, "real-time": ALVR applies
/// it while streaming, so no restart is needed (the dashboard's "can be changed in real-time").
pub fn is_real_time(path: &[PathSegment]) -> bool {
    match path {
        [PathSegment::Name(root), rest @ ..] if root == "session_settings" => {
            real_time_in(&Settings::schema(alvr_session::session_settings_default()), rest)
        }
        _ => false,
    }
}

fn real_time_in(node: &SchemaNode, path: &[PathSegment]) -> bool {
    use PathSegment::{Index, Name};
    match (node, path) {
        (SchemaNode::Section { entries, .. }, [Name(name), rest @ ..]) => {
            entries.iter().find(|e| e.name == *name).is_some_and(|e| e.flags.contains("real-time") || real_time_in(&e.content, rest))
        }
        (SchemaNode::Choice { variants, .. }, [Name(name), rest @ ..]) => variants
            .iter()
            .find(|v| v.name == *name)
            .is_some_and(|v| v.flags.contains("real-time") || v.content.as_ref().is_some_and(|c| real_time_in(c, rest))),
        (SchemaNode::Optional { content, .. } | SchemaNode::Switch { content, .. }, [Name(name), rest @ ..]) if name == "content" => {
            real_time_in(content, rest)
        }
        (SchemaNode::Array(items), [Index(index), rest @ ..]) => items.get(*index).is_some_and(|c| real_time_in(c, rest)),
        // shortcut: below vectors and dictionaries, and at an enum's `variant`, every path counts as restart-relevant, so it restarts SteamVR; walk them if a real-time path there matters
        _ => false,
    }
}

/// SteamVR's "Wed Oct 21 2026 05:00:36.212461 [Info] - text" lines; a line without that prefix
/// continues the one before and gets its time and level.
pub fn steamvr_log_lines(source: &str, lines: Vec<String>) -> Vec<LogLine> {
    let (mut t, mut level) = (String::new(), String::new());
    lines
        .into_iter()
        .map(|line| {
            let parsed = line.split_once(" [").and_then(|(stamp, rest)| {
                let (lvl, text) = rest.split_once("] - ")?;
                let time = stamp.rsplit(' ').next()?.get(..12).filter(|t| t.as_bytes()[2] == b':')?;
                Some((time.to_owned(), lvl.to_lowercase(), text.to_owned()))
            });
            let text = match parsed {
                Some((time, lvl, text)) => {
                    (t, level) = (time, lvl);
                    text
                }
                None => line,
            };
            LogLine { t: t.clone(), source: source.to_owned(), level: level.clone(), text }
        })
        .collect()
}

/// `lines` in time order, filtered by `q`, newest `limit` kept. `min_level` applies to ALVR events only.
pub fn filter_logs(mut lines: Vec<LogLine>, q: &LogQuery) -> Result<Vec<LogLine>, String> {
    let source = q.source.as_deref().unwrap_or("all");
    if !LOG_SOURCES.contains(&source) {
        return Err(format!("source must be one of {LOG_SOURCES:?}"));
    }
    let min = match q.min_level.as_deref() {
        Some(level) => Some(severity(level).ok_or("min_level must be error, warning, info or debug")?),
        None => None,
    };
    let limit = q.limit.unwrap_or(100);
    if !(1..=LOGS_KEPT).contains(&limit) {
        return Err(format!("limit must be 1 to {LOGS_KEPT}"));
    }
    let contains = q.contains.as_deref().map(str::to_lowercase);
    // shortcut: sorted by time of day, so a window across midnight sorts wrongly; carry dates if that matters
    lines.sort_by(|a, b| a.t.cmp(&b.t)); // stable: equal times keep their source order
    lines.retain(|l| {
        (source == "all" || l.source == source)
            && (l.source != "alvr" || min.is_none_or(|m| severity(&l.level).is_some_and(|s| s >= m)))
            && contains.as_ref().is_none_or(|c| l.text.to_lowercase().contains(c))
    });
    Ok(lines.split_off(lines.len().saturating_sub(limit)))
}

#[derive(Default)]
struct State {
    headset: Option<Headset>,
    dashboard: Option<Child>,
    started_steam: bool,
    restarts: u32,
    /// The watchdog restarts SteamVR at most once per start().
    watchdog_fired: bool,
    leaked: bool,
    managing: bool,
}

/// Steam, SteamVR with ALVR, and the headset, plus the dashboard role ALVR expects.
pub struct Stack {
    cfg: Config,
    paths: Paths,
    api: Api,
    state: Mutex<State>,
    /// Held for a whole SteamVR restart, so stop() never races one.
    restarting: Mutex<()>,
    /// Results of the last startup checks.
    checks: Mutex<Vec<Check>>,
    /// The OpenVR reader and the vrserver pid it connected to. Its own lock, held across OpenVR
    /// calls (bounded by OPENVR_TIMEOUT), never together with `state`.
    reader: Mutex<Option<(u32, OpenVr)>>,
    on_change: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// The poses every headset sends; outlives headset restarts. Locked only briefly.
    rig: Arc<Mutex<Rig>>,
    /// The newest LOGS_KEPT ALVR log events the supervisor saw, oldest first. Locked only briefly.
    alvr_logs: Mutex<VecDeque<LogLine>>,
}

pub fn restore_guard(steamvr_running: bool) -> Result<()> {
    if steamvr_running {
        bail!("SteamVR is running; run vr_stop first");
    }
    Ok(())
}

impl Stack {
    pub fn new(cfg: Config, paths: Paths) -> Arc<Stack> {
        // shortcut: port read once here, re-read after prepare if web_server_port overrides must apply to a first start
        let api = paths.session_file().map_or(Api { port: 8082 }, |f| Api::from_session_file(&f));
        let stack = Arc::new(Stack {
            cfg,
            paths,
            api,
            state: Mutex::default(),
            restarting: Mutex::new(()),
            checks: Mutex::default(),
            reader: Mutex::new(None),
            on_change: Mutex::new(None),
            rig: Arc::default(),
            alvr_logs: Mutex::default(),
        });
        stack.recheck();
        let weak = Arc::downgrade(&stack);
        thread::spawn(move || supervise(weak));
        let weak = Arc::downgrade(&stack);
        thread::spawn(move || watch(weak));
        stack
    }

    /// Runs all startup checks again and keeps their results for `readiness`. Only at `new` and vr_start:
    /// some checks are slow.
    pub fn recheck(&self) {
        *self.checks.lock() = checks::startup_checks(&self.cfg, &self.paths);
    }

    /// The kept check results, with `alvr_dashboard_absent` re-run, so closing a dashboard un-hides vr_start.
    pub fn checks(&self) -> Vec<Check> {
        let mut checks = self.checks.lock();
        let dashboard = checks::dashboard_check();
        checks.iter_mut().filter(|c| c.name == dashboard.name).for_each(|c| *c = dashboard.clone());
        checks.clone()
    }

    /// The kept check results plus the runtime state, read now.
    pub fn readiness(&self) -> Readiness {
        let (streaming, ours_running) = {
            let state = self.state.lock();
            let streaming = state.headset.as_ref().is_some_and(|h| h.status().streaming);
            (streaming, state.headset.is_some() || state.dashboard.is_some() || state.started_steam)
        };
        let steamvr_running = steamvr::is_steamvr_running();
        Readiness {
            failed: self.checks.lock().iter().filter(|c| !c.ok).map(|c| (c.name, c.reason.clone())).collect(),
            restore_file: self.paths.restore_file().exists(),
            steamvr_running,
            alvr_api: self.api.ping(),
            streaming,
            running: steamvr_running || ours_running,
        }
    }

    /// Called from the watcher thread whenever `readiness` changes.
    pub fn set_on_change(&self, f: Box<dyn Fn() + Send + Sync>) {
        *self.on_change.lock() = Some(f);
    }

    pub fn restore(&self) -> Result<()> {
        restore_guard(steamvr::is_steamvr_running())?;
        setup::restore(&self.paths)
    }

    /// A screenshot of the headset's stream. The wait for the frame runs outside the state lock.
    pub fn screenshot(&self, eye: Eye) -> Result<Screenshot> {
        let grabber = self.state.lock().headset.as_ref().map(Headset::grabber).context("no headset")?;
        grabber.screenshot(&self.api, eye)
    }

    /// Changes the head or a controller; returns how long until an animation is done.
    pub fn apply(&self, a: RigAction) -> Result<Duration> {
        self.rig.lock().apply(a, Instant::now()).map_err(|e| anyhow!(e))
    }

    /// Haptic events the app sent with `t_s` after `since_s`, oldest first; `clear` empties the log.
    pub fn haptics(&self, since_s: f64, clear: bool) -> Vec<HapticEvent> {
        self.state.lock().headset.as_ref().map(|h| h.haptics(Some(since_s), clear)).unwrap_or_default()
    }

    /// SteamVR's own view of devices and hand skeletons.
    pub fn tracked_state(&self, include_bones: bool) -> Result<openvr::TrackedState> {
        self.read(move |vr| vr.tracked_state(include_bones))
    }

    /// None when SteamVR is not running or the reader cannot connect.
    pub fn scene_app(&self) -> Option<openvr::SceneApp> {
        self.read(|vr| Ok(vr.scene_app())).ok()
    }

    /// None when SteamVR is not running, no scene app runs, or the reader cannot connect.
    pub fn frame_stats(&self) -> Option<openvr::FrameStats> {
        self.read(|vr| vr.frame_stats()).ok()
    }

    /// Runs `f` on the OpenVR reader, which connects on first use and again when vrserver's pid
    /// changed. A reader that does not answer within OPENVR_TIMEOUT is left on its thread.
    // shortcut: a reader left stuck keeps OpenVR's process-global client, so the next connect may fail until the MCP restarts; fine while no hang is seen
    fn read<T: Send + 'static>(&self, f: impl FnOnce(&OpenVr) -> Result<T> + Send + 'static) -> Result<T> {
        // the reader was dropped for a vrserver that is going away; do not reconnect to it
        if self.restarting.is_locked() {
            bail!("SteamVR is stopping or restarting");
        }
        let mut slot = self.reader.lock();
        let vrserver = steamvr::pids_named("vrserver").first().copied();
        let reader = match slot.take() {
            Some((pid, reader)) if Some(pid) == vrserver => Some(reader),
            Some((_, stale)) => {
                dispose_reader(stale);
                None
            }
            None => None,
        };
        let pid = vrserver.context("SteamVR is not running")?;
        let steamvr_dir = self.paths.steamvr_dir.clone().context("SteamVR not found; set steamvr_dir in config.toml")?;
        let state_dir = self.paths.state_dir.clone();
        let (reader, result) = within(OPENVR_TIMEOUT, move || -> Result<_> {
            let reader = match reader {
                Some(r) => r,
                None => OpenVr::connect(&steamvr_dir, &state_dir)?,
            };
            let result = f(&reader);
            Ok((reader, result))
        })
        .with_context(|| format!("OpenVR did not answer within {} s", OPENVR_TIMEOUT.as_secs()))??;
        *slot = Some((pid, reader));
        result
    }

    /// Polls the reader's events, so a SteamVR quit from outside drops it instead of SteamVR
    /// killing the MCP. Skipped while a read holds the reader; the watcher calls it every 1 s.
    fn pump_reader(&self) {
        let Some(mut slot) = self.reader.try_lock() else { return };
        let Some((pid, reader)) = slot.take() else { return };
        match within(OPENVR_TIMEOUT, move || {
            let quit = reader.poll_quit();
            (reader, quit)
        }) {
            Some((reader, false)) => *slot = Some((pid, reader)),
            Some((reader, true)) => dispose_reader(reader),
            None => warn!("OpenVR events did not answer within {} s; reader left running", OPENVR_TIMEOUT.as_secs()),
        }
    }

    /// Disconnects the reader while vrserver is still up; called before every stop and restart.
    fn drop_reader(&self) {
        if let Some((_, reader)) = self.reader.lock().take() {
            dispose_reader(reader);
        }
    }

    fn headset_status(&self) -> Option<HeadsetStatus> {
        self.state.lock().headset.as_ref().map(Headset::status)
    }

    fn check_version(&self) -> Result<()> {
        let version = self.api.version()?;
        if !alvr_api::compatible(&version) {
            bail!("ALVR server {} is not compatible with {}", version.trim(), *alvr_common::ALVR_VERSION);
        }
        Ok(())
    }

    pub fn start(&self) -> Result<StartReport> {
        self.state.lock().watchdog_fired = false;
        let started = self.start_inner();
        if started.is_err() {
            self.state.lock().managing = false;
        }
        started
    }

    fn start_inner(&self) -> Result<StartReport> {
        let mut report = StartReport::default();
        if let Some(h) = self.headset_status().filter(|h| h.streaming) {
            let restarts = self.state.lock().restarts;
            return Ok(StartReport { already_streaming: true, restarts, view_resolution: h.view_resolution, fps: h.fps, codec: h.codec, ..report });
        }
        self.paths.session_file().context("ALVR streamer not found; set streamer_dir in config.toml")?;

        self.state.lock().headset.get_or_insert_with(|| Headset::start(Arc::clone(&self.rig)));
        let hostname = headset::client_hostname()?;
        let values = setup::session_values(&self.cfg)?;
        let steamvr_running = steamvr::is_steamvr_running();
        let mut restart_adopted = false;
        if !steamvr_running {
            if steamvr::has_leftovers(&steamvr::alive_vr_processes()) {
                if !steamvr::kill_vr_processes() {
                    bail!("SteamVR processes still running after SIGKILL: {:?}", steamvr::alive_vr_processes());
                }
                // SteamVR has exited, so it no longer rewrites the file
                setup::undo_safe_mode_blocks(&self.paths)?;
            }
            report.unblocked_safe_mode = setup::prepare(&self.paths, &values, &hostname)?.unblocked_safe_mode;
        } else if self.api.ping() {
            self.check_version()?;
            setup::snapshot(&self.paths)?;
            self.api.post_json("/api/session/values", &values)?;
            for action in setup::client_actions() {
                self.api.post_json("/api/session/client-connections", &(&hostname, action))?;
            }
            report.adopted = true;
            // shortcut: ALVR TCP EOF spin (sockets/src/stream_socket/tcp.rs:137-147) leaves the server stuck Disconnecting; drop when fixed upstream
            restart_adopted = client_stuck(&self.session()?, &hostname);
        } else {
            bail!("SteamVR is running without the ALVR driver; close SteamVR and retry");
        }

        {
            let mut state = self.state.lock();
            if state.dashboard.is_none() {
                state.dashboard = Some(steamvr::spawn_dashboard_child(&self.paths.runtime_dir)?);
            }
        }
        let steam_dir = self.paths.steam_dir.as_deref();
        let log_from = steam_dir.map_or(0, steamvr::console_log_len);
        if !steamvr_running {
            report.started_steam = steamvr::launch(&self.cfg, STEAMVR_URL)?;
            self.state.lock().started_steam |= report.started_steam;
        }

        self.state.lock().managing = true;
        if restart_adopted {
            self.restart_steamvr()?;
        }
        let timeout = Duration::from_secs(self.cfg.timeouts.streaming_s);
        let deadline = Instant::now() + timeout;
        if !steamvr::wait_launched(steam_dir, log_from, timeout, || self.api.ping())? {
            bail!("ALVR server did not answer on port {} within {} s", self.api.port, timeout.as_secs());
        }
        self.check_version()?;
        let streaming = steamvr::wait_for(deadline.saturating_duration_since(Instant::now()), || {
            // DecoderConfig follows StreamingStarted
            self.headset_status().is_some_and(|h| h.streaming && h.codec.is_some())
        });
        let h = self.headset_status().unwrap_or_default();
        if !streaming {
            let mut reason = format!("headset not streaming after {} s; last HUD message: {:?}", timeout.as_secs(), h.hud_message);
            if !self.state.lock().started_steam {
                reason += "; Steam was already running, so the MCP could not set its environment (steam.env, steam.args)";
            }
            bail!(reason);
        }
        report.restarts = self.state.lock().restarts;
        Ok(StartReport { view_resolution: h.view_resolution, fps: h.fps, codec: h.codec, ..report })
    }

    pub fn stop(&self) -> Result<StopReport> {
        self.state.lock().managing = false;
        let _no_restart = self.restarting.lock();
        self.drop_reader();
        let mut report = StopReport::default();
        // ALVR's client spins forever on a TCP stream whose server went away
        // (sockets/src/stream_socket/tcp.rs:137-147), so disconnect it while the server is up
        let headset = self.state.lock().headset.take();
        if let Some(h) = headset {
            self.dispose(h);
        }
        if self.api.ping() {
            // on failure the signals below still stop SteamVR
            self.api.post("/api/steamvr/shutdown").map_err(|e| error!("{e:#}")).ok();
        }
        if !steamvr::wait_gone(VR_PROCESS_NAMES, Duration::from_secs(30)) {
            // the next start clears the safe-mode block this causes
            steamvr::kill_vr_processes();
            report.used_signals = true;
            // SteamVR has exited, so it no longer rewrites the file
            setup::undo_safe_mode_blocks(&self.paths).map_err(|e| error!("{e:#}")).ok();
        }

        let (dashboard, started_steam) = {
            let mut state = self.state.lock();
            (state.dashboard.take(), std::mem::take(&mut state.started_steam))
        };
        if let Some(mut child) = dashboard {
            child.kill().ok();
            child.wait().ok();
        }
        if started_steam {
            if steamvr::is_steam_running() {
                steamvr::launch(&self.cfg, "-shutdown")?; // Steam is running, so this only forwards -shutdown
            }
            report.closed_steam = steamvr::wait_gone(&["steam"], Duration::from_secs(60));
        }
        report.remaining_processes = VR_PROCESS_NAMES
            .iter()
            .filter(|n| !steamvr::pids_named(n).is_empty())
            .map(|n| n.to_string())
            .collect();
        Ok(report)
    }

    pub fn status(&self) -> StatusReport {
        let reader_connected = self.reader.lock().is_some(); // own statement: read() locks it again
        StatusReport {
            steam_running: steamvr::is_steam_running(),
            steamvr_running: steamvr::is_steamvr_running(),
            alvr_version: self.api.version().ok().map(|v| v.trim().to_owned()),
            headset: self.headset_status(),
            scene_app: reader_connected.then(|| self.scene_app()).flatten(),
            warning: self.state.lock().leaked.then_some(LEAKED),
        }
    }

    /// Sets ALVR session values and checks ALVR stored them. Then, unless ALVR's schema flags every
    /// path real-time, restarts SteamVR and waits for a new stream.
    pub fn set_alvr_settings(&self, values: Vec<SettingValue>) -> Result<SettingsReport> {
        if self.restarting.is_locked() {
            bail!("not ready: SteamVR is restarting");
        }
        let values = parse_values(values)?;
        let restart = !values.iter().all(|v| is_real_time(&v.path));
        if restart && !self.state.lock().managing {
            bail!("these settings need a SteamVR restart, which only a vr_start session does; run vr_start first");
        }
        let mut events = self.api.events()?;
        self.api.post_json("/api/session/values", &values)?;
        // ALVR answers 200 even when it rejects the values: it logs why and sends no new session
        // (server_core web_server.rs:181-185, server_io lib.rs:156-187)
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut mismatch = "ALVR sent no session within 3 s".to_owned();
        while Instant::now() < deadline {
            match events.next(Duration::from_millis(200))? {
                Some(Event { event_type: EventType::Session(session), .. }) => match verify_applied(&serde_json::to_value(&*session)?, &values) {
                    Ok(()) => break,
                    Err(e) => mismatch = e,
                },
                Some(Event { event_type: EventType::Log(LogEntry { severity: LogSeverity::Error, content }), .. })
                    if content.starts_with("Failed to set session values") =>
                {
                    bail!(content)
                }
                _ => (),
            }
        }
        if Instant::now() >= deadline {
            bail!("ALVR did not apply the values: {mismatch}");
        }
        drop(events);

        let status = || self.headset_status().unwrap_or_default();
        if !restart {
            return Ok(SettingsReport { streaming: status().streaming, ..Default::default() });
        }
        let restarts = self.state.lock().restarts; // own statement: status() locks state too
        let generation = status().stream_generation;
        // shortcut: ALVR TCP EOF spin (sockets/src/stream_socket/tcp.rs:137-147): on loopback a headset reconnect strands the server, so restart SteamVR instead of reconnecting for ALVR's handshake check; drop when fixed upstream
        self.restart_steamvr()?;
        // a restart ALVR asks for once the headset negotiates (e.g. a new fps) is the supervisor's
        let timeout = Duration::from_secs(self.cfg.timeouts.streaming_s);
        let streaming = steamvr::wait_for(timeout, || {
            let h = status();
            h.streaming && h.codec.is_some() && h.stream_generation > generation
        });
        if !streaming {
            bail!("headset not streaming {} s after the SteamVR restart; last HUD message: {:?}", timeout.as_secs(), status().hud_message);
        }
        let restarts = self.state.lock().restarts.saturating_sub(restarts);
        Ok(SettingsReport { restarted: restarts > 0, restarts, streaming })
    }

    /// Launches a Steam app or non-Steam shortcut (`id` in decimal) through Steam and returns once
    /// SteamVR reports a new scene app running.
    pub fn launch_app(&self, id: &str) -> Result<AppReport> {
        let id: u64 = id.trim().parse().with_context(|| format!("app_id must be a decimal number, not {id:?}"))?;
        let steam = self.paths.steam_dir.as_deref().context("Steam not found; set steam_dir in config.toml")?;
        let app = steamvr::find_app(steam, id)?;
        // SteamVR Home is a scene app too, so wait for a scene pid that was not there before
        // shortcut: relaunching an app that already runs times out, as Steam only focuses it; check its process if that matters
        let before = self.scene_app().map(|a| a.pid);
        let log_from = steamvr::console_log_len(steam);
        steamvr::launch(&self.cfg, &format!("steam://rungameid/{}", app.game_id))?;
        let timeout = Duration::from_secs(self.cfg.timeouts.app_s);
        let mut scene_pid = 0;
        let running = steamvr::wait_launched(Some(steam), log_from, timeout, || {
            let scene = self.scene_app().filter(|a| a.state == "running" && a.pid != 0 && Some(a.pid) != before);
            scene_pid = scene.map_or(0, |a| a.pid);
            scene_pid != 0
        })?;
        if !running {
            bail!(
                "{} ({}) has no running VR scene after {} s; compatibility tool: {}; last vrserver log lines:\n{}",
                app.name,
                app.game_id,
                timeout.as_secs(),
                app.compat_tool.as_deref().unwrap_or("none"),
                steamvr::log_tail(steam, "vrserver", 20).join("\n")
            );
        }
        Ok(AppReport { app, scene_pid })
    }

    /// Stops the scene app: SIGTERM, then SIGKILL after 15 s.
    // shortcut: kills only the pid SteamVR reports, never by pattern; a Proton game's wine process tree may outlive it, kill the tree if a stray process blocks the next launch
    pub fn stop_app(&self) -> Result<Value> {
        let pid = self.scene_app().map(|a| a.pid).filter(|&pid| pid != 0).context("no scene app is running")?;
        let gone = || !std::path::Path::new(&format!("/proc/{pid}")).exists();
        steamvr::terminate(pid, libc::SIGTERM);
        if !steamvr::wait_for(Duration::from_secs(15), gone) {
            steamvr::terminate(pid, libc::SIGKILL);
            if !steamvr::wait_for(Duration::from_secs(5), gone) {
                bail!("scene app {pid} still runs 5 s after SIGKILL");
            }
        }
        Ok(serde_json::json!({ "stopped_pid": pid }))
    }

    /// The Index flavour (`steamvr_input_2_0` off) or the skeletal-tracker flavour (on).
    pub fn set_hand_tracking_flavour(&self, skeletal_tracker: bool) -> Result<SettingsReport> {
        self.set_alvr_settings(vec![SettingValue { path: STEAMVR_INPUT_2_0.into(), value: skeletal_tracker.into() }])
    }

    /// ALVR log events the supervisor kept, merged with the tails of SteamVR's own logs, filtered.
    pub fn logs(&self, q: LogQuery) -> Result<Vec<LogLine>> {
        let mut lines: Vec<LogLine> = self.alvr_logs.lock().iter().cloned().collect();
        if let Some(steam) = &self.paths.steam_dir {
            for name in STEAMVR_LOGS {
                lines.extend(steamvr_log_lines(name, steamvr::log_tail(steam, name, LOGS_KEPT)));
            }
        }
        filter_logs(lines, &q).map_err(|e| anyhow!(e))
    }

    fn keep_log(&self, t: String, entry: LogEntry) {
        let mut ring = self.alvr_logs.lock();
        if ring.len() == LOGS_KEPT {
            ring.pop_front();
        }
        ring.push_back(LogLine { t, source: "alvr".into(), level: level_name(entry.severity), text: entry.content });
    }

    /// Replaces the headset with a fresh one and restarts SteamVR so ALVR accepts it.
    pub fn recreate_headset(&self) -> Result<()> {
        let old = self.state.lock().headset.take();
        if let Some(h) = old {
            self.dispose(h); // the old client must stop listening on 9943 before the new one starts
        }
        let new = Headset::start(Arc::clone(&self.rig));
        self.state.lock().headset = Some(new);
        // shortcut: ALVR TCP EOF spin (sockets/src/stream_socket/tcp.rs:137-147) leaves the server stuck Disconnecting; drop when fixed upstream
        self.restart_steamvr()
    }

    /// The server's session, which ALVR sends as an event in reply to GET /api/session.
    fn session(&self) -> Result<SessionConfig> {
        let mut events = self.api.events()?;
        self.api.get("/api/session")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(Event { event_type: EventType::Session(session), .. }) = events.next(Duration::from_millis(500))? {
                return Ok(*session);
            }
        }
        bail!("ALVR sent no session within 5 s")
    }

    /// Drops a headset taken out of the state; one whose drop does not return in time is leaked.
    fn dispose(&self, headset: Headset) {
        if within(HEADSET_TIMEOUT, move || drop(headset)).is_none() {
            self.mark_leaked();
        }
    }

    // shortcut: ALVR TCP EOF spin on the client after the server exits (sockets/src/stream_socket/tcp.rs:137-147) never returns from pause/drop; after a 5 s timeout the headset is abandoned on its helper thread, which spins one core until the MCP exits; drop when fixed upstream
    fn mark_leaked(&self) {
        warn!("{LEAKED}");
        self.state.lock().leaked = true;
    }

    fn restart_steamvr(&self) -> Result<()> {
        let _restart = self.restarting.lock();
        if !self.state.lock().managing {
            bail!("SteamVR is being stopped; no restart");
        }
        // as in stop(): a client still streaming when vrserver exits never disconnects, so pause it
        // first, outside the state lock; a client whose server is already gone is replaced
        let taken = self.state.lock().headset.take(); // own statement: the guard must not outlive it
        let headset = taken.and_then(|h| {
            let paused = within(HEADSET_TIMEOUT, move || {
                h.pause();
                h
            });
            if paused.is_none() {
                self.mark_leaked();
            }
            paused
        });
        self.drop_reader();
        let restarted = steamvr::restart_steamvr(&self.cfg, &self.paths, &self.api);
        let headset = headset.inspect(Headset::resume).unwrap_or_else(|| Headset::start(Arc::clone(&self.rig)));
        let spare = {
            let mut state = self.state.lock();
            match state.headset {
                None => state.headset.replace(headset),
                Some(_) => Some(headset), // start() made one meanwhile; ours goes, outside the lock
            }
        };
        if let Some(h) = spare {
            self.dispose(h);
        }
        restarted.context("SteamVR restart failed")?;
        self.state.lock().restarts += 1;
        Ok(())
    }
}

/// Drops an OpenVR reader within OPENVR_TIMEOUT, or leaves it on its thread.
fn dispose_reader(reader: OpenVr) {
    if within(OPENVR_TIMEOUT, move || drop(reader)).is_none() {
        warn!("OpenVR reader did not disconnect within {} s; left running", OPENVR_TIMEOUT.as_secs());
    }
}

/// Recomputes readiness every 1 s and reports changes. Not in `supervise`, which blocks for a whole restart.
fn watch(stack: Weak<Stack>) {
    let mut last = Readiness::default();
    loop {
        let Some(s) = stack.upgrade() else { return };
        s.pump_reader();
        let now = s.readiness();
        if now != last {
            if let Some(f) = &*s.on_change.lock() {
                f();
            }
            last = now;
        }
        drop(s);
        thread::sleep(Duration::from_secs(1));
    }
}

/// Whether the watchdog counts this moment toward "quiet": the events stream is up and the headset
/// is not streaming. Not while a restart runs: it holds the headset out of state, and the old
/// server keeps the events stream up until it exits.
fn counts_as_quiet(streaming: bool, events_attached: bool, restarting: bool) -> bool {
    events_attached && !streaming && !restarting
}

/// The dashboard's other job: restart SteamVR when ALVR asks, and reconnect a headset that went quiet.
fn supervise(stack: Weak<Stack>) {
    let mut events = None;
    // since when the API is up but the headset is not streaming
    let mut quiet_since: Option<Instant> = None;
    let mut last_hud = String::new();
    loop {
        let Some(s) = stack.upgrade() else { return };
        if !s.state.lock().managing {
            (events, quiet_since) = (None, None);
            drop(s);
            thread::sleep(Duration::from_millis(500));
            continue;
        }

        let mut restart = false;
        match &mut events {
            None if s.api.ping() => events = s.api.events().map_err(|e| error!("ALVR events: {e:#}")).ok(),
            None => thread::sleep(Duration::from_millis(500)),
            Some(stream) => match stream.next(Duration::from_millis(500)) {
                Ok(Some(Event { event_type: EventType::ServerRequestsSelfRestart, .. })) => restart = true,
                Ok(Some(Event { timestamp, event_type: EventType::Log(entry) })) => s.keep_log(timestamp, entry),
                Ok(_) => (),
                Err(e) => {
                    warn!("ALVR events stream closed: {e:#}");
                    events = None;
                }
            },
        }
        let status = s.headset_status().unwrap_or_default();
        // a request sent before the events stream attached still shows on the headset's HUD
        restart |= is_restart_hud(&status.hud_message) && !is_restart_hud(&last_hud);
        last_hud = status.hud_message;
        if counts_as_quiet(status.streaming, events.is_some(), s.restarting.is_locked()) {
            quiet_since.get_or_insert_with(Instant::now);
        } else {
            quiet_since = None;
        }

        if restart {
            s.restart_steamvr().map_err(|e| error!("{e:#}")).ok();
            (events, quiet_since) = (None, None);
            last_hud = s.headset_status().map(|h| h.hud_message).unwrap_or_default();
        } else if quiet_since.is_some_and(|t| t.elapsed() > WATCHDOG) && !s.state.lock().watchdog_fired {
            // seen in the spike: an old client stops listening after a restart
            // shortcut: ALVR TCP EOF spin (sockets/src/stream_socket/tcp.rs:137-147) leaves the server stuck Disconnecting; drop when fixed upstream
            s.state.lock().watchdog_fired = true;
            s.recreate_headset().map_err(|e| error!("{e:#}")).ok(); // restarts SteamVR too
            (events, quiet_since) = (None, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vr_restore_refuses_while_steamvr_runs() {
        assert_eq!(restore_guard(true).unwrap_err().to_string(), "SteamVR is running; run vr_stop first");
        assert!(restore_guard(false).is_ok());
    }

    #[test]
    fn adoption_restarts_only_for_a_stuck_entry() {
        let mut session = alvr_session::SessionConfig::default();
        assert!(!client_stuck(&session, "h"), "no entry: nothing to unstick");
        let entry = |state| alvr_session::ClientConnectionConfig {
            display_name: String::new(),
            current_ip: None,
            manual_ips: Default::default(),
            trusted: true,
            connection_state: state,
        };
        session.client_connections.insert("h".into(), entry(ConnectionState::Disconnected));
        assert!(!client_stuck(&session, "h"));
        session.client_connections.insert("h".into(), entry(ConnectionState::Disconnecting));
        assert!(client_stuck(&session, "h"));
    }

    #[test]
    fn within_reports_a_stuck_closure_as_leaked() {
        assert_eq!(within(Duration::from_secs(1), || 5), Some(5));
        assert_eq!(within(Duration::from_millis(20), || thread::sleep(Duration::from_millis(300))), None);
    }

    fn pairs(path: &str, value: serde_json::Value) -> Vec<PathValuePair> {
        vec![PathValuePair { path: parse_path(path).unwrap(), value }]
    }

    #[test]
    fn verify_applied_detects_mismatch() {
        let session = json!({ "session_settings": { "video": { "preferred_fps": 72.0 } } });
        let e = verify_applied(&session, &pairs("session_settings.video.preferred_fps", json!(90.0))).unwrap_err();
        assert!(e.contains("session_settings.video.preferred_fps") && e.contains("72"), "{e}");
    }

    #[test]
    fn verify_applied_accepts_match() {
        let mut session = serde_json::to_value(SessionConfig::default()).unwrap();
        // ALVR stores f32, so a requested 0.9 comes back as 0.8999999761581421
        session["session_settings"]["video"]["preferred_fps"] = json!(0.9f32);
        assert_eq!(verify_applied(&session, &pairs("session_settings.video.preferred_fps", json!(0.9))), Ok(()));
        let index = "session_settings.headset.controllers.content.hand_skeleton.content.steamvr_input_2_0";
        session["session_settings"]["headset"]["controllers"]["content"]["hand_skeleton"]["content"]["steamvr_input_2_0"] = json!(true);
        assert_eq!(verify_applied(&session, &pairs(index, json!(true))), Ok(()));
        let e = verify_applied(&session, &pairs("session_settings.video.nope", json!(1))).unwrap_err();
        assert!(e.contains("session_settings.video.nope"), "{e}");
    }

    #[test]
    fn settings_reject_bad_path() {
        let e = parse_values(vec![SettingValue { path: "video..fps".into(), value: json!(90.0) }]).unwrap_err();
        assert!(format!("{e:#}").contains("invalid path"), "{e:#}");
    }

    #[test]
    fn real_time_follows_alvr_schema() {
        let rt = |p: &str| is_real_time(&parse_path(p).unwrap());
        assert!(!rt("session_settings.video.preferred_fps"), "flagged steamvr-restart");
        assert!(!rt("session_settings.headset.controllers.content.hand_skeleton.content.steamvr_input_2_0"));
        assert!(rt("session_settings.extra.logging.log_haptics"), "flagged leaf");
        assert!(rt("session_settings.headset.controllers.content.haptics.enabled"), "under a flagged entry");
        assert!(!rt("session_settings.video.nope"));
        assert!(!rt("client_connections"));
    }

    fn line(t: &str, source: &str, level: &str, text: &str) -> LogLine {
        LogLine { t: t.into(), source: source.into(), level: level.into(), text: text.into() }
    }

    fn query(source: &str, contains: Option<&str>, min_level: Option<&str>, limit: Option<usize>) -> LogQuery {
        LogQuery { source: Some(source.into()), contains: contains.map(Into::into), min_level: min_level.map(Into::into), limit }
    }

    #[test]
    fn logs_filter_by_level_and_text() {
        let ring = vec![
            line("10:00:00.000", "alvr", "info", "Connected"),
            line("10:00:01.000", "alvr", "error", "Encoder FAILED"),
            line("10:00:02.000", "alvr", "error", "second error"),
            line("10:00:03.000", "vrserver", "info", "failed to load"),
        ];
        let texts = |q| filter_logs(ring.clone(), &q).unwrap().into_iter().map(|l| l.text).collect::<Vec<_>>();
        assert_eq!(texts(query("alvr", None, Some("error"), None)), ["Encoder FAILED", "second error"]);
        assert_eq!(texts(query("all", None, Some("error"), None)), ["Encoder FAILED", "second error", "failed to load"], "min_level is for ALVR only");
        assert_eq!(texts(query("all", Some("fAiLeD"), None, None)), ["Encoder FAILED", "failed to load"]);
        assert_eq!(texts(query("all", None, None, Some(2))), ["second error", "failed to load"], "limit keeps the newest");
        assert_eq!(texts(LogQuery::default()).len(), 4, "all sources by default");
        for bad in [query("steamvr", None, None, None), query("all", None, Some("fatal"), None), query("all", None, None, Some(0))] {
            assert!(filter_logs(ring.clone(), &bad).is_err());
        }
    }

    #[test]
    fn logs_merge_sources_in_time_order() {
        let steam = steamvr_log_lines(
            "vrserver",
            ["Wed Oct 21 2026 05:00:36.212461 [Info] - b", "continuation", "Wed Oct 21 2026 05:00:38.000001 [Error] - d"].map(String::from).to_vec(),
        );
        assert_eq!(steam[0], line("05:00:36.212", "vrserver", "info", "b"));
        assert_eq!(steam[1], line("05:00:36.212", "vrserver", "info", "continuation"), "belongs to the line before");
        let alvr = vec![line("05:00:35.000", "alvr", "info", "a"), line("05:00:37.000", "alvr", "warning", "c")];
        let merged = filter_logs([alvr, steam].concat(), &LogQuery::default()).unwrap();
        assert_eq!(merged.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["a", "b", "continuation", "c", "d"]);
    }

    #[test]
    fn headset_is_not_quiet_during_a_restart() {
        assert!(counts_as_quiet(false, true, false), "attached, not streaming, no restart");
        assert!(!counts_as_quiet(true, true, false), "streaming");
        assert!(!counts_as_quiet(false, false, false), "no events stream");
        assert!(!counts_as_quiet(false, true, true), "a restart holds the headset out of state");
    }

    #[test]
    fn restart_hud_matches_client_core_format() {
        // client_core's set_hud_message prefixes every message with version, hostname and IP
        let hud = |m: &str| format!("ALVR v21.0.0-dev14\nhostname: 1234.client.local.\nIP: 192.0.2.1\n\n{m}");
        assert!(is_restart_hud(&hud(SERVER_RESTART_MESSAGE)));
        assert!(!is_restart_hud(&hud("Searching for streamer...")));
    }
}
