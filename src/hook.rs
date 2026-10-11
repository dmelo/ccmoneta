//! statusLine hook.
//!
//! Claude Code pipes a JSON payload to the configured statusLine command on
//! every turn and shows whatever it prints. The payload already carries
//! `rate_limits`, so the hook saves them: the cheapest possible source, with no
//! network, credentials or rate limit. It then prints from the cache and starts
//! a background cost refresh if one is due, so it never makes Claude Code wait.

use std::io::Read;

use crate::accounts;
use crate::config::Config;
use crate::cost;
use crate::health;
use crate::limits::{self, Limits, Snapshot};
use crate::refresh;
use crate::store::{self, Job};

pub fn run(cfg: &Config) -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 1;
    }
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&input) else {
        return 1;
    };

    // The payload's limits are those of the account this Claude Code runs
    // under, so they are saved to that account alone. Under a config directory
    // no aimux profile names, they are shown but not saved: written anywhere,
    // they would overwrite another account's.
    let accounts = accounts::local();
    let several = accounts::several(&accounts);
    // With one account there is nothing to confuse it with, whatever
    // CLAUDE_CONFIG_DIR says.
    let current = if several {
        accounts::current(&accounts)
    } else {
        accounts.first()
    };
    let from_payload = limits::from_statusline(&payload);
    let shown = if from_payload.has_windows() {
        if let Some(a) = current {
            let _ = limits::save_for(
                a,
                several,
                &Snapshot {
                    limits: from_payload.clone(),
                    captured_at: store::now(),
                    source: "statusline".into(),
                    error: None,
                    read_at: None,
                },
            );
        }
        from_payload
    } else {
        // The payload has no windows until a session's first API response.
        // Show the last known ones rather than blanking them, and do not
        // overwrite a good snapshot with an empty one.
        current
            .and_then(|a| limits::load_for(a, several))
            .map(|s| s.limits)
            .unwrap_or_default()
    };
    let name = current.filter(|_| several).map(|a| a.name.as_str());

    let snap = cost::load();
    refresh::cost_if_stale(cfg, snap.as_ref());
    refresh::sync_if_due(cfg);
    let health = cfg.health.any().then(health::load).flatten();
    refresh::health_if_due(cfg, health.as_ref());
    let label = cost::today_label(
        snap.as_ref(),
        &store::job_state(Job::Cost),
        store::now(),
        chrono::Local::now().date_naive(),
    );
    println!(
        "{}",
        render(
            &shown,
            name,
            &payload,
            &label,
            health::marker(health.as_ref())
        )
    );
    0
}

/// The line Claude Code shows. Kept terse: it sits under the prompt.
fn render(
    limits: &Limits,
    account: Option<&str>,
    payload: &serde_json::Value,
    cost_label: &str,
    health: Option<String>,
) -> String {
    let mut parts = Vec::new();
    if let Some(model) = payload["model"]["display_name"].as_str() {
        parts.push(model.to_string());
    }
    // With several accounts, say whose limits these are.
    if let Some(name) = account {
        parts.push(name.to_string());
    }
    if let Some(w) = limits.five_hour {
        parts.push(format!("5h {:.0}%", w.percent));
    }
    if let Some(w) = limits.seven_day {
        parts.push(format!("7d {:.0}%", w.percent));
    }
    parts.push(cost_label.to_string());
    // Only when something is wrong: this line sits under the prompt on every
    // turn, so a healthy service and a current install say nothing at all.
    if let Some(h) = health {
        parts.push(h);
    }
    parts.join(" · ")
}
