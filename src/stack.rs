use crate::{
    alvr_api::{self, Api},
    checks::{self, Check, Readiness},
    clock,
    config::{Config, Paths},
    headset::{self, Eye, HapticEvent, Headset, HeadsetStatus, Screenshot},
    lease::{self, Lease, Owner},
    openvr::{self, OpenVr},
    rig::{self, AppliedStep, Rig, RigAction, SequenceReport, SequenceStep},
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
use alvr_session::{RecenteringMode, SessionConfig, Settings, settings_schema::SchemaNode};
use chrono::{Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::VecDeque,
    process::Child,
    sync::{Arc, Weak},
    thread,
    time::{Duration, Instant, SystemTime},
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
    /// The last SteamVR restart that failed, also one the supervisor ran: "<age> s ago: <reason>".
    pub last_restart_error: Option<String>,
    /// Session clock: seconds since MCP start, and the same instant and T0 as RFC 3339 UTC.
    pub now_t_s: f64,
    pub now_at: String,
    pub t0_at: String,
    /// The SteamVR lease; none when free, expired or dead.
    pub owner: Option<Owner>,
    /// `driver_*` sections SteamVR blocked in safe mode, which a start or restart would clear.
    pub blocked_drivers: Vec<String>,
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
    /// Seconds on the session clock; `at` is the same instant as RFC 3339 UTC.
    pub t_s: f64,
    pub at: String,
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
/// continues the one before and gets its time and level. One before any stamped line is dropped.
pub fn steamvr_log_lines(source: &str, lines: Vec<String>) -> Vec<LogLine> {
    let mut last: Option<(String, f64, String, String)> = None;
    lines
        .into_iter()
        .filter_map(|line| {
            let parsed = line.split_once(" [").and_then(|(stamp, rest)| {
                let (lvl, text) = rest.split_once("] - ")?;
                let naive = NaiveDateTime::parse_from_str(stamp, "%a %b %d %Y %H:%M:%S%.f").ok()?;
                let local = Local.from_local_datetime(&naive).earliest()?;
                let t_s = clock::wall_t_s(local.into());
                Some(((naive.format("%H:%M:%S%.3f").to_string(), t_s, lvl.to_lowercase()), text.to_owned()))
            });
            let text = match parsed {
                Some(((t, t_s, level), text)) => {
                    last = Some((t, t_s, clock::at(t_s), level));
                    text
                }
                None => line,
            };
            let (t, t_s, at, level) = last.clone()?;
            Some(LogLine { t_s, at, t, source: source.to_owned(), level, text })
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
    lines.sort_by(|a, b| a.t_s.total_cmp(&b.t_s)); // stable: equal times keep their source order
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
    /// When and why the last SteamVR restart failed.
    last_restart_error: Option<(Instant, String)>,
}

/// Steam, SteamVR with ALVR, and the headset, plus the dashboard role ALVR expects.
pub struct Stack {
    /// Config, Paths and the API port: reloaded by `recheck` while nothing runs. Locked only briefly.
    cfg: Mutex<Config>,
    paths: Mutex<Paths>,
    api: Mutex<Api>,
    state: Mutex<State>,
    /// Held for a whole SteamVR restart, so stop() never races one.
    restarting: Mutex<()>,
    /// Held for a whole vr_start, so a second one refuses.
    starting: Mutex<()>,
    /// Results of the last startup checks.
    checks: Mutex<Vec<Check>>,
    /// config.toml's text when it was last read, so vr_status notices an edit.
    config_text: Mutex<Option<String>>,
    /// The OpenVR reader and the vrserver pid it connected to. Its own lock, held across OpenVR
    /// calls (bounded by OPENVR_TIMEOUT), never together with `state`.
    reader: Mutex<Option<(u32, OpenVr)>>,
    on_change: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// The poses every headset sends; outlives headset restarts. Locked only briefly.
    rig: Arc<Mutex<Rig>>,
    /// The newest LOGS_KEPT ALVR log events the supervisor saw, oldest first. Locked only briefly.
    alvr_logs: Mutex<VecDeque<LogLine>>,
    /// The owner name and ttl the lease is taken and renewed with.
    lease: Mutex<(String, Duration)>,
}

const RECENTER_NOT_STAGE: &str =
    "recentering_mode is not Stage: ALVR re-origins the playspace around the head's last pose on every playspace update, so poses sent are no longer absolute";

/// The scene app's pid, unless the scene is SteamVR's own (Home runs as steamtours). `comm` names a
/// pid; one without a readable comm, or out of kill()'s single-process range, is refused.
pub fn game_pid(scene: Option<&openvr::SceneApp>, comm: impl Fn(u32) -> Option<String>) -> Result<u32, &'static str> {
    let pid = scene.map(|a| a.pid).filter(|&pid| pid != 0).ok_or("no scene app is running")?;
    let name = comm(pid).filter(|_| steamvr::signalable(pid).is_some()).ok_or("the scene app's pid is not a running process")?;
    match VR_PROCESS_NAMES.contains(&name.as_str()) {
        true => Err("no app running (scene is SteamVR Home)"),
        false => Ok(pid),
    }
}

/// vr_stop signals SteamVR only when it is ALVR's (`alvrs`), never one the user runs without ALVR.
pub fn stop_guard(steamvr_running: bool, alvrs: impl FnOnce() -> bool) -> Result<()> {
    if steamvr_running && !alvrs() {
        bail!("SteamVR is running without ALVR; close it yourself");
    }
    Ok(())
}

pub fn restore_guard(steamvr_running: bool) -> Result<()> {
    if steamvr_running {
        bail!("SteamVR is running; run vr_stop first");
    }
    Ok(())
}

impl Stack {
    pub fn new(cfg: Config, paths: Paths) -> Arc<Stack> {
        let stack = Arc::new(Stack {
            api: Mutex::new(api_for(&paths)),
            cfg: Mutex::new(cfg),
            paths: Mutex::new(paths),
            state: Mutex::default(),
            restarting: Mutex::new(()),
            starting: Mutex::new(()),
            checks: Mutex::default(),
            config_text: Mutex::new(Config::text()),
            reader: Mutex::new(None),
            on_change: Mutex::new(None),
            rig: Arc::default(),
            alvr_logs: Mutex::default(),
            lease: Mutex::new((format!("mcp-{}", std::process::id()), lease::DEFAULT_TTL)),
        });
        stack.run_checks();
        let weak = Arc::downgrade(&stack);
        thread::spawn(move || supervise(weak));
        let weak = Arc::downgrade(&stack);
        thread::spawn(move || watch(weak));
        stack
    }

    fn cfg(&self) -> Config {
        self.cfg.lock().clone()
    }

    fn paths(&self) -> Paths {
        self.paths.lock().clone()
    }

    fn api(&self) -> Api {
        *self.api.lock()
    }

    /// Re-reads config.toml, applies it while nothing runs, and runs all startup checks again. At
    /// vr_start, and at vr_status after an edit or while a core check fails: some checks are slow.
    pub fn recheck(&self) {
        *self.config_text.lock() = Config::text();
        if let Ok(cfg) = Config::load() {
            self.reload(cfg);
        }
        self.run_checks();
    }

    /// Applies `cfg` with freshly detected Paths, unless a vr_start, its session, an MCP-started Steam or SteamVR runs.
    fn reload(&self, cfg: Config) -> bool {
        let busy = {
            let state = self.state.lock();
            state.managing || state.headset.is_some() || state.dashboard.is_some() || state.started_steam
        };
        if busy || self.starting.is_locked() || steamvr::is_steamvr_running() {
            return false;
        }
        let paths = Paths::detect(&cfg);
        *self.api.lock() = api_for(&paths);
        *self.paths.lock() = paths;
        *self.cfg.lock() = cfg;
        true
    }

    fn run_checks(&self) {
        *self.checks.lock() = checks::startup_checks(&self.cfg(), &self.paths());
    }

    /// The kept check results, with `alvr_dashboard_absent` re-run. All checks run again, with
    /// config.toml re-read, when the file changed or a core check fails, so fixing config.toml or the
    /// machine un-hides vr_start without an MCP restart.
    pub fn checks(&self) -> Vec<Check> {
        let core_failing = self.checks.lock().iter().any(|c| !c.ok && checks::CORE.contains(&c.name));
        let edited = Config::text() != *self.config_text.lock();
        if core_failing || edited {
            self.recheck();
        }
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
            (streaming, state.managing || state.headset.is_some() || state.dashboard.is_some() || state.started_steam)
        };
        let steamvr_running = steamvr::is_steamvr_running();
        let alvr_api = self.api().ping();
        Readiness {
            failed: self.checks.lock().iter().filter(|c| !c.ok).map(|c| (c.name, c.reason.clone())).collect(),
            restore_file: self.paths().restore_file().exists(),
            steamvr_running,
            alvr_api,
            streaming,
            running: ours_running || (steamvr_running && (alvr_api || steamvr::vrserver_has_alvr())),
            leased_to: lease::blocking(lease::read(&self.paths().runtime_dir).as_ref(), std::process::id(), SystemTime::now(), lease::alive)
                .map(lease::holder_text),
        }
    }

    /// Before a mutating tool: takes the lease, or renews ours once half the ttl is gone.
    pub fn claim(&self) -> Result<()> {
        let (owner, ttl) = self.lease.lock().clone();
        let dir = self.paths().runtime_dir;
        let ours = lease::read(&dir).is_some_and(|l| l.pid == std::process::id() && !lease::renew_due(&l, ttl, SystemTime::now()));
        if ours {
            return Ok(());
        }
        lease::acquire(&dir, &owner, ttl).map(drop)
    }

    /// vr_acquire: takes or renews the lease under `owner`, which later claims keep using.
    pub fn acquire_lease(&self, owner: String, ttl_s: Option<u64>) -> Result<Lease> {
        if !(1..=64).contains(&owner.chars().count()) {
            bail!("owner must be 1 to 64 characters");
        }
        let ttl = match ttl_s {
            None => lease::DEFAULT_TTL,
            Some(s) if lease::TTL_S.contains(&s) => Duration::from_secs(s),
            Some(s) => bail!("ttl_s {s} is outside {} to {}", lease::TTL_S.start(), lease::TTL_S.end()),
        };
        let taken = lease::acquire(&self.paths().runtime_dir, &owner, ttl)?;
        *self.lease.lock() = (owner, ttl);
        Ok(taken)
    }

    /// Whether this MCP held the lease.
    pub fn release_lease(&self) -> Result<bool> {
        lease::release(&self.paths().runtime_dir)
    }

    /// Called from the watcher thread whenever `readiness` changes.
    pub fn set_on_change(&self, f: Box<dyn Fn() + Send + Sync>) {
        *self.on_change.lock() = Some(f);
    }

    /// A finished stop or restore leaves nothing running, so hold nothing.
    fn release_after_success(&self) {
        self.release_lease().map_err(|e| error!("{e:#}")).ok();
    }

    pub fn restore(&self) -> Result<()> {
        restore_guard(steamvr::is_steamvr_running())?;
        setup::restore(&self.paths())?;
        self.release_after_success();
        Ok(())
    }

    /// A screenshot of the headset's stream. The wait for the frame runs outside the state lock.
    pub fn screenshot(&self, eye: Eye) -> Result<Screenshot> {
        let grabber = self.state.lock().headset.as_ref().map(Headset::grabber).context("no headset")?;
        grabber.screenshot(&self.api(), eye)
    }

    /// Changes the head or a controller; returns how long until an animation is done.
    pub fn apply(&self, a: RigAction) -> Result<Duration> {
        self.rig.lock().apply(a, Instant::now()).map_err(|e| anyhow!(e))
    }

    /// Runs the steps at their `at_s` offsets and returns when the last one and the latest animation are done.
    /// A step the rig would refuse fails the call before any runs (checked on a copy of the rig); one that fails anyway, because the rig changed meanwhile, is reported and the rest still run. No lock is held while waiting.
    pub async fn run_sequence(&self, steps: Vec<SequenceStep>) -> Result<SequenceReport> {
        let rig = self.rig.lock().clone(); // own statement: no lock while planning
        let plan = rig::plan_sequence(steps, &rig).map_err(|e| anyhow!(e))?;
        let (started_t_s, start) = (clock::now_s(), tokio::time::Instant::now());
        let (mut end, mut steps) = (start, vec![]);
        for (step, action) in plan {
            tokio::time::sleep_until(start + Duration::from_secs_f32(step.at_s)).await;
            let result = self.apply(action);
            let applied_s = start.elapsed().as_secs_f32();
            if let Ok(done_after) = &result {
                end = end.max(tokio::time::Instant::now() + *done_after);
            }
            let (ok, error) = (result.is_ok(), result.err().map(|e| e.to_string()));
            steps.push(AppliedStep { at_s: step.at_s, action: step.action, applied_s, ok, error });
        }
        tokio::time::sleep_until(end).await;
        Ok(SequenceReport { started_t_s, started_at: clock::at(started_t_s), steps, total_s: start.elapsed().as_secs_f32() })
    }

    /// Sets the play area (meters). The tracking thread sends it; the result says what ALVR's recentering mode does with it.
    pub fn set_playspace(&self, width: f32, depth: f32) -> Result<Value> {
        self.rig.lock().set_playspace(width, depth).map_err(|e| anyhow!(e))?;
        let warning = match self.session() {
            Ok(session) if matches!(session.to_settings().headset.recentering_mode, RecenteringMode::Stage) => None,
            Ok(_) => Some(RECENTER_NOT_STAGE.to_owned()),
            Err(e) => Some(format!("could not read recentering_mode ({e:#}); {RECENTER_NOT_STAGE}")),
        };
        Ok(serde_json::json!({ "ok": true, "width": width, "depth": depth, "warning": warning }))
    }

    /// Sends the current play area again, which is what makes ALVR recenter.
    pub fn recenter(&self) -> Value {
        self.rig.lock().recenter();
        serde_json::json!({ "ok": true, "note": "In Stage recentering, the MCP's default, ALVR's recenter is the identity: poses stay absolute and nothing moves. Other recentering_mode values re-origin the playspace around the head's last pose." })
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
        let steamvr_dir = self.paths().steamvr_dir.clone().context("SteamVR not found; set steamvr_dir in config.toml")?;
        let state_dir = self.paths().state_dir.clone();
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
        let version = self.api().version()?;
        if !alvr_api::compatible(&version) {
            bail!("ALVR server {} is not compatible with {}", version.trim(), *alvr_common::ALVR_VERSION);
        }
        Ok(())
    }

    pub fn start(&self) -> Result<StartReport> {
        let Some(_starting) = self.starting.try_lock() else { bail!("not ready: vr_start is already running") };
        if self.restarting.is_locked() {
            bail!("not ready: SteamVR is restarting");
        }
        self.state.lock().watchdog_fired = false;
        let started = self.start_inner();
        if started.is_err() {
            self.state.lock().managing = false;
        }
        started
    }

    fn start_inner(&self) -> Result<StartReport> {
        let began = Instant::now();
        let mut report = StartReport::default();
        if let Some(h) = self.headset_status().filter(|h| h.streaming) {
            let restarts = {
                let mut state = self.state.lock();
                state.managing = true; // a stream the MCP left unmanaged gets its supervisor back
                state.restarts
            };
            return Ok(StartReport { already_streaming: true, restarts, view_resolution: h.view_resolution, fps: h.fps, codec: h.codec, ..report });
        }
        self.paths().session_file().context("ALVR streamer not found; set streamer_dir in config.toml")?;

        self.state.lock().headset.get_or_insert_with(|| Headset::start(Arc::clone(&self.rig)));
        let hostname = headset::client_hostname()?;
        let values = setup::session_values(&self.cfg())?;
        let steamvr_running = steamvr::is_steamvr_running();
        let mut restart_adopted = false;
        if !steamvr_running {
            if steamvr::has_leftovers(&steamvr::alive_vr_processes()) {
                if !steamvr::kill_vr_processes() {
                    bail!("SteamVR processes still running after SIGKILL: {:?}", steamvr::alive_vr_processes());
                }
            }
            // nothing runs, so SteamVR no longer rewrites the file; a crash or kill left blocks behind
            setup::undo_safe_mode_blocks(&self.paths())?;
            report.unblocked_safe_mode = setup::prepare(&self.paths(), &values, &hostname)?.unblocked_safe_mode;
        } else if self.api().ping() {
            self.check_version()?;
            setup::snapshot(&self.paths())?;
            self.api().post_json("/api/session/values", &values)?;
            for action in setup::client_actions() {
                self.api().post_json("/api/session/client-connections", &(&hostname, action))?;
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
                state.dashboard = Some(steamvr::spawn_dashboard_child(&self.paths().runtime_dir)?);
            }
        }
        let paths = self.paths();
        let steam_dir = paths.steam_dir.as_deref();
        let log_from = steam_dir.map_or(0, steamvr::console_log_len);
        if !steamvr_running {
            report.started_steam = steamvr::launch(&self.cfg(), STEAMVR_URL)?;
            self.state.lock().started_steam |= report.started_steam;
        }

        self.state.lock().managing = true;
        if restart_adopted {
            self.restart_steamvr(false)?;
        }
        let timeout = Duration::from_secs(self.cfg().timeouts.streaming_s);
        let deadline = Instant::now() + timeout;
        let stopped = || !self.state.lock().managing;
        if !steamvr::wait_launched(steam_dir, log_from, timeout, || stopped() || self.api().ping())? {
            bail!("ALVR server did not answer on port {} within {} s", self.api().port, timeout.as_secs());
        }
        if stopped() {
            bail!("vr_stop ended the session");
        }
        self.check_version()?;
        let streaming = self.wait_streaming(began, deadline.saturating_duration_since(Instant::now()), 0)?;
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
        let ours = {
            let state = self.state.lock();
            state.managing || state.started_steam
        };
        stop_guard(steamvr::is_steamvr_running(), || ours || self.api().ping() || steamvr::vrserver_has_alvr())?;
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
        if self.api().ping() {
            // on failure the signals below still stop SteamVR
            self.api().post("/api/steamvr/shutdown").map_err(|e| error!("{e:#}")).ok();
        }
        let gone = steamvr::wait_gone(VR_PROCESS_NAMES, Duration::from_secs(30)) || {
            report.used_signals = true;
            steamvr::kill_vr_processes()
        };
        if gone {
            // SteamVR has exited, so it no longer rewrites the file
            setup::undo_safe_mode_blocks(&self.paths()).map_err(|e| error!("{e:#}")).ok();
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
                steamvr::launch(&self.cfg(), "-shutdown")?; // Steam is running, so this only forwards -shutdown
            }
            report.closed_steam = steamvr::wait_gone(&["steam"], Duration::from_secs(60));
        }
        report.remaining_processes = VR_PROCESS_NAMES
            .iter()
            .filter(|n| !steamvr::pids_named(n).is_empty())
            .map(|n| n.to_string())
            .collect();
        self.release_after_success();
        Ok(report)
    }

    pub fn status(&self) -> StatusReport {
        let reader_connected = self.reader.lock().is_some(); // own statement: read() locks it again
        let (leaked, last_restart_error) = {
            let state = self.state.lock();
            (state.leaked, state.last_restart_error.as_ref().map(|(t, e)| format!("{} s ago: {e}", t.elapsed().as_secs())))
        };
        let now = clock::now_s();
        let held = lease::read(&self.paths().runtime_dir);
        // pid 0 is nobody, so our own live lease shows too
        let owner = lease::blocking(held.as_ref(), 0, SystemTime::now(), lease::alive)
            .map(|l| Owner { mine: l.pid == std::process::id(), lease: l.clone() });
        StatusReport {
            steam_running: steamvr::is_steam_running(),
            steamvr_running: steamvr::is_steamvr_running(),
            alvr_version: self.api().quick_version().ok().map(|v| v.trim().to_owned()),
            headset: self.headset_status(),
            scene_app: reader_connected.then(|| self.scene_app()).flatten(),
            warning: leaked.then_some(LEAKED),
            last_restart_error,
            now_t_s: now,
            now_at: clock::at(now),
            t0_at: clock::at(0.0),
            owner,
            blocked_drivers: self.paths().vrsettings_file().map(|f| setup::blocked_drivers(&f)).unwrap_or_default(),
        }
    }

    /// Waits up to `timeout` for a stream with a codec (DecoderConfig follows StreamingStarted) and a
    /// generation above `after`. Fails at once when vr_stop ends the session, or a SteamVR restart
    /// that failed after `since` (one the supervisor ran too) means no stream will come.
    fn wait_streaming(&self, since: Instant, timeout: Duration, after: u64) -> Result<bool> {
        let mut aborted = None;
        let streaming = steamvr::wait_for(timeout, || {
            let state = self.state.lock();
            aborted = match &state.last_restart_error {
                _ if !state.managing => Some("vr_stop ended the session".to_owned()),
                Some((t, e)) if *t >= since => Some(e.clone()),
                _ => None,
            };
            let h = state.headset.as_ref().map(Headset::status).unwrap_or_default();
            aborted.is_some() || (h.streaming && h.codec.is_some() && h.stream_generation > after)
        });
        match aborted {
            Some(why) => bail!(why),
            None => Ok(streaming),
        }
    }

    /// Sets ALVR session values and checks ALVR stored them. Then, unless ALVR's schema flags every
    /// path real-time, restarts SteamVR and waits for a new stream.
    pub fn set_alvr_settings(&self, values: Vec<SettingValue>) -> Result<SettingsReport> {
        let began = Instant::now();
        if self.restarting.is_locked() {
            bail!("not ready: SteamVR is restarting");
        }
        let values = parse_values(values)?;
        let restart = !values.iter().all(|v| is_real_time(&v.path));
        if restart && !self.state.lock().managing {
            bail!("these settings need a SteamVR restart, which only a vr_start session does; run vr_start first");
        }
        let mut events = self.api().events()?;
        self.api().post_json("/api/session/values", &values)?;
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
        const STORED: &str = "the values are already stored in ALVR; SteamVR may be down";
        // shortcut: ALVR TCP EOF spin (sockets/src/stream_socket/tcp.rs:137-147): on loopback a headset reconnect strands the server, so restart SteamVR instead of reconnecting for ALVR's handshake check; drop when fixed upstream
        self.restart_steamvr(false).context(STORED)?;
        // a restart ALVR asks for once the headset negotiates (e.g. a new fps) is the supervisor's
        let timeout = Duration::from_secs(self.cfg().timeouts.streaming_s);
        let streaming = self.wait_streaming(began, timeout, generation).context(STORED)?;
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
        let paths = self.paths();
        let steam = paths.steam_dir.as_deref().context("Steam not found; set steam_dir in config.toml")?;
        let app = steamvr::find_app(steam, id)?;
        // SteamVR Home is a scene app too, so wait for a scene pid that was not there before
        // shortcut: relaunching an app that already runs times out, as Steam only focuses it; check its process if that matters
        let before = self.scene_app().map(|a| a.pid);
        let log_from = steamvr::console_log_len(steam);
        steamvr::launch(&self.cfg(), &format!("steam://rungameid/{}", app.game_id))?;
        let timeout = Duration::from_secs(self.cfg().timeouts.app_s);
        let mut scene_pid = 0;
        let running = steamvr::wait_launched(Some(steam), log_from, timeout, || {
            let scene = self.scene_app().filter(|a| a.state == "running" && Some(a.pid) != before);
            scene_pid = game_pid(scene.as_ref(), steamvr::comm).unwrap_or(0);
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
        let pid = game_pid(self.scene_app().as_ref(), steamvr::comm).map_err(|e| anyhow!(e))?;
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
        if let Some(steam) = &self.paths().steam_dir {
            for name in STEAMVR_LOGS {
                lines.extend(steamvr_log_lines(name, steamvr::log_tail(steam, name, LOGS_KEPT)));
            }
        }
        filter_logs(lines, &q).map_err(|e| anyhow!(e))
    }

    fn keep_log(&self, t: String, entry: LogEntry) {
        let t_s = clock::now_s();
        let mut ring = self.alvr_logs.lock();
        if ring.len() == LOGS_KEPT {
            ring.pop_front();
        }
        ring.push_back(LogLine { t_s, at: clock::at(t_s), t, source: "alvr".into(), level: level_name(entry.severity), text: entry.content });
    }

    /// Replaces the headset with a fresh one and restarts SteamVR so ALVR accepts it.
    pub fn recreate_headset(&self) -> Result<()> {
        // shortcut: ALVR TCP EOF spin (sockets/src/stream_socket/tcp.rs:137-147) leaves the server stuck Disconnecting; drop when fixed upstream
        self.restart_steamvr(true)
    }

    /// The server's session, which ALVR sends as an event in reply to GET /api/session.
    fn session(&self) -> Result<SessionConfig> {
        let mut events = self.api().events()?;
        self.api().get("/api/session")?;
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

    /// Restarts SteamVR around the headset, or with a new headset if `fresh_headset`. A failure is
    /// kept for vr_status and for the streaming waits.
    fn restart_steamvr(&self, fresh_headset: bool) -> Result<()> {
        let _restart = self.restarting.lock();
        if !self.state.lock().managing {
            bail!("SteamVR is being stopped; no restart");
        }
        // as in stop(): a client still streaming when vrserver exits never disconnects, so pause it
        // first, outside the state lock; a client whose server is already gone is replaced
        let mut taken = self.state.lock().headset.take(); // own statement: the guard must not outlive it
        if fresh_headset && let Some(old) = taken.take() {
            self.dispose(old); // the old client must stop listening on 9943 before the new one starts
        }
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
        let mut started_steam = false;
        let stopped = || !self.state.lock().managing;
        let restarted = steamvr::restart_steamvr(&self.cfg(), &self.paths(), &self.api(), stopped, &mut started_steam);
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
        let mut state = self.state.lock();
        state.started_steam |= started_steam;
        record_restart(&mut state, restarted)
    }
}

/// Counts a restart that worked and forgets an older failure; keeps one that failed for vr_status
/// and the streaming waits, unless vr_stop caused it.
fn record_restart(state: &mut State, restarted: Result<()>) -> Result<()> {
    match restarted.context("SteamVR restart failed") {
        Ok(()) => {
            state.restarts += 1;
            state.last_restart_error = None;
            Ok(())
        }
        Err(e) => {
            if state.managing {
                state.last_restart_error = Some((Instant::now(), format!("{e:#}")));
            }
            Err(e)
        }
    }
}

/// The API at the session's port, ALVR's default 8082 while no streamer is known.
// shortcut: port read at MCP start and reload, re-read after prepare if web_server_port overrides must apply to a first start
fn api_for(paths: &Paths) -> Api {
    paths.session_file().map_or(Api { port: 8082 }, |f| Api::from_session_file(&f))
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
            None if s.api().ping() => events = s.api().events().map_err(|e| error!("ALVR events: {e:#}")).ok(),
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
            s.restart_steamvr(false).map_err(|e| error!("{e:#}")).ok();
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

    fn test_stack() -> Arc<Stack> {
        let paths = Paths {
            steam_dir: None,
            flatpak_steam: false,
            steamvr_dir: None,
            streamer_dir: None,
            state_dir: "/nonexistent".into(),
            runtime_dir: "/nonexistent".into(),
        };
        Stack::new(Config::default(), paths)
    }

    /// A temp Steam tree with a blocked driver and, when `restore`, a restore.json that unblocks it.
    fn blocked_stack(name: &str, restore: bool) -> (Arc<Stack>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("vr-test-mcp-stack-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let (steam_dir, state_dir) = (root.join("steam"), root.join("state"));
        std::fs::create_dir_all(steam_dir.join("config")).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(steam_dir.join("config/steamvr.vrsettings"), r#"{"driver_lighthouse":{"blocked_by_safe_mode":true}}"#).unwrap();
        if restore {
            let state = setup::RestoreState {
                vrsettings_file: Some(setup::FileState::Contents(r#"{"driver_lighthouse":{}}"#.into())),
                ..Default::default()
            };
            std::fs::write(state_dir.join("restore.json"), serde_json::to_vec(&state).unwrap()).unwrap();
        }
        let paths = Paths {
            steam_dir: Some(steam_dir),
            flatpak_steam: false,
            steamvr_dir: None,
            streamer_dir: None,
            runtime_dir: root.join("run"),
            state_dir,
        };
        (Stack::new(Config::default(), paths), root)
    }

    #[test]
    fn vr_stop_clears_safe_mode_blocks_when_steamvr_is_gone() {
        if !steamvr::alive_vr_processes().is_empty() {
            return eprintln!("skipped: SteamVR is running");
        }
        let (stack, root) = blocked_stack("stop", true);
        let file = stack.paths().vrsettings_file().unwrap();
        stack.claim().unwrap();
        stack.stop().unwrap();
        assert!(setup::blocked_drivers(&file).is_empty());
        assert!(stack.status().owner.is_none(), "a finished stop gives up the lease");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn vr_restore_releases_the_lease() {
        if !steamvr::alive_vr_processes().is_empty() {
            return eprintln!("skipped: SteamVR is running");
        }
        let (stack, root) = blocked_stack("restore", false);
        stack.claim().unwrap();
        stack.restore().unwrap();
        assert!(stack.status().owner.is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn vr_status_reports_blocked_drivers() {
        let (stack, root) = blocked_stack("status", false);
        assert_eq!(stack.status().blocked_drivers, ["driver_lighthouse"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn claim_takes_the_lease_and_status_shows_it() {
        let dir = std::env::temp_dir().join(format!("vr-test-mcp-stack-lease-{}", std::process::id()));
        let stack = test_stack();
        stack.paths.lock().runtime_dir = dir.clone();
        assert!(stack.status().owner.is_none());
        assert!(stack.acquire_lease(String::new(), None).is_err());
        assert!(stack.acquire_lease("x".into(), Some(599)).is_err());
        stack.claim().unwrap();
        let owner = stack.status().owner.unwrap();
        assert!(owner.mine && owner.lease.owner.starts_with("mcp-"));
        assert_eq!(stack.readiness().leased_to, None, "ours does not block us");
        assert_eq!(stack.acquire_lease("driver".into(), Some(600)).unwrap().owner, "driver");
        assert!(stack.release_lease().unwrap());
        assert!(stack.status().owner.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn seq_step(at_s: f32, action: &str, args: Value) -> SequenceStep {
        SequenceStep { at_s, action: action.into(), args }
    }

    #[tokio::test]
    async fn sequence_waits_for_animations_and_refuses_a_bad_step_before_any_runs() {
        let stack = test_stack();
        let head_y = |s: &Stack| s.rig.lock().tracking(Instant::now(), Duration::ZERO).device_motions[0].1.pose.position.y;
        let animate = seq_step(0.0, "animate_head", json!({ "waypoints": [{ "position": [0, 1.0, 0] }], "duration_s": 0.3 }));
        let bad = seq_step(0.1, "set_trigger", json!({ "hand": "left", "value": 7 }));
        let started = Instant::now();
        let err = stack.run_sequence(vec![animate.clone(), bad]).await.unwrap_err();
        assert!(err.to_string().starts_with("step 1:") && started.elapsed() < Duration::from_secs(1), "{err:#}");
        assert_eq!(head_y(&stack), 1.6, "the animation never started");
        assert!(stack.run_sequence(vec![seq_step(0.0, "screenshot", json!({}))]).await.is_err());

        let r = stack.run_sequence(vec![seq_step(0.1, "set_trigger", json!({ "hand": "left", "value": 1 })), animate]).await.unwrap();
        assert!(r.steps.iter().all(|s| s.ok) && r.steps[1].applied_s >= 0.1 && r.total_s >= 0.3 && r.total_s < 2.0, "{r:?}");
        assert_eq!(head_y(&stack), 1.0);
    }

    #[tokio::test]
    async fn sequence_reports_a_step_that_fails_mid_run_and_goes_on() {
        let stack = test_stack();
        let off = RigAction::SetControllerState(serde_json::from_value(json!({ "hand": "left", "connected": false })).unwrap());
        let steps = vec![
            seq_step(0.2, "set_trigger", json!({ "hand": "left", "value": 1 })),
            seq_step(0.3, "set_head_pose", json!({ "position": [0, 1, 0] })),
        ];
        // the controller drops out after the plan was checked
        let (r, ()) = tokio::join!(stack.run_sequence(steps), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stack.apply(off).unwrap();
        });
        let r = r.unwrap();
        assert_eq!(r.steps.iter().map(|s| s.ok).collect::<Vec<_>>(), [false, true]);
        assert!(r.steps[0].error.as_deref().unwrap().contains("not tracked"), "{r:?}");
    }

    #[test]
    fn streaming_waits_end_at_a_failed_restart_or_a_stop() {
        let stack = test_stack();
        let since = Instant::now();
        stack.state.lock().managing = true;
        stack.state.lock().last_restart_error = Some((since - Duration::from_secs(1), "older".into()));
        assert!(!stack.wait_streaming(since, Duration::from_millis(300), 0).unwrap(), "a failure before the wait is not its own");
        stack.state.lock().last_restart_error = Some((Instant::now(), "SteamVR restart failed: boom".into()));
        let waited = Instant::now();
        let e = stack.wait_streaming(since, Duration::from_secs(30), 0).unwrap_err();
        assert!(e.to_string().contains("boom") && waited.elapsed() < Duration::from_secs(2), "{e:#}");
        assert!(stack.status().last_restart_error.is_some_and(|e| e.contains("boom")), "vr_status shows it");
        stack.state.lock().managing = false;
        let e = stack.wait_streaming(Instant::now(), Duration::from_secs(30), 0).unwrap_err();
        assert_eq!(e.to_string(), "vr_stop ended the session");
    }

    #[test]
    fn config_reloads_only_while_nothing_runs() {
        let stack = test_stack();
        let cfg = |streaming_s| Config { timeouts: crate::config::Timeouts { streaming_s, app_s: 1 }, ..Default::default() };
        stack.state.lock().managing = true;
        assert!(!stack.reload(cfg(7)), "a vr_start session runs");
        assert_eq!(stack.cfg().timeouts.streaming_s, 240);
        stack.state.lock().managing = false;
        if !steamvr::is_steamvr_running() {
            assert!(stack.reload(cfg(7)));
            assert_eq!(stack.cfg().timeouts.streaming_s, 7);
        }
    }

    #[test]
    fn a_successful_restart_clears_the_last_failure() {
        let mut state = State { managing: true, ..Default::default() };
        assert!(record_restart(&mut state, Err(anyhow!("boom"))).is_err());
        assert!(state.last_restart_error.as_ref().is_some_and(|(_, e)| e.contains("boom")), "kept for vr_status");
        record_restart(&mut state, Ok(())).unwrap();
        assert_eq!((state.last_restart_error.is_none(), state.restarts), (true, 1));
    }

    #[test]
    fn vr_start_refuses_while_a_start_or_restart_runs() {
        let stack = test_stack();
        stack.state.lock().managing = true;
        let starting = stack.starting.lock();
        assert_eq!(stack.start().unwrap_err().to_string(), "not ready: vr_start is already running");
        drop(starting);
        let restarting = stack.restarting.lock();
        assert_eq!(stack.start().unwrap_err().to_string(), "not ready: SteamVR is restarting");
        drop(restarting);
        assert!(stack.state.lock().managing, "a refused start leaves the running session managed");
    }

    #[test]
    fn vr_restore_refuses_while_steamvr_runs() {
        assert_eq!(restore_guard(true).unwrap_err().to_string(), "SteamVR is running; run vr_stop first");
        assert!(restore_guard(false).is_ok());
    }

    #[test]
    fn vr_stop_leaves_a_steamvr_without_alvr_alone() {
        assert_eq!(stop_guard(true, || false).unwrap_err().to_string(), "SteamVR is running without ALVR; close it yourself");
        assert!(stop_guard(true, || true).is_ok());
        assert!(stop_guard(false, || panic!("not asked while SteamVR is down")).is_ok());
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
    fn steamvr_home_is_not_the_app() {
        let scene = |pid| openvr::SceneApp { pid, state: "running".into() };
        let comm = |pid| Some(if pid == 7 { "steamtours" } else { "Beat Saber.exe" }.to_owned());
        assert_eq!(game_pid(Some(&scene(9)), comm), Ok(9));
        assert_eq!(game_pid(Some(&scene(7)), comm), Err("no app running (scene is SteamVR Home)"));
        assert_eq!(game_pid(Some(&scene(0)), comm), Err("no scene app is running"));
        assert_eq!(game_pid(None, comm), Err("no scene app is running"));
        assert_eq!(game_pid(Some(&scene(9)), |_| None), Err("the scene app's pid is not a running process"), "unreadable comm");
        let big = i32::MAX as u32 + 1;
        assert_eq!(game_pid(Some(&scene(big)), comm), Err("the scene app's pid is not a running process"), "kill() would take it as a group");
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
        LogLine { t: t.into(), t_s: 0.0, at: String::new(), source: source.into(), level: level.into(), text: text.into() }
    }

    fn stamped(day: &str, time: &str) -> String {
        format!("{day} {time} [Info] - x")
    }

    #[test]
    fn steamvr_lines_carry_session_time() {
        let lines = steamvr_log_lines(
            "vrserver",
            ["Wed Oct 21 2026 05:00:36.212461 [Info] - a", "cont", "Wed Oct 21 2026 05:00:37.212461 [Info] - b"].map(String::from).to_vec(),
        );
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].t_s, lines[0].t_s);
        assert!((lines[2].t_s - lines[0].t_s - 1.0).abs() < 0.001);
        assert!(lines.iter().all(|l| l.at.ends_with('Z')));
        assert_eq!(lines[0].t, "05:00:36.212");
        assert!(steamvr_log_lines("vrserver", vec!["orphan".into()]).is_empty(), "no stamp yet: dropped");
    }

    #[test]
    fn logs_sort_across_midnight() {
        let steam = steamvr_log_lines(
            "vrserver",
            vec![stamped("Wed Oct 21 2026", "00:00:01.000000"), stamped("Tue Oct 20 2026", "23:59:59.000000")],
        );
        let sorted = filter_logs(steam, &LogQuery::default()).unwrap();
        assert_eq!(sorted.iter().map(|l| l.t.as_str()).collect::<Vec<_>>(), ["23:59:59.000", "00:00:01.000"]);
    }

    #[test]
    fn status_reports_session_clock() {
        let s = test_stack().status();
        assert!(s.now_t_s >= 0.0);
        assert!(s.t0_at.ends_with('Z') && s.now_at.ends_with('Z'));
        assert!(s.now_at >= s.t0_at);
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
        assert_eq!((steam[0].t.as_str(), steam[0].level.as_str(), steam[0].text.as_str()), ("05:00:36.212", "info", "b"));
        assert_eq!((steam[1].t.as_str(), steam[1].level.as_str(), steam[1].text.as_str()), ("05:00:36.212", "info", "continuation"), "belongs to the line before");
        let alvr_at = |t_s, level: &str, text: &str| LogLine { t_s, ..line("", "alvr", level, text) };
        let base = steam[0].t_s;
        let alvr = vec![alvr_at(base - 1.0, "info", "a"), alvr_at(base + 1.0, "warning", "c")];
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
