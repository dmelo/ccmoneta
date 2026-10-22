//! Dollar figures: the ledger's records, deduplicated, priced and added up.
//!
//! The records come from ledger.rs, which reads only what each transcript
//! gained since the last run; prices come from prices.rs. What is done here
//! follows ccusage, which ccmoneta used to run for these figures, so they
//! agree with it: the same deduplication (one reply counted once, however many
//! transcripts copied it), the same day boundaries (local time), and the same
//! per-request formula. Each record then goes to the account that ran it, and
//! is added to its day, model, project, machine and account.

use chrono::{NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::accounts::{self, Owners};
use crate::config::Config;
use crate::ledger::{self, ANTHROPIC_ID, FAST, Rec, SIDECHAIN, SPEED};
use crate::prices::{Prices, Tokens};
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
    /// The price list does not name this model, so its cost is counted as 0.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unpriced: bool,
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
    /// Models used in the window that the price list does not name. Their
    /// usage counts as $0, so every figure is low by their share.
    pub unpriced: Vec<String>,
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

/// A project's label: the last two segments of its transcript directory name.
///
/// Claude Code names that directory after the absolute path, with separators
/// replaced by '-'. That is lossy (a real '-' in a path is indistinguishable
/// from a separator), but "-home-me-code-project" reads better as
/// "code/project" than as the whole path, and the label is only ever shown,
/// never resolved back to a path. The same checkout lives under a different
/// home path on each machine, so the label is also what merges it.
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

/// One counted request, ready to add up.
#[derive(Debug, Clone)]
struct Use {
    host: String,
    /// The account key (see accounts::Account::key); names are put on at
    /// the end, so two accounts that share a name are never added together.
    account: String,
    /// The transcript directory under `projects/`.
    project: String,
    /// "YYYY-MM-DD", local time.
    date: String,
    /// As ccusage shows it: with `-fast` for fast mode, None for a line that
    /// names no model or the `<synthetic>` placeholder, which are counted in
    /// the totals but get no row of their own.
    model: Option<String>,
    unpriced: bool,
    cost: f64,
    tokens: Tokens,
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
    fn add(&mut self, project: &str, u: &Use) {
        *self.accounts.entry(u.account.clone()).or_default() += u.cost;
        if let Some(model) = &u.model {
            let slot = self
                .models
                .entry(model.clone())
                .or_insert_with(|| ModelBreakdown {
                    model_name: model.clone(),
                    ..Default::default()
                });
            slot.cost += u.cost;
            slot.input_tokens += u.tokens.input;
            slot.output_tokens += u.tokens.output;
            slot.cache_read_tokens += u.tokens.cache_read;
            slot.cache_creation_tokens += u.tokens.cache_5m + u.tokens.cache_1h;
            slot.unpriced |= u.unpriced;
        }
        let slot = self.projects.entry(project.to_string()).or_default();
        slot.0 += u.cost;
        slot.1.insert(u.host.clone());
        self.cache_read_tokens += u.tokens.cache_read;
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

/// Fold every counted request into the dashboard's figures. The window runs
/// from `first` to `today`, both included; requests outside it are skipped.
fn assemble(uses: &[Use], first: NaiveDate, today: NaiveDate) -> Costs {
    let key = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
    let today_key = key(today);
    let first_key = key(first);
    let week_start = key(today - chrono::Duration::days(6));

    let mut costs = Costs::default();
    let mut window = Tally::default();
    let mut days: BTreeMap<String, Tally> = BTreeMap::new();
    let mut by_day: HashMap<String, f64> = HashMap::new();
    let mut by_host: HashMap<String, f64> = HashMap::new();
    let mut unpriced: BTreeSet<String> = BTreeSet::new();
    for u in uses {
        if u.date < first_key || u.date > today_key {
            continue;
        }
        let project = project_label(&u.project);
        *by_host.entry(u.host.clone()).or_default() += u.cost;
        *by_day.entry(u.date.clone()).or_default() += u.cost;
        if u.date == today_key {
            costs.today += u.cost;
        }
        if u.date >= week_start {
            costs.week += u.cost;
        }
        costs.window += u.cost;
        costs.total_tokens += u.tokens.total();
        if u.unpriced
            && let Some(m) = &u.model
        {
            unpriced.insert(m.clone());
        }
        window.add(&project, u);
        days.entry(u.date.clone()).or_default().add(&project, u);
    }
    costs.by_host = by_host.into_iter().collect();
    by_cost_desc(&mut costs.by_host, |h| h.1, |h| &h.0);

    // Rebuild the series with one row per calendar day from `first` to today,
    // so a day without usage shows as an empty day rather than as a gap.
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
    costs.unpriced = unpriced.into_iter().collect();
    costs
}

// -------------------------------------------------------------- deduplication

/// What deduplication looks at in a record.
#[derive(Debug, Clone, Copy)]
struct Cand {
    message: u64,
    request: u64,
    session: u64,
    ts: i64,
    sidechain: bool,
    anthropic: bool,
    speed: bool,
    total: u64,
    cost: f64,
}

/// The exact key ccusage files a record under: message and request, plus the
/// session when there is no request id, plus the time when the message id is
/// not an Anthropic one either.
fn exact_key(c: &Cand) -> (u64, u64, u64, i64) {
    let (session, ts) = match (c.request, c.anthropic) {
        (0, true) => (c.session, 0),
        (0, false) => (c.session, c.ts),
        _ => (0, 0),
    };
    (c.message, c.request, session, ts)
}

/// Whether ccusage replaces the copy it kept with this one: a main-chain copy
/// beats a sidechain one, then more tokens, then a higher cost, then one that
/// names its speed.
fn replaces(new: &Cand, kept: &Cand) -> bool {
    if new.sidechain != kept.sidechain {
        return kept.sidechain;
    }
    if new.total != kept.total {
        return new.total > kept.total;
    }
    if new.cost != kept.cost {
        return new.cost > kept.cost;
    }
    new.speed && !kept.speed
}

/// A replay route: its kind, the message, and the session.
type Route = (u8, u64, u64);

/// The records ccusage would count, by index, with its rules: in reading
/// order, a record that matches one already kept, by exact key or by a
/// sidechain replaying a message of the same session, replaces it or is
/// dropped (see `replaces`). Records without a message id are all kept.
///
/// A port of ccusage's `push_deduped_daily_entry`. Route kinds: 0 indexes
/// every kept record, for sidechain candidates; 1 only sidechain ones, for
/// main-chain candidates.
fn dedupe(cands: &[Cand]) -> Vec<usize> {
    let mut kept: Vec<usize> = Vec::new();
    let mut exact: HashMap<(u64, u64, u64, i64), Vec<usize>> = HashMap::new();
    let mut routes: HashMap<Route, Vec<(usize, Option<u64>)>> = HashMap::new();
    for (i, c) in cands.iter().enumerate() {
        if c.message == 0 {
            kept.push(i);
            continue;
        }
        let key = exact_key(c);
        let by_key = exact.get(&key).and_then(|slots| {
            slots.iter().copied().find(|&s| {
                let e = &cands[kept[s]];
                e.message == c.message
                    && e.request == c.request
                    && (c.request != 0 || (e.session == c.session && (c.anthropic || e.ts == c.ts)))
            })
        });
        let found = by_key.or_else(|| {
            let kind = u8::from(!c.sidechain);
            routes.get(&(kind, c.message, c.session)).and_then(|v| {
                v.iter().find_map(|&(s, alias)| {
                    let e = &cands[kept[s]];
                    (e.message == c.message
                        && alias.unwrap_or(e.session) == c.session
                        && (c.sidechain || e.sidechain))
                        .then_some(s)
                })
            })
        });
        match found {
            Some(slot) => {
                let prev = cands[kept[slot]];
                if prev.session != c.session {
                    for session in [c.session, prev.session] {
                        for kind in [0, 1] {
                            routes
                                .entry((kind, c.message, session))
                                .or_default()
                                .push((slot, Some(session)));
                        }
                    }
                }
                if replaces(c, &prev) {
                    kept[slot] = i;
                    if exact_key(&prev) != key {
                        exact.entry(key).or_default().push(slot);
                    }
                    let moved = prev.session != c.session
                        || prev.sidechain != c.sidechain
                        || prev.message != c.message;
                    if moved {
                        routes
                            .entry((0, c.message, c.session))
                            .or_default()
                            .push((slot, None));
                        if c.sidechain {
                            routes
                                .entry((1, c.message, c.session))
                                .or_default()
                                .push((slot, None));
                        }
                    }
                }
            }
            None => {
                let slot = kept.len();
                kept.push(i);
                exact.entry(key).or_default().push(slot);
                routes
                    .entry((0, c.message, c.session))
                    .or_default()
                    .push((slot, None));
                if c.sidechain {
                    routes
                        .entry((1, c.message, c.session))
                        .or_default()
                        .push((slot, None));
                }
            }
        }
    }
    kept
}

// ------------------------------------------------------------------- sources

/// Who a root's turns belong to.
enum Attribution {
    /// A host's shared transcripts: per session, by markers.
    Owners(Owners),
    /// An isolated aimux profile's own `projects/`: all of it one account's.
    Account(String),
}

/// A `projects/` directory to read, and whose usage it holds.
struct Root {
    projects: PathBuf,
    host: String,
    who: Attribution,
}

impl Root {
    fn account(&self, session: &str, at: i64) -> String {
        match &self.who {
            Attribution::Owners(o) => o.at(session, at).to_string(),
            Attribution::Account(a) => a.clone(),
        }
    }
}

/// Every `projects/` to count, and each account's name by key. Names are this
/// machine's profile names; an account only another machine has is named as
/// that machine names it.
fn roots(hosts: &[Host]) -> (Vec<Root>, HashMap<String, String>) {
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
        out.push(Root {
            projects: host.config_dir.join("projects"),
            host: host.name.clone(),
            who: Attribution::Owners(owners),
        });
    }
    // A profile isolated with `aimux migrate isolate` keeps its transcripts in
    // a projects/ of its own instead of the shared one, so nothing above reads
    // them; every turn there is that account's.
    if let Some(host) = hosts.iter().find(|h| !h.remote) {
        for a in local.iter().filter(|a| !a.source) {
            let dir = a.config_dir.join("projects");
            if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
                out.push(Root {
                    projects: dir,
                    host: host.name.clone(),
                    who: Attribution::Account(a.key()),
                });
            }
        }
    }
    (out, names)
}

/// The session a transcript belongs to for attribution: the first part of its
/// path below the project, which is `<session>.jsonl` for a session's own
/// transcript and `<session>/` for its subagents'.
fn marker_session(rel: &Path) -> Option<String> {
    rel.components().nth(1).map(|c| {
        c.as_os_str()
            .to_string_lossy()
            .trim_end_matches(".jsonl")
            .to_string()
    })
}

/// Price, deduplicate and attribute the ledger's records.
fn uses(ledger: &ledger::Ledger, roots: &[Root], prices: &Prices) -> Vec<Use> {
    struct FileInfo<'a> {
        root: &'a Root,
        project: String,
        session: String,
        marker_session: String,
        path: String,
    }
    let info: HashMap<u32, FileInfo> = ledger
        .files
        .values()
        .filter_map(|f| {
            let root = roots.iter().find(|r| f.path.starts_with(&r.projects))?;
            let rel = f.path.strip_prefix(&root.projects).ok()?;
            let project = rel
                .components()
                .next()?
                .as_os_str()
                .to_string_lossy()
                .into_owned();
            Some((
                f.id,
                FileInfo {
                    root,
                    project,
                    session: f.session.clone(),
                    marker_session: marker_session(rel).unwrap_or_default(),
                    path: f.path.to_string_lossy().into_owned(),
                },
            ))
        })
        .collect();

    // ccusage's reading order: files by path, then lines in order.
    let mut recs: Vec<&Rec> = ledger
        .records
        .iter()
        .filter(|r| info.contains_key(&r.file))
        .collect();
    recs.sort_by(|a, b| {
        info[&a.file]
            .path
            .cmp(&info[&b.file].path)
            .then(a.off.cmp(&b.off))
    });

    // A model the list lacks is looked for across every entry, so each name is
    // looked up once, not once per record.
    let mut rates: HashMap<&str, Option<&crate::prices::Rate>> = HashMap::new();
    let priced: Vec<(f64, bool)> = recs
        .iter()
        .map(|r| {
            if let Some(c) = r.cost_usd {
                return (c, false);
            }
            let rate = r
                .model
                .as_deref()
                .and_then(|m| *rates.entry(m).or_insert_with(|| prices.find(m)));
            match rate {
                Some(rate) => (rate.cost(&r.tokens, r.is(FAST)), false),
                None => (0.0, r.tokens.total() > 0),
            }
        })
        .collect();
    let cands: Vec<Cand> = recs
        .iter()
        .zip(&priced)
        .map(|(r, (cost, _))| Cand {
            message: r.message,
            request: r.request,
            session: ledger::hash(
                r.session
                    .as_deref()
                    .unwrap_or(&info[&r.file].session)
                    .as_bytes(),
            ),
            ts: r.ts,
            sidechain: r.is(SIDECHAIN),
            anthropic: r.is(ANTHROPIC_ID),
            speed: r.is(SPEED),
            total: r.tokens.total(),
            cost: *cost,
        })
        .collect();

    dedupe(&cands)
        .into_iter()
        .filter_map(|i| {
            let r = recs[i];
            let f = &info[&r.file];
            let at = chrono::Local.timestamp_millis_opt(r.ts).single()?;
            let model = r.model.as_deref().filter(|m| *m != "<synthetic>").map(|m| {
                if r.is(FAST) {
                    format!("{m}-fast")
                } else {
                    m.to_string()
                }
            });
            Some(Use {
                host: f.root.host.clone(),
                account: f.root.account(&f.marker_session, r.ts.div_euclid(1000)),
                project: f.project.clone(),
                date: at.format("%Y-%m-%d").to_string(),
                unpriced: priced[i].1 && model.is_some(),
                model,
                cost: priced[i].0,
                tokens: r.tokens,
            })
        })
        .collect()
}

/// Gather the last `days` of cost data across every host (see `hosts`).
pub fn gather(days: i64) -> Result<Costs, String> {
    let today = chrono::Local::now().date_naive();
    // A `days`-day window includes today, so it starts `days - 1` days back;
    // starting `days` back would quietly make every "30d" figure cover 31.
    let first = today - chrono::Duration::days(days - 1);
    let window_start = first
        .and_hms_opt(0, 0, 0)
        .and_then(|t| t.and_local_timezone(chrono::Local).earliest())
        .map_or(0, |t| t.timestamp());
    let hosts = hosts();
    let (roots, names) = roots(&hosts);
    // Before the ledger, ccmoneta kept per-account copies of transcripts here.
    let _ = std::fs::remove_dir_all(store::cache_dir().join("views"));

    let prices = crate::prices::load()?;
    let dirs: Vec<PathBuf> = roots.iter().map(|r| r.projects.clone()).collect();
    // A day's margin, as ccusage takes: a transcript last written just before
    // the window can hold nothing in it, but its copies can.
    let ledger = ledger::update(&dirs, window_start - 86_400)?;
    let uses = uses(&ledger, &roots, &prices);

    let mut costs = assemble(&uses, first, today);
    let labels = account_labels(&names, uses.iter().map(|u| u.account.as_str()));
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

    fn used(host: &str, account: &str, project: &str, date: &str, cost: f64, model: &str) -> Use {
        Use {
            host: host.into(),
            account: account.into(),
            project: project.into(),
            date: date.into(),
            model: Some(model.into()),
            unpriced: false,
            cost,
            tokens: Tokens {
                input: 90,
                cache_read: 10,
                ..Default::default()
            },
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
        let uses = vec![
            used(
                "desk",
                "",
                "-home-me-code-alpha",
                "2026-09-14",
                1.0,
                "claude-opus-5",
            ),
            used(
                "desk",
                "",
                "-home-me-code-alpha",
                "2026-09-15",
                2.0,
                "claude-opus-5-5",
            ),
            used(
                "desk",
                "",
                "-home-me-code-beta",
                "2026-09-15",
                4.0,
                "claude-sonnet-5-5",
            ),
            // The same checkout under another home path merges with desk's.
            used(
                "laptop",
                "",
                "-Users-me-code-alpha",
                "2026-09-15",
                8.0,
                "claude-opus-5-5",
            ),
            // Outside the window: not counted anywhere.
            used(
                "desk",
                "",
                "-home-me-code-alpha",
                "2026-09-11",
                100.0,
                "claude-opus-5",
            ),
        ];
        let c = assemble(&uses, day("2026-09-12"), day("2026-09-15"));

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
        assert_eq!(
            c.days.keys().collect::<Vec<_>>(),
            ["2026-09-14", "2026-09-15"]
        );

        let monday = &c.days["2026-09-14"];
        assert_eq!(monday.models.len(), 1);
        assert_eq!(monday.models[0].model_name, "claude-opus-5");
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

        assert_eq!(c.projects[0].name, "code/alpha");
        assert_eq!(c.projects[0].cost, 11.0);
        assert_eq!(c.models.iter().map(|m| m.cost).sum::<f64>(), 15.0);
        assert_eq!(c.cache_read_tokens, 40);
        assert_eq!(c.total_tokens, 400);
    }

    #[test]
    fn accounts_add_up_per_day_and_over_the_window() {
        let uses = vec![
            used("desk", "main", "-p", "2026-09-14", 1.0, "claude-opus-5-5"),
            used("desk", "main", "-p", "2026-09-15", 2.0, "claude-opus-5-5"),
            used("desk", "second", "-p", "2026-09-15", 4.0, "claude-opus-5-5"),
            used(
                "laptop",
                "second",
                "-p",
                "2026-09-15",
                8.0,
                "claude-opus-5-5",
            ),
        ];
        let c = assemble(&uses, day("2026-09-14"), day("2026-09-15"));
        assert_eq!(
            c.by_account,
            vec![("second".to_string(), 12.0), ("main".to_string(), 3.0)]
        );
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
    fn unpriced_models_are_named_and_rows_without_a_model_still_count() {
        let mut free = used("desk", "", "-p", "2026-09-15", 0.0, "vendor/free-model");
        free.unpriced = true;
        let mut synthetic = used("desk", "", "-p", "2026-09-15", 0.5, "x");
        synthetic.model = None;
        let c = assemble(&[free, synthetic], day("2026-09-15"), day("2026-09-15"));
        assert_eq!(c.unpriced, ["vendor/free-model"]);
        assert!(c.models[0].unpriced);
        assert_eq!(c.models.len(), 1, "no row for a line without a model");
        assert_eq!(c.today, 0.5, "but its cost is in the totals");
    }

    fn cand(message: u64, request: u64, session: u64, total: u64) -> Cand {
        Cand {
            message,
            request,
            session,
            ts: 0,
            sidechain: false,
            anthropic: true,
            speed: true,
            total,
            cost: total as f64,
        }
    }

    #[test]
    fn a_reply_copied_into_several_transcripts_counts_once_at_its_largest() {
        // The same reply in two sessions (a resumed one copies its history),
        // the second copy with more tokens; and another reply.
        let kept = dedupe(&[cand(1, 7, 100, 5), cand(2, 8, 100, 3), cand(1, 7, 200, 9)]);
        assert_eq!(kept, [2, 1], "the larger copy takes the first one's place");
        // Equal copies: the first read stays.
        assert_eq!(dedupe(&[cand(1, 7, 100, 5), cand(1, 7, 200, 5)]), [0]);
    }

    #[test]
    fn without_a_request_id_a_reply_is_the_same_only_within_its_session() {
        let kept = dedupe(&[cand(1, 0, 100, 5), cand(1, 0, 200, 5), cand(1, 0, 100, 6)]);
        assert_eq!(kept, [2, 1]);
        // And for a non-Anthropic id, only at the same moment.
        let mut a = cand(1, 0, 100, 5);
        a.anthropic = false;
        let mut b = a;
        b.ts = 1;
        assert_eq!(dedupe(&[a, b]), [0, 1]);
        assert_eq!(dedupe(&[a, a]), [0]);
    }

    #[test]
    fn a_sidechain_replaying_a_message_is_not_counted_again() {
        let main = cand(1, 7, 100, 5);
        let mut replay = cand(1, 8, 100, 9);
        replay.sidechain = true;
        // A new request id, so no exact match: the replay route catches it, and
        // the main-chain copy is kept although the replay has more tokens.
        assert_eq!(dedupe(&[main, replay]), [0]);
        assert_eq!(dedupe(&[replay, main]), [1]);
        // Without the sidechain flag both count.
        let mut other = replay;
        other.sidechain = false;
        assert_eq!(dedupe(&[main, other]), [0, 1]);
        // Nor does a record without a message id ever merge.
        assert_eq!(dedupe(&[cand(0, 7, 100, 5), cand(0, 7, 100, 5)]), [0, 1]);
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
    fn the_old_cost_json_format_reads_as_no_snapshot() {
        let old = r#"{"today": 18.02, "captured_at": 1789000000}"#;
        assert!(serde_json::from_str::<CostSnapshot>(old).is_err());
    }
}
