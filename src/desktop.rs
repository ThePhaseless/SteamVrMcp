//! `screenshot source desktop`: the xdg-desktop-portal Screenshot call, the one desktop capture every compositor offers.
use crate::stack::within;
use alvr_common::{
    anyhow::{Context, Result, bail},
    parking_lot::Mutex,
    warn,
};
use std::{collections::HashMap, ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf, sync::Arc, time::{Duration, SystemTime}};
use zbus::{
    blocking::{Connection, Proxy, proxy::Builder},
    proxy::CacheProperties,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

pub const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Whether the portal answers on the session bus, and its Screenshot version. Only reads a property: it never takes a screenshot.
pub fn check() -> Result<String, String> {
    if cfg!(test) {
        return Err("not probed in unit tests".to_owned()); // unit tests never touch the session bus
    }
    let version = within(CHECK_TIMEOUT, || -> zbus::Result<u32> {
        let bus = Connection::session()?;
        let screenshot = Builder::<Proxy>::new(&bus)
            .destination(PORTAL)?
            .path(PORTAL_PATH)?
            .interface("org.freedesktop.portal.Screenshot")?
            .cache_properties(CacheProperties::No)
            .build()?;
        screenshot.get_property("version")
    });
    match version {
        Some(Ok(v)) => Ok(format!("xdg-desktop-portal Screenshot v{v}")),
        Some(Err(e)) => Err(format!("{PORTAL}: {e}")),
        None => Err(format!("no answer from {PORTAL} (deadline {} s)", CHECK_TIMEOUT.as_secs())),
    }
}

/// The object path the portal gives the Request for `token`, as the portal spec derives it from the caller's unique name.
pub fn request_path(unique_name: &str, token: &str) -> String {
    format!("{PORTAL_PATH}/request/{}/{token}", unique_name.trim_start_matches(':').replace('.', "_"))
}

/// The local path of a `file://` URI, `%XX` decoded.
pub fn file_uri_path(uri: &str) -> Result<PathBuf> {
    let rest = uri.strip_prefix("file://").filter(|r| r.starts_with('/')).with_context(|| format!("not a local file:/// URI: {uri}"))?;
    let (mut bytes, mut i) = (Vec::new(), 0);
    let raw = rest.as_bytes();
    while i < raw.len() {
        if raw[i] == b'%' {
            let hex = rest.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok());
            bytes.push(hex.with_context(|| format!("bad %-escape in {uri}"))?);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

/// The portal Request a screenshot call may leave open: the call and the deadline race to it.
#[derive(Debug, PartialEq)]
enum Pending<T> {
    Waiting,
    Handed(T),
    Abandoned,
}

impl<T> Pending<T> {
    /// The call got its Request: None to wait on, or the Request back to close when the deadline already passed.
    fn hand(&mut self, request: T) -> Option<T> {
        match self {
            Pending::Abandoned => Some(request),
            _ => {
                *self = Pending::Handed(request);
                None
            }
        }
    }

    /// The deadline passed: the Request to close if the call got one; a later one the call closes itself.
    fn abandon(&mut self) -> Option<T> {
        match std::mem::replace(self, Pending::Abandoned) {
            Pending::Handed(request) => Some(request),
            _ => None,
        }
    }
}

/// Request.Close: the portal drops the request, so a late approval writes no file.
fn close(bus: &Connection, path: &str) {
    let closed = Proxy::new(bus, PORTAL, path, "org.freedesktop.portal.Request").and_then(|r| r.call::<_, _, ()>("Close", &()));
    if let Err(e) = closed {
        warn!("closing portal request {path}: {e}");
    }
}

/// The PNG of the whole desktop, from the portal's non-interactive Screenshot. The first call may show the
/// desktop's one-time permission prompt. Reads the file the portal wrote and deletes it. The bus connect,
/// the call and the answer share one deadline; after it the Request is closed.
// shortcut: a thread blocked in the portal wait is left behind after RESPONSE_TIMEOUT, fine for one call at a time
// shortcut: desktop only, no window source (no portal API picks a window by title or pid), add ScreenCast with a restore token if a test needs one window
pub fn screenshot() -> Result<Vec<u8>> {
    let pending = Arc::new(Mutex::new(Pending::Waiting));
    let call = pending.clone();
    let shot = within(RESPONSE_TIMEOUT, move || -> Result<Vec<u8>> {
        let bus = Connection::session().context("session bus")?;
        let wall_ms = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let token = format!("vr_test_mcp_{}_{wall_ms}", std::process::id());
        let sender = bus.unique_name().context("no unique bus name")?.as_str().to_owned();
        let path = request_path(&sender, &token);
        // subscribe before the call: the portal may answer at once
        let request = Proxy::new(&bus, PORTAL, path.as_str(), "org.freedesktop.portal.Request")?;
        let responses = request.receive_signal("Response")?;
        let options = HashMap::from([("handle_token", Value::from(token.as_str())), ("interactive", Value::from(false))]);
        let portal = Proxy::new(&bus, PORTAL, PORTAL_PATH, "org.freedesktop.portal.Screenshot")?;
        let handle: OwnedObjectPath = portal.call("Screenshot", &("", options))?;
        if let Some((bus, late)) = call.lock().hand((bus.clone(), handle.to_string())) {
            close(&bus, &late);
            bail!("the deadline passed");
        }
        if handle.as_str() != path {
            bail!("the portal answered on {handle}, not {path}: its version does not follow the handle_token rule");
        }
        let signal = responses.into_iter().next().context("failed: the bus closed before the portal answered")?;
        let (code, mut results) = signal.body().deserialize::<(u32, HashMap<String, OwnedValue>)>().context("bad portal Response")?;
        match code {
            0 => (),
            1 => bail!("failed: the desktop screenshot was cancelled"),
            2 => bail!("failed: the desktop screenshot was denied (permission prompt refused?)"),
            n => bail!("failed: the portal ended the screenshot with code {n}"),
        }
        let uri: String = results.remove("uri").context("the portal answered without a uri")?.try_into()?;
        let file = file_uri_path(&uri)?;
        let png = std::fs::read(&file).with_context(|| file.display().to_string());
        let _ = std::fs::remove_file(&file);
        let png = png?;
        if !png.starts_with(PNG_MAGIC) {
            bail!("{} is not a PNG", file.display());
        }
        Ok(png)
    });
    if let Some(png) = shot {
        return png;
    }
    if let Some((bus, path)) = pending.lock().abandon() {
        within(CHECK_TIMEOUT, move || close(&bus, &path));
    }
    bail!("the portal did not answer (deadline {} s)", RESPONSE_TIMEOUT.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_handed_after_the_deadline_is_closed() {
        let mut p = Pending::Waiting;
        assert_eq!(p.hand(1), None, "in time: keep waiting");
        assert_eq!(p.abandon(), Some(1), "deadline after the handle: the caller closes it");
        let mut p = Pending::Waiting;
        assert_eq!(p.abandon(), None, "deadline before the handle: nothing to close yet");
        assert_eq!(p.hand(2), Some(2), "a late handle: the worker closes it");
    }

    #[test]
    fn request_path_follows_portal_rule() {
        assert_eq!(request_path(":1.42", "t"), "/org/freedesktop/portal/desktop/request/1_42/t");
    }

    #[test]
    fn file_uri_decodes_percent_and_rejects_others() {
        assert_eq!(file_uri_path("file:///home/u/Pictures/Screenshot%20a.png").unwrap(), PathBuf::from("/home/u/Pictures/Screenshot a.png"));
        for bad in ["https://x", "file://host/x", "file:///a%zz", "file:///a%2"] {
            assert!(file_uri_path(bad).is_err(), "{bad}");
        }
    }
}
