//! Each surface on its own, against a throwaway cache and a fake ccusage.
//!
//! Every test gets its own HOME, XDG_CACHE_HOME and XDG_CONFIG_HOME under the
//! system temp directory, so nothing touches the real cache, config or
//! transcripts. The fake ccusage logs each call and returns $12.34 for today.
//! HOME holds no Claude credentials, so the limits job fails without reaching
//! the network.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ccmoneta");

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("ccmoneta-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home/.claude/projects")).unwrap();
        fs::create_dir_all(root.join("cache")).unwrap();
        fs::create_dir_all(root.join("config")).unwrap();
        let fake = root.join("fake-ccusage");
        fs::write(
            &fake,
            r#"#!/bin/bash
echo "$1" >> "$FAKE_LOG"
if [ -n "$FAKE_SLEEP" ]; then sleep "$FAKE_SLEEP"; fi
if [ "$FAKE_FAIL" = 1 ]; then echo "fake ccusage failure" >&2; exit 1; fi
case "$1" in
  daily) printf '{"daily":[{"period":"%s","totalCost":12.34,"totalTokens":100,"modelBreakdowns":[{"modelName":"claude-opus-5","cost":12.34}]}]}' "$(date +%Y-%m-%d)" ;;
  session) printf '{"session":[]}' ;;
esac
"#,
        )
        .unwrap();
        Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
        Sandbox { root }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("BLOCK_BUTTON")
            .env("HOME", self.root.join("home"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("CCMONETA_CCUSAGE", self.root.join("fake-ccusage"))
            .env("FAKE_LOG", self.root.join("calls.log"));
        c
    }

    fn cache(&self, rel: &str) -> PathBuf {
        self.root.join("cache/ccmoneta").join(rel)
    }

    fn hook(&self, extra_env: &[(&str, &str)]) -> Output {
        let mut c = self.cmd(&["hook"]);
        for (k, v) in extra_env {
            c.env(k, v);
        }
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"model":{"display_name":"Opus 5"},"rate_limits":{"five_hour":{"used_percentage":7,"resets_at":4000000000},"seven_day":{"used_percentage":60,"resets_at":4000000000}}}"#)
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn bar(&self, extra_env: &[(&str, &str)]) -> String {
        let mut c = self.cmd(&["bar"]);
        for (k, v) in extra_env {
            c.env(k, v);
        }
        String::from_utf8(c.output().unwrap().stdout).unwrap()
    }

    fn calls(&self, kind: &str) -> usize {
        fs::read_to_string(self.root.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == kind)
            .count()
    }

    fn cost_job_running(&self) -> bool {
        let path = self.cache("locks/cost.lock");
        let Ok(file) = fs::OpenOptions::new().write(true).open(&path) else {
            return false;
        };
        file.try_lock().is_err()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn exists(p: &Path) -> bool {
    p.exists()
}

#[test]
fn the_status_line_alone_reaches_the_right_cost_from_a_cold_start() {
    let sb = Sandbox::new("hook-cold");
    let first = String::from_utf8(sb.hook(&[]).stdout).unwrap();
    assert!(
        first.contains("5h 7%") && first.contains("7d 60%"),
        "{first}"
    );
    assert!(
        first.contains("$…"),
        "no figure yet should read $…: {first}"
    );

    wait_until("cost.json", Duration::from_secs(20), || {
        exists(&sb.cache("cost.json"))
    });
    wait_until("the cost job to finish", Duration::from_secs(20), || {
        !sb.cost_job_running()
    });
    let second = String::from_utf8(sb.hook(&[]).stdout).unwrap();
    assert!(second.contains("$12.34"), "{second}");
}

#[test]
fn the_bar_alone_reaches_the_right_cost_from_a_cold_start() {
    let sb = Sandbox::new("bar-cold");
    let first = sb.bar(&[]);
    assert!(first.lines().next().unwrap().contains("$…"), "{first}");

    wait_until("cost.json", Duration::from_secs(20), || {
        exists(&sb.cache("cost.json"))
    });
    wait_until("the cost job to finish", Duration::from_secs(20), || {
        !sb.cost_job_running()
    });
    let second = sb.bar(&[]);
    assert!(
        second.lines().next().unwrap().contains("$12.34"),
        "{second}"
    );
}

#[test]
fn many_stale_callers_at_once_start_one_refresh() {
    let sb = Sandbox::new("single-flight");
    let children: Vec<_> = (0..20)
        .map(|_| {
            sb.cmd(&["bar"])
                .env("FAKE_SLEEP", "1")
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut c in children {
        c.wait().unwrap();
    }
    wait_until("cost.json", Duration::from_secs(30), || {
        exists(&sb.cache("cost.json"))
    });
    wait_until("the cost job to finish", Duration::from_secs(30), || {
        !sb.cost_job_running()
    });
    // Give any straggler that lost the race time to start, check, and exit.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(sb.calls("daily"), 1, "one host, so exactly one daily run");
    assert_eq!(sb.calls("session"), 1);
}

#[test]
fn the_hook_returns_at_once_while_a_refresh_is_running() {
    let sb = Sandbox::new("non-blocking");
    let start = Instant::now();
    let out = sb.hook(&[("FAKE_SLEEP", "5")]);
    let took = start.elapsed();
    // wait_with_output reads stdout to EOF, so this also proves the detached
    // refresh is not holding the hook's stdout open.
    assert!(took < Duration::from_secs(2), "hook took {took:?}");
    assert!(String::from_utf8(out.stdout).unwrap().contains("$…"));
    wait_until("the refresh to start", Duration::from_secs(5), || {
        sb.calls("daily") == 1
    });
    assert!(sb.cost_job_running(), "the refresh should still be running");
    assert!(!exists(&sb.cache("cost.json")));
}

#[test]
fn a_failing_refresh_shows_a_marker_and_backs_off() {
    let sb = Sandbox::new("backoff");
    let fail = [("FAKE_FAIL", "1")];
    sb.bar(&fail);
    wait_until(
        "the failure to be recorded",
        Duration::from_secs(20),
        || {
            fs::read_to_string(sb.cache("jobs/cost.json"))
                .is_ok_and(|s| s.contains("\"failures\": 1"))
        },
    );
    wait_until("the cost job to finish", Duration::from_secs(20), || {
        !sb.cost_job_running()
    });

    let shown = sb.bar(&fail);
    assert!(shown.lines().next().unwrap().contains("$?"), "{shown}");
    let state = fs::read_to_string(sb.cache("jobs/cost.json")).unwrap();
    assert!(state.contains("fake ccusage failure"), "{state}");

    // Clear the spawn debounce, so only the backoff stands between these bars
    // and another attempt.
    let _ = fs::remove_file(sb.cache("jobs/cost.spawned"));
    for _ in 0..5 {
        sb.bar(&fail);
    }
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(sb.calls("daily"), 1, "backoff should stop further attempts");
}

#[test]
fn yesterdays_figure_is_never_shown_as_today() {
    let sb = Sandbox::new("yesterday");
    let yesterday = local_date_yesterday();
    let snap = format!(
        r#"{{"generated_at": {}, "date": "{yesterday}", "costs": {{"today": 99.99, "hosts": []}}}}"#,
        now() - 60
    );
    fs::create_dir_all(sb.cache("")).unwrap();
    fs::write(sb.cache("cost.json"), snap).unwrap();

    let out = String::from_utf8(sb.hook(&[]).stdout).unwrap();
    assert!(!out.contains("99.99"), "{out}");
    assert!(out.contains("$…"), "{out}");
    wait_until("a refresh for today", Duration::from_secs(20), || {
        fs::read_to_string(sb.cache("cost.json")).is_ok_and(|s| s.contains("12.34"))
    });
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Yesterday's local date, from `date`, to avoid a dev-dependency for one test.
fn local_date_yesterday() -> String {
    let out = Command::new("date")
        .args(["-d", "yesterday", "+%Y-%m-%d"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}
