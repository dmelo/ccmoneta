//! `ccmoneta doctor`, against a throwaway environment: fake ssh, no
//! credentials, no status line.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_ccmoneta");

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "ccmoneta-doctor-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        for d in [
            "home/.claude/projects",
            "cache",
            "config/ccmoneta",
            "claude",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(
            root.join("config/ccmoneta/config.toml"),
            "[[sync.hosts]]\nname = \"laptop\"\nssh = \"laptop.test\"\n",
        )
        .unwrap();
        fs::write(
            root.join("fake-ssh"),
            "#!/bin/bash\nif [ \"$FAKE_SSH_FAIL\" = 1 ]; then echo 'Permission denied (publickey).' >&2; exit 255; fi\nexit 0\n",
        )
        .unwrap();
        Command::new("chmod")
            .arg("+x")
            .arg(root.join("fake-ssh"))
            .status()
            .unwrap();
        Sandbox { root }
    }

    fn doctor(&self, env: &[(&str, &str)]) -> (i32, String) {
        let mut c = Command::new(BIN);
        c.arg("doctor")
            .env("HOME", self.root.join("home"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CCMONETA_SSH", self.root.join("fake-ssh"));
        for (k, v) in env {
            c.env(k, v);
        }
        let Output { status, stdout, .. } = c.output().unwrap();
        (
            status.code().unwrap_or(-1),
            String::from_utf8_lossy(&stdout).into_owned(),
        )
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_working_setup_passes_and_says_what_is_limited() {
    let sb = Sandbox::new("healthy");
    let (code, out) = sb.doctor(&[]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("warn  no model prices yet"), "{out}");
    assert!(
        out.contains("ok    laptop: ssh to laptop.test works without prompting"),
        "{out}"
    );
    assert!(out.contains("warn  laptop: never synced"), "{out}");
    assert!(out.contains("warn  no Claude OAuth credentials"), "{out}");
    assert!(out.contains("warn  not installed in"), "{out}");
}

#[test]
fn an_unreachable_host_fails() {
    let sb = Sandbox::new("ssh");
    let (code, out) = sb.doctor(&[("FAKE_SSH_FAIL", "1")]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("FAIL  laptop: ssh to laptop.test failed"),
        "{out}"
    );
    assert!(out.contains("Permission denied (publickey)."), "{out}");
}

#[test]
fn models_without_a_price_are_named() {
    let sb = Sandbox::new("unpriced");
    let today = chrono::Local::now().format("%Y-%m-%d");
    fs::create_dir_all(sb.root.join("cache/ccmoneta")).unwrap();
    fs::write(
        sb.root.join("cache/ccmoneta/cost.json"),
        format!(
            r#"{{"generated_at": {}, "date": "{today}", "costs": {{"unpriced": ["vendor/new-model"]}}}}"#,
            chrono::Utc::now().timestamp()
        ),
    )
    .unwrap();
    let (_, out) = sb.doctor(&[]);
    assert!(
        out.contains("warn  no price for vendor/new-model: counted as $0"),
        "{out}"
    );
}

#[test]
fn a_bad_config_fails() {
    let sb = Sandbox::new("config");
    fs::write(
        sb.root.join("config/ccmoneta/config.toml"),
        "[cost]\nwindow_days = 0\n",
    )
    .unwrap();
    let (code, out) = sb.doctor(&[]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("FAIL") && out.contains("window_days"), "{out}");
}
