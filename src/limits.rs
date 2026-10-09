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
//!
//! With aimux running several accounts (see accounts.rs), limits are kept per
//! account, and the limits job reads them all from `aimux status --json`
//! instead of the endpoint: aimux holds each profile's login and already polls
//! them, sharing one reading between everything that asks.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::accounts::{self, Account};
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
    /// "statusline", "oauth" or "aimux", carried through to the UI so a stale
    /// or second-hand number can say where it came from.
    pub source: String,
    /// Why this account could not be read, when it could not: aimux reports a
    /// login that has expired, for one. Recorded on the account rather than
    /// failing the job, so one account's trouble does not hold back the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Snapshot {
    pub fn age(&self, now: i64) -> i64 {
        (now - self.captured_at).max(0)
    }
}

/// One account's limits, as the surfaces show them.
#[derive(Debug, Clone)]
pub struct AccountLimits {
    pub account: Account,
    pub snap: Option<Snapshot>,
}

/// Where an account's snapshot lives. A single account keeps `limits.json`,
/// as before accounts existed; several get a file each, named by account key.
fn path_for(account: &Account, several: bool) -> PathBuf {
    if !several {
        return store::cache_dir().join("limits.json");
    }
    store::cache_dir()
        .join("limits")
        .join(format!("{}.json", account.file_name()))
}

pub fn save_for(account: &Account, several: bool, snap: &Snapshot) -> std::io::Result<()> {
    store::write_json(&path_for(account, several), snap)
}

pub fn load_for(account: &Account, several: bool) -> Option<Snapshot> {
    store::read_json(&path_for(account, several))
}

/// Every account's cached limits, in `accounts::local()` order.
pub fn load_all(accounts: &[Account]) -> Vec<AccountLimits> {
    let several = accounts::several(accounts);
    accounts
        .iter()
        .map(|a| AccountLimits {
            account: a.clone(),
            snap: load_for(a, several),
        })
        .collect()
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

/// `CCMONETA_AIMUX` replaces the aimux executable, for the tests.
pub fn aimux_exe() -> std::ffi::OsString {
    std::env::var_os("CCMONETA_AIMUX")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "aimux".into())
}

/// Read `aimux status --json`: per profile, its windows or why there are none.
/// Percentages are 0-100 and reset times epoch milliseconds; a window aimux
/// could not read is null.
pub fn from_aimux(body: &serde_json::Value) -> HashMap<String, Result<Limits, String>> {
    let mut out = HashMap::new();
    let Some(profiles) = body["profiles"].as_object() else {
        return out;
    };
    for (name, p) in profiles {
        if p["cli"].as_str().is_some_and(|c| c != "claude") {
            continue;
        }
        let st = &p["status"];
        let result = if st.is_object() {
            let win = |pct: &str, reset: &str| -> Option<Window> {
                Some(Window {
                    percent: st[pct].as_f64()?,
                    resets_at: st[reset].as_i64().map(|ms| ms / 1000),
                })
            };
            Ok(Limits {
                five_hour: win("fiveHourPct", "fiveHourResetsAt"),
                seven_day: win("weeklyPct", "weeklyResetsAt"),
                seven_day_opus: None,
                spend_percent: None,
            })
        } else {
            Err(match p["error"].as_str() {
                Some("auth") => "login expired".to_string(),
                Some(e) => e.to_string(),
                None => "no reading".to_string(),
            })
        };
        out.insert(name.clone(), result);
    }
    out
}

/// Run `aimux status --json`. `--max-age` lets aimux hand back a reading it
/// took for someone else within that many seconds instead of probing again.
fn fetch_aimux(max_age: i64) -> Result<(serde_json::Value, i64), String> {
    let out = Command::new(aimux_exe())
        .args(["status", "--json", "--max-age", &max_age.to_string()])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "aimux not found, though ~/.aimux/config.yaml lists several accounts".to_string()
            } else {
                format!("aimux: {e}")
            }
        })?;
    if !out.status.success() {
        return Err(format!("aimux status exited {}", out.status));
    }
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("aimux status: bad JSON: {e}"))?;
    let at = body["fetchedAt"]
        .as_i64()
        .map(|ms| ms / 1000)
        .unwrap_or_else(store::now);
    Ok((body, at))
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

/// The limits job: read every account's limits and cache them.
///
/// One account polls the usage endpoint itself. Several are read through aimux,
/// which holds their logins. An account aimux could not read is saved with the
/// reason, so it is shown and not asked about again until it is due like the
/// others; the job fails only when no account could be read at all.
///
/// `max_age` is the limits' own threshold: aimux may hand back a reading it
/// took earlier, and one older than that would be due again on arrival.
pub fn refresh(max_age: i64) -> Result<(), String> {
    let accounts = accounts::local();
    if !accounts::several(&accounts) {
        let account = &accounts[0];
        let snap = Snapshot {
            limits: fetch_oauth()?,
            captured_at: store::now(),
            source: "oauth".into(),
            error: None,
        };
        return save_for(account, false, &snap)
            .map_err(|e| format!("writing {}: {e}", path_for(account, false).display()));
    }

    let (body, at) = fetch_aimux((max_age / 2).clamp(0, 120))?;
    let mut read = from_aimux(&body);
    let mut failed = Vec::new();
    for a in &accounts {
        let (limits, error) = match read.remove(&a.name) {
            Some(Ok(limits)) => (limits, None),
            Some(Err(e)) => (Limits::default(), Some(e)),
            None => (Limits::default(), Some("not in aimux status".to_string())),
        };
        if let Some(e) = &error {
            failed.push(format!("{}: {e}", a.name));
        }
        let snap = Snapshot {
            limits,
            captured_at: at,
            source: "aimux".into(),
            error,
        };
        save_for(a, true, &snap)
            .map_err(|e| format!("writing {}: {e}", path_for(a, true).display()))?;
    }
    if failed.len() == accounts.len() {
        Err(failed.join("; "))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aimux_status_reads_per_profile() {
        let body = serde_json::json!({
            "fetchedAt": 1_800_000_000_000_i64,
            "profiles": {
                "main": {"cli": "claude", "status": {"fiveHourPct": 12, "weeklyPct": 40,
                    "fiveHourResetsAt": 1_800_003_600_000_i64, "weeklyResetsAt": 1_800_300_000_000_i64,
                    "status": "allowed"}},
                "second": {"cli": "claude", "status": {"fiveHourPct": null, "weeklyPct": 5}},
                "expired": {"cli": "claude", "status": null, "error": "auth"},
                "coder": {"cli": "codex", "status": {"fiveHourPct": 1, "weeklyPct": 1}}
            }
        });
        let r = from_aimux(&body);
        assert_eq!(r.len(), 3, "codex profiles are not Claude accounts");
        let main = r["main"].as_ref().unwrap();
        assert_eq!(main.five_hour.unwrap().percent, 12.0);
        assert_eq!(main.five_hour.unwrap().resets_at, Some(1_800_003_600));
        assert_eq!(main.seven_day.unwrap().percent, 40.0);
        let second = r["second"].as_ref().unwrap();
        assert!(second.five_hour.is_none());
        assert_eq!(second.seven_day.unwrap().percent, 5.0);
        assert_eq!(r["expired"].as_ref().unwrap_err(), "login expired");
    }
}
