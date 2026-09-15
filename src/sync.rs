//! Mirroring other machines' transcripts, so their spend is counted here.
//!
//! For each host: list the transcripts modified in the last `sync.days` days
//! over ssh, fetch them with `rsync --files-from`, then delete local copies no
//! longer in that list. The copy lives in `<cache>/hosts/<name>/`, where cost.rs
//! finds it. See docs/design.md.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{self, Config, HostConfig};
use crate::store::{self, Job};

const DEFAULT_REMOTE_DIR: &str = "~/.claude/projects";

/// BatchMode makes ssh fail instead of prompting, since a background job could
/// never answer a prompt; ConnectTimeout bounds how long an unreachable host
/// holds the job up.
const SSH_OPTS: [&str; 4] = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"];

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub name: String,
    pub ssh: String,
    pub remote_dir: String,
}

impl Target {
    fn from_config(h: &HostConfig) -> Self {
        Target {
            name: h.name.clone(),
            ssh: h.ssh.clone().unwrap_or_else(|| h.name.clone()),
            remote_dir: h
                .remote_dir
                .clone()
                .unwrap_or_else(|| DEFAULT_REMOTE_DIR.into()),
        }
    }
}

pub fn targets(cfg: &Config) -> Vec<Target> {
    cfg.sync.hosts.iter().map(Target::from_config).collect()
}

/// A configured host by name, or else an ad-hoc one whose name is also its ssh
/// destination.
pub fn target_named(cfg: &Config, name: &str) -> Result<Target, String> {
    if let Some(h) = cfg.sync.hosts.iter().find(|h| h.name == name) {
        return Ok(Target::from_config(h));
    }
    if !config::valid_host_name(name) {
        return Err(format!(
            "{name:?} is not a usable host name (letters, digits, '.', '_' and '-'; not starting with '.' or '-')"
        ));
    }
    Ok(Target {
        name: name.into(),
        ssh: name.into(),
        remote_dir: DEFAULT_REMOTE_DIR.into(),
    })
}

fn host_dir(name: &str) -> PathBuf {
    store::cache_dir().join("hosts").join(name)
}

fn read_epoch(path: &Path) -> Option<i64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// A host's last successful sync and its last recorded error.
pub fn host_status(name: &str) -> (Option<i64>, Option<String>) {
    let dir = host_dir(name);
    let error = fs::read_to_string(dir.join("last-error"))
        .ok()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty());
    (read_epoch(&dir.join("last-sync")), error)
}

/// Whether ssh reaches the host without prompting, as the sync job needs.
pub fn check_reachable(t: &Target) -> Result<(), String> {
    let out = Command::new(ssh_program())
        .args(SSH_OPTS)
        .arg(&t.ssh)
        .arg("true")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running ssh: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{}{}", out.status, last_line(&out.stderr)))
    }
}

/// A host is due when its last attempt, successful or not, is older than
/// `refresh_seconds`. Counting attempts rather than successes stops an
/// unreachable host from being retried on every render.
pub fn is_due(last_attempt: Option<i64>, now: i64, refresh_seconds: i64) -> bool {
    last_attempt.is_none_or(|t| now - t >= refresh_seconds)
}

pub fn due_hosts(cfg: &Config, now: i64) -> Vec<Target> {
    targets(cfg)
        .into_iter()
        .filter(|t| {
            let attempt = read_epoch(&host_dir(&t.name).join("last-attempt"));
            is_due(attempt, now, cfg.sync.refresh_seconds)
        })
        .collect()
}

/// The remote `find` output reduced to plain relative paths, sorted and unique.
///
/// These names are handed to rsync and compared with local files before
/// anything is deleted, so only `./`-relative paths with no `..` component are
/// kept, whatever the remote printed.
pub fn filter_listing(text: &str) -> Vec<String> {
    let kept: BTreeSet<String> = text
        .lines()
        .filter(|l| l.starts_with("./"))
        .filter(|l| !l.split('/').any(|c| c == ".."))
        .map(String::from)
        .collect();
    kept.into_iter().collect()
}

/// Local copies absent from the remote list. The names come from the local
/// walk, so nothing outside the copy can be named.
pub fn stale_files(local: &[String], remote: &[String]) -> Vec<String> {
    let remote: BTreeSet<&str> = remote.iter().map(String::as_str).collect();
    local
        .iter()
        .filter(|p| !remote.contains(p.as_str()))
        .cloned()
        .collect()
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The directory for the remote shell to `cd` into. A leading `~/` has to stay
/// unquoted to be expanded; the rest is quoted.
pub fn remote_cd(dir: &str) -> String {
    if dir == "~" {
        return "~".into();
    }
    match dir.strip_prefix("~/") {
        Some(rest) => format!("~/{}", sh_quote(rest)),
        None => sh_quote(dir),
    }
}

/// rsync's source argument. rsync resolves a relative remote path from the
/// remote home, so `~/x` is passed as `x`.
pub fn rsync_source(t: &Target) -> String {
    let path = if t.remote_dir == "~" {
        "."
    } else {
        t.remote_dir.strip_prefix("~/").unwrap_or(&t.remote_dir)
    };
    format!("{}:{}/", t.ssh, path.trim_end_matches('/'))
}

/// `CCMONETA_SSH` replaces the ssh executable, for the listing and for rsync's
/// transport alike. The integration tests use it to reach a fake remote.
fn ssh_program() -> OsString {
    std::env::var_os("CCMONETA_SSH")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ssh".into())
}

fn last_line(bytes: &[u8]) -> String {
    match String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
    {
        Some(l) => format!(": {}", l.trim()),
        None => String::new(),
    }
}

/// Every `*.jsonl` under `root`, as `./relative/path`, not following symlinks.
fn local_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(kind) = e.file_type() else {
                continue;
            };
            let path = e.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file()
                && path.extension().is_some_and(|x| x == "jsonl")
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push(format!("./{}", rel.to_string_lossy()));
            }
        }
    }
    out
}

/// Remove directories left empty by pruning, deepest first, keeping `root`.
fn remove_empty_dirs(root: &Path) {
    let mut dirs = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.push(e.path());
                stack.push(e.path());
            }
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for dir in dirs {
        // Fails, harmlessly, on any directory that is not empty.
        let _ = fs::remove_dir(&dir);
    }
}

pub struct Report {
    pub files: usize,
    pub pruned: usize,
}

/// Sync one host, recording the attempt, and then either `last-sync` or
/// `last-error`. A failure deletes nothing and leaves `last-sync` as it was;
/// files rsync finished before failing stay updated.
pub fn sync_target(t: &Target, days: i64) -> Result<Report, String> {
    let dir = host_dir(&t.name);
    let dest = dir.join("projects");
    fs::create_dir_all(&dest).map_err(|e| format!("creating {}: {e}", dest.display()))?;
    let _ = store::write_text(&dir.join("last-attempt"), &store::now().to_string());
    let result = fetch_and_prune(t, days, &dir, &dest);
    match &result {
        Ok(_) => {
            let _ = store::write_text(&dir.join("last-sync"), &store::now().to_string());
            let _ = fs::remove_file(dir.join("last-error"));
        }
        Err(e) => {
            let _ = store::write_text(&dir.join("last-error"), e);
        }
    }
    result
}

fn fetch_and_prune(t: &Target, days: i64, dir: &Path, dest: &Path) -> Result<Report, String> {
    let listing = Command::new(ssh_program())
        .args(SSH_OPTS)
        .arg(&t.ssh)
        .arg(format!(
            "cd {} && find . -name '*.jsonl' -mtime -{days}",
            remote_cd(&t.remote_dir)
        ))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running ssh: {e}"))?;
    if !listing.status.success() {
        return Err(format!(
            "listing transcripts on {} failed ({}){}",
            t.ssh,
            listing.status,
            last_line(&listing.stderr)
        ));
    }
    let remote = filter_listing(&String::from_utf8_lossy(&listing.stdout));

    let list = dir.join(format!(".files.{}", std::process::id()));
    let body: String = remote.iter().map(|p| format!("{p}\n")).collect();
    fs::write(&list, body).map_err(|e| format!("writing {}: {e}", list.display()))?;
    let rsh = std::iter::once(ssh_program().to_string_lossy().into_owned())
        .chain(SSH_OPTS.iter().map(|s| s.to_string()))
        .collect::<Vec<_>>()
        .join(" ");
    let fetched = Command::new("rsync")
        .arg("-a")
        .arg(format!("--files-from={}", list.display()))
        .arg("-e")
        .arg(&rsh)
        .arg(rsync_source(t))
        .arg(format!("{}/", dest.display()))
        .stdin(Stdio::null())
        .output();
    let _ = fs::remove_file(&list);
    let fetched = fetched.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "rsync not found: install rsync".to_string()
        } else {
            format!("running rsync: {e}")
        }
    })?;
    if !fetched.status.success() {
        return Err(format!(
            "rsync from {} failed ({}){}",
            t.ssh,
            fetched.status,
            last_line(&fetched.stderr)
        ));
    }

    // An empty list is ambiguous: the host may have no recent sessions, or
    // something upstream went wrong. Deleting on it would wipe the whole copy,
    // so an empty list only skips pruning.
    let mut pruned = 0;
    if !remote.is_empty() {
        for stale in stale_files(&local_files(dest), &remote) {
            if fs::remove_file(dest.join(&stale)).is_ok() {
                pruned += 1;
            }
        }
        remove_empty_dirs(dest);
    }
    Ok(Report {
        files: remote.len(),
        pruned,
    })
}

/// The sync job: sync every due host, or every host when forced. It fails only
/// if every attempted host failed, so one unreachable host does not put the
/// others into backoff; each host records its own error.
pub fn run_due(cfg: &Config, force: bool) -> Result<(), String> {
    let now = store::now();
    let hosts = if force {
        targets(cfg)
    } else {
        due_hosts(cfg, now)
    };
    let mut failures = Vec::new();
    for t in &hosts {
        match sync_target(t, cfg.sync.days) {
            Ok(r) => store::log(&format!(
                "sync {}: {} files in window, pruned {}",
                t.name, r.files, r.pruned
            )),
            Err(e) => failures.push(format!("{}: {e}", t.name)),
        }
    }
    if !hosts.is_empty() && failures.len() == hosts.len() {
        return Err(failures.join("; "));
    }
    for f in &failures {
        store::log(&format!("sync {f}"));
    }
    Ok(())
}

/// `ccmoneta sync [host...]`: sync now, in the foreground, and report. A named
/// host missing from the config is synced ad hoc, with its name as the ssh
/// destination, which is what lets the systemd unit work without a config file.
pub fn run_cli(cfg: &Config, names: &[String]) -> i32 {
    let targets = if names.is_empty() {
        targets(cfg)
    } else {
        match names
            .iter()
            .map(|n| target_named(cfg, n))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(t) => t,
            Err(e) => {
                eprintln!("ccmoneta sync: {e}");
                return 2;
            }
        }
    };
    if targets.is_empty() {
        eprintln!(
            "ccmoneta sync: no hosts configured; add [[sync.hosts]] to {}, or name a host",
            config::path().display()
        );
        return 2;
    }
    let _lock = match store::try_lock(Job::Sync) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            println!("ccmoneta sync: a sync is already running");
            return 0;
        }
        Err(e) => {
            eprintln!("ccmoneta sync: {e}");
            return 1;
        }
    };
    let mut failed = false;
    for t in &targets {
        match sync_target(t, cfg.sync.days) {
            Ok(r) => println!(
                "ccmoneta sync: {}: {} files in window, pruned {}",
                t.name, r.files, r.pruned
            ),
            Err(e) => {
                eprintln!("ccmoneta sync: {}: {e}", t.name);
                failed = true;
            }
        }
    }
    i32::from(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(ssh: &str, dir: &str) -> Target {
        Target {
            name: "h".into(),
            ssh: ssh.into(),
            remote_dir: dir.into(),
        }
    }

    #[test]
    fn listing_keeps_only_plain_relative_paths() {
        let text = "./a/1.jsonl\n./b/c/2.jsonl\n../escape.jsonl\n./x/../../y.jsonl\n/etc/passwd\nnoise\n./a/1.jsonl\n";
        assert_eq!(filter_listing(text), vec!["./a/1.jsonl", "./b/c/2.jsonl"]);
        assert!(filter_listing("").is_empty());
    }

    #[test]
    fn stale_files_are_local_ones_missing_remotely() {
        let local = vec!["./a.jsonl".to_string(), "./gone.jsonl".to_string()];
        let remote = vec!["./a.jsonl".to_string(), "./new.jsonl".to_string()];
        assert_eq!(stale_files(&local, &remote), vec!["./gone.jsonl"]);
    }

    #[test]
    fn remote_paths_keep_the_home_expandable_and_quote_the_rest() {
        assert_eq!(remote_cd("~/.claude/projects"), "~/'.claude/projects'");
        assert_eq!(remote_cd("/srv/claude"), "'/srv/claude'");
        assert_eq!(remote_cd("~"), "~");
        assert_eq!(
            rsync_source(&target("me@box", "~/.claude/projects")),
            "me@box:.claude/projects/"
        );
        assert_eq!(
            rsync_source(&target("box", "/srv/claude/")),
            "box:/srv/claude/"
        );
        assert_eq!(rsync_source(&target("box", "~")), "box:./");
    }

    #[test]
    fn a_host_is_due_by_its_last_attempt() {
        assert!(is_due(None, 1000, 600));
        assert!(!is_due(Some(500), 1000, 600));
        assert!(is_due(Some(400), 1000, 600));
    }

    #[test]
    fn named_hosts_come_from_the_config_or_are_ad_hoc() {
        let cfg = config::parse("[[sync.hosts]]\nname = \"laptop\"\nssh = \"me@laptop.local\"\n")
            .unwrap();
        assert_eq!(target_named(&cfg, "laptop").unwrap().ssh, "me@laptop.local");
        let ad_hoc = target_named(&cfg, "desk").unwrap();
        assert_eq!(
            (ad_hoc.ssh.as_str(), ad_hoc.remote_dir.as_str()),
            ("desk", "~/.claude/projects")
        );
        assert!(target_named(&cfg, "../evil").is_err());
        assert!(target_named(&cfg, "-oProxyCommand=x").is_err());
    }
}
