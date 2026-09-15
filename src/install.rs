//! `ccmoneta install statusline|i3blocks|waybar`.
//!
//! `statusline` edits Claude Code's settings file, carefully: it backs the file
//! up, refuses to replace a different status line without `--force`, keeps the
//! other settings and their order, keeps the file's permissions, and reads back
//! what it wrote. `i3blocks` and `waybar` only print a block to paste, because a
//! bar's config is the user's to edit.

use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::config::Config;
use crate::{bar, store};

/// Claude Code reads user settings from `$CLAUDE_CONFIG_DIR/settings.json`,
/// with the directory defaulting to `~/.claude`.
pub fn settings_path() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| store::home().join(".claude"))
        .join("settings.json")
}

fn exe() -> String {
    store::self_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "ccmoneta".into())
}

/// A word for a POSIX shell command line, quoted only when it needs to be, so
/// the usual path stays readable.
pub fn sh_word(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[derive(Debug, PartialEq)]
pub enum Merge {
    /// The status line is already exactly this command.
    Unchanged,
    /// The new text of the settings file.
    Write(String),
    /// A different status line is configured; its current value.
    Conflict(Value),
}

/// Merge `statusLine` into the settings text, leaving every other key and the
/// key order as they were.
pub fn merge_statusline(
    existing: Option<&str>,
    command: &str,
    force: bool,
) -> Result<Merge, String> {
    let mut settings = match existing.map(str::trim) {
        None | Some("") => Map::new(),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => map,
            Ok(_) => return Err("it is not a JSON object".into()),
            Err(e) => return Err(format!("it is not valid JSON ({e}); fix it first")),
        },
    };
    let wanted = json!({ "type": "command", "command": command });
    match settings.get("statusLine") {
        Some(current) if *current == wanted => return Ok(Merge::Unchanged),
        Some(current) if !force => return Ok(Merge::Conflict(current.clone())),
        _ => {}
    }
    settings.insert("statusLine".into(), wanted);
    let mut text =
        serde_json::to_string_pretty(&Value::Object(settings)).map_err(|e| e.to_string())?;
    text.push('\n');
    Ok(Merge::Write(text))
}

fn statusline(force: bool) -> i32 {
    let path = settings_path();
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("ccmoneta install: reading {}: {e}", path.display());
            return 1;
        }
    };
    let command = format!("{} hook", sh_word(&exe()));
    let text = match merge_statusline(existing.as_deref(), &command, force) {
        Err(e) => {
            eprintln!("ccmoneta install: not changing {}: {e}", path.display());
            return 1;
        }
        Ok(Merge::Unchanged) => {
            println!(
                "The status line is already installed in {}.",
                path.display()
            );
            return 0;
        }
        Ok(Merge::Conflict(current)) => {
            eprintln!(
                "ccmoneta install: {} already has a different status line:\n  {current}\nRun `ccmoneta install statusline --force` to replace it; the file is backed up first.",
                path.display()
            );
            return 1;
        }
        Ok(Merge::Write(text)) => text,
    };

    let permissions = std::fs::metadata(&path).ok().map(|m| m.permissions());
    if existing.is_some() {
        let backup = path.with_extension(format!("json.bak-ccmoneta-{}", store::now()));
        if let Err(e) = std::fs::copy(&path, &backup) {
            eprintln!("ccmoneta install: backing up {}: {e}", path.display());
            return 1;
        }
        println!("Backed up {} to {}.", path.display(), backup.display());
    }
    if let Err(e) = store::write_text(&path, &text) {
        eprintln!("ccmoneta install: writing {}: {e}", path.display());
        return 1;
    }
    if let Some(p) = permissions {
        let _ = std::fs::set_permissions(&path, p);
    }

    let landed = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let command_now = landed
        .as_ref()
        .and_then(|v| v.pointer("/statusLine/command"))
        .and_then(Value::as_str);
    if command_now == Some(command.as_str()) {
        println!("Installed the status line in {}: {command}", path.display());
        0
    } else {
        eprintln!(
            "ccmoneta install: {} did not read back as expected; restore it from the backup",
            path.display()
        );
        1
    }
}

fn i3blocks() -> i32 {
    let exe = sh_word(&exe());
    println!(
        "\
# Add this to your i3blocks config, then reload i3.
[ccmoneta]
command={exe} bar
interval=60
# Optional: redraw as soon as a refresh finishes, not on the next interval.
# Uncomment the line below, and add to ~/.config/ccmoneta/config.toml:
#   [bar]
#   signal = 12
#   program = \"i3blocks\"
#signal=12"
    );
    0
}

fn waybar(cfg: &Config) -> i32 {
    let exe = sh_word(&exe());
    let terminal = bar::pick_terminal(
        cfg.bar.terminal.as_deref(),
        std::env::var("TERMINAL").ok().as_deref(),
        bar::on_path,
    )
    .unwrap_or_else(|| "x-terminal-emulator".into());
    let module = json!({
        "custom/ccmoneta": {
            "exec": format!("{exe} bar --format waybar"),
            "return-type": "json",
            "interval": 60,
            "on-click": format!("{} -e {exe}", sh_word(&terminal)),
        }
    });
    println!(
        "// Add \"custom/ccmoneta\" to one of the modules lists in your Waybar config,\n// and this module definition alongside them:"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&module).unwrap_or_default()
    );
    println!(
        "\
// style.css can target #custom-ccmoneta, and its classes: ok, warn, critical,
// unknown, stale and error.
// Optional: redraw as soon as a refresh finishes. Add \"signal\": 12 to the
// module, and to ~/.config/ccmoneta/config.toml:
//   [bar]
//   signal = 12
//   program = \"waybar\""
    );
    0
}

pub fn run(cfg: &Config, args: &[String]) -> i32 {
    let force = args.iter().any(|a| a == "--force");
    let rest: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--force")
        .collect();
    match (rest.as_slice(), force) {
        (["statusline"], _) => statusline(force),
        (["i3blocks"], false) => i3blocks(),
        (["waybar"], false) => waybar(cfg),
        _ => {
            eprintln!("usage: ccmoneta install statusline [--force] | i3blocks | waybar");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "/usr/local/bin/ccmoneta hook";

    #[test]
    fn a_missing_or_empty_file_gets_just_the_status_line() {
        for existing in [None, Some(""), Some("  \n")] {
            let Merge::Write(text) = merge_statusline(existing, CMD, false).unwrap() else {
                panic!("expected a write");
            };
            let v: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                v,
                json!({ "statusLine": { "type": "command", "command": CMD } })
            );
        }
    }

    #[test]
    fn other_settings_and_their_order_are_kept() {
        let existing =
            r#"{"model": "opus", "permissions": {"allow": ["Bash"]}, "effortLevel": "high"}"#;
        let Merge::Write(text) = merge_statusline(Some(existing), CMD, false).unwrap() else {
            panic!("expected a write");
        };
        let order: Vec<usize> = [
            "\"model\"",
            "\"permissions\"",
            "\"effortLevel\"",
            "\"statusLine\"",
        ]
        .iter()
        .map(|k| text.find(k).unwrap())
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{text}");
        assert!(text.ends_with('\n'));
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["permissions"]["allow"][0], "Bash");
    }

    #[test]
    fn the_same_status_line_is_left_alone_and_a_different_one_needs_force() {
        let same = format!(r#"{{"statusLine": {{"type": "command", "command": "{CMD}"}}}}"#);
        assert_eq!(
            merge_statusline(Some(&same), CMD, false).unwrap(),
            Merge::Unchanged
        );

        let other = r#"{"statusLine": {"type": "command", "command": "other-tool"}}"#;
        assert!(matches!(
            merge_statusline(Some(other), CMD, false).unwrap(),
            Merge::Conflict(_)
        ));
        assert!(matches!(
            merge_statusline(Some(other), CMD, true).unwrap(),
            Merge::Write(_)
        ));
    }

    #[test]
    fn invalid_settings_are_refused() {
        assert!(merge_statusline(Some("{not json"), CMD, true).is_err());
        assert!(merge_statusline(Some("[1, 2]"), CMD, true).is_err());
    }

    #[test]
    fn shell_words_are_quoted_only_when_needed() {
        assert_eq!(
            sh_word("/home/me/.local/bin/ccmoneta"),
            "/home/me/.local/bin/ccmoneta"
        );
        assert_eq!(
            sh_word("/opt/My Tools/ccmoneta"),
            "'/opt/My Tools/ccmoneta'"
        );
        assert_eq!(sh_word("it's"), r"'it'\''s'");
        assert_eq!(sh_word(""), "''");
    }
}
