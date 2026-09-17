//! The shared cache that every surface reads and every refresh job writes.
//!
//! Layout under `$XDG_CACHE_HOME/ccmoneta/` (default `~/.cache/ccmoneta/`):
//!
//!   limits.json          the latest limit windows, from the hook or the endpoint
//!   cost.json            the full cost data set, with when and for which day
//!   health.json          Claude's service status and the installed Claude Code version
//!   hosts/<name>/        another machine's mirrored transcripts, with its
//!                        last-sync, last-attempt and last-error
//!   jobs/<job>.json      a job's attempts, failures and backoff; written by that job
//!   jobs/<job>.spawned   when a surface last started that job; written by surfaces
//!   locks/<job>.lock     held while a job runs, so each job runs once at a time
//!   refresh.log          one line per job run

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// A duration in words, for "synced 3h 04m ago" and "resets in 2d 1h". Lives
/// here with `now()` because the bar, the dashboard, doctor and the health
/// check all render it and each used to carry its own copy.
pub fn ago(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// This executable, for starting it again.
///
/// Once the binary has been replaced, by an upgrade for instance, Linux reports
/// the running process's executable as the old path with " (deleted)" appended,
/// and that path cannot be started. The file now at the original path is the
/// upgraded build, which is what a long-running dashboard should start instead.
pub fn self_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(usable_exe(exe, |p| p.exists()))
}

pub fn usable_exe(exe: PathBuf, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let text = exe.to_string_lossy();
    match text.strip_suffix(" (deleted)") {
        Some(original) if exists(Path::new(original)) => PathBuf::from(original),
        _ => exe,
    }
}

pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache"))
        .join("ccmoneta")
}

/// Write through a per-process temp file and a rename, so a reader never sees a
/// half-written file and two concurrent writers never share a temp file.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    write_bytes(path, &serde_json::to_vec_pretty(value)?)
}

pub fn write_text(path: &Path, text: &str) -> std::io::Result<()> {
    write_bytes(path, text.as_bytes())
}

fn write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Cost,
    Limits,
    Sync,
    Health,
}

impl Job {
    pub fn name(self) -> &'static str {
        match self {
            Job::Cost => "cost",
            Job::Limits => "limits",
            Job::Sync => "sync",
            Job::Health => "health",
        }
    }

    pub fn parse(s: &str) -> Option<Job> {
        match s {
            "cost" => Some(Job::Cost),
            "limits" => Some(Job::Limits),
            "sync" => Some(Job::Sync),
            "health" => Some(Job::Health),
            _ => None,
        }
    }
}

/// Held for as long as a job runs. The lock belongs to the open file, so it is
/// released when this is dropped or when the process exits.
pub struct Lock {
    _file: File,
}

/// `Ok(None)` when another process already holds the job's lock.
pub fn try_lock(job: Job) -> std::io::Result<Option<Lock>> {
    let path = cache_dir()
        .join("locks")
        .join(format!("{}.lock", job.name()));
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(Lock { _file: file })),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Whether a run of `job` holds its lock right now. An unusable lock directory
/// counts as not running, so it cannot stop refreshes from ever being started.
pub fn is_running(job: Job) -> bool {
    matches!(try_lock(job), Ok(None))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct JobState {
    pub last_attempt: Option<i64>,
    pub last_success: Option<i64>,
    /// Consecutive failures since the last success.
    pub failures: u32,
    /// No new attempt before this time; set after a failure.
    pub next_allowed: Option<i64>,
    pub last_error: Option<String>,
}

impl JobState {
    pub fn allows(&self, now: i64) -> bool {
        self.next_allowed.is_none_or(|t| now >= t)
    }
}

fn job_file(job: Job, ext: &str) -> PathBuf {
    cache_dir()
        .join("jobs")
        .join(format!("{}.{ext}", job.name()))
}

pub fn job_state(job: Job) -> JobState {
    read_json(&job_file(job, "json")).unwrap_or_default()
}

pub fn save_job_state(job: Job, state: &JobState) {
    let _ = write_json(&job_file(job, "json"), state);
}

pub fn spawned_at(job: Job) -> Option<i64> {
    fs::read_to_string(job_file(job, "spawned"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn mark_spawned(job: Job, at: i64) {
    let _ = write_bytes(&job_file(job, "spawned"), at.to_string().as_bytes());
}

/// Append one line to `refresh.log`, rotating it once past 256 KB.
pub fn log(line: &str) {
    let path = cache_dir().join("refresh.log");
    if fs::metadata(&path).is_ok_and(|m| m.len() > 256 * 1024) {
        let _ = fs::rename(&path, path.with_extension("log.1"));
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(
            f,
            "{} pid={} {line}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            std::process::id()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_binary_is_started_from_its_original_path() {
        let replaced = PathBuf::from("/home/me/.local/bin/ccmoneta (deleted)");
        assert_eq!(
            usable_exe(replaced.clone(), |_| true),
            PathBuf::from("/home/me/.local/bin/ccmoneta")
        );
        // Nothing at the original path any more: keep what was reported.
        assert_eq!(usable_exe(replaced.clone(), |_| false), replaced);
        let current = PathBuf::from("/usr/bin/ccmoneta");
        assert_eq!(usable_exe(current.clone(), |_| true), current);
    }
}
