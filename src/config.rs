//! `$XDG_CONFIG_HOME/ccmoneta/config.toml`, default `~/.config/ccmoneta/config.toml`.
//!
//! Every key is optional, and a missing file means the defaults. Unknown keys
//! are an error rather than being ignored, so a typo is caught instead of
//! silently leaving a default in place. The dashboard and `ccmoneta refresh`
//! print the error; the bar and the hook fall back to the defaults.

use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub cost: CostConfig,
    pub limits: LimitsConfig,
    pub sync: SyncConfig,
    pub bar: BarConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CostConfig {
    /// Re-gather spend once the cached figures are older than this.
    pub refresh_seconds: i64,
    /// Days of history, counting today.
    pub window_days: i64,
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            refresh_seconds: 180,
            window_days: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Poll the usage endpoint once the cached limits are older than this.
    pub max_age_seconds: i64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_age_seconds: 900,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    /// Sync a host once its last attempt is older than this.
    pub refresh_seconds: i64,
    /// Copy transcripts modified within this many days.
    pub days: i64,
    pub hosts: Vec<HostConfig>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            refresh_seconds: 600,
            days: 31,
            hosts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// The label in the dashboard, the name of its directory in the cache, and
    /// the ssh destination when `ssh` is not set.
    pub name: String,
    /// An ssh destination: `host`, `user@host`, or an alias from ~/.ssh/config.
    #[serde(default)]
    pub ssh: Option<String>,
    /// Where that machine keeps transcripts. Default `~/.claude/projects`.
    #[serde(default)]
    pub remote_dir: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BarConfig {
    /// The terminal a click on the bar block opens the dashboard in. Default:
    /// $TERMINAL, then i3-sensible-terminal, then x-terminal-emulator.
    pub terminal: Option<String>,
    /// After a refresh, send SIGRTMIN+`signal` to processes named `program`, so
    /// the bar redraws at once rather than on its next interval.
    pub signal: Option<i32>,
    pub program: Option<String>,
}

/// Usable both as a directory name under the cache and as an ssh destination:
/// letters, digits, '.', '_' and '-', not starting with '.' or '-'.
pub fn valid_host_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub fn path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::store::home().join(".config"))
        .join("ccmoneta")
        .join("config.toml")
}

pub fn parse(text: &str) -> Result<Config, String> {
    let cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
    if cfg.cost.window_days < 1 {
        return Err("cost.window_days must be at least 1".into());
    }
    if cfg.cost.refresh_seconds < 30 {
        return Err("cost.refresh_seconds must be at least 30".into());
    }
    if cfg.limits.max_age_seconds < 60 {
        return Err("limits.max_age_seconds must be at least 60".into());
    }
    if cfg.sync.days < 1 {
        return Err("sync.days must be at least 1".into());
    }
    if cfg.sync.refresh_seconds < 60 {
        return Err("sync.refresh_seconds must be at least 60".into());
    }
    let mut seen = BTreeSet::new();
    for h in &cfg.sync.hosts {
        if !valid_host_name(&h.name) {
            return Err(format!(
                "sync.hosts: {:?} is not a usable name (letters, digits, '.', '_' and '-'; not starting with '.' or '-')",
                h.name
            ));
        }
        if !seen.insert(h.name.as_str()) {
            return Err(format!("sync.hosts: {:?} appears twice", h.name));
        }
        // A destination starting with '-' would reach ssh as an option.
        if let Some(ssh) = &h.ssh
            && (ssh.is_empty() || ssh.starts_with('-') || ssh.chars().any(char::is_whitespace))
        {
            return Err(format!(
                "sync.hosts.{}: ssh must be a destination such as user@host, with no spaces and no leading '-'",
                h.name
            ));
        }
        // It reaches both a remote shell and rsync's remote path, so it is
        // kept to characters neither treats specially.
        if let Some(dir) = &h.remote_dir
            && (dir.is_empty()
                || !dir
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~')))
        {
            return Err(format!(
                "sync.hosts.{}: remote_dir may contain only letters, digits and / . _ - ~",
                h.name
            ));
        }
    }
    if let Some(t) = &cfg.bar.terminal
        && t.trim().is_empty()
    {
        return Err("bar.terminal must not be empty".into());
    }
    match (cfg.bar.signal, cfg.bar.program.as_deref()) {
        (None, None) => {}
        (Some(n), Some(program)) => {
            // i3blocks and Waybar both document the range as 1 to N, where
            // SIGRTMIN+N = SIGRTMAX; N is 30 with glibc's SIGRTMIN of 34.
            if !(1..=30).contains(&n) {
                return Err("bar.signal must be between 1 and 30".into());
            }
            // pkill -x compares against the kernel's process name, which is cut
            // to 15 characters, and a longer pattern matches nothing. A leading
            // '-' would be read as an option.
            let usable = !program.is_empty()
                && program.len() <= 15
                && !program.starts_with(['-', '.'])
                && program
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
            if !usable {
                return Err(
                    "bar.program must be a process name of at most 15 letters, digits, '.', '_' or '-', such as i3blocks or waybar"
                        .into(),
                );
            }
        }
        _ => {
            return Err(
                "bar.signal and bar.program must be set together: an unhandled real-time signal terminates the process that receives it, so one is only sent to a program named here"
                    .into(),
            );
        }
    }
    Ok(cfg)
}

/// The config, or the defaults together with why the file could not be used.
pub fn load() -> (Config, Option<String>) {
    let path = path();
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), None),
        Err(e) => (Config::default(), Some(format!("{}: {e}", path.display()))),
        Ok(text) => match parse(&text) {
            Ok(cfg) => (cfg, None),
            Err(e) => (Config::default(), Some(format!("{}: {e}", path.display()))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_the_defaults() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn a_partial_section_keeps_the_other_defaults() {
        let cfg = parse("[cost]\nrefresh_seconds = 60\n").unwrap();
        assert_eq!(cfg.cost.refresh_seconds, 60);
        assert_eq!(cfg.cost.window_days, 30);
        assert_eq!(cfg.limits.max_age_seconds, 900);
    }

    #[test]
    fn unknown_keys_are_an_error() {
        assert!(parse("[cost]\nrefresh_secs = 60\n").is_err());
        assert!(parse("[colour]\n").is_err());
    }

    #[test]
    fn nonsense_values_are_an_error() {
        assert!(parse("[cost]\nwindow_days = 0\n").is_err());
        assert!(parse("[cost]\nrefresh_seconds = 1\n").is_err());
        assert!(parse("[limits]\nmax_age_seconds = 5\n").is_err());
        assert!(parse("[sync]\ndays = 0\n").is_err());
        assert!(parse("[sync]\nrefresh_seconds = 5\n").is_err());
    }

    #[test]
    fn sync_hosts_parse_with_optional_fields() {
        let cfg = parse(
            "[[sync.hosts]]\nname = \"laptop\"\n\n[[sync.hosts]]\nname = \"box\"\nssh = \"me@box.local\"\nremote_dir = \"~/alt/projects\"\n",
        )
        .unwrap();
        assert_eq!(cfg.sync.hosts.len(), 2);
        assert_eq!(cfg.sync.hosts[0].ssh, None);
        assert_eq!(cfg.sync.hosts[1].ssh.as_deref(), Some("me@box.local"));
        assert_eq!(cfg.sync.days, 31);
    }

    #[test]
    fn unsafe_or_duplicate_hosts_are_an_error() {
        let host = |body: &str| parse(&format!("[[sync.hosts]]\n{body}\n"));
        assert!(host("name = \"../evil\"").is_err());
        assert!(host("name = \".hidden\"").is_err());
        assert!(host("name = \"-oProxyCommand=x\"").is_err());
        assert!(host("name = \"ok\"\nssh = \"-oProxyCommand=x\"").is_err());
        assert!(host("name = \"ok\"\nssh = \"a b\"").is_err());
        assert!(host("name = \"ok\"\nremote_dir = \"~/a b\"").is_err());
        assert!(host("name = \"ok\"\nremote_dir = \"~/x;rm -rf ~\"").is_err());
        assert!(host("name = \"ok\"\nport = 22").is_err());
        assert!(parse("[[sync.hosts]]\nname = \"a\"\n[[sync.hosts]]\nname = \"a\"\n").is_err());
    }

    #[test]
    fn host_names_are_checked_for_the_cache_and_ssh() {
        assert!(valid_host_name("desk"));
        assert!(valid_host_name("laptop.local"));
        assert!(!valid_host_name(""));
        assert!(!valid_host_name("a/b"));
        assert!(!valid_host_name(".."));
        assert!(!valid_host_name("-x"));
    }

    #[test]
    fn a_bar_signal_needs_a_named_program_and_sane_values() {
        assert!(parse("[bar]\nsignal = 10\nprogram = \"i3blocks\"\n").is_ok());
        assert!(parse("[bar]\nterminal = \"alacritty\"\n").is_ok());
        assert!(parse("[bar]\nsignal = 10\n").is_err());
        assert!(parse("[bar]\nprogram = \"waybar\"\n").is_err());
        assert!(parse("[bar]\nsignal = 0\nprogram = \"waybar\"\n").is_err());
        assert!(parse("[bar]\nsignal = 31\nprogram = \"waybar\"\n").is_err());
        assert!(parse("[bar]\nsignal = 5\nprogram = \"a-sixteen-chars-\"\n").is_err());
        assert!(parse("[bar]\nsignal = 5\nprogram = \"-x\"\n").is_err());
        assert!(parse("[bar]\nterminal = \" \"\n").is_err());
    }
}
