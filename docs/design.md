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
| `cost` | runs ccusage (see Cost) | `cost.json`: the full dashboard data set, not only today's total |
| `limits` | polls `GET /api/oauth/usage` | `limits.json` |
| `sync` | mirrors configured remote machines' transcripts | `hosts/<name>/projects/`, `hosts/<name>/last-sync` |

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
- A missing `ccusage` is the most likely failure for a new user. Its error names the fix, and `ccmoneta doctor` reports it.
- Optionally, a successful job signals the bar so it redraws without waiting for its next interval: `pkill -RTMIN+<signal> -x <program>`, when `bar.signal` and `bar.program` are both set.
  - Both are required because an unhandled real-time signal terminates the process that receives it (signal(7)), so a signal is only ever sent to a program the user named.
  - `program` is at most 15 characters, because `pkill -x` matches the kernel's process name, which is cut to 15 characters, and procps refuses a longer pattern as matching nothing.
  - `signal` is 1 to 30: i3blocks and Waybar both document the range as 1 to N where SIGRTMIN+N = SIGRTMAX, and glibc's SIGRTMIN and SIGRTMAX here are 34 and 64.

## Cost

Unchanged in substance from today:

- ccusage prices the transcripts; ccmoneta carries no pricing table. A hard-coded table is what made CCMeter bill Opus 5 at Sonnet rates without warning.
- The daily series is one `ccusage daily --json --breakdown` run per host, in parallel, with `CLAUDE_CONFIG_DIR` set to that host's directory alone, summed by ccmoneta. Per-host totals therefore add up to the header by construction. The session report, used only for per-project figures, came to about $290 more than the daily totals over the same 30 days, which is why it is not used for totals.
- Verified ccusage behaviour relied on: `CLAUDE_CONFIG_DIR` takes a comma-separated list and rejects `:`; a path that does not exist is skipped silently; a window with no usage exits 0 with an empty `daily` list.
- `CCMONETA_CCUSAGE` overrides the ccusage executable. The integration tests use it to run against a fake ccusage with fixed output, so behaviour is testable without anyone's real transcripts.

## Limits

- Claude Code's status line payload carries `rate_limits.five_hour` / `seven_day` / `spend_limit`, each with `used_percentage` (0-100) and `resets_at` in epoch seconds.
- The usage endpoint reports the same windows as `utilization` (0-100) with an RFC 3339 `resets_at`, authenticated with the OAuth token from `~/.claude/.credentials.json` and `anthropic-beta: oauth-2025-04-20`.
- Both normalise to one `Window { percent, resets_at }`.
- Limits are per account, so they are never summed across machines.

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
- `ccmoneta doctor` checks ccusage, the credentials file (never printing the token), the cache directory, the config file, ssh and rsync reachability for each configured host, whether the status line is installed, and each job's last error.

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
- Runtime dependencies: `ccusage` (required for dollars), `curl` (limits when no status line is feeding them), and `ssh` plus `rsync` (sync only).

## Tests

- Unit: the staleness policy, backoff arithmetic, config defaults and parsing, the today-cost date rule, the `settings.json` merge, sync path filtering and prune set, and the Waybar JSON.
- Integration, against a throwaway `XDG_CACHE_HOME` and a fake ccusage:
  - each surface alone from a cold start reaches correct numbers
  - many concurrent stale callers produce exactly one refresh run
  - the hook returns immediately while a refresh is in flight
  - a failing job backs off
  - a successful refresh signals a stand-in bar named in the config, and nothing else
- Unit, bar: the i3blocks lines, the Waybar JSON being a single line with the documented keys and classes, the level thresholds, the markers for missing limits and failing spend, and the terminal fallback order.
- Integration, sync, against a fake remote reached through `CCMONETA_SSH` and real rsync:
  - recent transcripts are mirrored, and copies that aged out or vanished remotely are pruned
  - an unreachable host leaves the copy and `last-sync` untouched and records the error
  - an empty listing prunes nothing
  - a host named on the command line syncs without a config file, and an unsafe name is refused
  - a surface starts a due sync on its own
