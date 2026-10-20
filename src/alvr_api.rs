use alvr_common::{
    anyhow::{Context, Result, bail},
    semver::Version,
    warn,
};
use alvr_events::Event;
use alvr_session::SessionConfig;
use serde::Serialize;
use std::{
    io::{ErrorKind, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::Path,
    time::Duration,
};
use tungstenite::{Message, WebSocket, client::IntoClientRequest, stream::MaybeTlsStream};

const TIMEOUT: Duration = Duration::from_secs(10);

/// The ALVR server's REST and events API on 127.0.0.1.
pub struct Api {
    pub port: u16,
}

impl Api {
    /// The session's `connection.web_server_port`; ALVR's default (8082) when the file is unreadable.
    pub fn from_session_file(path: &Path) -> Api {
        let mut session = SessionConfig::default();
        if let Some(json) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()) {
            session.merge_from_json(&json).ok();
        }
        Api { port: session.to_settings().connection.web_server_port }
    }

    // shortcut: no chunked decoding; axum answers these routes with Content-Length, switch to a client crate if a route streams
    fn request(&self, method: &str, path: &str, body: Option<&str>, timeout: Duration) -> Result<String> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let mut stream = TcpStream::connect_timeout(&addr, timeout.min(Duration::from_secs(1)))
            .with_context(|| format!("ALVR API not reachable on port {}", self.port))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.write_all(build_request(method, path, body).as_bytes())?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        let (head, body) = response.split_once("\r\n\r\n").context("malformed HTTP response")?;
        let status = head.lines().next().unwrap_or_default();
        if !status.split(' ').nth(1).is_some_and(|code| code.starts_with('2')) {
            bail!("{method} {path}: {status}: {body}");
        }
        Ok(body.to_owned())
    }

    pub fn get(&self, path: &str) -> Result<String> {
        self.request("GET", path, None, TIMEOUT)
    }

    pub fn post(&self, path: &str) -> Result<()> {
        self.request("POST", path, None, TIMEOUT).map(drop)
    }

    pub fn post_json<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        self.request("POST", path, Some(&serde_json::to_string(body)?), TIMEOUT).map(drop)
    }

    pub fn ping(&self) -> bool {
        // short, so readiness answers at once even when ALVR stalls
        self.request("GET", "/api/ping", None, Duration::from_millis(500)).is_ok()
    }

    pub fn version(&self) -> Result<String> {
        self.get("/api/version")
    }

    pub fn events(&self) -> Result<EventStream> {
        let mut request = format!("ws://127.0.0.1:{}/api/events", self.port).into_client_request()?;
        request.headers_mut().insert("X-ALVR", "1".parse()?);
        let (socket, _) = tungstenite::client::connect(request)?;
        Ok(EventStream(socket))
    }
}

pub struct EventStream(WebSocket<MaybeTlsStream<TcpStream>>);

impl EventStream {
    /// The next event, or None when none arrives within `timeout`.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Event>> {
        if let MaybeTlsStream::Plain(stream) = self.0.get_ref() {
            stream.set_read_timeout(Some(timeout))?;
        }
        loop {
            match self.0.read() {
                // skip what does not parse, as ALVR's dashboard does (dashboard/src/data_sources.rs)
                Ok(Message::Text(text)) => match serde_json::from_str(&text) {
                    Ok(event) => return Ok(Some(event)),
                    Err(e) => warn!("skipping ALVR event that does not parse: {e}"),
                },
                Ok(Message::Close(_)) => bail!("ALVR closed the events stream"),
                Ok(_) => (),
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

pub fn build_request(method: &str, path: &str, body: Option<&str>) -> String {
    let body = body.unwrap_or_default();
    format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-ALVR: 1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// ALVR's `protocol_id` rule (common/src/version.rs) applied to another version.
pub fn protocol_id_of(version: &str) -> Result<String> {
    let v = Version::parse(version.trim())?;
    Ok(if v.pre.is_empty() { v.major.to_string() } else { format!("{}-{}", v.major, v.pre) })
}

/// True when a server of this version speaks our protocol: same major for stable, same pre-release otherwise.
pub fn compatible(server_version: &str) -> bool {
    protocol_id_of(server_version).is_ok_and(|id| id == alvr_common::protocol_id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatible_follows_protocol_id() {
        assert!(compatible(&alvr_common::ALVR_VERSION.to_string()));
        assert!(compatible("21.0.0-dev14+nightly.2026.10.19")); // build metadata ignored
        assert!(!compatible("21.0.0-dev13"));
        assert!(!compatible("20.14.1"));
    }

    #[test]
    fn stable_versions_match_on_major() {
        assert_eq!(protocol_id_of("21.3.0").unwrap(), "21");
        assert_eq!(protocol_id_of("21.0.0-dev14").unwrap(), "21-dev14");
    }

    #[test]
    fn ping_gives_up_fast_on_a_stalled_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); // accepts, never answers
        let api = Api { port: listener.local_addr().unwrap().port() };
        let started = std::time::Instant::now();
        assert!(!api.ping());
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    }

    #[test]
    fn request_bytes_carry_x_alvr() {
        let req = build_request("POST", "/api/session/values", Some("[]"));
        for part in ["POST /api/session/values HTTP/1.1\r\n", "X-ALVR: 1\r\n", "Content-Length: 2\r\n", "Connection: close\r\n"] {
            assert!(req.contains(part), "{part:?} missing in {req:?}");
        }
        assert!(req.ends_with("\r\n\r\n[]"));
    }
}
