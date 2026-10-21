//! One session clock: `t_s` counts seconds from T0 (MCP start); `at` is the same instant as RFC 3339 UTC.
use chrono::{DateTime, SecondsFormat, Utc};
use std::{
    sync::LazyLock,
    time::{Duration, Instant, SystemTime},
};

/// Forced once in `main`, so T0 is the MCP's start rather than the first use.
pub static T0: LazyLock<(Instant, SystemTime)> = LazyLock::new(|| (Instant::now(), SystemTime::now()));

pub fn now_s() -> f64 {
    T0.0.elapsed().as_secs_f64()
}

/// Negative for an instant before T0.
pub fn t_s(at: Instant) -> f64 {
    match at.checked_duration_since(T0.0) {
        Some(d) => d.as_secs_f64(),
        None => -T0.0.duration_since(at).as_secs_f64(),
    }
}

/// Signed seconds from T0's wall time.
pub fn wall_t_s(wall: SystemTime) -> f64 {
    match wall.duration_since(T0.1) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

pub fn rfc3339(wall: SystemTime) -> String {
    DateTime::<Utc>::from(wall).to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    DateTime::parse_from_rfc3339(s).ok().map(SystemTime::from)
}

/// The wall time of session second `t_s`.
pub fn at(t_s: f64) -> String {
    let offset = Duration::from_secs_f64(t_s.abs());
    rfc3339(if t_s < 0.0 { T0.1 - offset } else { T0.1 + offset })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn rfc3339_is_utc_with_millis() {
        let wall = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        assert_eq!(rfc3339(wall), "2023-11-14T22:13:20.123Z");
        assert_eq!(parse_rfc3339("2023-11-14T22:13:20.123Z"), Some(wall));
    }

    #[test]
    fn t_s_and_at_agree_with_the_wall_clock() {
        assert!((wall_t_s(SystemTime::now()) - now_s()).abs() < 0.05);
        let parsed = parse_rfc3339(&at(now_s())).unwrap();
        let off = parsed.duration_since(SystemTime::now()).unwrap_or_else(|e| e.duration());
        assert!(off < Duration::from_millis(50));
    }
}
