# Decisions

Why things are the way they are, with the evidence and what would reopen each. Architecture lives in [design.md](design.md); this file is the choices around it.

## Build this rather than adopt an existing tool (2026-09-15)

A survey of what already existed split cleanly in two, and nothing spanned both halves the way this needed:

- **Spend, but no limits.** [ccusage](https://github.com/ccusage/ccusage) is the de-facto standard (18k+ stars, actively maintained) and computes cost from transcripts, but knows nothing about a plan's 5-hour and 7-day limits: its statusline mode exposes only pricing, cache and context-window options.
- **Limits, but stale or differently shaped spend.** [CCMeter](https://github.com/hmenzagh/CCMeter) reads the real limits from `/api/oauth/usage` *and* prices transcripts, but its built-in price table stopped at an older Opus: a `model.contains(...)` match fell through to a Sonnet-rate fallback, so newer Opus usage was billed at 3/15 instead of 5/25, silently, with no "unknown model" marker anywhere in its UI. [ccburn](https://github.com/JuanjoFuchs/ccburn) reads the same endpoint and documents the 429 throttling that makes polling it unreliable.
- **Bars.** The Linux status-bar ecosystem is Waybar-first ([ai-usagebar](https://github.com/akitaonrails/ai-usagebar), [claudebar](https://github.com/mryll/claudebar), GNOME extensions, a KDE plasmoid). Nothing spoke the i3blocks format.

So: own the limits and the layout, delegate pricing to ccusage, and never carry a price table. **Reopen if** ccusage starts reporting plan limits, or if a tool appears that covers both halves on the surfaces this targets.

## Delegate pricing to ccusage, permanently (2026-09-15)

A hard-coded price table is the single failure that made an otherwise good tool report wrong dollars, and it fails *silently*: usage of a model the table does not know is priced as something else. ccusage maintains pricing as its whole reason to exist. `CCMONETA_CCUSAGE` selects the executable, which is also what makes the tests deterministic. **Reopen only** if ccusage is abandoned; even then, a wrong-price marker in the UI is a precondition.

## Name: ccmoneta (2026-09-15)

`ccstats` was checked and taken where it matters: the `ccstats` crate on crates.io is an active coding-agent cost analytics tool, so `cargo install ccstats` installs that; npm's `ccstats` is a Claude Code session statistics tool; two Go commands and at least six GitHub projects use it for Claude Code usage. `ccmeter` is worse: ten same-name GitHub projects in this space plus an npm package and a Homebrew tap. Alternatives with a plain description (`cchud`, `ccpulse`, `ccquota`, `cctally`, `ccgauge`, `quotabar`, `claude-hud`, `limitline`, `loadline`, `burnwatch`, `tokentide`, `fuelgauge`, `meterline`, `tidemark`) each already had a Claude or AI-usage project of the same name; `tallyline` was clean but says nothing about Claude. `ccmoneta` was free on crates.io, npm, PyPI and Homebrew core, with no GitHub repository of that name.

Moneta was the Roman goddess whose temple housed the mint, giving English *money*; her name is traditionally traced to *monere*, to warn, the root of *monitor*. Nearest confusable is ccMonet, an unrelated AI bookkeeping product. **Reopen if** publishing publicly and the similarity to ccMonet causes confusion.

## MIT licence (2026-09-15)

Matches the author's other published tool. The text is the standard MIT licence with the copyright holder named.

## Private repository, for now (2026-09-15)

Published to a private GitHub repo as a single squashed commit, because the development history's commit messages carry real machine names and spend figures. What keeps that safe is the `history` branch staying local (see ../CLAUDE.md).

**Open question:** whether to make the repository public. Nothing found so far blocks it — the published tree was scanned and holds no secrets, credentials, IP addresses, home paths, project names or real host names, and the squashed commit message has none either. What deserves a deliberate look first: `docs/design.md` and code comments quote measurements taken from the author's own usage (transcript volumes, run times, the size of one sync, a per-report difference in dollars), which are not identifying but do describe how heavily the author uses Claude Code.
