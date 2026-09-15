//! An unreadable `cost.json`, against a throwaway cache.
//!
//! HOME has no credentials and ccusage is `false`, so any refresh a surface
//! starts fails without touching the network or the file under test.

use std::fs;
use std::process::{Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_ccmoneta");

#[test]
fn an_unreadable_cost_json_is_logged_once_per_version() {
    let root =
        std::env::temp_dir().join(format!("ccmoneta-cost-cache-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for d in ["home/.claude/projects", "cache/ccmoneta", "config"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    let cost = root.join("cache/ccmoneta/cost.json");
    let log = root.join("cache/ccmoneta/refresh.log");
    let bar = || {
        let ok = Command::new(BIN)
            .arg("bar")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("BLOCK_BUTTON")
            .env("HOME", root.join("home"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("CCMONETA_CCUSAGE", "false")
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok);
    };
    let noted = || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("cost.json unreadable"))
            .count()
    };

    // The format written before snapshots existed.
    fs::write(&cost, r#"{"today": 18.02, "captured_at": 1789000000}"#).unwrap();
    for _ in 0..3 {
        bar();
    }
    assert_eq!(noted(), 1, "one version of the file, logged once");

    // A later, different unreadable version is logged again.
    std::thread::sleep(Duration::from_millis(20));
    fs::write(&cost, "not json").unwrap();
    bar();
    bar();
    assert_eq!(noted(), 2);

    let _ = fs::remove_dir_all(&root);
}
