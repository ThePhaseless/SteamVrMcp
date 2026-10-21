use crate::{
    alvr_api::{self, Api},
    checks::{self, Check, Readiness},
    config::{Config, Paths},
    headset::{self, Headset, HeadsetStatus},
    setup,
    steamvr::{self, STEAMVR_URL, VR_PROCESS_NAMES},
};
use alvr_common::{
    ConnectionState,
    anyhow::{Context, Result, bail},
    error,
    parking_lot::Mutex,
    warn,
};
use alvr_events::{Event, EventType};
use alvr_session::SessionConfig;
use serde::Serialize;
use std::{
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

/// A client that was streaming when vrserver went away is stuck in ALVR's TCP EOF spin, so its
/// pause and drop never return.
fn must_leak(streaming: bool, vrserver_running: bool) -> bool {
    streaming && !vrserver_running
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
    pub warning: Option<&'static str>,
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
    on_change: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
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
            on_change: Mutex::new(None),
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

        self.state.lock().headset.get_or_insert_with(Headset::start);
        let hostname = headset::client_hostname()?;
        let values = setup::session_values(&self.cfg)?;
        let steamvr_running = steamvr::is_steamvr_running();
        let mut restart_adopted = false;
        if !steamvr_running {
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
        if !steamvr_running {
            report.started_steam = steamvr::launch(&self.cfg, STEAMVR_URL)?;
            self.state.lock().started_steam |= report.started_steam;
        }

        self.state.lock().managing = true;
        if restart_adopted {
            self.restart_steamvr();
        }
        let timeout = Duration::from_secs(self.cfg.timeouts.streaming_s);
        let deadline = Instant::now() + timeout;
        if !steamvr::wait_for(timeout, || self.api.ping()) {
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
            for sig in [libc::SIGTERM, libc::SIGKILL] {
                for name in VR_PROCESS_NAMES {
                    steamvr::pids_named(name).into_iter().for_each(|pid| steamvr::terminate(pid, sig));
                }
                if steamvr::wait_gone(VR_PROCESS_NAMES, Duration::from_secs(5)) {
                    break;
                }
            }
            report.used_signals = true;
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
        StatusReport {
            steam_running: steamvr::is_steam_running(),
            steamvr_running: steamvr::is_steamvr_running(),
            alvr_version: self.api.version().ok().map(|v| v.trim().to_owned()),
            headset: self.headset_status(),
            warning: self.state.lock().leaked.then_some(LEAKED),
        }
    }

    /// Replaces the headset with a fresh one and restarts SteamVR so ALVR accepts it.
    pub fn recreate_headset(&self) {
        let old = self.state.lock().headset.take();
        if let Some(h) = old {
            self.dispose(h); // the old client must stop listening on 9943 before the new one starts
        }
        let new = Headset::start();
        self.state.lock().headset = Some(new);
        // shortcut: ALVR TCP EOF spin (sockets/src/stream_socket/tcp.rs:137-147) leaves the server stuck Disconnecting; drop when fixed upstream
        self.restart_steamvr();
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

    /// Drops a headset taken out of the state, or leaks it when ALVR's client could never finish.
    fn dispose(&self, headset: Headset) {
        if !must_leak(headset.status().streaming, steamvr::is_steamvr_running()) {
            drop(headset);
            return;
        }
        // shortcut: ALVR TCP EOF spin on the client after the server exits (sockets/src/stream_socket/tcp.rs:137-147) never returns from pause/drop; the leaked thread spins one core until the MCP exits; drop when fixed upstream
        warn!("{LEAKED}");
        self.state.lock().leaked = true;
        std::mem::forget(headset);
    }

    fn restart_steamvr(&self) {
        let _restart = self.restarting.lock();
        if !self.state.lock().managing {
            return;
        }
        // as in stop(): a client still streaming when vrserver exits never disconnects, so pause it
        // first, outside the state lock; a client whose server is already gone is replaced
        let taken = self.state.lock().headset.take(); // own statement: the guard must not outlive it
        let headset = taken.and_then(|h| {
            if must_leak(h.status().streaming, steamvr::is_steamvr_running()) {
                self.dispose(h);
                return None;
            }
            h.pause();
            Some(h)
        });
        let restarted = steamvr::restart_steamvr(&self.cfg, &self.paths, &self.api);
        let headset = headset.inspect(Headset::resume).unwrap_or_else(Headset::start);
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
        match restarted {
            Ok(()) => self.state.lock().restarts += 1,
            Err(e) => error!("SteamVR restart failed: {e:#}"),
        }
    }
}

/// Recomputes readiness every 1 s and reports changes. Not in `supervise`, which blocks for a whole restart.
fn watch(stack: Weak<Stack>) {
    let mut last = Readiness::default();
    loop {
        let Some(s) = stack.upgrade() else { return };
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
                Ok(_) => (), // shortcut: other events are dropped until Task 9's log ring keeps them
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
        if status.streaming || events.is_none() {
            quiet_since = None;
        } else {
            quiet_since.get_or_insert_with(Instant::now);
        }

        if restart {
            s.restart_steamvr();
            (events, quiet_since) = (None, None);
            last_hud = s.headset_status().map(|h| h.hud_message).unwrap_or_default();
        } else if quiet_since.is_some_and(|t| t.elapsed() > WATCHDOG) && !s.state.lock().watchdog_fired {
            // seen in the spike: an old client stops listening after a restart
            // shortcut: ALVR TCP EOF spin (sockets/src/stream_socket/tcp.rs:137-147) leaves the server stuck Disconnecting; drop when fixed upstream
            s.state.lock().watchdog_fired = true;
            s.recreate_headset(); // restarts SteamVR too
            (events, quiet_since) = (None, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn headset_leaks_only_when_its_server_is_gone_mid_stream() {
        assert!(!must_leak(true, true), "server alive: the client disconnects cleanly");
        assert!(must_leak(true, false), "server gone while streaming: pause and drop never return");
        assert!(!must_leak(false, false), "never streamed: no stream thread to spin");
    }

    #[test]
    fn restart_hud_matches_client_core_format() {
        // client_core's set_hud_message prefixes every message with version, hostname and IP
        let hud = |m: &str| format!("ALVR v21.0.0-dev14\nhostname: 1234.client.local.\nIP: 192.0.2.1\n\n{m}");
        assert!(is_restart_hud(&hud(SERVER_RESTART_MESSAGE)));
        assert!(!is_restart_hud(&hud("Searching for streamer...")));
    }
}
