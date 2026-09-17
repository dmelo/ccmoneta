//! Health on each surface, from a hand-written `health.json`.
//!
//! The snapshot is written fresh and `health.max_age_seconds` is long, so no
//! health job is ever due: these tests reach neither the status page nor the
//! version lookup, and need no network.

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
        let root = std::env::temp_dir().join(format!(
            "ccmoneta-health-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        for d in [
            "home/.claude/projects",
            "cache/ccmoneta",
            "config/ccmoneta",
            "claude",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(
            root.join("config/ccmoneta/config.toml"),
            "[health]\nmax_age_seconds = 3600\n",
        )
        .unwrap();
        let fake = root.join("fake-ccusage");
        fs::write(
            &fake,
            "#!/bin/bash\ncase \"$1\" in\n  --version) echo 'ccusage 99.0.0' ;;\n  daily) printf '{\"daily\":[]}' ;;\n  session) printf '{\"session\":[]}' ;;\nesac\n",
        )
        .unwrap();
        Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
        Sandbox { root }
    }

    /// A snapshot as the health job would have written it, checked just now.
    fn write_health(&self, status_indicator: &str, installed: &str, latest: &str, behind: bool) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let body = format!(
            r#"{{
              "checked_at": {now},
              "status": {{
                "indicator": "{status_indicator}",
                "description": "Partial outage",
                "claude_code": "major_outage",
                "degraded": [["Claude Code", "major_outage"]],
                "incidents": [{{"name": "Elevated errors", "impact": "major", "status": "investigating"}}]
              }},
              "status_error": null,
              "version": {{
                "installed": "{installed}",
                "latest": "{latest}",
                "channel": "latest",
                "source": "native",
                "behind": {behind},
                "skipped": null,
                "background_updates": false
              }},
              "version_error": null
            }}"#
        );
        fs::write(self.root.join("cache/ccmoneta/health.json"), body).unwrap();
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("BLOCK_BUTTON")
            .env("HOME", self.root.join("home"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("CCMONETA_CCUSAGE", self.root.join("fake-ccusage"));
        c
    }

    fn bar(&self, format: &str) -> String {
        let out = self.cmd(&["bar", "--format", format]).output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    fn hook(&self) -> String {
        let mut child = self
            .cmd(&["hook"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"model":{"display_name":"Opus 5"},"rate_limits":{"five_hour":{"used_percentage":7,"resets_at":4000000000}}}"#)
            .unwrap();
        let Output { stdout, .. } = child.wait_with_output().unwrap();
        String::from_utf8(stdout).unwrap()
    }

    fn doctor(&self) -> String {
        let out = self
            .cmd(&["doctor"])
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The health job writes its state the moment it attempts a run, so the
    /// absence of these files proves no surface in this test reached the status
    /// page or the version lookup. These tests must stay offline.
    fn health_job_ever_ran(&self) -> bool {
        self.root.join("cache/ccmoneta/jobs/health.json").exists()
            || self
                .root
                .join("cache/ccmoneta/jobs/health.spawned")
                .exists()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn an_outage_and_an_old_install_reach_every_surface() {
    let sb = Sandbox::new("bad");
    sb.write_health("major", "2.1.267", "2.1.280", true);

    let bar = sb.bar("i3blocks");
    let full = bar.lines().next().unwrap();
    assert!(full.contains("⚠ claude outage · cc 2.1.280"), "bar: {full}");

    let waybar: serde_json::Value = serde_json::from_str(sb.bar("waybar").trim()).unwrap();
    let class = waybar["class"].as_array().unwrap();
    assert!(
        class.iter().any(|c| c == "attention"),
        "waybar class: {class:?}"
    );
    let tooltip = waybar["tooltip"].as_str().unwrap();
    assert!(
        tooltip.contains("incident: Elevated errors (major, investigating)"),
        "tooltip: {tooltip}"
    );
    assert!(
        tooltip.contains("Claude Code 2.1.267 → 2.1.280 available (latest channel)"),
        "tooltip: {tooltip}"
    );

    let hook = sb.hook();
    assert!(
        hook.contains("⚠ claude outage · cc 2.1.280"),
        "hook: {hook}"
    );

    assert!(
        !sb.health_job_ever_ran(),
        "a fresh snapshot must not be refreshed"
    );

    let doctor = sb.doctor();
    assert!(doctor.contains("warn  status: Partial outage"), "{doctor}");
    assert!(
        doctor.contains("warn  Claude Code 2.1.267 → 2.1.280 available"),
        "{doctor}"
    );
    assert!(
        doctor.contains("warn  background auto-updates are off; run `claude update`"),
        "{doctor}"
    );
}

#[test]
fn a_healthy_check_stays_out_of_the_way() {
    let sb = Sandbox::new("good");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::write(
        sb.root.join("cache/ccmoneta/health.json"),
        format!(
            r#"{{"checked_at": {now},
                "status": {{"indicator": "none", "description": "All Systems Operational",
                            "claude_code": "operational", "degraded": [], "incidents": []}},
                "status_error": null,
                "version": {{"installed": "2.1.274", "latest": "2.1.274", "channel": "latest",
                             "source": "native", "behind": false, "skipped": null,
                             "background_updates": true}},
                "version_error": null}}"#
        ),
    )
    .unwrap();

    // Nothing on the block or the status line; the detail is in the tooltip.
    let full = sb.bar("i3blocks").lines().next().unwrap().to_string();
    assert!(!full.contains('⚠'), "bar: {full}");
    let hook = sb.hook();
    assert!(!hook.contains('⚠'), "hook: {hook}");

    let waybar: serde_json::Value = serde_json::from_str(sb.bar("waybar").trim()).unwrap();
    assert!(
        !waybar["class"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "attention"),
        "{waybar}"
    );
    assert!(
        waybar["tooltip"]
            .as_str()
            .unwrap()
            .contains("Claude Code 2.1.274 is current (latest channel)"),
        "{waybar}"
    );

    assert!(
        !sb.health_job_ever_ran(),
        "a fresh snapshot must not be refreshed"
    );

    let doctor = sb.doctor();
    assert!(
        doctor.contains("ok    status: All Systems Operational"),
        "{doctor}"
    );
    assert!(
        doctor.contains("ok    Claude Code 2.1.274 is current (latest channel)"),
        "{doctor}"
    );
}

#[test]
fn health_can_be_turned_off_entirely() {
    let sb = Sandbox::new("off");
    fs::write(
        sb.root.join("config/ccmoneta/config.toml"),
        "[health]\nstatus = false\nversion = false\n",
    )
    .unwrap();
    sb.write_health("major", "2.1.267", "2.1.280", true);

    // The cache file is there and says something is wrong, but with both checks
    // off there is no health section and no marker on any surface.
    let doctor = sb.doctor();
    // The section header, not a substring: the sandbox path contains "health".
    assert!(!doctor.lines().any(|l| l == "health"), "{doctor}");
    let full = sb.bar("i3blocks").lines().next().unwrap().to_string();
    assert!(!full.contains('⚠'), "bar: {full}");
    assert!(!sb.hook().contains('⚠'), "hook");
    assert!(
        !sb.health_job_ever_ran(),
        "checks are off, so nothing may poll"
    );
}
