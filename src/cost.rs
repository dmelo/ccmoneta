//! Dollar figures, delegated to ccusage.
//!
//! We deliberately do not carry our own model pricing table. CCMeter did, and
//! it broke silently the moment Opus 5 shipped: its `model.contains(...)` match
//! fell through to a Sonnet-rate fallback, so Opus usage was billed at 3/15
//! instead of 5/25 with no warning anywhere in the UI. ccusage tracks pricing
//! as its whole reason to exist, so we shell out and own only the layout.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::accounts::{self, Owners};
use crate::config::Config;
use crate::store::{self, JobState};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// The day, "YYYY-MM-DD". `ccusage claude` names it `date`; the all-agent
    /// reports call it `period`.
    #[serde(alias = "date")]
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

/// `ccusage claude daily --instances`: each project's days, keyed by the
/// project's transcript directory name.
#[derive(Debug, Deserialize)]
struct ProjectsReport {
    #[serde(default)]
    projects: HashMap<String, Vec<Entry>>,
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
    /// Models and projects for each day that had usage, keyed "YYYY-MM-DD".
    /// The dashboard shows one when a day is selected.
    pub days: BTreeMap<String, Day>,
    /// Per Claude account over the window, descending by cost, named as on
    /// this machine (see accounts.rs). One unnamed entry without aimux.
    pub by_account: Vec<(String, f64)>,
}

/// Models and projects for one day, or summed over the whole window.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Day {
    /// Per model, descending by cost.
    pub models: Vec<ModelBreakdown>,
    /// Per project, descending by cost.
    pub projects: Vec<Project>,
    pub cache_read_tokens: u64,
    /// Per Claude account, descending by cost.
    pub by_account: Vec<(String, f64)>,
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

fn ccusage(args: &[&str], config_dir: &Path) -> Result<Vec<u8>, String> {
    // One config directory per run. ccusage silently skips a CLAUDE_CONFIG_DIR
    // that does not exist, so a host with no data on disk would still be
    // listed as counted while contributing nothing; hosts() returns only
    // directories that actually contain projects/ to rule that out.
    let out = Command::new(ccusage_exe())
        .args(args)
        .env("CLAUDE_CONFIG_DIR", config_dir)
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
        let command = args
            .iter()
            .take_while(|a| !a.starts_with('-'))
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        return Err(match detail {
            Some(d) => format!("ccusage {command} exited {}: {}", out.status, d.trim()),
            None => format!("ccusage {command} exited {}", out.status),
        });
    }
    Ok(out.stdout)
}

/// A project's label: the last two segments of its transcript directory name.
///
/// ccusage keys projects by that directory name, which is the absolute path
/// with separators replaced by '-'. That is lossy (a real '-' in a path is
/// indistinguishable from a separator), but "-home-me-code-project" reads
/// better as "code/project" than as the whole path, and the label is only ever
/// shown, never resolved back to a path. The same checkout lives under a
/// different home path on each machine, so the label is also what merges it.
fn project_label(dir: &str) -> String {
    dir.trim_matches('-')
        .rsplit('-')
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("/")
}

/// Most expensive first; equal costs by name, since the sums come out of hash
/// maps and would otherwise swap places from one refresh to the next.
fn by_cost_desc<T>(items: &mut [T], cost: impl Fn(&T) -> f64, name: impl Fn(&T) -> &str) {
    items.sort_by(|a, b| {
        cost(b)
            .partial_cmp(&cost(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| name(a).cmp(name(b)))
    });
}

/// Models and projects summed over a set of days: the whole window, or one day.
#[derive(Default)]
struct Tally {
    models: HashMap<String, ModelBreakdown>,
    /// Project label -> (cost, hosts it ran on).
    projects: HashMap<String, (f64, BTreeSet<String>)>,
    cache_read_tokens: u64,
    accounts: HashMap<String, f64>,
}

impl Tally {
    fn add(&mut self, host: &str, account: &str, project: &str, e: &Entry) {
        *self.accounts.entry(account.to_string()).or_default() += e.total_cost;
        for m in &e.model_breakdowns {
            let slot = self
                .models
                .entry(m.model_name.clone())
                .or_insert_with(|| ModelBreakdown {
                    model_name: m.model_name.clone(),
                    ..Default::default()
                });
            slot.cost += m.cost;
            slot.input_tokens += m.input_tokens;
            slot.output_tokens += m.output_tokens;
            slot.cache_read_tokens += m.cache_read_tokens;
            slot.cache_creation_tokens += m.cache_creation_tokens;
        }
        let slot = self.projects.entry(project.to_string()).or_default();
        slot.0 += e.total_cost;
        slot.1.insert(host.to_string());
        self.cache_read_tokens += e.cache_read_tokens;
    }

    fn finish(self) -> Day {
        let mut models: Vec<ModelBreakdown> = self.models.into_values().collect();
        by_cost_desc(&mut models, |m| m.cost, |m| &m.model_name);
        let mut projects: Vec<Project> = self
            .projects
            .into_iter()
            .map(|(name, (cost, hosts))| Project {
                name,
                cost,
                hosts: hosts.into_iter().collect(),
            })
            .collect();
        by_cost_desc(&mut projects, |p| p.cost, |p| &p.name);
        let mut by_account: Vec<(String, f64)> = self.accounts.into_iter().collect();
        by_cost_desc(&mut by_account, |a| a.1, |a| &a.0);
        Day {
            models,
            projects,
            cache_read_tokens: self.cache_read_tokens,
            by_account,
        }
    }
}

/// One ccusage report: a host's transcripts, or the part one account ran.
struct Run {
    host: String,
    /// The account key (see accounts::Account::key); names are put on at
    /// the end, so two accounts that share a name are never added together.
    account: String,
    report: ProjectsReport,
}

/// Fold every run's report into the dashboard's figures. The window runs from
/// `first` to `today`, both included.
fn assemble(runs: &[Run], first: NaiveDate, today: NaiveDate) -> Costs {
    let key = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
    let today_key = key(today);
    let week_start = key(today - chrono::Duration::days(6));

    let mut costs = Costs::default();
    let mut window = Tally::default();
    let mut days: BTreeMap<String, Tally> = BTreeMap::new();
    let mut by_day: HashMap<String, f64> = HashMap::new();
    let mut by_host: HashMap<String, f64> = HashMap::new();
    for Run {
        host,
        account,
        report,
    } in runs
    {
        let host_total = by_host.entry(host.clone()).or_default();
        for (dir, entries) in &report.projects {
            let project = project_label(dir);
            for e in entries {
                *host_total += e.total_cost;
                *by_day.entry(e.period.clone()).or_default() += e.total_cost;
                if e.period == today_key {
                    costs.today += e.total_cost;
                }
                if e.period >= week_start {
                    costs.week += e.total_cost;
                }
                // ccusage was asked for exactly the window, so every entry is in it.
                costs.window += e.total_cost;
                costs.total_tokens += e.total_tokens;
                window.add(host, account, &project, e);
                days.entry(e.period.clone())
                    .or_default()
                    .add(host, account, &project, e);
            }
        }
    }
    costs.by_host = by_host.into_iter().collect();
    by_cost_desc(&mut costs.by_host, |h| h.1, |h| &h.0);

    // ccusage omits days with no usage, so its entries are not a calendar:
    // shown as-is, the days on either side of a gap sit next to each other
    // (Aug 22 directly before Aug 26, with Aug 23-25 simply absent). Rebuild
    // the series with one row per calendar day from `first` to today.
    let mut day = first;
    while day <= today {
        let k = key(day);
        let cost = by_day.get(&k).copied().unwrap_or(0.0);
        costs.daily.push((k, cost));
        day += chrono::Duration::days(1);
    }

    let whole = window.finish();
    costs.models = whole.models;
    costs.projects = whole.projects;
    costs.cache_read_tokens = whole.cache_read_tokens;
    costs.by_account = whole.by_account;
    costs.days = days.into_iter().map(|(d, t)| (d, t.finish())).collect();
    costs
}

/// Gather the last `days` of cost data across every host (see `hosts`).
///
/// One ccusage run per host, in parallel: the daily report split by project
/// (`--instances`), with models (`--breakdown`). That one report yields the
/// daily series, the models and the projects, for the window and for each
/// day, so they all add up to the header by construction. Projects used to
/// come from the session report, whose figures came out materially higher
/// than the daily totals over the same window. A run takes on the order of a
/// second per host, which is why they run in parallel.
pub fn gather(days: i64) -> Result<Costs, String> {
    let today = chrono::Local::now().date_naive();
    // A `days`-day window includes today, so it starts `days - 1` days back;
    // starting `days` back would quietly make every "30d" figure cover 31.
    let first = today - chrono::Duration::days(days - 1);
    let since = first.format("%Y%m%d").to_string();
    let hosts = hosts();
    let window_start = first
        .and_hms_opt(0, 0, 0)
        .and_then(|t| t.and_local_timezone(chrono::Local).earliest())
        .map_or(0, |t| t.timestamp());
    let (sources, names) = sources(&hosts, window_start)?;

    // A source with no usage in the window still exits 0 with an empty list,
    // so a quiet machine or account does not fail the whole report.
    let reports: Vec<Result<ProjectsReport, String>> = std::thread::scope(|s| {
        let workers: Vec<_> = sources
            .iter()
            .map(|src| {
                let since = since.as_str();
                s.spawn(move || -> Result<ProjectsReport, String> {
                    // `claude daily`, not `daily`: in ccusage 20 the bare
                    // report covers every agent CLI it finds (OpenCode, Codex,
                    // ...) on this machine, whatever CLAUDE_CONFIG_DIR says, so
                    // this machine's other agents would be added once per host.
                    let raw = ccusage(
                        &[
                            "claude",
                            "daily",
                            "--json",
                            "--breakdown",
                            "--instances",
                            "--since",
                            since,
                        ],
                        &src.config_dir,
                    )?;
                    serde_json::from_slice(&raw)
                        .map_err(|e| format!("daily JSON from {}: {e}", src.host))
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
    let runs = sources
        .into_iter()
        .zip(reports)
        .map(|(src, r)| {
            r.map(|report| Run {
                account: src.account,
                host: src.host,
                report,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut costs = assemble(&runs, first, today);
    let labels = account_labels(&names, runs.iter().map(|r| r.account.as_str()));
    let label = |key: &mut String| {
        if let Some(l) = labels.get(key.as_str()) {
            *key = l.clone();
        }
    };
    costs.by_account.iter_mut().for_each(|(k, _)| label(k));
    for day in costs.days.values_mut() {
        day.by_account.iter_mut().for_each(|(k, _)| label(k));
    }
    costs.hosts = hosts;
    Ok(costs)
}

/// What each account key is called on screen: its profile name, or, when it
/// has none or shares it with another account, the name (or "account") and
/// the start of its key.
fn account_labels<'a>(
    names: &HashMap<String, String>,
    keys: impl Iterator<Item = &'a str>,
) -> HashMap<String, String> {
    let mut keys: Vec<&str> = keys.collect();
    keys.sort_unstable();
    keys.dedup();
    let name = |k: &str| names.get(k).cloned().unwrap_or_default();
    keys.iter()
        .map(|k| {
            let n = name(k);
            let shared = keys.iter().filter(|o| name(o) == n).count() > 1;
            let label = if !n.is_empty() && !shared {
                n
            } else {
                let short: String = k.trim_start_matches("profile:").chars().take(8).collect();
                let base = if n.is_empty() {
                    "account".to_string()
                } else {
                    n
                };
                format!("{base} {short}")
            };
            (k.to_string(), label)
        })
        .collect()
}

/// Where one ccusage run reads: a host's own config directory, or the view of
/// it that holds one account's transcripts.
struct Source {
    host: String,
    /// The account key (see accounts::Account::key).
    account: String,
    config_dir: PathBuf,
}

/// What to run ccusage on, and each account's name by key.
///
/// A host whose sessions all belong to one account is read in place, as before
/// accounts existed. A host where several accounts ran sessions is split into
/// one view per account (see `split`). Names are this machine's profile names;
/// an account only another machine has is named as that machine names it.
fn sources(
    hosts: &[Host],
    window_start: i64,
) -> Result<(Vec<Source>, HashMap<String, String>), String> {
    let local = accounts::local();
    let mut names: HashMap<String, String> =
        local.iter().map(|a| (a.key(), a.name.clone())).collect();
    let local_default = local
        .iter()
        .find(|a| a.source)
        .or(local.first())
        .map(accounts::Account::key)
        .unwrap_or_default();
    let mut out = Vec::new();
    for host in hosts {
        let owners = if host.remote {
            match std::fs::read_to_string(crate::sync::markers_path(&host.name)) {
                Ok(text) => {
                    let (remote, owners) = accounts::parse_remote_markers(&text);
                    for r in remote {
                        let name = names.entry(r.key).or_default();
                        if name.is_empty() {
                            *name = r.name;
                        }
                    }
                    owners
                }
                // Not synced since accounts existed: all of it is the source
                // account's, which is how it was counted before.
                Err(_) => Owners::new(local_default.clone()),
            }
        } else {
            accounts::local_owners(&local)
        };
        if owners.accounts().len() <= 1 {
            out.push(Source {
                host: host.name.clone(),
                account: owners.default.clone(),
                config_dir: host.config_dir.clone(),
            });
        } else {
            out.extend(split(host, &owners, window_start)?);
        }
    }
    // A profile isolated with `aimux migrate isolate` keeps its transcripts in
    // a projects/ of its own instead of the shared one, so nothing above reads
    // them; every turn there is that account's.
    let local_host = hosts.iter().find(|h| !h.remote).map(|h| h.name.clone());
    for a in local.iter().filter(|a| !a.source) {
        let dir = a.config_dir.join("projects");
        let own = std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir());
        if let (true, Some(host)) = (own, &local_host) {
            out.push(Source {
                host: host.clone(),
                account: a.key(),
                config_dir: a.config_dir.clone(),
            });
        }
    }
    Ok((out, names))
}

/// Split a host's transcripts into one view per account, under
/// `<cache>/views/<host>/<account>/projects/`, rebuilt on every run.
///
/// A session only the default account ran is hard-linked whole into that
/// account's view; ccusage ignores symlinks, and a hard link costs nothing on
/// the same filesystem (it is copied otherwise). A session another account had
/// a hand in is split line by line, each line going to the account that ran it
/// at its timestamp (see accounts::Owners). Transcripts last written before the
/// window cannot hold any of its usage, so they are left out.
fn split(host: &Host, owners: &Owners, window_start: i64) -> Result<Vec<Source>, String> {
    let root = store::cache_dir().join("views").join(&host.name);
    let _ = std::fs::remove_dir_all(&root);
    let keys = owners.accounts();
    let view = |key: &str| root.join(accounts::file_name(key));
    for k in &keys {
        let dir = view(k).join("projects");
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    let projects = host.config_dir.join("projects");
    for file in transcripts(&projects) {
        let fresh = std::fs::metadata(&file)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| d.as_secs() as i64 >= window_start);
        let Ok(rel) = file.strip_prefix(&projects) else {
            continue;
        };
        // <project>/<session>.jsonl, or <project>/<session>/subagents/...
        let Some(session) = rel.components().nth(1).map(|c| {
            c.as_os_str()
                .to_string_lossy()
                .trim_end_matches(".jsonl")
                .to_string()
        }) else {
            continue;
        };
        if !fresh {
            continue;
        }
        if owners.only_default(&session) {
            link_or_copy(&file, &view(&owners.default).join("projects").join(rel))?;
        } else {
            split_file(&file, rel, &session, owners, &view)?;
        }
    }
    Ok(keys
        .into_iter()
        .map(|k| Source {
            host: host.name.clone(),
            config_dir: view(&k),
            account: k,
        })
        .collect())
}

/// Every `*.jsonl` under `root`, at any depth, not following symlinks.
fn transcripts(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(kind) = e.file_type() else {
                continue;
            };
            let path = e.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() && path.extension().is_some_and(|x| x == "jsonl") {
                out.push(path);
            }
        }
    }
    out
}

fn link_or_copy(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    match std::fs::hard_link(from, to).or_else(|_| std::fs::copy(from, to).map(|_| ())) {
        Ok(()) => Ok(()),
        // Claude Code removed it since the listing: there is nothing to count.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("linking {} into a view: {e}", from.display())),
    }
}

/// Write each account's lines of one transcript into its view. A line with no
/// timestamp goes with the line before it.
///
/// Lines are handled as bytes and copied as they are, so a line Claude Code is
/// still writing, which can end inside a multi-byte character, reaches ccusage
/// exactly as it would have unsplit; ccusage skips what does not parse. A file
/// removed since the listing is skipped.
fn split_file(
    file: &Path,
    rel: &Path,
    session: &str,
    owners: &Owners,
    view: &dyn Fn(&str) -> PathBuf,
) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Stamp {
        timestamp: Option<String>,
    }
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("reading {}: {e}", file.display())),
    };
    let mut parts: HashMap<String, Vec<u8>> = HashMap::new();
    let mut owner = owners.default.clone();
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let at = serde_json::from_slice::<Stamp>(line)
            .ok()
            .and_then(|s| s.timestamp)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
            .map(|t| t.timestamp());
        if let Some(at) = at {
            owner = owners.at(session, at).to_string();
        }
        let part = parts.entry(owner.clone()).or_default();
        part.extend_from_slice(line);
        part.push(b'\n');
    }
    for (account, body) in parts {
        let to = view(&account).join("projects").join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        std::fs::write(&to, body).map_err(|e| format!("writing {}: {e}", to.display()))?;
    }
    Ok(())
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

    fn entry(date: &str, cost: f64, model: &str) -> Entry {
        Entry {
            period: date.into(),
            total_cost: cost,
            total_tokens: 100,
            cache_read_tokens: 10,
            model_breakdowns: vec![ModelBreakdown {
                model_name: model.into(),
                cost,
                ..Default::default()
            }],
        }
    }

    fn report(projects: &[(&str, Vec<Entry>)]) -> ProjectsReport {
        ProjectsReport {
            projects: projects
                .iter()
                .map(|(dir, es)| (dir.to_string(), es.clone()))
                .collect(),
        }
    }

    #[test]
    fn project_labels_keep_the_last_two_path_segments() {
        assert_eq!(project_label("-home-me-code-project"), "code/project");
        assert_eq!(project_label("-Users-me-code-project"), "code/project");
        assert_eq!(project_label("-home-me"), "home/me");
    }

    #[test]
    fn days_hold_their_own_models_and_projects_and_add_up() {
        let reports = vec![
            (
                "desk".to_string(),
                report(&[
                    (
                        "-home-me-code-alpha",
                        vec![
                            entry("2026-09-14", 1.0, "claude-opus-5"),
                            entry("2026-09-15", 2.0, "claude-opus-5-5"),
                        ],
                    ),
                    (
                        "-home-me-code-beta",
                        vec![entry("2026-09-15", 4.0, "claude-sonnet-5-5")],
                    ),
                ]),
            ),
            (
                "laptop".to_string(),
                // The same checkout under another home path merges with desk's.
                report(&[(
                    "-Users-me-code-alpha",
                    vec![entry("2026-09-15", 8.0, "claude-opus-5-5")],
                )]),
            ),
        ];
        let runs: Vec<Run> = reports
            .into_iter()
            .map(|(host, report)| Run {
                host,
                account: String::new(),
                report,
            })
            .collect();
        let c = assemble(&runs, day("2026-09-12"), day("2026-09-15"));

        assert_eq!(c.today, 14.0);
        assert_eq!(c.window, 15.0);
        assert_eq!(
            c.daily,
            vec![
                ("2026-09-12".to_string(), 0.0),
                ("2026-09-13".to_string(), 0.0),
                ("2026-09-14".to_string(), 1.0),
                ("2026-09-15".to_string(), 14.0),
            ]
        );
        assert_eq!(
            c.by_host,
            vec![("laptop".to_string(), 8.0), ("desk".to_string(), 7.0)]
        );

        // Days with no usage have no entry; the dashboard shows them as empty.
        assert_eq!(
            c.days.keys().collect::<Vec<_>>(),
            ["2026-09-14", "2026-09-15"]
        );

        let monday = &c.days["2026-09-14"];
        assert_eq!(monday.models.len(), 1);
        assert_eq!(monday.models[0].model_name, "claude-opus-5");
        assert_eq!(monday.projects.len(), 1);
        assert_eq!(monday.projects[0].name, "code/alpha");
        assert_eq!(monday.projects[0].hosts, ["desk"]);

        let tuesday = &c.days["2026-09-15"];
        let projects: Vec<(&str, f64, Vec<String>)> = tuesday
            .projects
            .iter()
            .map(|p| (p.name.as_str(), p.cost, p.hosts.clone()))
            .collect();
        assert_eq!(
            projects,
            vec![
                (
                    "code/alpha",
                    10.0,
                    vec!["desk".to_string(), "laptop".to_string()]
                ),
                ("code/beta", 4.0, vec!["desk".to_string()]),
            ]
        );
        let models: Vec<(&str, f64)> = tuesday
            .models
            .iter()
            .map(|m| (m.model_name.as_str(), m.cost))
            .collect();
        assert_eq!(
            models,
            vec![("claude-opus-5-5", 10.0), ("claude-sonnet-5-5", 4.0)]
        );
        assert_eq!(tuesday.cache_read_tokens, 30);

        // The window still sums every day.
        assert_eq!(c.projects[0].name, "code/alpha");
        assert_eq!(c.projects[0].cost, 11.0);
        assert_eq!(c.models.iter().map(|m| m.cost).sum::<f64>(), 15.0);
        assert_eq!(c.cache_read_tokens, 40);
    }

    #[test]
    fn accounts_add_up_per_day_and_over_the_window() {
        let run = |host: &str, account: &str, entries: Vec<Entry>| Run {
            host: host.into(),
            account: account.into(),
            report: report(&[("-home-me-code-alpha", entries)]),
        };
        let runs = vec![
            run(
                "desk",
                "main",
                vec![
                    entry("2026-09-14", 1.0, "claude-opus-5-5"),
                    entry("2026-09-15", 2.0, "claude-opus-5-5"),
                ],
            ),
            run(
                "desk",
                "second",
                vec![entry("2026-09-15", 4.0, "claude-opus-5-5")],
            ),
            run(
                "laptop",
                "second",
                vec![entry("2026-09-15", 8.0, "claude-opus-5-5")],
            ),
        ];
        let c = assemble(&runs, day("2026-09-14"), day("2026-09-15"));
        assert_eq!(
            c.by_account,
            vec![("second".to_string(), 12.0), ("main".to_string(), 3.0)]
        );
        // A host's total counts every account that ran there.
        assert_eq!(
            c.by_host,
            vec![("laptop".to_string(), 8.0), ("desk".to_string(), 7.0)]
        );
        assert_eq!(
            c.days["2026-09-14"].by_account,
            vec![("main".to_string(), 1.0)]
        );
        assert_eq!(
            c.days["2026-09-15"].by_account,
            vec![("second".to_string(), 12.0), ("main".to_string(), 2.0)]
        );
        assert_eq!(c.today, 14.0);
    }

    #[test]
    fn a_split_session_sends_each_line_to_the_account_that_ran_it() {
        let dir = std::env::temp_dir().join(format!("ccmoneta-split-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("in.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        // Epoch 1_800_000_000 is 2027-01-15T08:00:00Z.
        std::fs::write(
            &file,
            "{\"timestamp\":\"2027-01-15T07:59:00Z\",\"n\":1}\n\
             {\"n\":2}\n\
             {\"timestamp\":\"2027-01-15T08:01:00Z\",\"n\":3}\n",
        )
        .unwrap();
        let mut owners = Owners::new("main".into());
        owners.add("s1", 1_800_000_000, "second");
        let view = |k: &str| dir.join(k);
        split_file(&file, Path::new("p/s1.jsonl"), "s1", &owners, &view).unwrap();
        let read =
            |k: &str| std::fs::read_to_string(dir.join(k).join("projects/p/s1.jsonl")).unwrap();
        assert_eq!(
            read("main"),
            "{\"timestamp\":\"2027-01-15T07:59:00Z\",\"n\":1}\n{\"n\":2}\n",
            "a line with no timestamp stays with the one before"
        );
        assert_eq!(
            read("second"),
            "{\"timestamp\":\"2027-01-15T08:01:00Z\",\"n\":3}\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn splitting_keeps_bytes_as_they_are_and_skips_a_vanished_file() {
        let dir = std::env::temp_dir().join(format!("ccmoneta-bytes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("in.jsonl");
        // A complete line, then one cut off inside a two-byte character.
        let body: Vec<u8> = [
            b"{\"timestamp\":\"2027-01-15T07:59:00Z\"}\n".as_slice(),
            b"{\"t\":\"\xc3",
        ]
        .concat();
        std::fs::write(&file, &body).unwrap();
        let owners = Owners::new("main".into());
        let view = |k: &str| dir.join(k);
        split_file(&file, Path::new("p/s.jsonl"), "s", &owners, &view).unwrap();
        let out = std::fs::read(dir.join("main/projects/p/s.jsonl")).unwrap();
        assert_eq!(out, [body.as_slice(), b"\n"].concat());
        split_file(
            &dir.join("gone.jsonl"),
            Path::new("p/g.jsonl"),
            "g",
            &owners,
            &view,
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn accounts_are_labelled_apart_even_when_names_clash() {
        let names: HashMap<String, String> = [
            ("uuid-aaaa1111", "main"),
            ("uuid-bbbb2222", "main"),
            ("uuid-cccc3333", "work"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let l = account_labels(
            &names,
            [
                "uuid-aaaa1111",
                "uuid-bbbb2222",
                "uuid-cccc3333",
                "uuid-dddd4444",
            ]
            .into_iter(),
        );
        assert_eq!(l["uuid-cccc3333"], "work");
        assert_eq!(l["uuid-aaaa1111"], "main uuid-aaa");
        assert_eq!(l["uuid-bbbb2222"], "main uuid-bbb");
        assert_eq!(l["uuid-dddd4444"], "account uuid-ddd");
    }

    #[test]
    fn the_instances_report_parses_as_ccusage_writes_it() {
        let raw = r#"{"projects":{"-home-me-code-alpha":[{"date":"2026-09-15","totalCost":1.5,"totalTokens":9,"cacheReadTokens":3,"project":"-home-me-code-alpha","modelBreakdowns":[{"modelName":"claude-opus-5-5","cost":1.5,"inputTokens":1,"outputTokens":2,"cacheReadTokens":3,"cacheCreationTokens":3}]}]},"totals":{}}"#;
        let r: ProjectsReport = serde_json::from_str(raw).unwrap();
        let e = &r.projects["-home-me-code-alpha"][0];
        assert_eq!(e.period, "2026-09-15");
        assert_eq!(e.model_breakdowns[0].cost, 1.5);
    }

    #[test]
    fn the_old_cost_json_format_reads_as_no_snapshot() {
        let old = r#"{"today": 18.02, "captured_at": 1789000000}"#;
        assert!(serde_json::from_str::<CostSnapshot>(old).is_err());
    }
}
