# ccmoneta design: correct numbers from any single surface

Status: implemented. The shared cache and background refresh landed in 33cdc65, sync in 056196e, the bar formats, portability and redraw signal in 8f788ac, logging of an unreadable cache in 66f964d, and `install` and `doctor` in fcb2a9d. "What goes wrong today" describes the code as of 77e2d1a, before this work.

## Goal

ccmoneta shows two things about Claude Code usage, the plan's 5-hour and 7-day limit percentages and the API-equivalent dollar spend, on three surfaces:

| Surface | Command | Lifetime |
|---|---|---|
| Dashboard (TUI) | `ccmoneta` | long-running, interactive |
| Status bar block (i3blocks, swaybar, Waybar) | `ccmoneta bar` | short-lived, re-run by the bar on an interval |
| Claude Code status line | `ccmoneta hook` | short-lived, re-run by Claude Code on every turn |

A user may run any one of these and none of the others. Whichever they run must show correct, current numbers on its own, and must say so when it cannot. The tool must also work for users other than its author: no machine names, paths or services from one setup built in, and nothing that has to be installed as a system service just to get correct numbers.

## What goes wrong today

- **Dollars are computed only by the dashboard.** The bar and the status line read a figure the dashboard saved. Without the dashboard open, that figure ages, gets a `~` after an hour, and disappears after midnight.
- **The bar can block for up to 10 seconds.** When the limits cache is stale it calls the usage endpoint synchronously (`curl --max-time 10`) inside the bar invocation.
- **Multi-machine sync is a bash script plus systemd units**, with the remote machine's name in the unit instance. That is Linux-only and specific to one setup.
- **Small local assumptions:** the hostname is read from `/proc`, and a bar click opens `alacritty` unless `$TERMINAL` is set.

## Design: serve the cache, refresh in the background

Every surface goes through one core in the binary. The core never makes a surface wait on slow work:

1. Read the cached value and render it immediately, with its age.
2. Decide, with a pure policy function, whether that value is stale.
3. If it is, start `ccmoneta refresh <job>` as a detached background process and return. The next render, by whichever surface runs next, shows the new value.

Nothing needs to be installed beyond the binary: the act of looking at any surface is what keeps the data fresh, and it refreshes only while someone is looking.

### Jobs

| Job | Does | Produces |
|---|---|---|
| `cost` | reads what the transcripts gained and prices it (see Cost) | `cost.json`: the full dashboard data set, not only today's total; `ledger.jsonl`, `prices.json` |
| `limits` | polls `GET /api/oauth/usage` | `limits.json` |
| `sync` | mirrors configured remote machines' transcripts | `hosts/<name>/projects/`, `hosts/<name>/last-sync` |
| `health` | reads the status page, and the published version for this install's channel | `health.json` |

### Single flight

Several surfaces can notice the same stale value at once (a bar tick, a status line render and an open dashboard). Each job runs under an exclusive lock file, `locks/<job>.lock`, taken with the standard library's `File::try_lock`. A job that cannot take its lock exits at once, because another run is already doing the work.

A caller also skips spawning when a spawn for that job was recorded in the last 30 seconds (`jobs/<job>.spawned`). Without that, a status line rendering several times a second would start a process per render during the few seconds before the first child takes the lock.

Verified on rustc 1.98.1: while one handle holds `try_lock` on a file, a second handle's `try_lock` returns `WouldBlock`, and succeeds once the first is dropped.

### Detaching

The refresh child is spawned with stdin, stdout and stderr all on `/dev/null`, in its own process group. A surface's caller reads its output to the end: Claude Code reads the hook's stdout, and i3blocks and Waybar read the bar's. A child still holding that pipe would keep the caller waiting for the whole refresh. This is tested directly: `ccmoneta hook | cat` must return in well under a second while a refresh runs.

### Staleness policy

Pure functions, unit-tested, with thresholds from the config file:

- **cost** is stale when there is no `cost.json`; when it was generated more than `cost.refresh_seconds` ago (default 180); when it was generated on an earlier local calendar day, since its "today" is then a finished day; when any mirrored host has synced since it was generated; or when the set of hosts it covered differs from the hosts now configured.
- **limits** needs a poll when the cached snapshot is older than `limits.max_age_seconds` (default 900) and the job's backoff allows it. The status line hook never polls: Claude Code hands it fresh limits on every turn, and it saves them.
- **health** is due when the cached check is older than `health.max_age_seconds` (default 900, floor 300). Both halves are other people's services, so this is the slowest schedule, its backoff holds even on a manual refresh, and one half failing still writes the other.
- **sync** is due for a host when its last *attempt*, successful or not, is older than `sync.refresh_seconds` (default 600). Counting attempts rather than successes stops a sleeping laptop from being retried on every render.

### Backoff

The usage endpoint is rate-limited. Each job records `last_attempt`, `last_success`, consecutive `failures`, `next_allowed` and `last_error` in `jobs/<job>.json`, written only by the job itself. After a failure `next_allowed` moves out exponentially (1, 2, 4 ... capped at 30 minutes); a success resets it. Because this lives in the shared cache, the bar, the hook and the dashboard all respect the same backoff.

Sync is the exception. Each host spaces its own attempts (see the policy above), and the sync job counts as failed only when every host it attempted failed, so one unreachable machine does not put the others into backoff.

### What each surface does

- **Status line hook:** save the limits from the payload; render model, limits and today's cost; trigger `cost` if stale. When the payload carries no `rate_limits` (before a session's first response, or on accounts without plan limits), show the cached limits instead.
- **Bar:** render cached limits and today's cost; trigger `limits` and `cost` if stale. Never blocks on the network.
- **Dashboard:** render from the caches immediately at startup, even if stale, with ages shown; re-read the cache files every few seconds; trigger jobs on the same policy. `r` forces `cost` and `limits` past their thresholds; the single-flight lock and the limits backoff still apply.

### Cold start and errors

- With no cache at all, surfaces show `…` where a value will appear and trigger the jobs. Nothing blocks.
- A failing job keeps the last good value on screen, with its age, rather than replacing it with nothing. If there has never been a good value, surfaces show a short marker (`$?`) and the dashboard shows `last_error`.
- With no price list yet and no network, the cost job fails with the curl error; `ccmoneta doctor` reports whether prices have been downloaded.
- Optionally, a successful job signals the bar so it redraws without waiting for its next interval: `pkill -RTMIN+<signal> -x <program>`, when `bar.signal` and `bar.program` are both set.
  - Both are required because an unhandled real-time signal terminates the process that receives it (signal(7)), so a signal is only ever sent to a program the user named.
  - `program` is at most 15 characters, because `pkill -x` matches the kernel's process name, which is cut to 15 characters, and procps refuses a longer pattern as matching nothing.
  - `signal` is 1 to 30: i3blocks and Waybar both document the range as 1 to N where SIGRTMIN+N = SIGRTMAX, and glibc's SIGRTMIN and SIGRTMAX here are 34 and 64.

## Cost

- Spend is computed by ccmoneta itself, from the transcripts, by ccusage's rules, so the two agree. ccmoneta ran `ccusage claude daily` once per host and per account until that cost more than it was worth: every refresh reread every transcript in the window (several GB on a busy machine) and, since ccusage takes a directory, each account's share had to be laid out as files for it to read. Checked against ccusage on real data: identical token totals, and identical dollars per day, account, machine and model for every model the price list names.
- ledger.rs reads the transcripts incrementally into `ledger.jsonl`. Claude Code only appends, so a transcript is read once and afterwards only from where the last read ended; a line not yet finished waits for its newline. The ledger is append-only: each run adds the records it found and one line per transcript that grew, and nothing else, so a quiet run writes nothing there. It is rewritten without dead lines only once those outnumber the live ones.
  - A transcript is known by its path and checked by a hash of the last 4 KB read. One that shrank or no longer matches is read again from the start and its old records dropped: rsync replaces a changed file rather than appending to it, so that is what every synced transcript looks like.
  - One that disappears, or was last written more than a day before the window (ccusage skips those too), has its records dropped.
  - Records hold tokens, not dollars, and the session and time of the turn rather than an account: a new price list, or a marker that arrives later with a sync, applies to all of history on the next run.
- Which lines count is ccusage's choice, followed in ledger.rs: a line with `"usage":{` carrying input and output counts and a timestamp; minus a `null` in one of the fields ccusage requires, at any depth; a `speed` other than `standard` or `fast`; an empty id or model; a `version` that is not one. Advisor iterations (`usage.iterations[]` of type `advisor_message`) are records of their own at their own model. With a `cache_creation` split, the five-minute and one-hour writes come from it and `cache_creation_input_tokens` is ignored. Agent-progress lines are not read; no current transcript has them.
- Deduplication is ccusage's `push_deduped_daily_entry`, ported in cost.rs: over every record, in ccusage's reading order (transcripts by path, then line), a record matching one kept, by message and request id (by message and session when there is no request id) or as a sidechain replaying a message of the same session, replaces it if it is a main-chain copy, has more tokens, costs more or names its speed, in that order, and is dropped otherwise. Claude Code streams a reply as several lines, and a resumed session copies its history into a new transcript, so about three in four usage lines are copies. ledger.rs collapses the copies within one transcript as it reads them, since the rule gives the same winner either way; that halves the ledger.
- prices.rs downloads LiteLLM's model list, the one ccusage prices with, with curl, at most once a day, into `prices.json`; an unchanged download is not written again. A failed download keeps the copy already there. The rates: exact key first, else the longest key that contains the model name or is contained in it at word boundaries, never across a version (`claude-opus-5` is not `claude-opus-5-5`). Per request: each kind of token at its rate, the part above 200k tokens at the `_above_200k_tokens` rate where the list has one, one-hour cache writes at twice the input rate, fast mode times the entry's `fast` multiplier; a `costUSD` on the line is used as it is. A model the list does not name costs $0 and is reported: `unpriced` in `cost.json`, `no price` in the dashboard, a warning in `ccmoneta doctor`. That marker is the condition docs/decisions.md set for doing the pricing here.
- Where ccmoneta and ccusage still differ: ccusage falls back to models.dev for a model LiteLLM lacks (ccmoneta reports it unpriced instead), applies its own fast-mode multipliers for two older Opus models whose LiteLLM entries have none, and reads `ccusage.json` and `CCUSAGE_MODEL_ALIASES`. None of these touch current Claude models.
- Days are local calendar days. A record outside the window is in the ledger, for deduplication, and in no figure.
- `CCMONETA_PRICES_URL` replaces the price list's address. The integration tests point it at a local file (curl reads `file://`), so they run without the network.

## Accounts

aimux runs each Claude subscription as a profile: a config directory of its own (`CLAUDE_CONFIG_DIR`) with a private login, while `projects/` is shared with the source profile, `~/.claude`. accounts.rs reads the Claude profiles from `~/.aimux/config.yaml` (only the `profiles:` block, in the shape aimux writes, to keep a YAML library out) and identifies each by `oauthAccount.accountUuid` from its `.claude.json`. Profile names differ between machines; the uuid does not. Without aimux there is one unnamed account, and with one aimux account it is not named on screen; either way every path below is the one that existed before accounts.

- **Limits** are kept per account in `limits/<uuid>.json`. The limits job reads them all with `aimux status --json --max-age N`, which reuses a reading aimux took for someone else within N seconds: half the limits' own threshold, at most 120, so a reused reading is never due again on arrival. An account aimux cannot read (an expired login) is saved with the reason and shown as such; the job fails only when no account could be read, so one account's trouble does not back off the others. The status line hook finds its account from the inherited `CLAUDE_CONFIG_DIR` and saves the payload's windows there only; under a directory no profile names, it shows them without saving, since saving anywhere would overwrite another account's.
- **Spend** is attributed per turn with aimux's rule (accounts::Owners): Claude Code creates `session-env/<session>/` in the config directory of each profile that opens a session; a turn goes to the account whose marker for its session was born last before the turn, and to the source account when there is none. Birth times come from `statx` on Linux and `st_birthtime` on macOS, with the modification time as a fallback, which can only make a marker look younger.
- Each record goes to its account when the figures are added up: by the host's markers, for the session its transcript belongs to (`<session>.jsonl`, or `<session>/` for subagents) at the turn's time. A local profile with a real `projects/` of its own (`aimux migrate isolate`) is one more root, all of it that account's.
- Sync runs accounts::REMOTE_MARKERS in a second ssh command, piped to `sh -s` because the login shell may be zsh, and keeps its output as `hosts/<name>/markers.tsv`: one line per profile (uuid, name, whether it is the source) and one per marker (session id, birth time). It asks `stat` which flavour it is first: GNU's `-f` means filesystem status, so trying the BSD form and falling back printed several lines per marker on Linux. A failure here is logged and never fails the transcript sync; that host's spend then all goes to the source account, as before. A host with its own `remote_dir` gets no markers, since the script reads `~/.claude` and aimux's profiles, which need not be where its transcripts come from.

## Limits

- Claude Code's status line payload carries `rate_limits.five_hour` / `seven_day` / `spend_limit`, each with `used_percentage` (0-100) and `resets_at` in epoch seconds.
- The usage endpoint reports the same windows as `utilization` (0-100) with an RFC 3339 `resets_at`, authenticated with the OAuth token from `~/.claude/.credentials.json` and `anthropic-beta: oauth-2025-04-20`.
- Both normalise to one `Window { percent, resets_at }`.
- Limits are per account, so they are never summed across machines.

## Health

Two questions, one job, one cache file.

**Service status** is Statuspage's summary (`/api/v2/summary.json`): the overall indicator, the `Claude Code` component — matched on its component id, since ids survive renames — every component that is not operational, and the unresolved incidents. The indicator alone is not enough: Statuspage leaves it at `none` for some partial degradations, so a component that is not `operational` also counts as degraded.

**The installed version** is the first token of `claude --version`. Where the *published* version lives depends on the install, so the lookup mirrors the product's own updater rather than guessing:

| Install | Lookup | Channel from |
|---|---|---|
| native, or unrecognised | `GET https://downloads.claude.ai/claude-code-releases/<channel>`, plain text | `autoUpdatesChannel` in settings, default `latest` |
| npm or bun global | `npm view @anthropic-ai/claude-code@<channel> version`, registry pinned, run from `$HOME` | the same setting |
| Homebrew | `https://formulae.brew.sh/api/cask/<cask>.json` | the cask name: `claude-code` is stable, `claude-code@latest` is latest |

Three rules keep this honest:

- A channel or cask name reaches a URL and a command line, so a value that is not exactly one of the two known names is never passed on; it falls back to the default. The registry pin and the `$HOME` working directory keep a project's own `.npmrc` out of the resolution.
- A Homebrew install's channel comes from its cask, not from settings, or a stable-cask user would permanently read as behind against the faster channel.
- Versions are compared numerically, part by part, ignoring a leading `v` and any `+build` suffix — `2.1.9` is older than `2.1.10`, which string comparison gets wrong. An install *ahead* of its channel reads as current, and an unparseable version reads as neither.

A failed lookup is not a claim: `behind` stays false, and the reason is shown as unknown rather than as out of date. When `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` is set, the version lookup is skipped entirely, because Claude Code's own updater suppresses the same fetch in that mode.

Surfaces stay quiet while everything is fine: the bar block and the status line append `⚠ claude <state>` or `⚠ cc <version>` only when there is something to say, and carry the detail in the tooltip. The dashboard has a health pane, and `doctor` a health section, which both always show the current answer. `autoUpdates: false` in `~/.claude.json` is reported alongside an out-of-date install, because it is why the install went stale.

## Multi-machine sync

`ccmoneta sync [host...]` replaces the bash script. Hosts come from the config file; nothing is named in code.

- For each host: list transcripts modified in the last `sync.days` days over `ssh -o BatchMode=yes`, keep only plain relative paths (no absolute paths, no `..`), fetch them with `rsync --files-from`, then delete local copies no longer in the list. An empty list skips the deletion, since it may mean something went wrong rather than no recent sessions.
- Each host's directory records `last-attempt` on every try, `last-sync` only on success, and `last-error` on failure, removed by the next success. A failure deletes nothing and leaves `last-sync` as it was; files rsync finished before failing stay updated.
- ssh runs with `BatchMode=yes`, so it fails rather than prompting, and `ConnectTimeout=10`. It does not force `IdentitiesOnly`, which would lock out keys that exist only in an agent; anything host-specific belongs in `~/.ssh/config`.
- Requires `ssh` and `rsync` locally and on the remote. macOS's `openrsync` works as the sending side for rsync 3.5.
- `ccmoneta sync <host>` accepts a host that is not in the config file and syncs it with its name as the ssh destination, which is what the optional systemd unit relies on.
- `CCMONETA_SSH` replaces the ssh executable, for the listing and for rsync's transport. The integration tests use it to reach a fake remote.
- Because sync is a job, any surface triggers it when a host is due. A systemd timer (`ccmoneta sync <host>`) stays available for people who want mirrors kept current while no surface is running, but is not required.

## Configuration

`$XDG_CONFIG_HOME/ccmoneta/config.toml`, defaulting to `~/.config/ccmoneta/config.toml`. Every key is optional; no file means defaults and no remote hosts.

```toml
[cost]
refresh_seconds = 180
window_days = 30

[limits]
max_age_seconds = 900

[health]
max_age_seconds = 900              # floor 300: both lookups hit someone else's service
status = true                      # https://status.claude.com
version = true                     # installed Claude Code vs its release channel

[sync]
refresh_seconds = 600
days = 31

[[sync.hosts]]
name = "laptop"                    # label in the dashboard, and the default ssh target
ssh = "me@laptop.local"            # optional
remote_dir = "~/.claude/projects"  # optional

[bar]
terminal = "alacritty"             # for click-to-open; default $TERMINAL, then i3-sensible-terminal, then x-terminal-emulator
signal = 10                        # optional, with program: redraw the bar after a refresh (SIGRTMIN+10)
program = "i3blocks"               # the bar process to signal; at most 15 characters
```

Host entries are validated before use, because each value reaches the filesystem, ssh, or a remote shell:

- `name` becomes a directory under the cache, so it is limited to letters, digits, `.`, `_` and `-`, and may not start with `.` or `-`. Names must be unique.
- `ssh` may not contain whitespace or start with `-`, so it cannot be read as an ssh option.
- `remote_dir` is limited to letters, digits and `/ . _ - ~`, since it reaches both the remote shell and rsync's remote path.

Cache: `$XDG_CACHE_HOME/ccmoneta/`, defaulting to `~/.cache/ccmoneta/`.

## Setup commands

- `ccmoneta install statusline` merges `statusLine` into Claude Code's `settings.json`: back it up first, refuse to replace a different existing status line without `--force`, write atomically, and re-parse the result before replacing the original.
- `ccmoneta install i3blocks` and `ccmoneta install waybar` print a ready config block with this binary's absolute path. They do not edit the bar's config.
- `ccmoneta doctor` checks the model price list and names any model it has no price for, the credentials file (never printing the token), the cache directory, the config file, ssh and rsync reachability for each configured host, whether the status line is installed, and each job's last error.

## Bar output formats

- `ccmoneta bar` (default `--format i3blocks`): three lines, full text, short text, colour. i3blocks and swaybar understand this.
- `ccmoneta bar --format waybar`: one JSON line with `text`, `tooltip`, `class` (a string or an array) and `percentage`, per Waybar's custom module documentation (`waybar-custom(5)`).
  - `class` is the level (`ok`, `warn`, `critical`, or `unknown` before any limits arrive), plus `stale` when the limits are older than the poll threshold and `error` when today's spend is unavailable after a failed refresh.
  - `percentage` is the fuller of the two windows.
  - `tooltip` lists each window with its reset, where the limits came from and how long ago, the spend totals and per-host split, each mirrored host's sync age, and the last spend refresh error.
- Both formats come from one view built from the cache, so they cannot disagree.

## Portability

- Hostname from the `gethostname` crate, not `/proc`.
- A click on the i3blocks block opens the dashboard in `bar.terminal`, else `$TERMINAL`, else the first of `i3-sensible-terminal` and `x-terminal-emulator` on PATH. `i3-sensible-terminal` has its own search order, which can differ from the terminal in use (it tries xterm before alacritty), so `bar.terminal` is the dependable setting. Waybar clicks run the module's own `on-click`.
- Linux is the tested platform. The dashboard and status line hook use nothing Linux-specific and are expected to work on macOS, but have not been run there. The bar targets Linux bars.
- Runtime dependencies: `curl` (model prices, and limits when no status line is feeding them), and `ssh` plus `rsync` (sync only).

## Tests

- Unit: the staleness policy, backoff arithmetic, config defaults and parsing, the today-cost date rule, the `settings.json` merge, sync path filtering and prune set, and the Waybar JSON.
- Integration, against a throwaway `XDG_CACHE_HOME`, transcripts written by the test and a local price list (a fake curl where a test needs the download to fail or hang):
  - each surface alone from a cold start reaches correct numbers
  - many concurrent stale callers produce exactly one refresh run
  - the hook returns immediately while a refresh is in flight
  - a failing job backs off
  - a successful refresh signals a stand-in bar named in the config, and nothing else
- Integration, ledger: appended turns are counted and a quiet run writes nothing to the ledger; a line still being written waits for its end; a transcript replaced under the same path is read again; a deleted one takes its spend with it; a reply copied into another transcript counts once at its largest; a ledger cut short by a crash recovers; a ledger mostly of dead lines is compacted; a failed price download keeps the last list; a model the list does not name is reported, not guessed.
- Unit, pricing and deduplication: model matching (exact, dated, prefixed, never a newer version), the per-request formula with the above-200k tiers and one-hour writes, the line filter, advisor iterations, and the dedupe rules (largest copy wins, requestless ids per session, sidechain replays).
- Unit, bar: the i3blocks lines, the Waybar JSON being a single line with the documented keys and classes, the level thresholds, the markers for missing limits and failing spend, and the terminal fallback order.
- Integration, sync, against a fake remote reached through `CCMONETA_SSH` and real rsync:
  - recent transcripts are mirrored, and copies that aged out or vanished remotely are pruned
  - an unreachable host leaves the copy and `last-sync` untouched and records the error
  - an empty listing prunes nothing
  - a host named on the command line syncs without a config file, and an unsafe name is refused
  - a surface starts a due sync on its own
