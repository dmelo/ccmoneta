//! Keeping the cache fresh without making any surface wait.
//!
//! Surfaces render what is cached, then call `cost_if_stale`, `limits_if_due` and
//! `sync_if_due`, which start `ccmoneta refresh <job>` as a detached background
//! process when the cached value is out of date. The job takes a lock, so however many surfaces
//! notice at once, the work happens once. See docs/design.md.

use std::collections::BTreeSet;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use chrono::NaiveDate;

use crate::config::Config;
use crate::cost::{self, CostSnapshot};
use crate::health::{self, HealthSnapshot};
use crate::limits::{self, Snapshot};
use crate::store::{self, Job, JobState};
use crate::sync;

/// A surface that saw a job spawned this recently does not spawn it again. This
/// covers the gap between starting a child and the child taking its lock, when
/// a surface called repeatedly would otherwise start a process per call.
const SPAWN_DEBOUNCE: i64 = 30;
const BACKOFF_CAP: i64 = 30 * 60;

/// Delay before the next attempt after `failures` consecutive failures: 1, 2, 4
/// ... minutes, capped at 30.
pub fn backoff_secs(failures: u32) -> i64 {
    if failures == 0 {
        return 0;
    }
    (60i64 << (failures - 1).min(16)).min(BACKOFF_CAP)
}

/// Whether a cost snapshot needs regenerating. `hosts` is every host present
/// now, with its last successful sync.
pub fn cost_is_stale(
    snap: Option<&CostSnapshot>,
    hosts: &[(String, Option<i64>)],
    now: i64,
    today: NaiveDate,
    refresh_seconds: i64,
) -> bool {
    let Some(snap) = snap else {
        return true;
    };
    if now - snap.generated_at >= refresh_seconds {
        return true;
    }
    // Its "today" is a finished day, however recently it was generated.
    if snap.date != today.format("%Y-%m-%d").to_string() {
        return true;
    }
    let covered: BTreeSet<&str> = snap.costs.hosts.iter().map(|h| h.name.as_str()).collect();
    let present: BTreeSet<&str> = hosts.iter().map(|(name, _)| name.as_str()).collect();
    if covered != present {
        return true;
    }
    hosts
        .iter()
        .any(|(_, synced)| synced.is_some_and(|t| t > snap.generated_at))
}

pub fn limits_poll_due(snap: Option<&Snapshot>, now: i64, max_age: i64) -> bool {
    snap.is_none_or(|s| s.age(now) >= max_age)
}

/// The status page and the version lookup are other people's services, so this
/// is deliberately the slowest of the schedules.
pub fn health_is_due(snap: Option<&HealthSnapshot>, now: i64, max_age: i64) -> bool {
    snap.is_none_or(|s| s.age(now) >= max_age)
}

pub fn should_spawn(
    running: bool,
    spawned_at: Option<i64>,
    state: &JobState,
    now: i64,
    force: bool,
) -> bool {
    if running {
        return false;
    }
    if force {
        return true;
    }
    state.allows(now) && spawned_at.is_none_or(|t| now - t >= SPAWN_DEBOUNCE)
}

/// Fold one run's outcome into the job's state.
pub fn record_outcome(state: &mut JobState, outcome: &Result<(), String>, now: i64) {
    match outcome {
        Ok(()) => {
            state.last_success = Some(now);
            state.failures = 0;
            state.next_allowed = None;
            state.last_error = None;
        }
        Err(e) => {
            state.failures += 1;
            state.next_allowed = Some(now + backoff_secs(state.failures));
            state.last_error = Some(e.clone());
        }
    }
}

fn today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

fn present_hosts() -> Vec<(String, Option<i64>)> {
    cost::hosts()
        .into_iter()
        .map(|h| (h.name, h.last_sync))
        .collect()
}

pub fn cost_if_stale(cfg: &Config, snap: Option<&CostSnapshot>) {
    let stale = cost_is_stale(
        snap,
        &present_hosts(),
        store::now(),
        today(),
        cfg.cost.refresh_seconds,
    );
    if stale {
        trigger(Job::Cost, false);
    }
}

pub fn limits_if_due(cfg: &Config, snap: Option<&Snapshot>) {
    if limits_poll_due(snap, store::now(), cfg.limits.max_age_seconds) {
        trigger(Job::Limits, false);
    }
}

pub fn health_if_due(cfg: &Config, snap: Option<&HealthSnapshot>) {
    if cfg.health.any() && health_is_due(snap, store::now(), cfg.health.max_age_seconds) {
        trigger(Job::Health, false);
    }
}

pub fn sync_if_due(cfg: &Config) {
    if !sync::due_hosts(cfg, store::now()).is_empty() {
        trigger(Job::Sync, false);
    }
}

/// Start `ccmoneta refresh <job>` in the background, unless it is already running
/// or was just started.
pub fn trigger(job: Job, force: bool) {
    let now = store::now();
    let spawn = should_spawn(
        store::is_running(job),
        store::spawned_at(job),
        &store::job_state(job),
        now,
        force,
    );
    if !spawn {
        return;
    }
    let Some(exe) = store::self_exe() else {
        return;
    };
    store::mark_spawned(job, now);
    let mut cmd = Command::new(exe);
    cmd.arg("refresh").arg(job.name());
    if force {
        cmd.arg("--force");
    }
    // Inherit nothing. Callers read this process's stdout (Claude Code shows the
    // hook's output, bars show the bar's), and a child holding that pipe open
    // could keep them waiting for the whole refresh. Its own process group
    // keeps signals sent to the caller's group from reaching it.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if let Ok(mut child) = cmd.spawn() {
        // Reap it, so a long-running dashboard does not collect zombies. When a
        // short-lived caller exits first, the child is simply re-parented.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Ask the bar to redraw now, when the config names it. The signal is only sent
/// to the program named there: an unhandled real-time signal terminates the
/// process that receives it.
fn notify_bar(cfg: &Config) {
    let (Some(signal), Some(program)) = (cfg.bar.signal, cfg.bar.program.as_deref()) else {
        return;
    };
    let _ = Command::new("pkill")
        .arg(format!("-RTMIN+{signal}"))
        .args(["-x", program])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `ccmoneta refresh <job> [--force]`.
pub fn run(cfg: &Config, job: Job, force: bool) -> i32 {
    let lock = match store::try_lock(job) {
        Ok(Some(lock)) => lock,
        // Another run holds the lock and is doing this work.
        Ok(None) => return 0,
        Err(e) => {
            store::log(&format!("{} lock unavailable: {e}", job.name()));
            return 1;
        }
    };

    let now = store::now();
    let mut state = store::job_state(job);
    // Re-check under the lock: another run may have finished between this one
    // being spawned and taking the lock.
    let needed = force
        || match job {
            Job::Cost => cost_is_stale(
                cost::load().as_ref(),
                &present_hosts(),
                now,
                today(),
                cfg.cost.refresh_seconds,
            ),
            Job::Limits => {
                limits_poll_due(limits::load().as_ref(), now, cfg.limits.max_age_seconds)
            }
            Job::Sync => !sync::due_hosts(cfg, now).is_empty(),
            Job::Health => {
                cfg.health.any()
                    && health_is_due(health::load().as_ref(), now, cfg.health.max_age_seconds)
            }
        };
    let held_back = match job {
        // The usage endpoint is rate-limited, so its backoff holds even on a
        // manual refresh.
        Job::Limits => !state.allows(now),
        // A manual cost refresh goes ahead, so fixing a missing ccusage takes
        // effect at once.
        Job::Cost => !force && !state.allows(now),
        // Each host spaces its own attempts (see sync::is_due), so one
        // unreachable host does not hold back the others.
        Job::Sync => false,
        // Someone else's services: a manual refresh does not skip the backoff.
        Job::Health => !state.allows(now),
    };
    if !needed || held_back {
        return 0;
    }

    state.last_attempt = Some(now);
    store::save_job_state(job, &state);
    let started = std::time::Instant::now();
    let outcome = match job {
        Job::Cost => cost::refresh(cfg),
        Job::Limits => limits::refresh(),
        Job::Sync => sync::run_due(cfg, force),
        Job::Health => health::refresh(&cfg.health),
    };
    record_outcome(&mut state, &outcome, store::now());
    store::save_job_state(job, &state);
    store::log(&format!(
        "{} {} in {:.1}s{}",
        job.name(),
        if outcome.is_ok() { "ok" } else { "failed" },
        started.elapsed().as_secs_f64(),
        outcome
            .as_ref()
            .err()
            .map(|e| format!(": {e}"))
            .unwrap_or_default()
    ));
    drop(lock);
    if outcome.is_ok() {
        notify_bar(cfg);
    }
    i32::from(outcome.is_err())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::{Costs, Host};
    use crate::limits::Limits;
    use std::path::PathBuf;

    const NOW: i64 = 1_800_000_000;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn snap(generated_at: i64, date: &str, hosts: &[&str]) -> CostSnapshot {
        CostSnapshot {
            generated_at,
            date: date.into(),
            costs: Costs {
                hosts: hosts
                    .iter()
                    .map(|n| Host {
                        name: n.to_string(),
                        config_dir: PathBuf::new(),
                        remote: false,
                        last_sync: None,
                    })
                    .collect(),
                ..Default::default()
            },
        }
    }

    fn present(hosts: &[(&str, Option<i64>)]) -> Vec<(String, Option<i64>)> {
        hosts.iter().map(|(n, s)| (n.to_string(), *s)).collect()
    }

    #[test]
    fn backoff_doubles_from_a_minute_and_caps_at_thirty() {
        assert_eq!(backoff_secs(0), 0);
        assert_eq!(backoff_secs(1), 60);
        assert_eq!(backoff_secs(2), 120);
        assert_eq!(backoff_secs(3), 240);
        assert_eq!(backoff_secs(6), 1800);
        assert_eq!(backoff_secs(40), 1800);
    }

    #[test]
    fn no_snapshot_is_stale() {
        assert!(cost_is_stale(None, &[], NOW, day("2026-09-15"), 180));
    }

    #[test]
    fn a_recent_snapshot_for_today_and_the_same_hosts_is_fresh() {
        let s = snap(NOW - 60, "2026-09-15", &["desk"]);
        let hosts = present(&[("desk", None)]);
        assert!(!cost_is_stale(
            Some(&s),
            &hosts,
            NOW,
            day("2026-09-15"),
            180
        ));
    }

    #[test]
    fn an_old_snapshot_is_stale() {
        let s = snap(NOW - 180, "2026-09-15", &["desk"]);
        let hosts = present(&[("desk", None)]);
        assert!(cost_is_stale(Some(&s), &hosts, NOW, day("2026-09-15"), 180));
    }

    #[test]
    fn a_snapshot_from_yesterday_is_stale_however_recent() {
        let s = snap(NOW - 5, "2026-09-14", &["desk"]);
        let hosts = present(&[("desk", None)]);
        assert!(cost_is_stale(Some(&s), &hosts, NOW, day("2026-09-15"), 180));
    }

    #[test]
    fn a_change_in_hosts_is_stale() {
        let s = snap(NOW - 60, "2026-09-15", &["desk"]);
        let added = present(&[("desk", None), ("laptop", Some(NOW - 600))]);
        assert!(cost_is_stale(Some(&s), &added, NOW, day("2026-09-15"), 180));
        let removed = present(&[]);
        assert!(cost_is_stale(
            Some(&s),
            &removed,
            NOW,
            day("2026-09-15"),
            180
        ));
    }

    #[test]
    fn a_host_synced_after_the_snapshot_makes_it_stale() {
        let s = snap(NOW - 60, "2026-09-15", &["desk", "laptop"]);
        let later = present(&[("desk", None), ("laptop", Some(NOW - 10))]);
        assert!(cost_is_stale(Some(&s), &later, NOW, day("2026-09-15"), 180));
        let earlier = present(&[("desk", None), ("laptop", Some(NOW - 600))]);
        assert!(!cost_is_stale(
            Some(&s),
            &earlier,
            NOW,
            day("2026-09-15"),
            180
        ));
    }

    #[test]
    fn limits_are_polled_only_when_missing_or_old() {
        let snap = |age: i64| Snapshot {
            limits: Limits::default(),
            captured_at: NOW - age,
            source: "statusline".into(),
        };
        assert!(limits_poll_due(None, NOW, 900));
        assert!(!limits_poll_due(Some(&snap(899)), NOW, 900));
        assert!(limits_poll_due(Some(&snap(900)), NOW, 900));
    }

    #[test]
    fn health_is_checked_only_when_missing_or_old() {
        let snap = |age: i64| HealthSnapshot {
            checked_at: NOW - age,
            ..Default::default()
        };
        assert!(health_is_due(None, NOW, 900));
        assert!(!health_is_due(Some(&snap(899)), NOW, 900));
        assert!(health_is_due(Some(&snap(900)), NOW, 900));
    }

    #[test]
    fn spawning_respects_running_debounce_and_backoff() {
        let idle = JobState::default();
        let backing_off = JobState {
            next_allowed: Some(NOW + 60),
            ..Default::default()
        };
        assert!(should_spawn(false, None, &idle, NOW, false));
        assert!(!should_spawn(true, None, &idle, NOW, false));
        assert!(!should_spawn(false, Some(NOW - 5), &idle, NOW, false));
        assert!(should_spawn(false, Some(NOW - 30), &idle, NOW, false));
        assert!(!should_spawn(false, None, &backing_off, NOW, false));
        // Force skips the debounce and backoff, never the lock.
        assert!(should_spawn(false, Some(NOW - 5), &backing_off, NOW, true));
        assert!(!should_spawn(true, None, &idle, NOW, true));
    }

    #[test]
    fn failures_back_off_and_a_success_resets() {
        let mut state = JobState::default();
        record_outcome(&mut state, &Err("boom".into()), NOW);
        assert_eq!(state.failures, 1);
        assert_eq!(state.next_allowed, Some(NOW + 60));
        assert_eq!(state.last_error.as_deref(), Some("boom"));
        record_outcome(&mut state, &Err("boom".into()), NOW);
        assert_eq!(state.next_allowed, Some(NOW + 120));
        record_outcome(&mut state, &Ok(()), NOW);
        assert_eq!(state.failures, 0);
        assert_eq!(state.next_allowed, None);
        assert_eq!(state.last_error, None);
        assert_eq!(state.last_success, Some(NOW));
    }
}
