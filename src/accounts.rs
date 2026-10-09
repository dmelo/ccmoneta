//! Claude accounts on this machine, as aimux sets them up.
//!
//! [aimux](https://github.com/Digital-Threads/aimux) runs several Claude
//! subscriptions side by side: each profile is a Claude config directory of its
//! own (`CLAUDE_CONFIG_DIR`), with a private login, while transcripts stay shared
//! in the source profile's `projects/`. Limits belong to a login, so they are
//! kept per account; spend is split per account by the markers each session
//! leaves in the config directory that ran it (see `Owners`).
//!
//! An account is identified by the `accountUuid` its config directory's
//! `.claude.json` records, not by its profile name: the same subscription can be
//! `secondary` on one machine and `personal` on another. It is labelled with the
//! profile name used on this machine.
//!
//! Without aimux there is exactly one account, unnamed, at `~/.claude`, and
//! everything behaves as it did before accounts existed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::store;

#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    /// The aimux profile name; empty when aimux is not in use.
    pub name: String,
    /// `oauthAccount.accountUuid`, when the config directory records one.
    pub uuid: Option<String>,
    pub config_dir: PathBuf,
    /// Further profiles logged into the same account, folded into this one.
    /// Their markers count for it like its own.
    pub also: Vec<PathBuf>,
    /// The subscription, as Claude names it: "Max 20x", "Max 5x", "Pro"...
    pub plan: Option<String>,
    /// aimux's source profile, `~/.claude`: the account turns go to when no
    /// marker says otherwise.
    pub source: bool,
}

impl Account {
    /// What identifies this account in caches and across machines.
    pub fn key(&self) -> String {
        self.uuid
            .clone()
            .unwrap_or_else(|| format!("profile:{}", self.name))
    }

    /// The account key as a file name.
    pub fn file_name(&self) -> String {
        file_name(&self.key())
    }

    /// Its name with its plan, as the limits show it: `main (Max 20x)`.
    pub fn titled(&self) -> String {
        match &self.plan {
            Some(plan) if !self.name.is_empty() => format!("{} ({plan})", self.name),
            _ => self.name.clone(),
        }
    }
}

/// A key as a file name: letters, digits and '-' kept, anything else '_'.
pub fn file_name(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `CCMONETA_AIMUX_DIR` replaces `~/.aimux`, for the tests.
pub fn aimux_dir() -> PathBuf {
    std::env::var_os("CCMONETA_AIMUX_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| store::home().join(".aimux"))
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => store::home().join(rest),
        None if path == "~" => store::home(),
        None => PathBuf::from(path),
    }
}

fn unquote(v: &str) -> &str {
    let v = v.trim();
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(v)
}

/// The Claude profiles in aimux's `config.yaml`: (name, path, is_source), in
/// file order.
///
/// Only the `profiles:` block is read, and only the shape aimux writes: a
/// two-space-indented name per profile with its keys four spaces in. That keeps
/// a YAML library out of the build; a file this does not understand yields no
/// profiles, which means "no aimux", and doctor says so.
pub fn parse_profiles(yaml: &str) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    let mut in_profiles = false;
    let mut current: Option<(String, Option<String>, bool, String)> = None;
    let finish = |p: Option<(String, Option<String>, bool, String)>,
                  out: &mut Vec<(String, String, bool)>| {
        if let Some((name, Some(path), source, cli)) = p
            && cli == "claude"
        {
            out.push((name, path, source));
        }
    };
    for line in yaml.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let text = line.trim();
        if indent == 0 {
            if in_profiles {
                finish(current.take(), &mut out);
            }
            in_profiles = text == "profiles:";
            continue;
        }
        if !in_profiles {
            continue;
        }
        if indent == 2 {
            finish(current.take(), &mut out);
            if let Some(name) = text.strip_suffix(':') {
                current = Some((unquote(name).to_string(), None, false, "claude".into()));
            }
        } else if indent == 4
            && let Some(p) = current.as_mut()
            && let Some((key, value)) = text.split_once(':')
        {
            let value = unquote(value);
            match key.trim() {
                "path" => p.1 = Some(value.to_string()),
                "is_source" => p.2 = value == "true",
                "cli" => p.3 = value.to_string(),
                _ => {}
            }
        }
    }
    if in_profiles {
        finish(current.take(), &mut out);
    }
    out
}

/// The `oauthAccount` object Claude Code records for a config directory. It
/// keeps `.claude.json` inside `CLAUDE_CONFIG_DIR` when that is set, and in the
/// home directory for the default `~/.claude`.
fn oauth_account(config_dir: &Path) -> Option<serde_json::Value> {
    let mut candidates = vec![config_dir.join(".claude.json")];
    if config_dir == store::home().join(".claude") {
        candidates.push(store::home().join(".claude.json"));
    }
    candidates.iter().find_map(|f| {
        let raw = std::fs::read(f).ok()?;
        let mut v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
        let account = v.get_mut("oauthAccount")?.take();
        account.is_object().then_some(account)
    })
}

/// `oauthAccount.accountUuid` for a config directory.
pub fn account_uuid(config_dir: &Path) -> Option<String> {
    identity(config_dir).0
}

/// The subscription an `oauthAccount` records, as Claude names it.
///
/// `organizationType` names the plan (`claude_max`, `claude_pro`, ...) and
/// `organizationRateLimitTier` its size (`default_claude_max_20x`). The size is
/// read only from a tier that ends in `_<n>x`; anything else is shown by its
/// plan name alone, and an account with no plan type shows none.
pub fn plan_label(account: &serde_json::Value) -> Option<String> {
    let kind = account["organizationType"]
        .as_str()?
        .strip_prefix("claude_")?;
    let mut chars = kind.chars();
    let mut label: String = chars.next()?.to_uppercase().chain(chars).collect();
    let size = account["organizationRateLimitTier"]
        .as_str()
        .and_then(|t| t.rsplit('_').next())
        .filter(|s| {
            s.len() > 1 && s.ends_with('x') && s[..s.len() - 1].bytes().all(|b| b.is_ascii_digit())
        });
    if let Some(size) = size {
        label.push(' ');
        label.push_str(size);
    }
    Some(label)
}

/// An account's uuid and plan, from one read of its record.
fn identity(config_dir: &Path) -> (Option<String>, Option<String>) {
    let Some(account) = oauth_account(config_dir) else {
        return (None, None);
    };
    let uuid = account["accountUuid"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    (uuid, plan_label(&account))
}

/// Every account on this machine, source first. Profiles logged into the same
/// account count once, under the first profile's name.
pub fn local() -> Vec<Account> {
    let profiles = std::fs::read_to_string(aimux_dir().join("config.yaml"))
        .map(|y| parse_profiles(&y))
        .unwrap_or_default();
    if profiles.is_empty() {
        let dir = store::home().join(".claude");
        let (uuid, plan) = identity(&dir);
        return vec![Account {
            name: String::new(),
            uuid,
            plan,
            config_dir: dir,
            also: Vec::new(),
            source: true,
        }];
    }
    let mut out: Vec<Account> = Vec::new();
    for (name, path, source) in profiles {
        let dir = expand_home(&path);
        let (uuid, plan) = identity(&dir);
        // A second profile on the same account: its sessions are that
        // account's, and if it is the source, so is the account.
        if let Some(first) = out.iter_mut().find(|a| uuid.is_some() && a.uuid == uuid) {
            first.also.push(dir);
            first.source |= source;
            continue;
        }
        out.push(Account {
            name,
            uuid,
            plan,
            config_dir: dir,
            also: Vec::new(),
            source,
        });
    }
    out.sort_by_key(|a| !a.source);
    out
}

/// Whether more than one account is in use, which is when names are shown.
pub fn several(accounts: &[Account]) -> bool {
    accounts.len() > 1
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The account a Claude Code process belongs to, from its `CLAUDE_CONFIG_DIR`
/// (unset meaning `~/.claude`). The status line hook inherits it.
pub fn current(accounts: &[Account]) -> Option<&Account> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| store::home().join(".claude"));
    accounts
        .iter()
        .find(|a| same_dir(&a.config_dir, &dir))
        .or_else(|| {
            // A profile logged into the same account as one listed earlier is
            // folded into that one, so match it by its uuid instead.
            let uuid = account_uuid(&dir)?;
            accounts.iter().find(|a| a.uuid.as_deref() == Some(&uuid))
        })
}

/// When a file came into being: its birth time where the filesystem records
/// one, else its modification time. For a directory that only ever moves the
/// time later, so a turn is then credited less, never wrongly, to its account.
fn born(meta: &std::fs::Metadata) -> Option<i64> {
    let t = meta.created().or_else(|_| meta.modified()).ok()?;
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

/// The sessions a config directory has run: Claude Code creates
/// `session-env/<session id>/` in the `CLAUDE_CONFIG_DIR` of every session it
/// starts or resumes there. A symlinked `session-env` (aimux before 0.27.0
/// shared one between profiles) says nothing about who ran what, so it counts
/// as none.
pub fn markers(config_dir: &Path) -> Vec<(String, i64)> {
    let dir = config_dir.join("session-env");
    if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let at = born(&e.metadata().ok()?)?;
            Some((e.file_name().to_string_lossy().into_owned(), at))
        })
        .collect()
}

/// Which account ran each turn of a session.
///
/// The rule is aimux's own: a turn belongs to the account whose marker for its
/// session is the newest one not after the turn, since a marker appears when an
/// account first opens the session. A turn with no such marker, which is every
/// turn from before aimux and any session whose marker Claude Code has since
/// cleaned up, goes to the source account. One marker per account cannot date a
/// second visit, so a session that goes A, B, then A again stays with B.
#[derive(Debug, Default, Clone)]
pub struct Owners {
    /// Session id -> (born, account key), oldest first.
    sessions: HashMap<String, Vec<(i64, String)>>,
    pub default: String,
}

impl Owners {
    pub fn new(default: String) -> Self {
        Owners {
            sessions: HashMap::new(),
            default,
        }
    }

    pub fn add(&mut self, session: &str, born: i64, account: &str) {
        let list = self.sessions.entry(session.to_string()).or_default();
        list.push((born, account.to_string()));
        list.sort();
    }

    /// The account that ran `session` at epoch second `at`.
    pub fn at(&self, session: &str, at: i64) -> &str {
        let mut owner = self.default.as_str();
        for (born, account) in self.sessions.get(session).into_iter().flatten() {
            if *born > at {
                break;
            }
            owner = account;
        }
        owner
    }

    /// Whether every turn of `session` goes to the default account, which a
    /// session no other account has a marker for always does.
    pub fn only_default(&self, session: &str) -> bool {
        self.sessions
            .get(session)
            .is_none_or(|l| l.iter().all(|(_, a)| *a == self.default))
    }

    /// Every account a turn could go to.
    pub fn accounts(&self) -> Vec<String> {
        let mut all: Vec<String> = self
            .sessions
            .values()
            .flatten()
            .map(|(_, a)| a.clone())
            .chain(std::iter::once(self.default.clone()))
            .collect();
        all.sort();
        all.dedup();
        all
    }
}

/// This machine's owners, from every account's markers.
pub fn local_owners(accounts: &[Account]) -> Owners {
    let default = accounts
        .iter()
        .find(|a| a.source)
        .or(accounts.first())
        .map(Account::key)
        .unwrap_or_default();
    let mut owners = Owners::new(default);
    for a in accounts {
        let key = a.key();
        for dir in std::iter::once(&a.config_dir).chain(&a.also) {
            for (session, born) in markers(dir) {
                owners.add(&session, born, &key);
            }
        }
    }
    owners
}

/// The shell command the sync job runs on another machine to list its
/// accounts and markers, read back by `parse_remote_markers`. It prints, per
/// config directory, one `@` line (`@\t<uuid>\t<profile>\t<1 if source>`) and
/// then one `<session>\t<born>` line per marker.
///
/// The uuid is the first `accountUuid` at or after `"oauthAccount"`, the
/// object the local side reads it from; a file without one names no uuid.
///
/// Only names and times leave the machine. The profile directories are found by
/// aimux's default layout, `~/.aimux/profiles/<name>`. `stat` is asked which
/// flavour it is first: GNU's `-f` means "filesystem status", so trying the BSD
/// form and falling back prints several lines of filesystem details per marker
/// on Linux. The birth time is `%W` (GNU) or `%B` (BSD); where it is 0 or
/// missing the modification time stands in.
pub const REMOTE_MARKERS: &str = r#"if stat -c %Y . >/dev/null 2>&1; then gnu=1; else gnu=0; fi
for d in "$HOME/.claude" "$HOME"/.aimux/profiles/*; do
  [ -d "$d" ] || continue
  if [ "$d" = "$HOME/.claude" ]; then n=main; s=1; j="$d/.claude.json"; [ -f "$j" ] || j="$HOME/.claude.json"
  else n=$(basename "$d"); s=0; j="$d/.claude.json"; fi
  u=$(tr -d '\n' < "$j" 2>/dev/null | grep -o '"oauthAccount": *{.*' | grep -o '"accountUuid": *"[^"]*"' | head -n 1 | sed 's/.*"\([^"]*\)"$/\1/')
  printf '@\t%s\t%s\t%s\n' "$u" "$n" "$s"
  [ -d "$d/session-env" ] && [ ! -L "$d/session-env" ] || continue
  (cd "$d/session-env" && for m in *; do
    [ -e "$m" ] || continue
    if [ "$gnu" = 1 ]; then b=$(stat -c %W "$m" 2>/dev/null); else b=$(stat -f %B "$m" 2>/dev/null); fi
    case "$b" in ''|0|-*) if [ "$gnu" = 1 ]; then b=$(stat -c %Y "$m" 2>/dev/null); else b=$(stat -f %m "$m" 2>/dev/null); fi;; esac
    printf '%s\t%s\n' "$m" "$b"
  done)
done"#;

/// A remote account, as `REMOTE_MARKERS` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteAccount {
    pub key: String,
    pub name: String,
    pub source: bool,
}

/// Read `REMOTE_MARKERS` output into the remote machine's accounts and owners.
/// Lines that do not fit are skipped; a session id is used only as a lookup
/// key, so nothing from the remote is ever treated as a path.
pub fn parse_remote_markers(text: &str) -> (Vec<RemoteAccount>, Owners) {
    let mut accounts: Vec<RemoteAccount> = Vec::new();
    let mut marks: Vec<(String, i64, String)> = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["@", uuid, name, source] => {
                let key = if uuid.is_empty() {
                    format!("profile:{name}")
                } else {
                    uuid.to_string()
                };
                if !accounts.iter().any(|a| a.key == key) {
                    accounts.push(RemoteAccount {
                        key: key.clone(),
                        name: name.to_string(),
                        source: *source == "1",
                    });
                }
                current = Some(key);
            }
            [session, born] => {
                if let (Some(key), Ok(born)) = (&current, born.parse::<i64>()) {
                    marks.push((session.to_string(), born, key.clone()));
                }
            }
            _ => {}
        }
    }
    let default = accounts
        .iter()
        .find(|a| a.source)
        .or(accounts.first())
        .map(|a| a.key.clone())
        .unwrap_or_default();
    let mut owners = Owners::new(default);
    for (session, born, key) in marks {
        owners.add(&session, born, &key);
    }
    (accounts, owners)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "version: 1
shared_source: /home/me/.claude
profiles:
  main:
    cli: claude
    path: /home/me/.claude
    is_source: true
  secondary:
    cli: claude
    path: ~/.aimux/profiles/secondary
  coder:
    cli: codex
    path: ~/.aimux/profiles/coder
private:
  - .credentials.json
";

    #[test]
    fn claude_profiles_are_read_from_the_config() {
        assert_eq!(
            parse_profiles(CONFIG),
            vec![
                ("main".to_string(), "/home/me/.claude".to_string(), true),
                (
                    "secondary".to_string(),
                    "~/.aimux/profiles/secondary".to_string(),
                    false
                ),
            ]
        );
        assert!(parse_profiles("version: 1\n").is_empty());
        assert!(parse_profiles("not yaml at all").is_empty());
    }

    #[test]
    fn plans_are_named_from_the_account_record() {
        let plan = |kind: &str, tier: &str| {
            plan_label(&serde_json::json!({
                "organizationType": kind,
                "organizationRateLimitTier": tier,
            }))
        };
        assert_eq!(
            plan("claude_max", "default_claude_max_20x").as_deref(),
            Some("Max 20x")
        );
        assert_eq!(
            plan("claude_max", "default_claude_max_5x").as_deref(),
            Some("Max 5x")
        );
        assert_eq!(
            plan("claude_pro", "default_claude_ai").as_deref(),
            Some("Pro")
        );
        assert_eq!(plan("claude_team", "").as_deref(), Some("Team"));
        assert_eq!(plan_label(&serde_json::json!({})), None);
        assert_eq!(plan("something_else", "x").as_deref(), None);
    }

    #[test]
    fn a_turn_belongs_to_the_newest_marker_not_after_it() {
        let mut o = Owners::new("main".into());
        o.add("s1", 100, "second");
        o.add("s2", 50, "main");
        o.add("s2", 200, "second");
        assert_eq!(o.at("s1", 99), "main", "before the marker: default");
        assert_eq!(o.at("s1", 100), "second");
        assert_eq!(o.at("s2", 150), "main");
        assert_eq!(o.at("s2", 250), "second", "handed over mid-session");
        assert_eq!(o.at("unknown", 0), "main");
        assert!(!o.only_default("s1"));
        assert!(o.only_default("unknown"));
        assert_eq!(o.accounts(), ["main", "second"]);
    }

    #[test]
    fn remote_markers_parse_and_ignore_junk() {
        let text =
            "@\tuuid-a\tmain\t1\ns1\t100\nnoise\n@\tuuid-b\tpersonal\t0\ns1\t200\ns2\tnot-a-time\n";
        let (accounts, owners) = parse_remote_markers(text);
        assert_eq!(
            accounts,
            vec![
                RemoteAccount {
                    key: "uuid-a".into(),
                    name: "main".into(),
                    source: true
                },
                RemoteAccount {
                    key: "uuid-b".into(),
                    name: "personal".into(),
                    source: false
                },
            ]
        );
        assert_eq!(owners.default, "uuid-a");
        assert_eq!(owners.at("s1", 150), "uuid-a");
        assert_eq!(owners.at("s1", 250), "uuid-b");
        assert_eq!(owners.at("s2", 250), "uuid-a");
    }
}
