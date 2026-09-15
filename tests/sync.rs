//! `ccmoneta sync` against a fake remote.
//!
//! A fake ssh, installed through CCMONETA_SSH, drops ssh's options and the
//! destination and runs the remaining command locally with HOME set to a fake
//! remote home. The listing and rsync's transport both go through it, and real
//! rsync does the copying. Nothing touches the real cache, config or network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ccmoneta");

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Option<Self> {
        if Command::new("rsync")
            .arg("--version")
            .stdout(Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("skipping: rsync is not installed");
            return None;
        }
        let root =
            std::env::temp_dir().join(format!("ccmoneta-sync-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for d in [
            "home/.claude/projects",
            "remote/.claude/projects",
            "cache",
            "config/ccmoneta",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let ssh = root.join("fake-ssh");
        fs::write(
            &ssh,
            r#"#!/bin/bash
if [ "$FAKE_SSH_FAIL" = 1 ]; then echo "ssh: connect to host laptop.test port 22: No route to host" >&2; exit 255; fi
while [ $# -gt 0 ]; do
  case "$1" in -o) shift 2 ;; -*) shift ;; *) break ;; esac
done
shift
cd "$FAKE_REMOTE_HOME" && HOME="$FAKE_REMOTE_HOME" exec bash -c "$*"
"#,
        )
        .unwrap();
        let ccusage = root.join("fake-ccusage");
        fs::write(&ccusage, "#!/bin/bash\ncase \"$1\" in daily) printf '{\"daily\":[]}';; session) printf '{\"session\":[]}';; esac\n").unwrap();
        Command::new("chmod")
            .arg("+x")
            .arg(&ssh)
            .arg(&ccusage)
            .status()
            .unwrap();
        Some(Sandbox { root })
    }

    fn config(&self, toml: &str) {
        fs::write(self.root.join("config/ccmoneta/config.toml"), toml).unwrap();
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("BLOCK_BUTTON")
            .env("HOME", self.root.join("home"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("CCMONETA_SSH", self.root.join("fake-ssh"))
            .env("CCMONETA_CCUSAGE", self.root.join("fake-ccusage"))
            .env("FAKE_REMOTE_HOME", self.root.join("remote"));
        c
    }

    fn sync(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = self.cmd(&[&["sync"], args].concat());
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    fn remote(&self, rel: &str) -> PathBuf {
        self.root.join("remote/.claude/projects").join(rel)
    }

    fn copy(&self, host: &str, rel: &str) -> PathBuf {
        self.root.join("cache/ccmoneta/hosts").join(host).join(rel)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn put(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn age(path: &Path, when: &str) {
    assert!(
        Command::new("touch")
            .args(["-d", when])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
}

const LAPTOP: &str = "[[sync.hosts]]\nname = \"laptop\"\nssh = \"laptop.test\"\n";

#[test]
fn recent_transcripts_are_mirrored_and_aged_out_copies_pruned() {
    let Some(sb) = Sandbox::new("mirror") else {
        return;
    };
    sb.config(LAPTOP);
    put(&sb.remote("p1/recent.jsonl"), "a\n");
    put(&sb.remote("p1/old.jsonl"), "old\n");
    age(&sb.remote("p1/old.jsonl"), "40 days ago");
    put(
        &sb.remote("p2/uuid/subagents/workflows/wf_1/agent.jsonl"),
        "w\n",
    );
    put(&sb.copy("laptop", "projects/p9/gone.jsonl"), "gone\n");

    let out = sb.sync(&[], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("laptop: 2 files in window, pruned 1"),
        "{stdout}"
    );
    assert!(sb.copy("laptop", "projects/p1/recent.jsonl").exists());
    assert!(
        sb.copy(
            "laptop",
            "projects/p2/uuid/subagents/workflows/wf_1/agent.jsonl"
        )
        .exists()
    );
    assert!(
        !sb.copy("laptop", "projects/p1/old.jsonl").exists(),
        "outside the window"
    );
    assert!(
        !sb.copy("laptop", "projects/p9/gone.jsonl").exists(),
        "no longer on the remote"
    );
    assert!(
        !sb.copy("laptop", "projects/p9").exists(),
        "left empty by pruning"
    );
    assert!(sb.copy("laptop", "last-sync").exists());
    assert!(!sb.copy("laptop", "last-error").exists());
}

#[test]
fn an_unreachable_host_leaves_the_copy_and_records_why() {
    let Some(sb) = Sandbox::new("unreachable") else {
        return;
    };
    sb.config(LAPTOP);
    put(&sb.copy("laptop", "projects/p1/kept.jsonl"), "kept\n");
    put(&sb.copy("laptop", "last-sync"), "123");

    let out = sb.sync(&[], &[("FAKE_SSH_FAIL", "1")]);
    assert!(!out.status.success());
    assert!(sb.copy("laptop", "projects/p1/kept.jsonl").exists());
    assert_eq!(
        fs::read_to_string(sb.copy("laptop", "last-sync")).unwrap(),
        "123"
    );
    let err = fs::read_to_string(sb.copy("laptop", "last-error")).unwrap();
    assert!(err.contains("No route to host"), "{err}");
    assert!(sb.copy("laptop", "last-attempt").exists());
}

#[test]
fn an_empty_listing_keeps_the_copy() {
    let Some(sb) = Sandbox::new("empty") else {
        return;
    };
    sb.config(LAPTOP);
    put(&sb.copy("laptop", "projects/p1/kept.jsonl"), "kept\n");

    let out = sb.sync(&[], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(sb.copy("laptop", "projects/p1/kept.jsonl").exists());
}

#[test]
fn a_named_host_syncs_without_config_and_unsafe_names_are_refused() {
    let Some(sb) = Sandbox::new("ad-hoc") else {
        return;
    };
    put(&sb.remote("p1/recent.jsonl"), "a\n");

    let out = sb.sync(&["laptop.test"], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(sb.copy("laptop.test", "projects/p1/recent.jsonl").exists());

    let refused = sb.sync(&["../escape"], &[]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(!sb.root.join("cache/ccmoneta/escape").exists());
    assert!(!sb.root.join("cache/escape").exists());
}

#[test]
fn a_surface_starts_a_due_sync_on_its_own() {
    let Some(sb) = Sandbox::new("surface") else {
        return;
    };
    sb.config(LAPTOP);
    put(&sb.remote("p1/recent.jsonl"), "a\n");

    assert!(
        sb.cmd(&["bar"])
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    let start = Instant::now();
    while !sb.copy("laptop", "last-sync").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the bar did not start a sync"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(sb.copy("laptop", "projects/p1/recent.jsonl").exists());
}
