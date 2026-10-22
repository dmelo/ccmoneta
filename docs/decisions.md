# Decisions

Why things are the way they are, with the evidence and what would reopen each. Architecture lives in [design.md](design.md); this file is the choices around it.

## Build this rather than adopt an existing tool (2026-09-15)

A survey of what already existed split cleanly in two, and nothing spanned both halves the way this needed:

- **Spend, but no limits.** [ccusage](https://github.com/ccusage/ccusage) is the de-facto standard (18k+ stars, actively maintained) and computes cost from transcripts, but knows nothing about a plan's 5-hour and 7-day limits: its statusline mode exposes only pricing, cache and context-window options.
- **Limits, but stale or differently shaped spend.** [CCMeter](https://github.com/hmenzagh/CCMeter) reads the real limits from `/api/oauth/usage` *and* prices transcripts, but its built-in price table stopped at an older Opus: a `model.contains(...)` match fell through to a Sonnet-rate fallback, so newer Opus usage was billed at 3/15 instead of 5/25, silently, with no "unknown model" marker anywhere in its UI. [ccburn](https://github.com/JuanjoFuchs/ccburn) reads the same endpoint and documents the 429 throttling that makes polling it unreliable.
- **Bars.** The Linux status-bar ecosystem is Waybar-first ([ai-usagebar](https://github.com/akitaonrails/ai-usagebar), [claudebar](https://github.com/mryll/claudebar), GNOME extensions, a KDE plasmoid). Nothing spoke the i3blocks format.

So: own the limits and the layout, delegate pricing to ccusage, and never carry a price table. **Reopen if** ccusage starts reporting plan limits, or if a tool appears that covers both halves on the surfaces this targets.

## Delegate pricing to ccusage, permanently (2026-09-15)

**Superseded on 2026-10-21** by the next entry; kept for the reasoning, which still holds for the price table.

A hard-coded price table is the single failure that made an otherwise good tool report wrong dollars, and it fails *silently*: usage of a model the table does not know is priced as something else. ccusage maintains pricing as its whole reason to exist. `CCMONETA_CCUSAGE` selects the executable, which is also what makes the tests deterministic. **Reopen only** if ccusage is abandoned; even then, a wrong-price marker in the UI is a precondition.

## Compute spend here, by ccusage's rules and from its price source (2026-10-21)

Reopened for a reason the entry above did not foresee: running ccusage became the expensive part of ccmoneta. ccusage takes a directory and rereads every transcript in its window each time it runs, which on a heavily used machine is several gigabytes every few minutes; and to split spend by account, ccmoneta had to lay out each account's share of the transcripts as files for it to read, which at its worst wrote over a gigabyte per refresh. The rereading cannot be changed from outside ccusage; the per-account files can be cut down to hard links and the files that changed, but stay a second tree of transcripts to keep in step.

So ccmoneta reads the transcripts itself, incrementally, and keeps tokens in a ledger (see design.md, Cost). What made the entry above right is kept:

- **No price table here.** Rates come from LiteLLM's model list, the list ccusage prices with, downloaded and cached. Nothing in this repo names a model's price.
- **No silent wrong price.** A model the list does not name counts as $0 and is shown as `no price` on the dashboard and named by `ccmoneta doctor`: the wrong-price marker the entry above made a precondition.
- **ccusage's semantics, not new ones.** Which lines count, deduplication, day boundaries and the per-request formula are ported from ccusage, and the result was checked against it on real data: the same token totals, and the same dollars per day, account, machine and model for every model the list names.

**Reopen if** the figures drift from ccusage's for current Claude models, or LiteLLM's list stops being maintained.

## Name: ccmoneta (2026-09-15)

`ccstats` was checked and taken where it matters: the `ccstats` crate on crates.io is an active coding-agent cost analytics tool, so `cargo install ccstats` installs that; npm's `ccstats` is a Claude Code session statistics tool; two Go commands and at least six GitHub projects use it for Claude Code usage. `ccmeter` is worse: ten same-name GitHub projects in this space plus an npm package and a Homebrew tap. Alternatives with a plain description (`cchud`, `ccpulse`, `ccquota`, `cctally`, `ccgauge`, `quotabar`, `claude-hud`, `limitline`, `loadline`, `burnwatch`, `tokentide`, `fuelgauge`, `meterline`, `tidemark`) each already had a Claude or AI-usage project of the same name; `tallyline` was clean but says nothing about Claude. `ccmoneta` was free on crates.io, npm, PyPI and Homebrew core, with no GitHub repository of that name.

Moneta was the Roman goddess whose temple housed the mint, giving English *money*; her name is traditionally traced to *monere*, to warn, the root of *monitor*. Nearest confusable is ccMonet, an unrelated AI bookkeeping product. **Reopen if** publishing publicly and the similarity to ccMonet causes confusion.

## MIT licence (2026-09-15)

Matches the author's other published tool. The text is the standard MIT licence with the copyright holder named.

## Public repository (2026-09-17)

Private at first (2026-09-15), because the development history's commit messages carry real machine names and spend figures. Made public on 2026-09-17, after two scans of the published tree found no secrets, credentials, IP addresses, home paths, project names or real host names, in the files or in the commit messages; path examples in tests are `/home/me/...` and hosts are `desk` and `laptop`.

The one thing the second scan did turn up was the open question the private-repo entry had left: four places quoted measurements taken from the author's own machines — a day's spend used as a column-width example, the size of one mirrored host's transcripts, single-run timings, and the dollar difference between the two ccusage reports. None identified anyone, but together they described how heavily the author uses Claude Code, and the repo's own rule in ../CLAUDE.md forbids real spend figures. Each was replaced with the finding it supported rather than deleted: the session report still "came out materially higher", runs still take "on the order of a second per host", and the column arithmetic uses a made-up `$123.45`.

**What keeps this safe going forward:** the `history` branch must never be pushed, and a public remote makes an accidental `git push --all` or `--mirror` worse than it was before. Push `main` by name.
