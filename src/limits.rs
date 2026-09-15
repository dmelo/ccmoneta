//! The 5-hour / 7-day usage windows.
//!
//! Two sources report the same numbers under different names, so both are
//! normalised into `Window` on the way in:
//!
//! * Claude Code's statusLine payload: `used_percentage` (0-100) and a
//!   `resets_at` in **epoch seconds**. The hook saves it on every turn; it costs
//!   no network, but only moves while Claude Code is running.
//! * `GET /api/oauth/usage`: `utilization` (also 0-100) and a `resets_at` in
//!   **RFC 3339**. Always current, but rate-limited, so the limits job polls it
//!   only when the cached snapshot has gone stale, and backs off on failure
//!   (see refresh.rs).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

use crate::store;

/// A single usage window, normalised: `percent` is always 0-100 and
/// `resets_at` is always epoch seconds.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Window {
    pub percent: f64,
    pub resets_at: Option<i64>,
}

impl Window {
    /// Seconds until this window resets, or None if it has no reset time or
    /// the reset has already passed.
    pub fn resets_in(&self, now: i64) -> Option<i64> {
        self.resets_at.filter(|t| *t > now).map(|t| t - now)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Limits {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub seven_day_opus: Option<Window>,
    /// Extra-usage spend against its cap, 0-100. Only set behind a spend limit.
    pub spend_percent: Option<f64>,
}

impl Limits {
    pub fn has_windows(&self) -> bool {
        self.five_hour.is_some() || self.seven_day.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub limits: Limits,
    /// Epoch seconds when these numbers were captured, not when they were
    /// written, so a re-read of a cached file still ages correctly.
    pub captured_at: i64,
    /// "statusline" or "oauth", carried through to the UI so a stale or
    /// second-hand number can say where it came from.
    pub source: String,
}

impl Snapshot {
    pub fn age(&self, now: i64) -> i64 {
        (now - self.captured_at).max(0)
    }
}

fn snapshot_path() -> PathBuf {
    store::cache_dir().join("limits.json")
}

pub fn save(snap: &Snapshot) -> std::io::Result<()> {
    store::write_json(&snapshot_path(), snap)
}

pub fn load() -> Option<Snapshot> {
    store::read_json(&snapshot_path())
}

/// Parse the `rate_limits` object out of a statusLine payload.
///
/// Absent windows are normal, not an error: the payload documents them as
/// "present only while the API reports it and its resets_at has not passed",
/// and the whole object is missing until the first API response of a session.
pub fn from_statusline(payload: &serde_json::Value) -> Limits {
    let rl = &payload["rate_limits"];
    let win = |key: &str| -> Option<Window> {
        let w = rl.get(key)?;
        Some(Window {
            percent: w.get("used_percentage")?.as_f64()?,
            resets_at: w.get("resets_at").and_then(|v| v.as_i64()),
        })
    };
    Limits {
        five_hour: win("five_hour"),
        seven_day: win("seven_day"),
        seven_day_opus: None,
        spend_percent: win("spend_limit").map(|w| w.percent),
    }
}

fn parse_rfc3339(v: Option<&serde_json::Value>) -> Option<i64> {
    let s = v?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
}

/// Parse the `/api/oauth/usage` response body.
pub fn from_oauth(body: &serde_json::Value) -> Limits {
    let win = |key: &str| -> Option<Window> {
        let w = body.get(key)?;
        if w.is_null() {
            return None;
        }
        Some(Window {
            percent: w.get("utilization")?.as_f64()?,
            resets_at: parse_rfc3339(w.get("resets_at")),
        })
    };
    Limits {
        five_hour: win("five_hour"),
        seven_day: win("seven_day"),
        seven_day_opus: win("seven_day_opus"),
        spend_percent: body
            .get("spend")
            .and_then(|s| s.get("percent"))
            .and_then(|p| p.as_f64()),
    }
}

/// Whether the usage endpoint can be polled at all, without exposing the token.
pub fn has_oauth_token() -> bool {
    oauth_token().is_some_and(|t| !t.is_empty())
}

fn oauth_token() -> Option<String> {
    let raw = std::fs::read(store::home().join(".claude/.credentials.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    v["claudeAiOauth"]["accessToken"].as_str().map(String::from)
}

/// Poll the usage endpoint. Shells out to curl so the binary needs no TLS stack;
/// the limits job runs this rarely, so a process per poll costs nothing.
pub fn fetch_oauth() -> Result<Limits, String> {
    let token = oauth_token().ok_or("no OAuth token in ~/.claude/.credentials.json")?;
    let out = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "10",
            "-H",
            "anthropic-version: 2023-06-01",
            "-H",
            "anthropic-beta: oauth-2025-04-20",
            "-H",
            &format!("Authorization: Bearer {token}"),
            "https://api.anthropic.com/api/oauth/usage",
        ])
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("curl exited {}", out.status));
    }
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("bad JSON: {e}"))?;
    // curl exits 0 on an HTTP error status, so a 429 arrives here as a JSON
    // error body; the absence of any window is what actually signals it.
    if body.get("five_hour").is_none() && body.get("limits").is_none() {
        return Err("usage endpoint returned no windows (rate limited?)".into());
    }
    Ok(from_oauth(&body))
}

/// The limits job: poll the endpoint and cache the result.
pub fn refresh() -> Result<(), String> {
    let limits = fetch_oauth()?;
    save(&Snapshot {
        limits,
        captured_at: store::now(),
        source: "oauth".into(),
    })
    .map_err(|e| format!("writing {}: {e}", snapshot_path().display()))
}
