# Ideas

What the other Claude Code monitoring tools do that ccmoneta does not, and which of it is worth taking. Surveyed 2026-09-17. This is a backlog, not a plan: nothing here is committed to, and each entry says what it would cost us.

The ranking weighs one thing above novelty — **does it use data we already have, on a surface we already own?** ccmoneta's advantage is that it is the only tool covering a terminal dashboard, an i3blocks/Waybar block and the status line from one cache, and that it reads *real* plan limits rather than inferring them.

## The landscape

| Tool | Shape | What it is best at |
|---|---|---|
| [ccusage](https://github.com/ryoppippi/ccusage) | CLI | the pricing engine we already delegate to; `daily`, `weekly`, `monthly`, `session`, `blocks`, `statusline`; now reads **18 agent CLIs**, not only Claude Code |
| [Claude-Code-Usage-Monitor](https://github.com/Maciek-roboblog/Claude-Code-Usage-Monitor) | TUI | live burn rate, time-to-limit forecasting, P90 limit discovery, plan auto-detection |
| [claude-dashboard](https://github.com/uppinote20/claude-dashboard) | status line | context-window bar, budget thresholds, depletion estimates, cache-efficiency badge |
| [ccflare](https://github.com/snipeship/ccflare) / [sniffly](https://github.com/chiphuyen/sniffly) | web dashboard | long-range analytics; sniffly adds **error analysis** across transcripts |
| [claude-powerline](https://github.com/Owloops/claude-powerline) | status line | plugin-native install, themes, vim-style segments |
| [claude-usage](https://github.com/phuryn/claude-usage), [claude-code-stats](https://github.com/AeternaLabsHQ/claude-code-stats) | local web UI | Chart.js history, model filters, date ranges |

## Worth doing, in order

### 1. Context window on the status line and the bar

The payload Claude Code already hands our hook carries `context_window.used_percentage`, `remaining_percentage`, `context_window_size`, the four token counts, and `exceeds_200k_tokens`. Verified against the installed binary's own schema, not documentation.

Cost: near zero — no new I/O, no network, no ccusage call. It is the one number that changes a decision *during* a turn ("compact now, or start a session"), and every status-line tool in the survey shows it. Both fields can be null before a session's first response, which we already handle for limits.

### 2. Burn rate and time to limit

The single reason Claude-Code-Usage-Monitor exists. We have the two windows and their reset times, but only as instants; keeping a small ring of past samples in the cache turns them into a rate, and a rate answers the question a percentage cannot: *will I hit the wall before it resets?*

Cost: a bounded history file plus a projection function. No new source of truth — it is arithmetic on limits we already poll. Show it only when a window is actually going to be exhausted first.

### 3. Budgets with thresholds

A configured daily, weekly or 30-day budget, with the bar changing colour and `notify-send` firing once per threshold crossing. claude-dashboard uses 80% and 95%.

Cost: small, config plus a crossing-detector that must not re-fire on every bar tick. Fits our existing `class`/colour plumbing exactly.

### 4. Other agent CLIs, not just Claude Code

ccusage now reports 18 agent CLIs (Codex, OpenClaw, Gemini CLI, Copilot CLI and more) with `--by-agent`. Anyone running more than one agent is now paying in several places and seeing it in none.

Cost: moderate. Our `gather()` already runs one ccusage report per host; a second axis (per agent) means deciding whether the header total means "Claude Code" or "everything", which is a product decision before it is a code one.

### 5. Five-hour blocks as a first-class view

`ccusage blocks` groups usage into the same 5-hour windows the plan limits use, with burn rate and a projected block total. Our spend view is per calendar day, which does not line up with how the limit actually resets.

Cost: one more ccusage report, plus a pane. Pairs naturally with #2.

### 6. Read the newer `rate_limits.limits[]` shape

The binary carries a newer rate-limit schema: a `limits` array whose rows are classified by `kind` (`session`, `weekly_all`, `weekly_scoped`), explicitly documented as "classify a row on this, never on a label", alongside `subscription_type` and per-model scoping. We parse the older `five_hour` / `seven_day` / `spend_limit` object.

Cost: small, and it is **maintenance, not a feature** — the day the old shape goes away, every number we show goes blank. Worth doing before it is urgent.

### 7. Weekly grouping in the dashboard

ccusage added a `weekly` report. A month of daily rows answers "what did Tuesday cost"; a week view answers "is this month worse than the last", which is the question a monthly bill actually poses.

## Worth considering

- **Cache efficiency.** We already collect cache-read tokens; a hit ratio is a derived number away, and the payload also exposes `prompt_cache.hit_ratio`. Useful once, then rarely — it changes behaviour less than the tools showing it imply.
- **Cost per change.** The payload has `total_lines_added` / `total_lines_removed` and `pr.number`; cost per PR is a genuinely novel metric nobody in the survey computes. Fun, and possibly a trap: it rewards the wrong thing if taken seriously.
- **An MCP server.** ccusage ships one, so Claude itself can answer "what have I spent today". Niche while the status line already shows it.
- **Terminal title / tmux segment.** Cheap, and `ccmoneta bar` is most of it already.
- **Error analysis across transcripts** (sniffly's angle). The transcripts we already mirror contain every failed tool call; nobody in this space surfaces *why* sessions went badly. Furthest from what ccmoneta is for, and the most interesting thing on this page.

## Not worth taking

- **P90 / historical limit discovery.** Claude-Code-Usage-Monitor infers limits from past usage because it has no better source. We read the real ones from the usage endpoint; inferring them would be a downgrade.
- **Plan auto-detection.** Same reason: `subscription_type` is in the payload.
- **A web dashboard.** ccflare, sniffly and two others already do it well, and it is a fourth surface to keep correct — the opposite of this project's premise.
- **Our own price table.** Settled in [decisions.md](decisions.md), and CCMeter is the cautionary tale.
