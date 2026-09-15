//! `ccmoneta install statusline`, against a throwaway CLAUDE_CONFIG_DIR.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_ccmoneta");

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "ccmoneta-install-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("claude")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        Sandbox { root }
    }

    fn settings(&self) -> PathBuf {
        self.root.join("claude/settings.json")
    }

    fn install(&self, extra: &[&str]) -> Output {
        Command::new(BIN)
            .args([&["install", "statusline"], extra].concat())
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .output()
            .unwrap()
    }

    fn backups(&self) -> usize {
        fs::read_dir(self.root.join("claude"))
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("settings.json.bak-ccmoneta-")
            })
            .count()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn installs_keeping_other_settings_their_order_and_permissions() {
    let sb = Sandbox::new("keep");
    fs::write(
        sb.settings(),
        "{\n  \"model\": \"opus\",\n  \"permissions\": {\"allow\": []}\n}\n",
    )
    .unwrap();
    fs::set_permissions(sb.settings(), fs::Permissions::from_mode(0o600)).unwrap();

    let out = sb.install(&[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = fs::read_to_string(sb.settings()).unwrap();
    let v = json(&sb.settings());
    assert_eq!(v["statusLine"]["command"], format!("{BIN} hook"));
    assert_eq!(v["statusLine"]["type"], "command");
    assert!(text.find("\"model\"").unwrap() < text.find("\"permissions\"").unwrap());
    assert!(text.find("\"permissions\"").unwrap() < text.find("\"statusLine\"").unwrap());
    assert_eq!(
        fs::metadata(sb.settings()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(sb.backups(), 1);

    let again = sb.install(&[]);
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stdout).contains("already installed"));
    assert_eq!(sb.backups(), 1, "an unchanged file is not backed up again");
}

#[test]
fn a_different_status_line_is_only_replaced_with_force() {
    let sb = Sandbox::new("force");
    let original = "{\"statusLine\": {\"type\": \"command\", \"command\": \"other-tool\"}}\n";
    fs::write(sb.settings(), original).unwrap();

    let refused = sb.install(&[]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(fs::read_to_string(sb.settings()).unwrap(), original);
    assert_eq!(sb.backups(), 0);

    let forced = sb.install(&["--force"]);
    assert!(forced.status.success());
    assert_eq!(
        json(&sb.settings())["statusLine"]["command"],
        format!("{BIN} hook")
    );
    assert_eq!(sb.backups(), 1);
}

#[test]
fn invalid_settings_are_left_untouched() {
    let sb = Sandbox::new("invalid");
    fs::write(sb.settings(), "{not json").unwrap();
    let out = sb.install(&["--force"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(fs::read_to_string(sb.settings()).unwrap(), "{not json");
    assert_eq!(sb.backups(), 0);
}

#[test]
fn a_missing_settings_file_is_created() {
    let sb = Sandbox::new("missing");
    let out = sb.install(&[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        json(&sb.settings()),
        serde_json::json!({ "statusLine": { "type": "command", "command": format!("{BIN} hook") } })
    );
}
