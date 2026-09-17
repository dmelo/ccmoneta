//! The dashboard.
//!
//!   spend, one row per day  | limits
//!                           | health
//!                           | models
//!                           | projects
//!
//! Spend gets the full-height column because it is the tallest pane: a row per
//! calendar day for a month. Limits are a few fixed lines, so they take a
//! strip.
//!
//! Everything on screen comes from the shared cache, re-read every couple of
//! seconds. The refreshes that fill it run as separate `ccmoneta refresh`
//! processes shared with the bar and the status line (see refresh.rs), so the
//! dashboard never blocks on ccusage or the network, and closing it does not
//! stop the bar or the status line from being correct.

use std::cell::Cell;
use std::time::{Duration, Instant};

use chrono::{Datelike, NaiveDate, Weekday};
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::config::Config;
use crate::cost::{self, CostSnapshot, Costs, Host};
use crate::health::{self, HealthSnapshot};
use crate::limits::{self, Snapshot, Window};
use crate::refresh;
use crate::store::{self, Job, JobState};

/// How often the cache files are re-read. They are small and local.
const REREAD: Duration = Duration::from_secs(2);

struct App {
    cfg: Config,
    limits: Option<Snapshot>,
    limits_state: JobState,
    cost: Option<CostSnapshot>,
    cost_state: JobState,
    health: Option<HealthSnapshot>,
    /// Hosts as they are now, for sync ages; the snapshot's copy is as of when
    /// it was gathered.
    hosts: Vec<Host>,
    refreshing: bool,
    read_at: Instant,
    /// Rows scrolled past the newest day.
    scroll: usize,
    /// The largest useful `scroll` for the current pane height. Written by the
    /// draw pass, which is the only place that knows how many rows fit, so the
    /// scroll keys can clamp to it and ↑ never has to unwind phantom overshoot.
    max_scroll: Cell<usize>,
}

impl App {
    fn new(cfg: Config) -> Self {
        let mut app = App {
            cfg,
            limits: None,
            limits_state: JobState::default(),
            cost: None,
            cost_state: JobState::default(),
            health: None,
            hosts: Vec::new(),
            refreshing: false,
            read_at: Instant::now(),
            scroll: 0,
            max_scroll: Cell::new(0),
        };
        app.reload();
        app
    }

    /// Re-read the cache, and start whatever refresh is due. Stale figures are
    /// shown at once with their age rather than hidden until a refresh lands.
    fn reload(&mut self) {
        self.limits = limits::load();
        self.limits_state = store::job_state(Job::Limits);
        self.cost = cost::load();
        self.cost_state = store::job_state(Job::Cost);
        self.hosts = cost::hosts();
        self.health = health::load();
        self.refreshing = store::is_running(Job::Cost);
        refresh::limits_if_due(&self.cfg, self.limits.as_ref());
        refresh::cost_if_stale(&self.cfg, self.cost.as_ref());
        refresh::sync_if_due(&self.cfg);
        refresh::health_if_due(&self.cfg, self.health.as_ref());
        self.read_at = Instant::now();
    }

    fn tick(&mut self) {
        if self.read_at.elapsed() >= REREAD {
            self.reload();
        }
    }

    /// `r`: refresh now, past the freshness thresholds. The lock still keeps it
    /// to one run, and the usage endpoint's backoff still holds.
    fn refresh(&mut self) {
        refresh::trigger(Job::Cost, true);
        refresh::trigger(Job::Limits, true);
        if self.cfg.health.any() {
            refresh::trigger(Job::Health, true);
        }
        self.reload();
    }

    fn costs(&self) -> Option<&Costs> {
        self.cost.as_ref().map(|s| &s.costs)
    }

    fn days(&self) -> i64 {
        self.cfg.cost.window_days
    }
}

fn pct_color(pct: f64) -> Color {
    match pct {
        p if p >= 75.0 => Color::Red,
        p if p >= 50.0 => Color::Yellow,
        _ => Color::Green,
    }
}

fn human_reset(secs: i64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}

fn human_tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.1}B", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}K", n as f64 / 1e3),
        n => n.to_string(),
    }
}

/// A horizontal bar `width` cells wide, filled to `ratio` using eighth-blocks
/// for sub-cell precision.
fn bar(ratio: f64, width: usize) -> String {
    const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let units = (ratio.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let full = units / 8;
    let mut s = "█".repeat(full);
    if full < width {
        s.push_str(EIGHTHS[units % 8]);
    }
    s
}

fn draw_limits(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" limits ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(snap) = &app.limits else {
        let text = match &app.limits_state.last_error {
            Some(e) => format!("no data yet: {e}"),
            None => "no data yet: fetching, or run Claude Code once".into(),
        };
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };

    // Our own bar rather than ratatui's Gauge: Gauge centres its label over the
    // fill, so the percentage ends up half-covered by the bar it describes.
    let bar_w = (inner.width as usize).saturating_sub(28).clamp(8, 24);
    let now = store::now();
    let row = |label: &str, w: Window| -> Line {
        let reset = w
            .resets_in(now)
            .map(|r| format!("  resets {}", human_reset(r)))
            .unwrap_or_default();
        let filled = bar(w.percent / 100.0, bar_w);
        let pad = " ".repeat(bar_w.saturating_sub(filled.chars().count()));
        Line::from(vec![
            Span::raw(format!("{label:<5}")),
            Span::styled(filled, Style::default().fg(pct_color(w.percent))),
            Span::styled(pad.replace(' ', "░"), Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(" {:>3.0}%", w.percent),
                Style::default()
                    .fg(pct_color(w.percent))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(reset, Style::default().fg(Color::Gray)),
        ])
    };

    let mut lines = Vec::new();
    if let Some(w) = snap.limits.five_hour {
        lines.push(row("5h", w));
    }
    if let Some(w) = snap.limits.seven_day {
        lines.push(row("7d", w));
    }
    // Shown only when the usage endpoint reports a separate Opus window (this
    // account's returns null), rather than as an empty row every time.
    if let Some(w) = snap.limits.seven_day_opus {
        lines.push(row("opus", w));
    }
    let age = snap.age(now);
    let staleness = if age > app.cfg.limits.max_age_seconds {
        format!("{} ago", human_reset(age))
    } else {
        "live".into()
    };
    lines.push(Line::styled(
        format!("via {} · {}", snap.source, staleness),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Scale ceiling for the day bars: roughly the 90th percentile of days with
/// usage. Scaling to the maximum lets one heavy day flatten every ordinary day
/// into the same stub; capping keeps ordinary days legible, and anything above
/// the cap is drawn full-width with a ▶ so its own printed figure carries it.
fn scale_cap(daily: &[(String, f64)]) -> f64 {
    let mut used: Vec<f64> = daily.iter().map(|(_, c)| *c).filter(|c| *c > 0.0).collect();
    if used.is_empty() {
        return 1.0;
    }
    used.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((used.len() - 1) as f64 * 0.9).floor() as usize;
    used[idx].max(0.01)
}

/// Per-host spend over the window, then, on a line of its own so the totals do
/// not push it off a narrow pane, how old each mirrored host's copy is. A
/// remote host's figures are only as current as its last `ccmoneta-sync`, so an
/// old or missing sync is called out instead of silently under-counting.
fn host_lines(c: &Costs, hosts: &[Host], days: i64) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let warn = Style::default().fg(Color::Yellow);
    let now = store::now();

    let mut spans = vec![Span::styled(format!("{days}d  "), dim)];
    for (i, (name, cost)) in c.by_host.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", dim));
        }
        spans.push(Span::raw(format!("{name} ${cost:.2}")));
    }
    let mut lines = vec![Line::from(spans)];

    // Indented to sit under the host names, past the "30d  " label.
    let mut sync = vec![Span::raw("     ")];
    for h in hosts.iter().filter(|h| h.remote) {
        if sync.len() > 1 {
            sync.push(Span::styled(" · ", dim));
        }
        match h.last_sync {
            Some(at) => {
                let age = (now - at).max(0);
                // Sync runs every 10 minutes by default; three missed runs is
                // when the copy is stale enough to matter.
                let style = if age > 30 * 60 { warn } else { dim };
                sync.push(Span::styled(
                    format!("{} synced {} ago", h.name, human_reset(age)),
                    style,
                ));
            }
            None => sync.push(Span::styled(format!("{} never synced", h.name), warn)),
        }
    }
    if sync.len() > 1 {
        lines.push(Line::from(sync));
    }
    lines
}

fn draw_spend(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" spend ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(c) = app.costs() else {
        let (text, color) = match &app.cost_state.last_error {
            Some(e) => (e.clone(), Color::Red),
            None => ("gathering…".to_string(), Color::DarkGray),
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(color))
                .wrap(ratatui::widgets::Wrap { trim: true }),
            inner,
        );
        return;
    };

    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::from(vec![
        Span::raw("today "),
        Span::styled(format!("${:.2}", c.today), bold),
        Span::raw("   7d "),
        Span::styled(format!("${:.2}", c.week), bold),
        Span::raw(format!("   {}d ", app.days())),
        Span::styled(format!("${:.2}", c.window), bold),
    ])];
    lines.extend(host_lines(c, &app.hosts, app.days()));
    lines.push(Line::from(""));

    // Newest first: today is what you open this to see.
    let days: Vec<&(String, f64)> = c.daily.iter().rev().collect();
    let body = (inner.height as usize).saturating_sub(lines.len());
    // Reserve a footer line only when rows are actually cut off, so a tall
    // terminal shows every day with nothing wasted.
    let visible = if days.len() > body {
        body.saturating_sub(1)
    } else {
        days.len()
    };
    let max_scroll = days.len().saturating_sub(visible);
    app.max_scroll.set(max_scroll);
    let scroll = app.scroll.min(max_scroll);

    // "Sep 12 Fri" (10) + 2 + bar + 2 + "$116.29" right-aligned in 9.
    let bar_w = (inner.width as usize).saturating_sub(23).max(4);
    let cap = scale_cap(&c.daily);
    let today = chrono::Local::now().date_naive();

    for (day, cost) in days.iter().skip(scroll).take(visible) {
        let Ok(date) = NaiveDate::parse_from_str(day, "%Y-%m-%d") else {
            continue;
        };
        let weekend = matches!(date.weekday(), Weekday::Sat | Weekday::Sun);
        let mut label_style = Style::default();
        if weekend {
            label_style = label_style.fg(Color::DarkGray);
        }
        if date == today {
            label_style = label_style.add_modifier(Modifier::BOLD);
        }

        let mut spans = vec![
            Span::styled(date.format("%b %d %a").to_string(), label_style),
            Span::raw("  "),
        ];
        let over = *cost > cap;
        if over {
            spans.push(Span::styled(
                "█".repeat(bar_w.saturating_sub(1)),
                Style::default().fg(Color::Cyan),
            ));
            spans.push(Span::styled("▶", Style::default().fg(Color::Yellow)));
        } else {
            let filled = bar(cost / cap, bar_w);
            let pad = bar_w.saturating_sub(filled.chars().count());
            spans.push(Span::styled(filled, Style::default().fg(Color::Cyan)));
            spans.push(Span::raw(" ".repeat(pad)));
        }
        let value_style = if *cost == 0.0 {
            Style::default().fg(Color::DarkGray)
        } else if over {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        spans.push(Span::styled(
            format!("  {:>9}", format!("${cost:.2}")),
            value_style,
        ));
        lines.push(Line::from(spans));
    }

    if visible < days.len() {
        let first = scroll + 1;
        let last = scroll + visible;
        lines.push(Line::styled(
            format!("↑↓ scroll · days {first}–{last} of {}", days.len()),
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Service status and the installed Claude Code version. Each line is coloured
/// by what it says, so a problem is visible without reading the words.
fn health_rows(app: &App) -> Vec<Line<'_>> {
    let Some(snap) = &app.health else {
        return vec![Line::styled(
            "checking…",
            Style::default().fg(Color::DarkGray),
        )];
    };
    let wrong = |l: &str| {
        l.starts_with("incident:")
            || l.contains("available")
            || l.starts_with("run `")
            || l.starts_with("background auto-updates")
            || (l.starts_with("status: ") && !l.contains("All Systems Operational"))
            || (l.contains(": ") && l.ends_with("_outage"))
            || l.ends_with("degraded_performance")
            || l.ends_with("partial_outage")
    };
    let unknown =
        |l: &str| l.contains("unavailable") || l.contains("unknown") || l.contains("skipped");
    health::lines(snap, store::now(), false)
        .into_iter()
        .map(|l| {
            let style = if wrong(&l) {
                Style::default().fg(Color::Red)
            } else if unknown(&l) {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Green)
            };
            Line::styled(l, style)
        })
        .collect()
}

fn draw_health(frame: &mut Frame, area: Rect, rows: Vec<Line>) {
    let block = Block::bordered().title(" health ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(rows), inner);
}

fn draw_list(frame: &mut Frame, area: Rect, title: &str, app: &App, rows: Vec<Line>) {
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.cost.is_some() {
        frame.render_widget(Paragraph::new(rows), inner);
    } else if app.cost_state.last_error.is_none() {
        frame.render_widget(
            Paragraph::new("…").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
    }
}

fn model_rows(app: &App) -> Vec<Line<'_>> {
    let Some(c) = app.costs() else {
        return vec![];
    };
    let total: f64 = c.models.iter().map(|m| m.cost).sum();
    let mut out: Vec<Line> = c
        .models
        .iter()
        .map(|m| {
            let share = if total > 0.0 {
                m.cost / total * 100.0
            } else {
                0.0
            };
            let name = m.model_name.trim_start_matches("claude-");
            Line::from(format!(
                "{name:<16} {:>9} {:>9} {:>4.0}%",
                human_tokens(m.input_tokens + m.output_tokens + m.cache_read_tokens),
                format!("${:.2}", m.cost),
                share
            ))
        })
        .collect();
    out.push(Line::styled(
        format!(
            "{:<16} {:>9}",
            "cache read",
            human_tokens(c.cache_read_tokens)
        ),
        Style::default().fg(Color::DarkGray),
    ));
    out
}

fn project_rows(app: &App) -> Vec<Line<'_>> {
    let Some(c) = app.costs() else {
        return vec![];
    };
    // Tag each project with its hosts only when more than one host is counted;
    // on a single machine the tag would say the same thing on every row.
    let tag_hosts = c.hosts.len() > 1;
    c.projects
        .iter()
        .map(|p| {
            let short: String = p.name.chars().take(24).collect();
            let mut spans = vec![Span::raw(format!(
                "{short:<26} {:>9}",
                format!("${:.2}", p.cost)
            ))];
            if tag_hosts {
                spans.push(Span::styled(
                    format!("  {}", p.hosts.join("+")),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

fn draw(frame: &mut Frame, app: &App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(outer[0]);

    let limit_lines = app.limits.as_ref().map_or(1, |s| {
        [
            s.limits.five_hour,
            s.limits.seven_day,
            s.limits.seven_day_opus,
        ]
        .iter()
        .filter(|w| w.is_some())
        .count()
            + 1
    });
    let models = model_rows(app);
    let health = if app.cfg.health.any() {
        health_rows(app)
    } else {
        vec![]
    };
    let mut constraints = vec![
        // Each pane sized to its content. Projects takes whatever is left: it
        // is the longest list and the least consulted.
        Constraint::Length(limit_lines as u16 + 2),
        Constraint::Length(models.len().max(1) as u16 + 2),
        Constraint::Min(3),
    ];
    if !health.is_empty() {
        constraints.insert(1, Constraint::Length(health.len() as u16 + 2));
    }
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(cols[1]);

    draw_spend(frame, cols[0], app);
    draw_limits(frame, right[0], app);
    let rest = if health.is_empty() {
        1
    } else {
        draw_health(frame, right[1], health);
        2
    };
    draw_list(frame, right[rest], " models ", app, models);
    draw_list(frame, right[rest + 1], " projects ", app, project_rows(app));
    let mut footer = String::from("q quit · r refresh · ↑↓ scroll");
    // How old the spend figures are. A failed refresh keeps the old figures up,
    // so this age, and the failure note, are what tell a current dashboard from
    // a stale one.
    if let Some(snap) = &app.cost {
        let age = (store::now() - snap.generated_at).max(0);
        footer.push_str(&format!(" · spend {} old", human_reset(age)));
        if app.cost_state.last_error.is_some() {
            footer.push_str(" · last refresh failed");
        }
    }
    if app.refreshing {
        footer.push_str(" · refreshing");
    }
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
        outer[1],
    );
}

pub fn run(cfg: Config) -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new(cfg);
    let result = loop {
        app.tick();
        if let Err(e) = terminal.draw(|frame| draw(frame, &app)) {
            break Err(e);
        }
        // Short poll so keys feel immediate without a busy loop.
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                KeyCode::Char('r') => app.refresh(),
                KeyCode::Down | KeyCode::Char('j') => {
                    app.scroll = (app.scroll + 1).min(app.max_scroll.get());
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.scroll = app.scroll.saturating_sub(1);
                }
                _ => {}
            }
        }
    };
    ratatui::restore();
    result
}
