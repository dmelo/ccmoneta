//! Several Claude accounts under aimux, against a throwaway HOME.
//!
//! The sandbox holds an aimux config with two profiles, each logged into its
//! own account, a fake aimux that reports their limits, and a fake ccusage that
//! "prices" a config directory at $1 per transcript line, so a split can be
//! read straight off the dollars.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_ccmoneta");

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("ccmoneta-accounts-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        for d in [
            "home/.claude/projects/-home-me-code-p",
            "home/.claude/session-env",
            "home/.aimux/profiles/second/session-env",
            "cache",
            "config",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(
            home.join(".aimux/config.yaml"),
            format!(
                "version: 1\nshared_source: {h}/.claude\nprofiles:\n  main:\n    cli: claude\n    path: {h}/.claude\n    is_source: true\n  second:\n    cli: claude\n    path: ~/.aimux/profiles/second\nprivate:\n  - .credentials.json\n",
                h = home.display()
            ),
        )
        .unwrap();
        fs::write(
            home.join(".claude/.claude.json"),
            r#"{"oauthAccount":{"accountUuid":"uuid-main","organizationType":"claude_max","organizationRateLimitTier":"default_claude_max_20x"}}"#,
        )
        .unwrap();
        fs::write(
            home.join(".aimux/profiles/second/.claude.json"),
            r#"{"oauthAccount":{"accountUuid":"uuid-second","organizationType":"claude_pro"}}"#,
        )
        .unwrap();

        let aimux = root.join("fake-aimux");
        fs::write(
            &aimux,
            r#"#!/bin/bash
case "$1" in
  --version) echo 0.33.0 ;;
  status) printf '{"fetchedAt":%s000,"profiles":{"main":{"cli":"claude","status":{"fiveHourPct":12,"weeklyPct":34}},"second":{"cli":"claude","status":{"fiveHourPct":56,"weeklyPct":78}}}}' "$(date +%s)" ;;
esac
"#,
        )
        .unwrap();
        let ccusage = root.join("fake-ccusage");
        fs::write(
            &ccusage,
            r#"#!/bin/bash
case " $* " in *" --instances "*) ;; *) exit 0 ;; esac
n=$(find "$CLAUDE_CONFIG_DIR/projects" -name '*.jsonl' -exec cat {} + 2>/dev/null | grep -c .)
printf '{"projects":{"-home-me-code-p":[{"date":"%s","totalCost":%s,"totalTokens":1,"modelBreakdowns":[]}]}}' "$(date +%Y-%m-%d)" "$n"
"#,
        )
        .unwrap();
        Command::new("chmod")
            .arg("+x")
            .arg(&aimux)
            .arg(&ccusage)
            .status()
            .unwrap();
        Sandbox { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("BLOCK_BUTTON")
            .env_remove("CCMONETA_AIMUX_DIR")
            .env("HOME", self.home())
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("CCMONETA_AIMUX", self.root.join("fake-aimux"))
            .env("CCMONETA_CCUSAGE", self.root.join("fake-ccusage"));
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn cache(&self, rel: &str) -> PathBuf {
        self.root.join("cache/ccmoneta").join(rel)
    }

    fn cost(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.cache("cost.json")).unwrap()).unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn limits_come_from_aimux_for_every_account() {
    let sb = Sandbox::new("limits");
    let o = sb.run(&["refresh", "limits", "--force"]);
    assert!(o.status.success(), "{o:?}");
    for (file, five) in [("uuid-main", 12.0), ("uuid-second", 56.0)] {
        let snap: serde_json::Value =
            serde_json::from_slice(&fs::read(sb.cache(&format!("limits/{file}.json"))).unwrap())
                .unwrap();
        assert_eq!(snap["limits"]["five_hour"]["percent"], five, "{file}");
        assert_eq!(snap["source"], "aimux");
    }
    let bar = stdout(&sb.run(&["bar"]));
    let first = bar.lines().next().unwrap();
    assert!(
        first.starts_with("main 5h 12% · 7d 34% │ second 5h 56% · 7d 78%"),
        "{first}"
    );
    let waybar: serde_json::Value =
        serde_json::from_str(&stdout(&sb.run(&["bar", "--format", "waybar"]))).unwrap();
    let tooltip = waybar["tooltip"].as_str().unwrap();
    assert!(tooltip.contains("main (Max 20x): 5h 12%"), "{tooltip}");
    assert!(tooltip.contains("second (Pro): 5h 56%"), "{tooltip}");
}

#[test]
fn the_status_line_saves_only_its_own_account() {
    let sb = Sandbox::new("hook");
    let payload = r#"{"model":{"display_name":"Opus"},"rate_limits":{"five_hour":{"used_percentage":42,"resets_at":4102444800},"seven_day":{"used_percentage":9,"resets_at":4102444800}}}"#;
    let mut child = sb
        .cmd(&["hook"])
        .env(
            "CLAUDE_CONFIG_DIR",
            sb.home().join(".aimux/profiles/second"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    let line = stdout(&o);
    assert!(line.starts_with("Opus · second · 5h 42% · 7d 9%"), "{line}");
    let snap: serde_json::Value =
        serde_json::from_slice(&fs::read(sb.cache("limits/uuid-second.json")).unwrap()).unwrap();
    assert_eq!(snap["limits"]["five_hour"]["percent"], 42.0);
    assert!(
        !sb.cache("limits/uuid-main.json").exists(),
        "another account's limits must not be written"
    );
    assert!(!sb.cache("limits.json").exists());
}

#[test]
fn spend_is_split_by_the_account_that_ran_each_turn() {
    let sb = Sandbox::new("spend");
    // The second account opened this session now: the turn from 2020 is the
    // source account's, the two from an hour ahead are the second's.
    fs::create_dir_all(sb.home().join(".aimux/profiles/second/session-env/s1")).unwrap();
    let later = chrono::Utc::now() + chrono::Duration::hours(1);
    let later = later.to_rfc3339();
    fs::write(
        sb.home().join(".claude/projects/-home-me-code-p/s1.jsonl"),
        format!(
            "{{\"timestamp\":\"2020-01-01T00:00:00Z\"}}\n{{\"timestamp\":\"{later}\"}}\n{{\"timestamp\":\"{later}\"}}\n"
        ),
    )
    .unwrap();
    // A session no marker names stays whole with the source account.
    fs::write(
        sb.home().join(".claude/projects/-home-me-code-p/s2.jsonl"),
        format!("{{\"timestamp\":\"{later}\"}}\n"),
    )
    .unwrap();

    let o = sb.run(&["refresh", "cost", "--force"]);
    assert!(o.status.success(), "{o:?}");
    let costs = &sb.cost()["costs"];
    assert_eq!(
        costs["by_account"],
        serde_json::json!([["main", 2.0], ["second", 2.0]]),
        "{costs}"
    );
    assert_eq!(costs["window"], 4.0, "the accounts add up to the total");
}

#[test]
fn one_account_reads_the_transcripts_in_place() {
    let sb = Sandbox::new("single");
    fs::write(
        sb.home().join(".claude/projects/-home-me-code-p/s1.jsonl"),
        "{}\n{}\n",
    )
    .unwrap();
    // No marker anywhere: nothing to split, so no views are built.
    let o = sb.run(&["refresh", "cost", "--force"]);
    assert!(o.status.success(), "{o:?}");
    assert_eq!(
        sb.cost()["costs"]["by_account"],
        serde_json::json!([["main", 2.0]])
    );
    assert!(!sb.cache("views").exists());
}

#[test]
fn the_remote_listing_names_accounts_and_markers() {
    let sb = Sandbox::new("remote");
    fs::create_dir_all(sb.home().join(".claude/session-env/s1")).unwrap();
    fs::create_dir_all(sb.home().join(".aimux/profiles/second/session-env/s2")).unwrap();
    let script = include_str!("../src/accounts.rs")
        .split("pub const REMOTE_MARKERS: &str = r#\"")
        .nth(1)
        .and_then(|s| s.split("\"#;").next())
        .unwrap();
    let mut child = Command::new("sh")
        .arg("-s")
        .env("HOME", sb.home())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = stdout(&child.wait_with_output().unwrap());
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert_eq!(lines[0], "@\tuuid-main\tmain\t1");
    assert!(lines[1].starts_with("s1\t"), "{out}");
    assert_eq!(lines[2], "@\tuuid-second\tsecond\t0");
    let born: i64 = lines[3].strip_prefix("s2\t").unwrap().parse().unwrap();
    assert!((born - chrono::Utc::now().timestamp()).abs() < 120, "{out}");
}

#[test]
fn without_aimux_the_status_line_saves_whatever_the_config_dir() {
    let sb = Sandbox::new("noaimux");
    fs::remove_file(sb.home().join(".aimux/config.yaml")).unwrap();
    let payload = r#"{"rate_limits":{"five_hour":{"used_percentage":42,"resets_at":4102444800}}}"#;
    let mut child = sb
        .cmd(&["hook"])
        .env("CLAUDE_CONFIG_DIR", sb.root.join("elsewhere"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let line = stdout(&child.wait_with_output().unwrap());
    assert!(
        line.starts_with("5h 42%"),
        "no account name with one account: {line}"
    );
    let snap: serde_json::Value =
        serde_json::from_slice(&fs::read(sb.cache("limits.json")).unwrap()).unwrap();
    assert_eq!(snap["limits"]["five_hour"]["percent"], 42.0);
}

#[test]
fn an_isolated_profile_counts_its_own_transcripts() {
    let sb = Sandbox::new("isolated");
    fs::write(
        sb.home().join(".claude/projects/-home-me-code-p/s1.jsonl"),
        "{}\n",
    )
    .unwrap();
    // `aimux migrate isolate` gives the profile a real projects/ of its own.
    let own = sb
        .home()
        .join(".aimux/profiles/second/projects/-home-me-code-p");
    fs::create_dir_all(&own).unwrap();
    fs::write(own.join("s9.jsonl"), "{}\n{}\n{}\n").unwrap();
    let o = sb.run(&["refresh", "cost", "--force"]);
    assert!(o.status.success(), "{o:?}");
    assert_eq!(
        sb.cost()["costs"]["by_account"],
        serde_json::json!([["second", 3.0], ["main", 1.0]])
    );
}

#[test]
fn an_account_aimux_cannot_read_is_shown_without_failing_the_others() {
    let sb = Sandbox::new("expired");
    fs::write(
        sb.root.join("fake-aimux"),
        r#"#!/bin/bash
case "$1" in
  status) printf '{"fetchedAt":%s000,"profiles":{"main":{"cli":"claude","status":{"fiveHourPct":12,"weeklyPct":34}},"second":{"cli":"claude","status":null,"error":"auth"}}}' "$(date +%s)" ;;
esac
"#,
    )
    .unwrap();
    let o = sb.run(&["refresh", "limits", "--force"]);
    assert!(
        o.status.success(),
        "one readable account is a success: {o:?}"
    );
    let bar = stdout(&sb.run(&["bar"]));
    let first = bar.lines().next().unwrap();
    assert!(
        first.starts_with("main 5h 12% · 7d 34% │ second ✗"),
        "{first}"
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(sb.cache("jobs/limits.json")).unwrap()).unwrap();
    assert_eq!(state["failures"], 0, "{state}");
}

/// The second profile's credentials, with only the two expiry times set.
fn credentials(sb: &Sandbox, access_in: i64, refresh_in: i64) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    fs::write(
        sb.home().join(".aimux/profiles/second/.credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"expiresAt":{},"refreshTokenExpiresAt":{}}}}}"#,
            now_ms + access_in * 1000,
            now_ms + refresh_in * 1000
        ),
    )
    .unwrap();
}

fn second_limits(sb: &Sandbox) -> serde_json::Value {
    serde_json::from_slice(&fs::read(sb.cache("limits/uuid-second.json")).unwrap()).unwrap()
}

#[test]
fn a_lapsed_token_is_not_called_an_expired_login_and_keeps_the_last_reading() {
    let sb = Sandbox::new("lapsed");
    // A good reading first, then aimux's probe is refused.
    assert!(sb.run(&["refresh", "limits", "--force"]).status.success());
    fs::write(
        sb.root.join("fake-aimux"),
        r#"#!/bin/bash
case "$1" in
  status) printf '{"fetchedAt":%s000,"profiles":{"main":{"cli":"claude","status":{"fiveHourPct":12,"weeklyPct":34}},"second":{"cli":"claude","status":null,"error":"auth"}}}' "$(date +%s)" ;;
esac
"#,
    )
    .unwrap();

    credentials(&sb, -3600, 86400 * 20);
    assert!(sb.run(&["refresh", "limits", "--force"]).status.success());
    let snap = second_limits(&sb);
    let error = snap["error"].as_str().unwrap();
    assert!(
        error.starts_with("token expired 1h 00m ago; renews with the next second session"),
        "{error}"
    );
    assert_eq!(
        snap["limits"]["five_hour"]["percent"], 56.0,
        "the last reading is kept"
    );
    assert!(snap["read_at"].as_i64().is_some(), "{snap}");

    credentials(&sb, -3600, -60);
    assert!(sb.run(&["refresh", "limits", "--force"]).status.success());
    let error = second_limits(&sb)["error"].as_str().unwrap().to_string();
    assert_eq!(error, "login expired: run `aimux auth login second`");
}

#[test]
fn a_second_profile_on_the_same_account_credits_that_account() {
    let sb = Sandbox::new("folded");
    let twin = sb.home().join(".aimux/profiles/twin");
    fs::create_dir_all(twin.join("session-env/s1")).unwrap();
    fs::write(
        twin.join(".claude.json"),
        r#"{"oauthAccount":{"accountUuid":"uuid-second"}}"#,
    )
    .unwrap();
    let config = sb.home().join(".aimux/config.yaml");
    let yaml = fs::read_to_string(&config).unwrap().replace(
        "private:",
        "  twin:\n    cli: claude\n    path: ~/.aimux/profiles/twin\nprivate:",
    );
    fs::write(&config, yaml).unwrap();
    let later = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    fs::write(
        sb.home().join(".claude/projects/-home-me-code-p/s1.jsonl"),
        format!("{{\"timestamp\":\"{later}\"}}\n"),
    )
    .unwrap();
    let o = sb.run(&["refresh", "cost", "--force"]);
    assert!(o.status.success(), "{o:?}");
    // The fake ccusage reports an empty view as $0, which real ccusage omits.
    let spent: Vec<serde_json::Value> = sb.cost()["costs"]["by_account"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a[1].as_f64() != Some(0.0))
        .cloned()
        .collect();
    assert_eq!(
        spent,
        vec![serde_json::json!(["second", 1.0])],
        "twin's session is the second account's, listed once"
    );
}
