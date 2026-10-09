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
use crate::{accounts, cost, health, install, limits, sync};

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

/// A ccusage verified to count every entry; releases between it and 20.0.20
/// were not checked, so this is a floor that is safe, not the exact first fix.
/// 20.0.20 skips any assistant
/// entry whose `usage.iterations[]` carries `"model": null`, as newer Claude
/// Code writes it, and exits 0 without a warning, so those calls are simply
/// missing from every total.
const CCUSAGE_MIN: [u32; 3] = [20, 0, 26];

/// Whether `ccusage --version` output names a release older than CCUSAGE_MIN.
/// Output that does not parse is not called old: doctor has already shown it.
fn ccusage_too_old(version: &str) -> bool {
    let Some(v) = version.split_whitespace().last() else {
        return false;
    };
    let parts: Option<Vec<u32>> = v.split('.').map(|p| p.parse().ok()).collect();
    match parts {
        Some(p) if p.len() == 3 => p.as_slice() < CCUSAGE_MIN.as_slice(),
        _ => false,
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
        Ok(v) if ccusage_too_old(&v) => r.warn(&format!(
            "ccusage: {v}; older than {}, which silently drops transcript entries written by newer Claude Code, so spend reads low. Upgrade: `npm install -g ccusage@latest`",
            CCUSAGE_MIN.map(|n| n.to_string()).join(".")
        )),
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
    let accts = accounts::local();
    let several = accounts::several(&accts);
    if several {
        let names: Vec<&str> = accts.iter().map(|a| a.name.as_str()).collect();
        r.ok(&format!(
            "aimux: {} accounts ({}); spend and limits are kept per account",
            accts.len(),
            names.join(", ")
        ));
        match version_of(limits::aimux_exe(), "--version") {
            Ok(v) => r.ok(&format!("aimux {v}, for reading every account's limits")),
            Err(e) => r.fail(&format!(
                "aimux: {e}; without it only the status line updates limits, one account at a time"
            )),
        }
        for a in &accts {
            if a.uuid.is_none() {
                r.warn(&format!(
                    "{}: no account in {}/.claude.json; is it logged in? Its spend goes to {}",
                    a.name,
                    a.config_dir.display(),
                    accts
                        .iter()
                        .find(|x| x.source)
                        .map_or("the source", |x| x.name.as_str())
                ));
            }
        }
    }
    for a in &accts {
        let label = if several {
            format!("{}: ", a.name)
        } else {
            String::new()
        };
        match limits::load_for(a, several) {
            Some(s) => r.ok(&format!(
                "{label}limits via {}, {} ago",
                s.source,
                ago(s.age(now))
            )),
            None => r.warn(&format!(
                "{label}no limits cached yet; they arrive with the next Claude Code turn, or from {}",
                if several { "aimux" } else { "the usage endpoint" }
            )),
        }
    }
    if several {
        // aimux holds every login; ccmoneta's own endpoint poll is not used.
    } else if limits::has_oauth_token() {
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

#[cfg(test)]
mod tests {
    use super::ccusage_too_old;

    #[test]
    fn ccusage_below_the_floor_is_old() {
        assert!(ccusage_too_old("ccusage 20.0.20"));
        assert!(ccusage_too_old("ccusage 19.9.99"));
        assert!(!ccusage_too_old("ccusage 20.0.26"));
        assert!(!ccusage_too_old("ccusage 20.1.0"));
        assert!(!ccusage_too_old("ccusage 99.0.0"));
        // Unparseable output is shown as-is rather than called old.
        assert!(!ccusage_too_old("ccusage dev"));
        assert!(!ccusage_too_old(""));
    }
}
