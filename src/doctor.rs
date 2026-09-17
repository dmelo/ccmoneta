//! `ccmoneta doctor`: check what each surface depends on, and say how to fix
//! what is missing.
//!
//! Exits non-zero only when something needed for correct numbers fails. Things
//! that merely limit a surface, such as no credentials for polling the usage
//! endpoint, are warnings.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::config::{self, Config};
use crate::store::{self, Job, ago};
use crate::{cost, health, install, limits, sync};

struct Report {
    failed: bool,
}

impl Report {
    fn ok(&self, what: &str) {
        println!("  ok    {what}");
    }

    fn warn(&self, what: &str) {
        println!("  warn  {what}");
    }

    fn fail(&mut self, what: &str) {
        println!("  FAIL  {what}");
        self.failed = true;
    }
}

/// The first line of `<program> <arg>`, or why it could not run.
fn version_of(program: impl AsRef<OsStr>, arg: &str) -> Result<String, String> {
    let out = Command::new(program.as_ref())
        .arg(arg)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "not found".to_string()
            } else {
                e.to_string()
            }
        })?;
    if !out.status.success() {
        return Err(format!("exited {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string())
}

fn job_line(r: &Report, job: Job, now: i64) {
    let state = store::job_state(job);
    if state.failures > 0 {
        let retry = state
            .next_allowed
            .filter(|t| *t > now)
            .map(|t| format!("; next attempt in {}", ago(t - now)))
            .unwrap_or_default();
        r.warn(&format!(
            "{} refresh has failed {} time(s) in a row: {}{retry}",
            job.name(),
            state.failures,
            state.last_error.as_deref().unwrap_or("no error recorded")
        ));
    } else if let Some(t) = state.last_success {
        r.ok(&format!(
            "{} refresh last succeeded {} ago",
            job.name(),
            ago(now - t)
        ));
    }
}

/// Other `ccmoneta` processes whose executable has been replaced since they
/// started: a dashboard left open across an upgrade keeps running the old code.
/// Linux only; empty elsewhere.
fn processes_on_a_replaced_binary() -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let me = std::process::id();
    entries
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            if pid == me {
                return None;
            }
            let exe = std::fs::read_link(e.path().join("exe")).ok()?;
            let exe = exe.to_string_lossy();
            let deleted = exe.strip_suffix(" (deleted)")?;
            let name = Path::new(deleted).file_name()?;
            (name == "ccmoneta").then_some(pid)
        })
        .collect()
}

pub fn run(cfg: &Config, cfg_error: Option<&str>) -> i32 {
    let mut r = Report { failed: false };
    let now = store::now();
    let exe = store::self_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    println!("ccmoneta {} at {exe}", env!("CARGO_PKG_VERSION"));

    println!("config");
    let path = config::path();
    match cfg_error {
        Some(e) => r.fail(&format!("{e}; running with defaults")),
        None if path.exists() => r.ok(&format!("{} loaded", path.display())),
        None => r.ok(&format!("{} not present; using defaults", path.display())),
    }

    println!("cache");
    let dir = store::cache_dir();
    let probe = dir.join(format!(".doctor.{}", std::process::id()));
    match store::write_text(&probe, "ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            r.ok(&format!("{} is writable", dir.display()));
        }
        Err(e) => r.fail(&format!("{} is not writable: {e}", dir.display())),
    }

    println!("spend");
    match version_of(cost::ccusage_exe(), "--version") {
        Ok(v) => r.ok(&format!("ccusage: {v}")),
        Err(e) => r.fail(&format!(
            "ccusage: {e}; install it with `npm install -g ccusage`, or set CCMONETA_CCUSAGE"
        )),
    }
    match cost::load() {
        Some(s) => r.ok(&format!(
            "cost.json covers {}, gathered {} ago",
            s.date,
            ago(now - s.generated_at)
        )),
        None if cost::cache_path().exists() => r.warn(
            "cost.json exists but does not read; the next refresh replaces it (see refresh.log)",
        ),
        None => r.warn("no cost.json yet; any surface starts the first refresh"),
    }
    job_line(&r, Job::Cost, now);

    println!("limits");
    match limits::load() {
        Some(s) => r.ok(&format!("limits via {}, {} ago", s.source, ago(s.age(now)))),
        None => r.warn(
            "no limits cached yet; they arrive with the next Claude Code turn, or from the usage endpoint",
        ),
    }
    if limits::has_oauth_token() {
        r.ok("Claude OAuth credentials found, for polling the usage endpoint");
    } else {
        r.warn(
            "no Claude OAuth credentials in ~/.claude/.credentials.json; limits come only from the status line hook",
        );
    }
    match version_of("curl", "--version") {
        Ok(_) => r.ok("curl is available"),
        Err(e) => r.warn(&format!("curl: {e}; the usage endpoint cannot be polled")),
    }
    job_line(&r, Job::Limits, now);

    if cfg.health.any() {
        println!("health");
        match health::load() {
            Some(snap) => {
                // A problem with Claude, or an out-of-date install, is a
                // warning: neither stops ccmoneta's numbers being right. The
                // line says which it is, so nothing here reads the wording.
                for line in health::lines(&snap)
                    .into_iter()
                    .chain(health::age_line(&snap, now))
                {
                    match line.severity {
                        health::Severity::Warn => r.warn(&line.text),
                        health::Severity::Ok | health::Severity::Unknown => r.ok(&line.text),
                    }
                }
            }
            None => r.warn("no health check yet; any surface starts the first one"),
        }
        job_line(&r, Job::Health, now);
    }

    println!("status line");
    let settings = install::settings_path();
    let command = std::fs::read_to_string(&settings)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| {
            v.pointer("/statusLine/command")
                .and_then(Value::as_str)
                .map(String::from)
        });
    match command {
        Some(c) if c.contains("ccmoneta") && c.ends_with(" hook") => {
            r.ok(&format!("installed in {}: {c}", settings.display()));
        }
        Some(c) => r.warn(&format!(
            "{} runs a different status line: {c}",
            settings.display()
        )),
        None => r.warn(&format!(
            "not installed in {}; run `ccmoneta install statusline`",
            settings.display()
        )),
    }

    println!("sync");
    let targets = sync::targets(cfg);
    if targets.is_empty() {
        r.ok("no hosts configured");
    } else {
        match version_of("rsync", "--version") {
            Ok(v) => r.ok(&format!("rsync: {v}")),
            Err(e) => r.fail(&format!("rsync: {e}")),
        }
        for t in &targets {
            match sync::check_reachable(t) {
                Ok(()) => r.ok(&format!(
                    "{}: ssh to {} works without prompting",
                    t.name, t.ssh
                )),
                Err(e) => r.fail(&format!("{}: ssh to {} failed: {e}", t.name, t.ssh)),
            }
            let (last_sync, last_error) = sync::host_status(&t.name);
            match last_sync {
                Some(at) => r.ok(&format!("{}: synced {} ago", t.name, ago(now - at))),
                None => r.warn(&format!("{}: never synced", t.name)),
            }
            if let Some(e) = last_error {
                r.warn(&format!("{}: last sync failed: {e}", t.name));
            }
        }
        job_line(&r, Job::Sync, now);
    }

    println!("processes");
    let stale = processes_on_a_replaced_binary();
    if stale.is_empty() {
        r.ok("no ccmoneta process is running from a replaced binary");
    }
    for pid in stale {
        r.warn(&format!(
            "pid {pid} is ccmoneta running from a binary that has since been replaced; restart it so it runs the installed build"
        ));
    }

    i32::from(r.failed)
}
