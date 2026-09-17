//! Dollar figures, delegated to ccusage.
//!
//! We deliberately do not carry our own model pricing table. CCMeter did, and
//! it broke silently the moment Opus 5 shipped: its `model.contains(...)` match
//! fell through to a Sonnet-rate fallback, so Opus usage was billed at 3/15
//! instead of 5/25 with no warning anywhere in the UI. ccusage tracks pricing
//! as its whole reason to exist, so we shell out and own only the layout.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::process::Command;

use crate::config::Config;
use crate::store::{self, JobState};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelBreakdown {
    #[serde(rename = "modelName")]
    pub model_name: String,
    pub cost: f64,
    #[serde(rename = "inputTokens", default)]
    pub input_tokens: u64,
    #[serde(rename = "outputTokens", default)]
    pub output_tokens: u64,
    #[serde(rename = "cacheReadTokens", default)]
    pub cache_read_tokens: u64,
    #[serde(rename = "cacheCreationTokens", default)]
    pub cache_creation_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    /// A date for `daily`, a session UUID for `session`.
    pub period: String,
    #[serde(rename = "totalCost")]
    pub total_cost: f64,
    #[serde(rename = "totalTokens", default)]
    pub total_tokens: u64,
    #[serde(rename = "cacheReadTokens", default)]
    pub cache_read_tokens: u64,
    #[serde(rename = "modelBreakdowns", default)]
    pub model_breakdowns: Vec<ModelBreakdown>,
}

#[derive(Debug, Deserialize)]
struct DailyReport {
    #[serde(default)]
    daily: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
struct SessionReport {
    #[serde(default)]
    session: Vec<Entry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Costs {
    /// Oldest-first, one per calendar day in the window; days without usage
    /// are present with a cost of 0.
    pub daily: Vec<(String, f64)>,
    pub today: f64,
    pub week: f64,
    /// Total over the whole window: `days` calendar days ending today.
    pub window: f64,
    /// Per model, descending by cost.
    pub models: Vec<ModelBreakdown>,
    /// Per project, descending by cost.
    pub projects: Vec<Project>,
    /// Per host over the window, descending by cost.
    pub by_host: Vec<(String, f64)>,
    /// Every host whose transcripts were counted.
    pub hosts: Vec<Host>,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub cost: f64,
    /// Hosts this project had sessions on, sorted.
    pub hosts: Vec<String>,
}

/// A source of transcripts: this machine, or another one mirrored into the
/// cache by sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub name: String,
    /// A Claude config directory, i.e. one containing `projects/`.
    pub config_dir: PathBuf,
    pub remote: bool,
    /// Epoch seconds of the last successful sync. Always None for this
    /// machine; None for a remote host means it has never synced.
    pub last_sync: Option<i64>,
}

/// What the cost job writes to `cost.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostSnapshot {
    pub generated_at: i64,
    /// The local calendar day the figures were gathered for, "YYYY-MM-DD".
    pub date: String,
    pub costs: Costs,
}

/// This machine's label in the dashboard: its host name, lower-cased.
fn local_host_name() -> String {
    let name = gethostname::gethostname()
        .to_string_lossy()
        .trim()
        .to_lowercase();
    if name.is_empty() {
        "local".into()
    } else {
        name
    }
}

/// This machine first, then every mirrored host found under
/// `<cache>/hosts/<name>/projects/`, sorted by name.
pub fn hosts() -> Vec<Host> {
    let mut hosts = vec![Host {
        name: local_host_name(),
        config_dir: store::home().join(".claude"),
        remote: false,
        last_sync: None,
    }];
    let Ok(entries) = std::fs::read_dir(store::cache_dir().join("hosts")) else {
        return hosts;
    };
    let mut remote: Vec<Host> = entries
        .flatten()
        .filter_map(|e| {
            let dir = e.path();
            if !dir.join("projects").is_dir() {
                return None;
            }
            let last_sync = std::fs::read_to_string(dir.join("last-sync"))
                .ok()
                .and_then(|s| s.trim().parse().ok());
            Some(Host {
                name: e.file_name().to_string_lossy().into_owned(),
                config_dir: dir,
                remote: true,
                last_sync,
            })
        })
        .collect();
    remote.sort_by(|a, b| a.name.cmp(&b.name));
    hosts.extend(remote);
    hosts
}

/// `CCMONETA_CCUSAGE` overrides the executable, which is how the integration
/// tests run against a fake ccusage with fixed output.
pub fn ccusage_exe() -> std::ffi::OsString {
    std::env::var_os("CCMONETA_CCUSAGE")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ccusage".into())
}

fn ccusage(args: &[&str], hosts: &[Host]) -> Result<Vec<u8>, String> {
    // Every host passed here is read in the same run: gather() passes one host
    // per daily report and all of them for sessions. The list is
    // comma-separated: ccusage rejects ':' outright. It also silently skips
    // a path that does not exist, so a host with no data on disk would still be
    // listed as counted while contributing nothing; hosts() returns only
    // directories that actually contain projects/ to rule that out.
    let dirs = hosts
        .iter()
        .map(|h| h.config_dir.display().to_string())
        .collect::<Vec<_>>()
        .join(",");
    let out = Command::new(ccusage_exe())
        .args(args)
        .env("CLAUDE_CONFIG_DIR", &dirs)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ccusage not found: install it with `npm install -g ccusage`, or set CCMONETA_CCUSAGE"
                    .to_string()
            } else {
                format!("ccusage: {e}")
            }
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr.lines().rev().find(|l| !l.trim().is_empty());
        return Err(match detail {
            Some(d) => format!("ccusage {} exited {}: {}", args[0], out.status, d.trim()),
            None => format!("ccusage {} exited {}", args[0], out.status),
        });
    }
    Ok(out.stdout)
}

fn ymd(days_ago: i64) -> String {
    (chrono::Local::now() - chrono::Duration::days(days_ago))
        .format("%Y%m%d")
        .to_string()
}

/// Map session UUID -> (project name, host name), across every host.
///
/// `ccusage session` keys by session UUID and carries no project field, but the
/// transcripts are laid out as <config>/projects/<encoded-path>/<uuid>.jsonl,
/// so the directory name recovers the project and the config dir the host. The
/// encoded name is the absolute path with separators replaced by '-', which is
/// lossy (a real '-' in a path is indistinguishable); we only ever show it as a
/// label, never resolve it back to a path, so the ambiguity is harmless here.
fn session_projects(hosts: &[Host]) -> HashMap<String, (String, String)> {
    let mut map = HashMap::new();
    for host in hosts {
        let Ok(dirs) = std::fs::read_dir(host.config_dir.join("projects")) else {
            continue;
        };
        for dir in dirs.flatten() {
            label_sessions(&dir, &host.name, &mut map);
        }
    }
    map
}

fn label_sessions(
    dir: &std::fs::DirEntry,
    host: &str,
    map: &mut HashMap<String, (String, String)>,
) {
    let label = dir
        .file_name()
        .to_string_lossy()
        .trim_matches('-')
        .to_string();
    // Keep the last two path segments: "-home-me-code-project" reads
    // better as "code/project" than as the whole absolute path.
    let short = label
        .rsplit('-')
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("/");
    // Walk the whole project directory, not just its top level. Workflow
    // runs live at <uuid>/subagents/workflows/wf_<id>/, and ccusage reports
    // each as a session named after that directory, so directory names are
    // recorded against the project as well as transcript file stems.
    let mut stack = vec![dir.path()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            // file_type() does not follow symlinks, so a link cannot loop the walk.
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            let id = if is_dir {
                Some(name.as_str())
            } else {
                name.strip_suffix(".jsonl")
            };
            if let Some(id) = id {
                map.entry(id.to_string())
                    .or_insert_with(|| (short.clone(), host.to_string()));
            }
            if is_dir {
                stack.push(e.path());
            }
        }
    }
}

/// Gather the last `days` of cost data across every host (see `hosts`).
///
/// The daily series is one ccusage run per host, in parallel, summed here;
/// sessions are a single run across all hosts, used only for projects. Summing
/// per-host daily reports makes the per-host totals add up to the header by
/// construction, which the session report did not: its per-host figures came
/// out materially higher than the daily totals over the same window. A single
/// run of either report takes on the order of a second per host, which is why
/// they run in parallel rather than one after another.
pub fn gather(days: i64) -> Result<Costs, String> {
    // A `days`-day window includes today, so it starts `days - 1` days back;
    // starting `days` back would quietly make every "30d" figure cover 31.
    let since = ymd(days - 1);
    let hosts = hosts();

    // A host with no usage in the window still exits 0 with an empty list, so
    // a quiet machine does not fail the whole report.
    let reports: Vec<Result<DailyReport, String>> = std::thread::scope(|s| {
        let workers: Vec<_> = hosts
            .iter()
            .map(|host| {
                let since = since.as_str();
                s.spawn(move || -> Result<DailyReport, String> {
                    let raw = ccusage(
                        &["daily", "--json", "--breakdown", "--since", since],
                        std::slice::from_ref(host),
                    )?;
                    serde_json::from_slice(&raw)
                        .map_err(|e| format!("daily JSON from {}: {e}", host.name))
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| {
                w.join()
                    .unwrap_or_else(|_| Err("daily worker panicked".into()))
            })
            .collect()
    });

    let mut costs = Costs::default();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let week_start = (chrono::Local::now() - chrono::Duration::days(6))
        .format("%Y-%m-%d")
        .to_string();

    let mut models: HashMap<String, ModelBreakdown> = HashMap::new();
    for (host, report) in hosts.iter().zip(reports) {
        let report = report?;
        let mut host_total = 0.0;
        for e in &report.daily {
            host_total += e.total_cost;
            // Days repeat across hosts here; the calendar rebuild below sums them.
            costs.daily.push((e.period.clone(), e.total_cost));
            if e.period == today {
                costs.today += e.total_cost;
            }
            if e.period.as_str() >= week_start.as_str() {
                costs.week += e.total_cost;
            }
            // ccusage was asked for exactly the window, so every entry is in it.
            costs.window += e.total_cost;
            costs.cache_read_tokens += e.cache_read_tokens;
            costs.total_tokens += e.total_tokens;
            for m in &e.model_breakdowns {
                let slot = models
                    .entry(m.model_name.clone())
                    .or_insert(ModelBreakdown {
                        model_name: m.model_name.clone(),
                        cost: 0.0,
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                    });
                slot.cost += m.cost;
                slot.input_tokens += m.input_tokens;
                slot.output_tokens += m.output_tokens;
                slot.cache_read_tokens += m.cache_read_tokens;
                slot.cache_creation_tokens += m.cache_creation_tokens;
            }
        }
        costs.by_host.push((host.name.clone(), host_total));
    }
    costs
        .by_host
        .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    // ccusage omits days with no usage, so its entries are not a calendar:
    // shown as-is, the days on either side of a gap sit next to each other
    // (Aug 22 directly before Aug 26, with Aug 23-25 simply absent). Rebuild
    // the series with one row per calendar day from `since` to today.
    let mut by_day: HashMap<String, f64> = HashMap::new();
    for (day, cost) in costs.daily.drain(..) {
        *by_day.entry(day).or_insert(0.0) += cost;
    }
    let last = chrono::Local::now().date_naive();
    let mut day = last - chrono::Duration::days(days - 1);
    while day <= last {
        let key = day.format("%Y-%m-%d").to_string();
        let cost = by_day.get(&key).copied().unwrap_or(0.0);
        costs.daily.push((key, cost));
        day += chrono::Duration::days(1);
    }
    costs.models = models.into_values().collect();
    costs.models.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Projects are best-effort: a failure here should not cost us the rest of
    // the dashboard, so a broken session report just leaves the pane empty.
    // They come from the session report, so they need not add up to the daily
    // totals; over the same window that report came out higher.
    if let Ok(raw) = ccusage(&["session", "--json", "--since", &since], &hosts)
        && let Ok(report) = serde_json::from_slice::<SessionReport>(&raw)
    {
        let map = session_projects(&hosts);
        let mut by_project: HashMap<String, (f64, BTreeSet<String>)> = HashMap::new();
        for e in report.session {
            let (name, host) = map
                .get(&e.period)
                .cloned()
                .unwrap_or_else(|| ("?".into(), "?".into()));
            let slot = by_project.entry(name).or_insert((0.0, BTreeSet::new()));
            slot.0 += e.total_cost;
            slot.1.insert(host);
        }
        let mut projects: Vec<Project> = by_project
            .into_iter()
            .map(|(name, (cost, hosts))| Project {
                name,
                cost,
                hosts: hosts.into_iter().collect(),
            })
            .collect();
        projects.sort_by(|a, b| {
            b.cost
                .partial_cmp(&a.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        costs.projects = projects;
    }
    costs.hosts = hosts;
    Ok(costs)
}

pub fn cache_path() -> PathBuf {
    store::cache_dir().join("cost.json")
}

/// A `cost.json` that does not parse, such as one in an older format, reads as
/// no snapshot, which makes the next surface start a refresh. The reason is
/// logged, so a cache that keeps reading as empty can be diagnosed.
pub fn load() -> Option<CostSnapshot> {
    let path = cache_path();
    let raw = std::fs::read(&path).ok()?;
    match serde_json::from_slice(&raw) {
        Ok(snap) => Some(snap),
        Err(e) => {
            note_unreadable(&path, &e);
            None
        }
    }
}

/// Log an unreadable `cost.json` once per version of the file, keyed on its
/// modification time, however many surfaces read it: the hook alone reads it on
/// every Claude Code turn.
fn note_unreadable(path: &std::path::Path, error: &serde_json::Error) {
    let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        return;
    };
    let stamp = match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format!("{}.{:09}", d.as_secs(), d.subsec_nanos()),
        Err(_) => return,
    };
    let marker = path.with_extension("json.unreadable");
    if std::fs::read_to_string(&marker).ok().as_deref() == Some(stamp.as_str()) {
        return;
    }
    let _ = store::write_text(&marker, &stamp);
    store::log(&format!(
        "cost.json unreadable, reading as no snapshot: {error}"
    ));
}

/// The cost job: gather and cache.
pub fn refresh(cfg: &Config) -> Result<(), String> {
    // Taken before gathering, so a gather that runs past midnight is recorded
    // against the day it started, reads as stale, and is redone.
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let costs = gather(cfg.cost.window_days)?;
    let snap = CostSnapshot {
        generated_at: store::now(),
        date,
        costs,
    };
    let path = cache_path();
    store::write_json(&path, &snap).map_err(|e| format!("writing {}: {e}", path.display()))?;
    // Read back what was just written. A snapshot that does not parse reads as
    // missing on every surface, so fail here, where the error is recorded and
    // shown as `$?`, rather than let every surface show `$…` indefinitely.
    let raw = std::fs::read(&path).map_err(|e| format!("reading back {}: {e}", path.display()))?;
    serde_json::from_slice::<CostSnapshot>(&raw)
        .map(|_| ())
        .map_err(|e| format!("{} does not read back: {e}", path.display()))
}

/// Today's spend as the bar and the status line show it:
///
/// * `$12.34`: gathered for today within the last hour.
/// * `~$12.34`: gathered for today, but over an hour ago: no surface has run
///   since, or refreshes are failing. Spend only grows within a day, so this is
///   a lower bound.
/// * `$…`: nothing usable for today yet; a refresh is due or running.
/// * `$?`: nothing usable, and the last refresh failed.
///
/// A snapshot from an earlier day is never shown: its "today" is a day that is
/// over, and no marker would make it today's spend.
pub fn today_label(
    snap: Option<&CostSnapshot>,
    state: &JobState,
    now: i64,
    today: NaiveDate,
) -> String {
    let usable = snap.filter(|s| s.date == today.format("%Y-%m-%d").to_string());
    match usable {
        Some(s) if now - s.generated_at > 3600 => format!("~${:.2}", s.costs.today),
        Some(s) => format!("${:.2}", s.costs.today),
        None if state.failures > 0 => "$?".into(),
        None => "$…".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn snap(age: i64, date: &str, today: f64) -> CostSnapshot {
        CostSnapshot {
            generated_at: NOW - age,
            date: date.into(),
            costs: Costs {
                today,
                ..Default::default()
            },
        }
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn today_label_covers_every_case() {
        let ok = JobState::default();
        let failing = JobState {
            failures: 2,
            ..Default::default()
        };
        let today = day("2026-09-15");
        let fresh = snap(60, "2026-09-15", 12.34);
        let old = snap(7200, "2026-09-15", 12.34);
        let yesterday = snap(60, "2026-09-14", 99.99);

        assert_eq!(today_label(Some(&fresh), &ok, NOW, today), "$12.34");
        assert_eq!(today_label(Some(&old), &failing, NOW, today), "~$12.34");
        assert_eq!(today_label(Some(&yesterday), &ok, NOW, today), "$…");
        assert_eq!(today_label(Some(&yesterday), &failing, NOW, today), "$?");
        assert_eq!(today_label(None, &ok, NOW, today), "$…");
        assert_eq!(today_label(None, &failing, NOW, today), "$?");
    }

    #[test]
    fn a_snapshot_round_trips_through_json() {
        let s = snap(0, "2026-09-15", 12.34);
        let back: CostSnapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.date, "2026-09-15");
        assert_eq!(back.costs.today, 12.34);
    }

    #[test]
    fn the_old_cost_json_format_reads_as_no_snapshot() {
        let old = r#"{"today": 18.02, "captured_at": 1789000000}"#;
        assert!(serde_json::from_str::<CostSnapshot>(old).is_err());
    }
}
