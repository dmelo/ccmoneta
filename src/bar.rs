//! Status bar block.
//!
//! `--format i3blocks`, the default, prints the three-line plain-text form
//! (full_text, short_text, color) that i3blocks understands; i3blocks assembles
//! the i3bar JSON itself, and swaybar renders the result the same way.
//! `--format waybar` prints one JSON line with `text`, `tooltip`, `class` and
//! `percentage`, the shape a Waybar custom module with `"return-type": "json"`
//! expects (waybar-custom(5)).
//!
//! The block only reads the cache. When something is stale it starts a
//! background refresh and shows what it has, so a bar tick never waits on the
//! network or on ccusage.

use chrono::NaiveDate;

use crate::config::Config;
use crate::cost::{self, CostSnapshot, Host};
use crate::limits::{self, Snapshot};
use crate::refresh;
use crate::store::{self, Job, JobState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    I3blocks,
    Waybar,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        match s {
            "i3blocks" => Some(Format::I3blocks),
            "waybar" => Some(Format::Waybar),
            _ => None,
        }
    }
}

/// Everything read from the cache that a bar shows.
pub struct Inputs<'a> {
    pub cfg: &'a Config,
    pub limits: Option<&'a Snapshot>,
    pub limits_state: &'a JobState,
    pub cost: Option<&'a CostSnapshot>,
    pub cost_state: &'a JobState,
    pub hosts: &'a [Host],
    pub now: i64,
    pub today: NaiveDate,
}

/// What a bar shows, before it is printed in either format.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub full: String,
    pub short: String,
    pub color: &'static str,
    /// "ok", "warn", "critical", or "unknown" when there are no limits yet.
    pub level: &'static str,
    /// The fuller of the two windows, 0-100.
    pub percentage: u8,
    pub tooltip: String,
    /// The limits are older than the poll threshold.
    pub stale: bool,
    /// Nothing usable for today's spend, and the last refresh failed.
    pub cost_error: bool,
}

/// Matches the thresholds the macOS app uses, so the two read the same.
fn level(pct: f64) -> (&'static str, &'static str) {
    match pct {
        p if p >= 75.0 => ("critical", "#ff5555"),
        p if p >= 50.0 => ("warn", "#f1fa8c"),
        _ => ("ok", "#50fa7b"),
    }
}

fn short_reset(secs: i64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 24 {
        format!("{}d", h / 24)
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m")
    }
}

fn ago(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}

fn cost_lines(inp: &Inputs) -> Vec<String> {
    let mut out = Vec::new();
    let today = inp.today.format("%Y-%m-%d").to_string();
    match inp.cost.filter(|s| s.date == today) {
        Some(s) => {
            let c = &s.costs;
            out.push(format!(
                "today ${:.2} · 7d ${:.2} · {}d ${:.2}",
                c.today, c.week, inp.cfg.cost.window_days, c.window
            ));
            if c.by_host.len() > 1 {
                let hosts: Vec<String> = c
                    .by_host
                    .iter()
                    .map(|(name, v)| format!("{name} ${v:.2}"))
                    .collect();
                out.push(hosts.join(" · "));
            }
            out.push(format!(
                "spend gathered {} ago",
                ago(inp.now - s.generated_at)
            ));
        }
        None => out.push("spend: nothing gathered for today yet".into()),
    }
    for h in inp.hosts.iter().filter(|h| h.remote) {
        out.push(match h.last_sync {
            Some(t) => format!("{} synced {} ago", h.name, ago(inp.now - t)),
            None => format!("{} never synced", h.name),
        });
    }
    if let Some(e) = &inp.cost_state.last_error {
        out.push(format!("last spend refresh failed: {e}"));
    }
    out
}

pub fn view(inp: &Inputs) -> View {
    let cost_label = cost::today_label(inp.cost, inp.cost_state, inp.now, inp.today);
    let cost_error = cost_label == "$?";

    let Some(snap) = inp.limits else {
        // No limits cached yet: the first poll is running, or keeps failing.
        let marker = if inp.limits_state.failures > 0 {
            "?"
        } else {
            "…"
        };
        let mut tooltip = vec![match &inp.limits_state.last_error {
            Some(e) => format!("limits unavailable: {e}"),
            None => "limits: not fetched yet".to_string(),
        }];
        tooltip.extend(cost_lines(inp));
        return View {
            full: format!("cc {marker} · {cost_label}"),
            short: format!("cc {marker}"),
            color: "#6272a4",
            level: "unknown",
            percentage: 0,
            tooltip: tooltip.join("\n"),
            stale: false,
            cost_error,
        };
    };

    let five = snap.limits.five_hour;
    let seven = snap.limits.seven_day;
    let worst = [five, seven]
        .into_iter()
        .flatten()
        .map(|w| w.percent)
        .fold(0.0_f64, f64::max);
    let stale = snap.age(inp.now) > inp.cfg.limits.max_age_seconds;

    let mut full = String::new();
    if let Some(w) = five {
        full.push_str(&format!("5h {:.0}%", w.percent));
        if let Some(r) = w.resets_in(inp.now) {
            full.push_str(&format!(" ({})", short_reset(r)));
        }
    }
    if let Some(w) = seven {
        if !full.is_empty() {
            full.push_str(" · ");
        }
        full.push_str(&format!("7d {:.0}%", w.percent));
    }
    full.push_str(&format!(" · {cost_label}"));
    // Limits older than the poll threshold are marked rather than hidden.
    if stale {
        full.push_str(" ⋯");
    }

    let short = match (five, seven) {
        (Some(a), Some(b)) => format!("{:.0}/{:.0}", a.percent, b.percent),
        (Some(a), None) => format!("{:.0}%", a.percent),
        (None, Some(b)) => format!("{:.0}%", b.percent),
        (None, None) => "?".into(),
    };

    let mut tooltip = Vec::new();
    for (label, window) in [
        ("5h", five),
        ("7d", seven),
        ("opus", snap.limits.seven_day_opus),
    ] {
        if let Some(w) = window {
            let reset = w
                .resets_in(inp.now)
                .map(|r| format!(", resets in {}", ago(r)))
                .unwrap_or_default();
            tooltip.push(format!("{label} {:.0}%{reset}", w.percent));
        }
    }
    tooltip.push(format!(
        "limits via {}, {} ago",
        snap.source,
        ago(snap.age(inp.now))
    ));
    tooltip.extend(cost_lines(inp));

    let (level, color) = level(worst);
    View {
        full,
        short,
        color,
        level,
        percentage: worst.round().clamp(0.0, 100.0) as u8,
        tooltip: tooltip.join("\n"),
        stale,
        cost_error,
    }
}

pub fn i3blocks(v: &View) -> String {
    format!("{}\n{}\n{}", v.full, v.short, v.color)
}

/// One line: serde_json escapes the tooltip's newlines, and Waybar requires the
/// whole object on a single line.
pub fn waybar(v: &View) -> String {
    let mut class = vec![v.level];
    if v.stale {
        class.push("stale");
    }
    if v.cost_error {
        class.push("error");
    }
    serde_json::json!({
        "text": v.full,
        "tooltip": v.tooltip,
        "class": class,
        "percentage": v.percentage,
    })
    .to_string()
}

/// The terminal to open the dashboard in: the config, then $TERMINAL, then the
/// first of i3-sensible-terminal and x-terminal-emulator found on PATH.
pub fn pick_terminal(
    configured: Option<&str>,
    env: Option<&str>,
    on_path: impl Fn(&str) -> bool,
) -> Option<String> {
    if let Some(t) = configured.filter(|t| !t.trim().is_empty()) {
        return Some(t.to_string());
    }
    if let Some(t) = env.filter(|t| !t.trim().is_empty()) {
        return Some(t.to_string());
    }
    ["i3-sensible-terminal", "x-terminal-emulator"]
        .into_iter()
        .find(|t| on_path(t))
        .map(String::from)
}

pub fn on_path(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(program))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

pub fn run(cfg: &Config, format: Format) -> i32 {
    // A left click on an i3blocks block re-runs this command with BLOCK_BUTTON
    // set, so the block is its own launcher. Waybar clicks run the module's
    // own `on-click` command instead.
    if format == Format::I3blocks && std::env::var("BLOCK_BUTTON").as_deref() == Ok("1") {
        let terminal = pick_terminal(
            cfg.bar.terminal.as_deref(),
            std::env::var("TERMINAL").ok().as_deref(),
            on_path,
        );
        let exe = store::self_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "ccmoneta".into());
        if let Some(terminal) = terminal {
            let _ = std::process::Command::new(terminal)
                .args(["-e", &exe])
                .spawn();
        }
    }

    let limits_snap = limits::load();
    refresh::limits_if_due(cfg, limits_snap.as_ref());
    let cost_snap = cost::load();
    refresh::cost_if_stale(cfg, cost_snap.as_ref());
    refresh::sync_if_due(cfg);

    let limits_state = store::job_state(Job::Limits);
    let cost_state = store::job_state(Job::Cost);
    let hosts = cost::hosts();
    let v = view(&Inputs {
        cfg,
        limits: limits_snap.as_ref(),
        limits_state: &limits_state,
        cost: cost_snap.as_ref(),
        cost_state: &cost_state,
        hosts: &hosts,
        now: store::now(),
        today: chrono::Local::now().date_naive(),
    });
    match format {
        Format::I3blocks => println!("{}", i3blocks(&v)),
        Format::Waybar => println!("{}", waybar(&v)),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::Costs;
    use crate::limits::{Limits, Window};
    use std::path::PathBuf;

    const NOW: i64 = 1_800_000_000;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 15).unwrap()
    }

    fn limits(five: f64, seven: f64, age: i64) -> Snapshot {
        let w = |p| {
            Some(Window {
                percent: p,
                resets_at: Some(NOW + 3 * 3600),
            })
        };
        Snapshot {
            limits: Limits {
                five_hour: w(five),
                seven_day: w(seven),
                ..Default::default()
            },
            captured_at: NOW - age,
            source: "statusline".into(),
        }
    }

    fn cost_snap() -> CostSnapshot {
        CostSnapshot {
            generated_at: NOW - 60,
            date: "2026-09-15".into(),
            costs: Costs {
                today: 12.34,
                week: 50.0,
                window: 200.0,
                by_host: vec![("laptop".into(), 150.0), ("desk".into(), 50.0)],
                ..Default::default()
            },
        }
    }

    fn remote(name: &str, last_sync: Option<i64>) -> Host {
        Host {
            name: name.into(),
            config_dir: PathBuf::new(),
            remote: true,
            last_sync,
        }
    }

    fn render(
        limits: Option<&Snapshot>,
        limits_state: &JobState,
        cost: Option<&CostSnapshot>,
        cost_state: &JobState,
        hosts: &[Host],
    ) -> View {
        let cfg = Config::default();
        view(&Inputs {
            cfg: &cfg,
            limits,
            limits_state,
            cost,
            cost_state,
            hosts,
            now: NOW,
            today: today(),
        })
    }

    #[test]
    fn i3blocks_prints_full_short_and_colour() {
        let ok = JobState::default();
        let l = limits(7.0, 60.0, 30);
        let c = cost_snap();
        let v = render(Some(&l), &ok, Some(&c), &ok, &[]);
        assert_eq!(
            i3blocks(&v),
            "5h 7% (3h00m) · 7d 60% · $12.34\n7/60\n#f1fa8c"
        );
        assert_eq!((v.level, v.percentage, v.stale), ("warn", 60, false));
    }

    #[test]
    fn waybar_is_one_json_line_with_the_documented_keys() {
        let ok = JobState::default();
        let l = limits(80.0, 20.0, 3600);
        let c = cost_snap();
        let hosts = [remote("laptop", Some(NOW - 300))];
        let out = waybar(&render(Some(&l), &ok, Some(&c), &ok, &hosts));
        assert!(!out.contains('\n'), "{out}");
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(json["text"], "5h 80% (3h00m) · 7d 20% · $12.34 ⋯");
        assert_eq!(json["percentage"], 80);
        assert_eq!(json["class"], serde_json::json!(["critical", "stale"]));
        let tooltip = json["tooltip"].as_str().unwrap();
        assert!(tooltip.contains("5h 80%, resets in 3h 00m"), "{tooltip}");
        assert!(
            tooltip.contains("today $12.34 · 7d $50.00 · 30d $200.00"),
            "{tooltip}"
        );
        assert!(
            tooltip.contains("laptop $150.00 · desk $50.00"),
            "{tooltip}"
        );
        assert!(tooltip.contains("laptop synced 5m ago"), "{tooltip}");
    }

    #[test]
    fn thresholds_pick_the_level() {
        assert_eq!(level(49.9).0, "ok");
        assert_eq!(level(50.0).0, "warn");
        assert_eq!(level(75.0).0, "critical");
    }

    #[test]
    fn missing_limits_and_a_failing_cost_refresh_are_marked() {
        let failing = JobState {
            failures: 1,
            last_error: Some("ccusage not found".into()),
            ..Default::default()
        };
        let ok = JobState::default();
        let v = render(None, &ok, None, &failing, &[]);
        assert_eq!(v.full, "cc … · $?");
        assert_eq!((v.level, v.cost_error), ("unknown", true));
        assert!(
            v.tooltip
                .contains("last spend refresh failed: ccusage not found")
        );
        let json: serde_json::Value = serde_json::from_str(&waybar(&v)).unwrap();
        assert_eq!(json["class"], serde_json::json!(["unknown", "error"]));

        let limits_failing = JobState {
            failures: 2,
            ..Default::default()
        };
        assert_eq!(render(None, &limits_failing, None, &ok, &[]).short, "cc ?");
    }

    #[test]
    fn terminal_comes_from_config_then_env_then_defaults() {
        let none = |_: &str| false;
        let sensible = |t: &str| t == "i3-sensible-terminal";
        assert_eq!(
            pick_terminal(Some("alacritty"), Some("kitty"), sensible).as_deref(),
            Some("alacritty")
        );
        assert_eq!(
            pick_terminal(None, Some("kitty"), sensible).as_deref(),
            Some("kitty")
        );
        assert_eq!(
            pick_terminal(None, Some(""), sensible).as_deref(),
            Some("i3-sensible-terminal")
        );
        assert_eq!(
            pick_terminal(None, None, |t: &str| t == "x-terminal-emulator").as_deref(),
            Some("x-terminal-emulator")
        );
        assert_eq!(pick_terminal(None, None, none), None);
    }
}
