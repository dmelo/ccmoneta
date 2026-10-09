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
//! ↑↓ (or the mouse wheel) move a cursor through the days; Enter or a click
//! pins that day, and models and projects then show that day alone, titled
//! with its date. Enter on the pinned day, or Esc, returns to the window.
//!
//! Everything on screen comes from the shared cache, re-read every couple of
//! seconds. The refreshes that fill it run as separate `ccmoneta refresh`
//! processes shared with the bar and the status line (see refresh.rs), so the
//! dashboard never blocks on ccusage or the network, and closing it does not
//! stop the bar or the status line from being correct.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use chrono::{Datelike, NaiveDate, Weekday};
use ratatui::Frame;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::accounts;
use crate::config::Config;
use crate::cost::{self, CostSnapshot, Costs, Host, ModelBreakdown, Project};
use crate::health::{self, HealthSnapshot};
use crate::limits::{self, AccountLimits, Snapshot, Window};
use crate::refresh;
use crate::store::{self, Job, JobState, ago};

/// How often the cache files are re-read. They are small and local.
const REREAD: Duration = Duration::from_secs(2);

struct App {
    cfg: Config,
    /// Every account's limits; one, unnamed, without aimux.
    limits: Vec<AccountLimits>,
    limits_state: JobState,
    cost: Option<CostSnapshot>,
    cost_state: JobState,
    health: Option<HealthSnapshot>,
    /// Hosts as they are now, for sync ages; the snapshot's copy is as of when
    /// it was gathered.
    hosts: Vec<Host>,
    refreshing: bool,
    read_at: Instant,
    /// The day under the cursor, as an index into the days newest first.
    cursor: usize,
    /// The day models and projects are showing, "YYYY-MM-DD"; None for the
    /// whole window. Kept as a date, not an index, so it stays on its day when
    /// midnight shifts every row down one.
    pinned: Option<String>,
    /// Rows scrolled past the newest day. Set by the draw pass, the only place
    /// that knows how many rows fit, so that the cursor stays in view.
    scroll: Cell<usize>,
    /// Where each visible day was drawn: (screen row, index newest first),
    /// for mapping a click back to its day. Written by the draw pass.
    day_rows: RefCell<Vec<(u16, usize)>>,
    /// The spend pane, so a click elsewhere on a matching row is not a day.
    spend_area: Cell<Rect>,
    /// The limits pane's inner width, for sizing its bars. Set by the draw pass.
    limits_width: Cell<u16>,
}

impl App {
    fn new(cfg: Config) -> Self {
        let mut app = App {
            cfg,
            limits: Vec::new(),
            limits_state: JobState::default(),
            cost: None,
            cost_state: JobState::default(),
            health: None,
            hosts: Vec::new(),
            refreshing: false,
            read_at: Instant::now(),
            cursor: 0,
            pinned: None,
            scroll: Cell::new(0),
            day_rows: RefCell::new(Vec::new()),
            spend_area: Cell::new(Rect::default()),
            limits_width: Cell::new(0),
        };
        app.reload();
        app
    }

    /// Re-read the cache, and start whatever refresh is due. Stale figures are
    /// shown at once with their age rather than hidden until a refresh lands.
    fn reload(&mut self) {
        self.limits = limits::load_all(&accounts::local());
        self.limits_state = store::job_state(Job::Limits);
        self.cost = cost::load();
        self.cost_state = store::job_state(Job::Cost);
        self.hosts = cost::hosts();
        self.health = self.cfg.health.any().then(health::load).flatten();
        self.refreshing = store::is_running(Job::Cost);
        refresh::limits_if_due(&self.cfg, &self.limits);
        refresh::cost_if_stale(&self.cfg, self.cost.as_ref());
        refresh::sync_if_due(&self.cfg);
        refresh::health_if_due(&self.cfg, self.health.as_ref());
        self.read_at = Instant::now();
        // The window moves at midnight: a pinned day that has left it has
        // nothing left to show, and the cursor must stay on a row.
        let count = self.day_count();
        self.cursor = self.cursor.min(count.saturating_sub(1));
        if let (Some(day), Some(c)) = (&self.pinned, self.costs())
            && !c.daily.iter().any(|(d, _)| d == day)
        {
            self.pinned = None;
        }
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

    fn day_count(&self) -> usize {
        self.costs().map_or(0, |c| c.daily.len())
    }

    /// The date of the day at `index`, newest first.
    fn day_at(&self, index: usize) -> Option<String> {
        let c = self.costs()?;
        c.daily.iter().rev().nth(index).map(|(d, _)| d.clone())
    }

    fn move_cursor(&mut self, by: isize) {
        let last = self.day_count().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(by).min(last);
    }

    /// Pin the day at `index`, or unpin it if it is the one already pinned.
    fn toggle(&mut self, index: usize) {
        self.cursor = index;
        let day = self.day_at(index);
        self.pinned = if day == self.pinned { None } else { day };
    }

    /// The day index drawn at screen position (x, y), if it is one.
    fn day_under(&self, x: u16, y: u16) -> Option<usize> {
        let area = self.spend_area.get();
        if x < area.x || x >= area.x + area.width {
            return None;
        }
        self.day_rows
            .borrow()
            .iter()
            .find(|(row, _)| *row == y)
            .map(|(_, i)| *i)
    }

    /// What models and projects show: the pinned day, or the whole window.
    fn view(&self) -> Option<View<'_>> {
        let c = self.costs()?;
        Some(match &self.pinned {
            None => View {
                day: None,
                models: &c.models,
                projects: &c.projects,
                cache_read_tokens: c.cache_read_tokens,
                by_account: &c.by_account,
            },
            // A day with no usage has no entry; it shows as empty.
            Some(day) => {
                let d = c.days.get(day);
                View {
                    day: NaiveDate::parse_from_str(day, "%Y-%m-%d").ok(),
                    models: d.map_or(&[], |d| &d.models),
                    projects: d.map_or(&[], |d| &d.projects),
                    cache_read_tokens: d.map_or(0, |d| d.cache_read_tokens),
                    by_account: d.map_or(&[], |d| &d.by_account),
                }
            }
        })
    }
}

struct View<'a> {
    /// The pinned day; None for the whole window.
    day: Option<NaiveDate>,
    models: &'a [ModelBreakdown],
    projects: &'a [Project],
    cache_read_tokens: u64,
    /// Spend per Claude account; shown only when there are several.
    by_account: &'a [(String, f64)],
}

impl View<'_> {
    /// A pane title, naming the day when one is pinned.
    fn title(&self, pane: &str) -> String {
        match self.day {
            Some(d) => format!(" {pane} · {} ", d.format("%b %d %a")),
            None => format!(" {pane} "),
        }
    }
}

fn pct_color(pct: f64) -> Color {
    match pct {
        p if p >= 75.0 => Color::Red,
        p if p >= 50.0 => Color::Yellow,
        _ => Color::Green,
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

/// The limits pane's lines. One account shows its windows and where they came
/// from; several show each under its name.
fn limit_rows(app: &App) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let several = app.limits.len() > 1;
    let mut lines = Vec::new();
    for a in &app.limits {
        let name = a.account.titled();
        match &a.snap {
            Some(snap) => {
                let now = store::now();
                let age = snap.age(now);
                let staleness = if age > app.cfg.limits.max_age_seconds {
                    format!("{} ago", ago(age))
                } else {
                    "live".into()
                };
                let mut source = format!("via {} · {}", snap.source, staleness);
                // One account has no header to carry its plan, so it leads here.
                if let (false, Some(plan)) = (several, &a.account.plan) {
                    source = format!("{plan} · {source}");
                }
                if several {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{name}  "),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(source.clone(), dim),
                    ]));
                }
                lines.extend(window_rows(snap, app.limits_width.get(), now));
                if !several {
                    lines.push(Line::styled(source, dim));
                }
            }
            None => {
                let why = match &app.limits_state.last_error {
                    Some(e) => format!("no data yet: {e}"),
                    None => "no data yet: fetching, or run Claude Code once".into(),
                };
                if several {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{name}  "),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled("no data yet", dim),
                    ]));
                } else {
                    lines.push(Line::styled(why, dim));
                }
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled("no data yet", dim));
    }
    lines
}

/// One row per window an account reports.
fn window_rows(snap: &Snapshot, width: u16, now: i64) -> Vec<Line<'static>> {
    // Our own bar rather than ratatui's Gauge: Gauge centres its label over the
    // fill, so the percentage ends up half-covered by the bar it describes.
    let bar_w = (width as usize).saturating_sub(28).clamp(8, 24);
    let row = |label: &str, w: Window| -> Line<'static> {
        let reset = w
            .resets_in(now)
            .map(|r| format!("  resets {}", ago(r)))
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
    lines
}

fn draw_limits(frame: &mut Frame, area: Rect, rows: Vec<Line>) {
    let block = Block::bordered().title(" limits ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(rows), inner);
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
/// Spend per Claude account, when aimux runs several: over the window, or for
/// the pinned day, labelled to match the panes below.
fn account_line(app: &App, c: &Costs) -> Option<Line<'static>> {
    if c.by_account.len() < 2 {
        return None;
    }
    let dim = Style::default().fg(Color::DarkGray);
    let view = app.view()?;
    let label = match view.day {
        Some(d) => d.format("%b %d").to_string(),
        None => format!("{}d", app.days()),
    };
    // Two spaces after the label, as the per-host line has after "30d".
    let mut spans = vec![Span::styled(format!("{label}  "), dim)];
    if view.by_account.is_empty() {
        spans.push(Span::styled("no usage", dim));
    }
    for (i, (name, cost)) in view.by_account.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", dim));
        }
        spans.push(Span::raw(format!("{name} ${cost:.2}")));
    }
    Some(Line::from(spans))
}

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
                    format!("{} synced {} ago", h.name, ago(age)),
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
    if let Some(line) = account_line(app, c) {
        lines.push(line);
    }
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
    // Scroll only as far as it takes to keep the cursor on screen.
    let cursor = app.cursor.min(days.len().saturating_sub(1));
    let mut scroll = app.scroll.get().min(max_scroll);
    if cursor < scroll {
        scroll = cursor;
    } else if visible > 0 && cursor >= scroll + visible {
        scroll = cursor + 1 - visible;
    }
    app.scroll.set(scroll);
    app.spend_area.set(inner);
    let mut rows = Vec::new();

    // "Sep 12 Fri" (10) + 2 + bar + 2 + "$123.45" right-aligned in 9.
    let bar_w = (inner.width as usize).saturating_sub(23).max(4);
    let cap = scale_cap(&c.daily);
    let today = chrono::Local::now().date_naive();

    for (index, (day, cost)) in days.iter().enumerate().skip(scroll).take(visible) {
        let Ok(date) = NaiveDate::parse_from_str(day, "%Y-%m-%d") else {
            continue;
        };
        rows.push((inner.y + lines.len() as u16, index));
        let weekend = matches!(date.weekday(), Weekday::Sat | Weekday::Sun);
        let mut label_style = Style::default();
        if weekend {
            label_style = label_style.fg(Color::DarkGray);
        }
        if date == today {
            label_style = label_style.add_modifier(Modifier::BOLD);
        }
        if app.pinned.as_deref() == Some(day.as_str()) {
            label_style = label_style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
        }
        if index == cursor {
            label_style = label_style.add_modifier(Modifier::REVERSED);
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
            format!("days {first}–{last} of {}", days.len()),
            Style::default().fg(Color::DarkGray),
        ));
    }
    app.day_rows.replace(rows);
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
    health::lines(snap)
        .into_iter()
        .chain(health::age_line(snap, store::now()))
        .map(|line| {
            let colour = match line.severity {
                health::Severity::Ok => Color::Green,
                health::Severity::Warn => Color::Red,
                health::Severity::Unknown => Color::DarkGray,
            };
            Line::styled(line.text, Style::default().fg(colour))
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
    let Some(v) = app.view() else {
        return vec![];
    };
    if v.models.is_empty() && v.day.is_some() {
        return vec![Line::styled(
            "no usage on this day",
            Style::default().fg(Color::DarkGray),
        )];
    }
    let total: f64 = v.models.iter().map(|m| m.cost).sum();
    let mut out: Vec<Line> = v
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
            human_tokens(v.cache_read_tokens)
        ),
        Style::default().fg(Color::DarkGray),
    ));
    out
}

fn project_rows(app: &App) -> Vec<Line<'_>> {
    let (Some(c), Some(v)) = (app.costs(), app.view()) else {
        return vec![];
    };
    // Tag each project with its hosts only when more than one host is counted;
    // on a single machine the tag would say the same thing on every row.
    let tag_hosts = c.hosts.len() > 1;
    v.projects
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

    // The bars inside are sized from the pane's width, known only here.
    app.limits_width.set(cols[1].width.saturating_sub(2));
    let limits = limit_rows(app);
    let models = model_rows(app);
    let health = if app.cfg.health.any() {
        health_rows(app)
    } else {
        vec![]
    };
    // Each pane is sized to its content as it is added, and remembers its own
    // index, so the health pane being absent cannot shift anything out from
    // under the panes below it. Projects takes whatever is left: it is the
    // longest list and the least consulted.
    let mut constraints = vec![Constraint::Length(limits.len() as u16 + 2)];
    let mut add = |c: Constraint| {
        constraints.push(c);
        constraints.len() - 1
    };
    let health_at = (!health.is_empty()).then(|| add(Constraint::Length(health.len() as u16 + 2)));
    let models_at = add(Constraint::Length(models.len().max(1) as u16 + 2));
    let projects_at = add(Constraint::Min(3));
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(cols[1]);

    draw_spend(frame, cols[0], app);
    draw_limits(frame, right[0], limits);
    if let Some(at) = health_at {
        draw_health(frame, right[at], health);
    }
    let (models_title, projects_title) = app.view().map_or_else(
        || (" models ".to_string(), " projects ".to_string()),
        |v| (v.title("models"), v.title("projects")),
    );
    draw_list(frame, right[models_at], &models_title, app, models);
    draw_list(
        frame,
        right[projects_at],
        &projects_title,
        app,
        project_rows(app),
    );
    let mut footer = String::from("q quit · r refresh · ↑↓ enter/click a day");
    if app.pinned.is_some() {
        footer.push_str(" · esc all days");
    }
    // How old the spend figures are. A failed refresh keeps the old figures up,
    // so this age, and the failure note, are what tell a current dashboard from
    // a stale one.
    if let Some(snap) = &app.cost {
        let age = (store::now() - snap.generated_at).max(0);
        footer.push_str(&format!(" · spend {} old", ago(age)));
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
    // Clicks pick a day. While the dashboard is open the terminal no longer
    // selects text on a plain drag; holding Shift usually still does.
    // Without it the keys still work, so a terminal that refuses is not fatal.
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let mut app = App::new(cfg);
    // Every exit goes through the `break` below, errors included, so the
    // terminal always gets mouse capture turned off and its screen restored.
    let result = loop {
        app.tick();
        if let Err(e) = terminal.draw(|frame| draw(frame, &app)) {
            break Err(e);
        }
        // Short poll so keys feel immediate without a busy loop.
        match event::poll(Duration::from_millis(250)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => break Err(e),
        }
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(e) => break Err(e),
        };
        match ev {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') => break Ok(()),
                // Esc backs out of a pinned day first, and quits from the window.
                KeyCode::Esc if app.pinned.is_some() => app.pinned = None,
                KeyCode::Esc => break Ok(()),
                KeyCode::Char('r') => app.refresh(),
                KeyCode::Down | KeyCode::Char('j') => app.move_cursor(1),
                KeyCode::Up | KeyCode::Char('k') => app.move_cursor(-1),
                KeyCode::Enter => app.toggle(app.cursor),
                _ => {}
            },
            Event::Mouse(m) => match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(i) = app.day_under(m.column, m.row) {
                        app.toggle(i);
                    }
                }
                MouseEventKind::ScrollDown => app.move_cursor(1),
                MouseEventKind::ScrollUp => app.move_cursor(-1),
                _ => {}
            },
            _ => {}
        }
    };
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}
