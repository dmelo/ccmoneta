//! Service health and the installed Claude Code version.
//!
//! Two questions that are cheap to answer and awkward to notice by hand:
//!
//! * Is Claude up? `GET https://status.claude.com/api/v2/summary.json` is the
//!   Statuspage summary: an overall indicator, one row per component, and the
//!   unresolved incidents. Only the `Claude Code` component and the overall
//!   indicator are shown; the rest is carried in the tooltip.
//! * Is this machine's Claude Code current? The installed version comes from
//!   `claude --version`; where the newest version is published depends on how
//!   Claude Code was installed, so the lookup follows the same rules the
//!   product's own updater uses (see `latest_url` and `LookupPlan`).
//!
//! Both are polled by one background job on the same schedule as the other
//! caches, and neither ever blocks a surface: `health.json` is read, and a
//! refresh is started when it has aged out.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::store;

/// Statuspage's component id for Claude Code. Ids are stable across renames,
/// so the name is only a fallback.
const CLAUDE_CODE_COMPONENT: &str = "yyzkbfz2thpt";
const SUMMARY_URL: &str = "https://status.claude.com/api/v2/summary.json";
const RELEASES_URL: &str = "https://downloads.claude.ai/claude-code-releases";
const NPM_PACKAGE: &str = "@anthropic-ai/claude-code";
const NPM_REGISTRY: &str = "https://registry.npmjs.org/";

/// The two channels Claude Code publishes. A channel name reaches a URL and a
/// command line, so only these two are ever used, however the settings file
/// spells it.
const CHANNELS: [&str; 2] = ["stable", "latest"];
const DEFAULT_CHANNEL: &str = "latest";

/// The Homebrew casks, and the channel each one tracks. A brew install takes
/// its channel from the cask it was installed from, not from settings.
const CASKS: [(&str, &str); 2] = [("claude-code", "stable"), ("claude-code@latest", "latest")];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Incident {
    pub name: String,
    /// Statuspage's own wording: none, minor, major or critical.
    pub impact: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceStatus {
    /// none, minor, major, critical — Statuspage's overall indicator.
    pub indicator: String,
    pub description: String,
    /// The `Claude Code` component, when the page still lists one.
    pub claude_code: Option<String>,
    /// Every component that is not operational, whatever it is called.
    pub degraded: Vec<(String, String)>,
    pub incidents: Vec<Incident>,
}

impl ServiceStatus {
    pub fn is_ok(&self) -> bool {
        self.indicator == "none" && self.degraded.is_empty()
    }

    /// What a surface shows in one word.
    pub fn short(&self) -> &'static str {
        match self.indicator.as_str() {
            "none" if self.degraded.is_empty() => "ok",
            "none" | "minor" => "degraded",
            "major" => "outage",
            "critical" => "critical",
            _ => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VersionState {
    pub installed: String,
    /// None when the lookup was skipped or failed; `behind` is then false,
    /// because nothing is known to be newer.
    pub latest: Option<String>,
    pub channel: String,
    /// native, npm, brew — how the newest version was looked up.
    pub source: String,
    pub behind: bool,
    /// Set when no lookup was made, with the reason.
    pub skipped: Option<String>,
    /// `autoUpdates` from ~/.claude.json. `Some(false)` is why an install goes
    /// stale, and is usually the user's own choice rather than a lock.
    #[serde(default)]
    pub background_updates: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HealthSnapshot {
    pub checked_at: i64,
    pub status: Option<ServiceStatus>,
    pub status_error: Option<String>,
    pub version: Option<VersionState>,
    pub version_error: Option<String>,
}

impl HealthSnapshot {
    pub fn age(&self, now: i64) -> i64 {
        (now - self.checked_at).max(0)
    }

    /// Whether anything here is worth putting in front of the user: a service
    /// problem, or an out-of-date install. Lookup failures are not — they say
    /// something about this machine's network, not about Claude.
    pub fn needs_attention(&self) -> bool {
        self.status.as_ref().is_some_and(|s| !s.is_ok())
            || self.version.as_ref().is_some_and(|v| v.behind)
    }
}

fn snapshot_path() -> PathBuf {
    store::cache_dir().join("health.json")
}

pub fn save(snap: &HealthSnapshot) -> std::io::Result<()> {
    store::write_json(&snapshot_path(), snap)
}

pub fn load() -> Option<HealthSnapshot> {
    store::read_json(&snapshot_path())
}

// ---------------------------------------------------------------- status page

/// Parse the Statuspage summary. A missing component or incident list is
/// normal — the page's shape is theirs to change — so only the indicator is
/// required.
pub fn parse_summary(body: &serde_json::Value) -> Result<ServiceStatus, String> {
    let status = body.get("status").ok_or("no status object")?;
    let indicator = status
        .get("indicator")
        .and_then(|v| v.as_str())
        .ok_or("no status indicator")?
        .to_string();
    let description = status
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let components = body
        .get("components")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let component_status = |c: &serde_json::Value| -> Option<(String, String)> {
        Some((
            c.get("name")?.as_str()?.to_string(),
            c.get("status")?.as_str()?.to_string(),
        ))
    };
    let claude_code = components
        .iter()
        .find(|c| {
            c.get("id").and_then(|v| v.as_str()) == Some(CLAUDE_CODE_COMPONENT)
                || c.get("name").and_then(|v| v.as_str()) == Some("Claude Code")
        })
        .and_then(component_status)
        .map(|(_, s)| s);
    let degraded = components
        .iter()
        // Statuspage marks a container row as a group; its own status merely
        // repeats its children's.
        .filter(|c| c.get("group").and_then(|v| v.as_bool()) != Some(true))
        .filter_map(component_status)
        .filter(|(_, s)| s != "operational")
        .collect();

    let incidents = body
        .get("incidents")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|i| {
            Some(Incident {
                name: i.get("name")?.as_str()?.to_string(),
                impact: i
                    .get("impact")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                status: i
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            })
        })
        .collect();

    Ok(ServiceStatus {
        indicator,
        description,
        claude_code,
        degraded,
        incidents,
    })
}

fn fetch_status() -> Result<ServiceStatus, String> {
    let body = curl_json(SUMMARY_URL)?;
    parse_summary(&body)
}

// -------------------------------------------------------------------- version

/// Compare two version strings numerically, part by part, ignoring a leading
/// `v` and any `+build` metadata. Returns None when either side is not a
/// dotted number, so an unexpected string is never read as "behind".
pub fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let parts = |v: &str| -> Option<Vec<u64>> {
        let v = v.trim().trim_start_matches('v');
        // Build metadata and pre-release suffixes do not order numerically.
        let v = v.split(['+', '-']).next()?;
        let parts: Vec<u64> = v
            .split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<_>>()?;
        (!parts.is_empty()).then_some(parts)
    };
    let (mut a, mut b) = (parts(a)?, parts(b)?);
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    Some(a.cmp(&b))
}

/// An install is behind only when the published version is provably greater. A
/// version ahead of the channel, which a pre-release build is, reads as current.
pub fn is_behind(installed: &str, latest: &str) -> bool {
    compare_versions(installed, latest) == Some(std::cmp::Ordering::Less)
}

/// The first whitespace-delimited token of `claude --version`, which prints
/// e.g. "2.1.274 (Claude Code)".
pub fn parse_installed(output: &str) -> Option<String> {
    output.split_whitespace().next().map(String::from)
}

/// The release channel, from `autoUpdatesChannel` in Claude Code's settings.
/// The value reaches a URL and a command line, so anything that is not exactly
/// a known channel name falls back to the default rather than being passed on.
pub fn channel_from_settings(settings: Option<&serde_json::Value>) -> String {
    settings
        .and_then(|v| v.get("autoUpdatesChannel"))
        .and_then(|v| v.as_str())
        .filter(|c| CHANNELS.contains(c))
        .unwrap_or(DEFAULT_CHANNEL)
        .to_string()
}

/// The Homebrew cask a binary was installed from, when its resolved path runs
/// through a Caskroom. Only the two known cask names count, for the same reason
/// the channel is validated.
pub fn cask_from_path(exe: &Path) -> Option<&'static str> {
    let mut components = exe.components().map(|c| c.as_os_str().to_string_lossy());
    while let Some(c) = components.next() {
        if c == "Caskroom" {
            let name = components.next()?;
            return CASKS.iter().map(|(c, _)| *c).find(|c| *c == name);
        }
    }
    None
}

/// Where to look for the newest version, and which channel that lookup is for.
#[derive(Debug, Clone, PartialEq)]
pub enum LookupPlan {
    /// The plain-text channel file. Native and anything unrecognised.
    Native { channel: String },
    /// `npm view`, for npm and bun global installs.
    Npm { channel: String },
    /// The cask's own JSON, which can lag the channels by hours.
    Brew { cask: &'static str, channel: String },
}

impl LookupPlan {
    pub fn channel(&self) -> &str {
        match self {
            LookupPlan::Native { channel } | LookupPlan::Npm { channel } => channel,
            LookupPlan::Brew { channel, .. } => channel,
        }
    }

    pub fn source(&self) -> &'static str {
        match self {
            LookupPlan::Native { .. } => "native",
            LookupPlan::Npm { .. } => "npm",
            LookupPlan::Brew { .. } => "brew",
        }
    }
}

/// Decide how to look the newest version up. A Homebrew install is detected
/// from the resolved executable path, because `installMethod` has no value for
/// it, and its cask fixes the channel: a stable-cask user compared against the
/// faster channel would permanently read as behind.
pub fn plan_lookup(install_method: Option<&str>, exe: Option<&Path>, channel: &str) -> LookupPlan {
    if let Some(cask) = exe.and_then(cask_from_path) {
        let channel = CASKS
            .iter()
            .find(|(c, _)| *c == cask)
            .map(|(_, ch)| *ch)
            .unwrap_or(DEFAULT_CHANNEL);
        return LookupPlan::Brew {
            cask,
            channel: channel.to_string(),
        };
    }
    let method = install_method.unwrap_or_default();
    if method.contains("npm") || method.contains("bun") || method.contains("yarn") {
        return LookupPlan::Npm {
            channel: channel.to_string(),
        };
    }
    LookupPlan::Native {
        channel: channel.to_string(),
    }
}

pub fn claude_exe() -> OsString {
    std::env::var_os("CCMONETA_CLAUDE")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "claude".into())
}

/// The absolute, symlink-resolved path of the `claude` that would run, for the
/// Caskroom check. None when it is not on PATH.
fn resolve_claude() -> Option<PathBuf> {
    let exe = claude_exe();
    let path = Path::new(&exe);
    let found = if path.components().count() > 1 {
        path.to_path_buf()
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(path))
            .find(|p| p.is_file())?
    };
    std::fs::canonicalize(found).ok()
}

fn claude_settings() -> Option<serde_json::Value> {
    let raw = std::fs::read(crate::install::settings_path()).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// `installMethod` and `autoUpdates` from `~/.claude.json`, which is where
/// Claude Code records how it was installed and whether it updates itself.
fn install_facts() -> (Option<String>, Option<bool>) {
    let Ok(raw) = std::fs::read(store::home().join(".claude.json")) else {
        return (None, None);
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return (None, None);
    };
    (
        v.get("installMethod")
            .and_then(|m| m.as_str())
            .map(String::from),
        v.get("autoUpdates").and_then(|a| a.as_bool()),
    )
}

/// Whether Claude Code's own non-essential network traffic is turned off. Its
/// updater suppresses the version lookup in that mode, so this does too rather
/// than restoring egress the user has switched off.
fn nonessential_traffic_disabled() -> bool {
    std::env::var_os("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")
        .is_some_and(|v| !v.is_empty() && v != "0" && v != "false")
}

fn fetch_latest(plan: &LookupPlan) -> Result<String, String> {
    match plan {
        LookupPlan::Native { channel } => {
            // The channel is one of two literals, never the raw settings value.
            let out = curl_text(&format!("{RELEASES_URL}/{channel}"))?;
            let version = out.trim();
            if version.is_empty() || version.len() > 64 {
                return Err("channel file did not return a version".into());
            }
            Ok(version.to_string())
        }
        LookupPlan::Npm { channel } => {
            // Run from HOME with the registry pinned: a project's own .npmrc
            // could otherwise redirect the lookup.
            let out = Command::new("npm")
                .args([
                    "view",
                    &format!("{NPM_PACKAGE}@{channel}"),
                    "version",
                    "--registry",
                    NPM_REGISTRY,
                ])
                .current_dir(store::home())
                .stdin(Stdio::null())
                .output()
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        "npm not found".to_string()
                    } else {
                        format!("npm: {e}")
                    }
                })?;
            if !out.status.success() {
                return Err(format!("npm view exited {}", out.status));
            }
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if version.is_empty() {
                return Err("npm view returned nothing".into());
            }
            Ok(version)
        }
        LookupPlan::Brew { cask, .. } => {
            let body = curl_json(&format!("https://formulae.brew.sh/api/cask/{cask}.json"))?;
            body.get("version")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or_else(|| "cask JSON has no version".into())
        }
    }
}

fn check_version() -> Result<VersionState, String> {
    let out = Command::new(claude_exe())
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "claude not found on PATH".to_string()
            } else {
                format!("claude --version: {e}")
            }
        })?;
    if !out.status.success() {
        return Err(format!("claude --version exited {}", out.status));
    }
    let installed = parse_installed(&String::from_utf8_lossy(&out.stdout))
        .ok_or("claude --version printed nothing")?;

    let channel = channel_from_settings(claude_settings().as_ref());
    let (method, background_updates) = install_facts();
    let plan = plan_lookup(method.as_deref(), resolve_claude().as_deref(), &channel);

    if nonessential_traffic_disabled() {
        return Ok(VersionState {
            installed,
            latest: None,
            channel: plan.channel().to_string(),
            source: plan.source().to_string(),
            behind: false,
            skipped: Some("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC is set".into()),
            background_updates,
        });
    }

    // A failed lookup is not a failed check: the installed version is still
    // worth caching, and nothing is claimed about being behind.
    let (latest, skipped) = match fetch_latest(&plan) {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e)),
    };
    let behind = latest.as_deref().is_some_and(|l| is_behind(&installed, l));
    Ok(VersionState {
        installed,
        latest,
        channel: plan.channel().to_string(),
        source: plan.source().to_string(),
        behind,
        skipped,
        background_updates,
    })
}

// ----------------------------------------------------------------------- http

/// Shells out to curl, like the usage endpoint, so the binary needs no TLS
/// stack. This job runs a few times an hour.
fn curl_text(url: &str) -> Result<String, String> {
    let out = Command::new("curl")
        .args(["-sS", "--fail", "--max-time", "10", url])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "curl not found".to_string()
            } else {
                format!("curl: {e}")
            }
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr.lines().rev().find(|l| !l.trim().is_empty());
        return Err(match detail {
            Some(d) => format!("curl exited {}: {}", out.status, d.trim()),
            None => format!("curl exited {}", out.status),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn curl_json(url: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(&curl_text(url)?).map_err(|e| format!("bad JSON: {e}"))
}

/// The health job: both checks, cached together. One failing does not lose the
/// other, and a snapshot is written either way so the schedule keeps its pace.
pub fn refresh(check_status: bool, check_version_too: bool) -> Result<(), String> {
    let (status, status_error) = if check_status {
        match fetch_status() {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e)),
        }
    } else {
        (None, None)
    };
    let (version, version_error) = if check_version_too {
        match check_version() {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        }
    } else {
        (None, None)
    };

    let snap = HealthSnapshot {
        checked_at: store::now(),
        status,
        status_error,
        version,
        version_error,
    };
    let errors: Vec<&str> = [snap.status_error.as_deref(), snap.version_error.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    save(&snap).map_err(|e| format!("writing {}: {e}", snapshot_path().display()))?;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// The lines a tooltip or the dashboard shows, most important first.
pub fn lines(snap: &HealthSnapshot, now: i64, include_age: bool) -> Vec<String> {
    let mut out = Vec::new();
    match (&snap.status, &snap.status_error) {
        (Some(s), _) => {
            out.push(format!("status: {}", s.description));
            if let Some(cc) = &s.claude_code
                && cc != "operational"
            {
                out.push(format!("Claude Code: {cc}"));
            }
            for (name, state) in &s.degraded {
                out.push(format!("{name}: {state}"));
            }
            for i in &s.incidents {
                out.push(format!("incident: {} ({}, {})", i.name, i.impact, i.status));
            }
        }
        (None, Some(e)) => out.push(format!("status unavailable: {e}")),
        (None, None) => {}
    }
    match (&snap.version, &snap.version_error) {
        (Some(v), _) => {
            let mut line = match &v.latest {
                Some(latest) if v.behind => {
                    format!("Claude Code {} → {latest} available", v.installed)
                }
                Some(_) => format!("Claude Code {} is current", v.installed),
                None => format!("Claude Code {}", v.installed),
            };
            line.push_str(&format!(" ({} channel)", v.channel));
            out.push(line);
            if v.behind {
                if v.background_updates == Some(false) {
                    out.push("background auto-updates are off; run `claude update`".into());
                } else {
                    out.push("run `claude update`".into());
                }
            }
            if let Some(why) = &v.skipped {
                out.push(format!("version lookup skipped: {why}"));
            }
        }
        (None, Some(e)) => out.push(format!("version unknown: {e}")),
        (None, None) => {}
    }
    if include_age && snap.checked_at > 0 {
        out.push(format!(
            "health checked {} ago",
            crate::bar::ago(snap.age(now))
        ));
    }
    out
}

/// The marker a bar or the status line appends, and nothing at all when there
/// is nothing to say. Surfaces stay quiet while everything is fine.
pub fn marker(snap: Option<&HealthSnapshot>) -> Option<String> {
    let snap = snap?;
    let mut parts = Vec::new();
    if let Some(s) = &snap.status
        && !s.is_ok()
    {
        parts.push(format!("claude {}", s.short()));
    }
    if let Some(v) = &snap.version
        && v.behind
        && let Some(latest) = &v.latest
    {
        parts.push(format!("cc {latest}"));
    }
    (!parts.is_empty()).then(|| format!("⚠ {}", parts.join(" · ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(indicator: &str, cc: &str, incidents: &str) -> serde_json::Value {
        serde_json::from_str(&format!(
            r#"{{
              "status": {{"indicator": "{indicator}", "description": "All Systems Operational"}},
              "components": [
                {{"id": "rwppv331jlwc", "name": "claude.ai", "status": "operational", "group": false}},
                {{"id": "yyzkbfz2thpt", "name": "Claude Code", "status": "{cc}", "group": false}}
              ],
              "incidents": [{incidents}]
            }}"#
        ))
        .unwrap()
    }

    #[test]
    fn an_all_clear_summary_reads_as_ok() {
        let s = parse_summary(&summary("none", "operational", "")).unwrap();
        assert!(s.is_ok());
        assert_eq!(s.short(), "ok");
        assert_eq!(s.claude_code.as_deref(), Some("operational"));
        assert!(s.degraded.is_empty() && s.incidents.is_empty());
        assert_eq!(marker(Some(&snapshot(Some(s), None))), None);
    }

    fn snapshot(status: Option<ServiceStatus>, version: Option<VersionState>) -> HealthSnapshot {
        HealthSnapshot {
            checked_at: 1_800_000_000,
            status,
            status_error: None,
            version,
            version_error: None,
        }
    }

    #[test]
    fn a_degraded_component_is_reported_even_when_the_overall_indicator_is_none() {
        // Statuspage leaves the indicator at "none" for some partial
        // degradations, so the components decide as well.
        let s = parse_summary(&summary("none", "degraded_performance", "")).unwrap();
        assert!(!s.is_ok());
        assert_eq!(s.short(), "degraded");
        assert_eq!(
            s.degraded,
            vec![(
                "Claude Code".to_string(),
                "degraded_performance".to_string()
            )]
        );
        let m = marker(Some(&snapshot(Some(s), None))).unwrap();
        assert_eq!(m, "⚠ claude degraded");
    }

    #[test]
    fn an_outage_with_an_incident_is_carried_through() {
        let incident =
            r#"{"name": "Elevated errors", "impact": "major", "status": "investigating"}"#;
        let s = parse_summary(&summary("major", "major_outage", incident)).unwrap();
        assert_eq!(s.short(), "outage");
        assert_eq!(s.incidents.len(), 1);
        assert_eq!(s.incidents[0].name, "Elevated errors");
        let out = lines(&snapshot(Some(s), None), 1_800_000_000, false);
        assert!(
            out.iter().any(|l| l == "Claude Code: major_outage"),
            "{out:?}"
        );
        assert!(
            out.iter()
                .any(|l| l == "incident: Elevated errors (major, investigating)"),
            "{out:?}"
        );
    }

    #[test]
    fn a_malformed_summary_is_an_error_rather_than_a_false_all_clear() {
        assert!(parse_summary(&serde_json::json!({})).is_err());
        assert!(parse_summary(&serde_json::json!({"status": {}})).is_err());
    }

    #[test]
    fn versions_compare_numerically_not_as_strings() {
        use std::cmp::Ordering;
        // The string comparison that this replaces gets this pair wrong.
        assert_eq!(compare_versions("2.1.9", "2.1.10"), Some(Ordering::Less));
        assert_eq!(
            compare_versions("2.1.274", "2.1.274"),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_versions("v2.2.0", "2.1.274"),
            Some(Ordering::Greater)
        );
        // Build metadata and pre-release suffixes are ignored.
        assert_eq!(
            compare_versions("2.1.274+abc123", "2.1.274"),
            Some(Ordering::Equal)
        );
        assert_eq!(compare_versions("2.1", "2.1.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("nightly", "2.1.274"), None);
    }

    #[test]
    fn only_a_provably_newer_release_counts_as_behind() {
        assert!(is_behind("2.1.267", "2.1.274"));
        assert!(!is_behind("2.1.274", "2.1.274"));
        // Ahead of the channel, which a pre-release build is.
        assert!(!is_behind("2.2.0", "2.1.274"));
        // Nothing parseable: never claim the user is out of date.
        assert!(!is_behind("unknown", "2.1.274"));
    }

    #[test]
    fn the_installed_version_is_the_first_token() {
        assert_eq!(
            parse_installed("2.1.274 (Claude Code)\n").as_deref(),
            Some("2.1.274")
        );
        assert_eq!(parse_installed("  \n").as_deref(), None);
    }

    #[test]
    fn the_channel_falls_back_unless_it_is_exactly_a_known_name() {
        let settings = |v: &str| serde_json::from_str::<serde_json::Value>(v).unwrap();
        assert_eq!(
            channel_from_settings(Some(&settings(r#"{"autoUpdatesChannel": "stable"}"#))),
            "stable"
        );
        assert_eq!(channel_from_settings(Some(&settings("{}"))), "latest");
        assert_eq!(channel_from_settings(None), "latest");
        // Anything else reaches a URL and a command line, so it is not used.
        assert_eq!(
            channel_from_settings(Some(&settings(r#"{"autoUpdatesChannel": "../../evil"}"#))),
            "latest"
        );
        assert_eq!(
            channel_from_settings(Some(&settings(r#"{"autoUpdatesChannel": "LATEST"}"#))),
            "latest"
        );
    }

    #[test]
    fn a_brew_install_is_detected_from_its_caskroom_path_and_fixes_the_channel() {
        let stable = PathBuf::from("/opt/homebrew/Caskroom/claude-code/2.1.267/claude");
        let latest = PathBuf::from("/opt/homebrew/Caskroom/claude-code@latest/2.1.274/claude");
        assert_eq!(cask_from_path(&stable), Some("claude-code"));
        assert_eq!(cask_from_path(&latest), Some("claude-code@latest"));
        assert_eq!(
            cask_from_path(Path::new("/home/me/.local/bin/claude")),
            None
        );
        // An unknown cask name is not passed into a URL.
        assert_eq!(
            cask_from_path(Path::new("/opt/homebrew/Caskroom/evil/1/claude")),
            None
        );

        // The cask decides the channel, not settings: a stable-cask user
        // compared against `latest` would permanently read as behind.
        assert_eq!(
            plan_lookup(Some("native"), Some(&stable), "latest"),
            LookupPlan::Brew {
                cask: "claude-code",
                channel: "stable".into()
            }
        );
        assert_eq!(
            plan_lookup(None, Some(&latest), "stable"),
            LookupPlan::Brew {
                cask: "claude-code@latest",
                channel: "latest".into()
            }
        );
    }

    #[test]
    fn the_install_method_picks_the_lookup() {
        let local = PathBuf::from("/home/me/.local/bin/claude");
        assert_eq!(
            plan_lookup(Some("native"), Some(&local), "latest"),
            LookupPlan::Native {
                channel: "latest".into()
            }
        );
        assert_eq!(
            plan_lookup(Some("npm-global"), Some(&local), "stable"),
            LookupPlan::Npm {
                channel: "stable".into()
            }
        );
        assert_eq!(
            plan_lookup(Some("bun"), None, "latest"),
            LookupPlan::Npm {
                channel: "latest".into()
            }
        );
        // Unknown or absent: the plain-text channel file, which is what the
        // product falls back to as well.
        assert_eq!(
            plan_lookup(None, None, "latest"),
            LookupPlan::Native {
                channel: "latest".into()
            }
        );
        assert_eq!(
            plan_lookup(Some("native"), Some(&local), "latest").source(),
            "native"
        );
    }

    #[test]
    fn a_behind_version_produces_a_marker_and_the_update_command() {
        let v = VersionState {
            installed: "2.1.267".into(),
            latest: Some("2.1.274".into()),
            channel: "latest".into(),
            source: "native".into(),
            behind: true,
            skipped: None,
            background_updates: Some(false),
        };
        let snap = snapshot(None, Some(v));
        assert!(snap.needs_attention());
        assert_eq!(marker(Some(&snap)).unwrap(), "⚠ cc 2.1.274");
        let out = lines(&snap, 1_800_000_000, false);
        assert!(
            out.iter()
                .any(|l| l == "Claude Code 2.1.267 → 2.1.274 available (latest channel)"),
            "{out:?}"
        );
        // autoUpdates is off in this snapshot, which is why it went stale.
        assert!(
            out.iter()
                .any(|l| l == "background auto-updates are off; run `claude update`"),
            "{out:?}"
        );
    }

    #[test]
    fn a_failed_lookup_never_reads_as_behind_or_as_needing_attention() {
        let v = VersionState {
            installed: "2.1.274".into(),
            latest: None,
            channel: "latest".into(),
            source: "native".into(),
            behind: false,
            skipped: Some("curl exited 6".into()),
            background_updates: None,
        };
        let snap = snapshot(None, Some(v));
        assert!(!snap.needs_attention());
        assert_eq!(marker(Some(&snap)), None);
        let out = lines(&snap, 1_800_000_000, true);
        assert!(
            out.iter().any(|l| l.contains("version lookup skipped")),
            "{out:?}"
        );
        assert!(
            out.iter().any(|l| l.starts_with("health checked")),
            "{out:?}"
        );
    }
}
