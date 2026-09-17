# ccmoneta

Named after Moneta, the Roman goddess whose temple housed Rome's mint and gave English the word *money*; her name is traditionally traced to *monere*, to warn, the root of *monitor*.

Claude Code usage limits and spend, in the terminal, in a status bar, and in Claude Code's own status line.

- **Limits:** the plan's 5-hour and 7-day usage percentages, with reset times. These come from Anthropic, through the JSON Claude Code passes to its status line, or through `GET /api/oauth/usage` when no status line has reported recently.
- **Spend:** what the usage would have cost at API prices, today, over 7 days and over 30 days, per model, per project and per machine. [ccusage](https://github.com/ccusage/ccusage) computes it from Claude Code's transcripts; ccmoneta keeps no pricing table of its own.
- **Health:** whether Claude is operational, from [status.claude.com](https://status.claude.com), and whether this machine's Claude Code is the newest release on its channel. Both stay out of sight until there is something to say.

## Three surfaces, each correct on its own

| Surface | Command |
|---|---|
| Dashboard | `ccmoneta` |
| Status bar block (i3blocks, swaybar, Waybar) | `ccmoneta bar`, or `ccmoneta bar --format waybar` |
| Claude Code status line | `ccmoneta hook` |

Use any of them, or all. Each one renders from a shared cache and, when a value is stale, starts a refresh in the background, so none of them waits on the network or on ccusage and none depends on another being open. [docs/design.md](docs/design.md) explains how.

## Requirements

- Linux. The dashboard and the status line hook use nothing Linux-specific but have not been tested elsewhere.
- [ccusage](https://github.com/ccusage/ccusage), for spend: `npm install -g ccusage`.
- `curl`, to poll the usage endpoint when the status line is not feeding limits, and to check Claude's status page and the published Claude Code version.
- `ssh` and `rsync`, only to count other machines.
- A Rust toolchain to build.

## Install

```bash
cargo install --path .
```

This installs `ccmoneta` into the `bin` folder of cargo's install root, `~/.cargo/bin` unless `CARGO_INSTALL_ROOT` or `CARGO_HOME` says otherwise; that folder needs to be on your `PATH`. Then set up whichever surfaces you want, and check the result:

```bash
ccmoneta install statusline   # adds the hook to Claude Code's settings.json
ccmoneta install i3blocks     # prints a block to paste into your i3blocks config
ccmoneta install waybar       # prints a module to paste into your Waybar config
ccmoneta doctor               # checks everything below, and says how to fix what fails
```

`install statusline` edits `$CLAUDE_CONFIG_DIR/settings.json`, which defaults to `~/.claude/settings.json`. It backs the file up first, keeps your other settings and their order, keeps the file's permissions, and will not replace a different status line unless you pass `--force`. The bar commands only print; they never edit a bar's config.

**After upgrading, restart any open dashboard.** A dashboard keeps running the build it was started from. `ccmoneta doctor` lists any ccmoneta process whose binary has since been replaced.

## Dashboard

`q` or `Esc` quits, `r` refreshes now, `↑`/`↓` or `k`/`j` scroll the day list. It shows limits, service status and the installed Claude Code version, a row per day of spend with the figure printed, spend per model and per project, and each mirrored machine's sync age. The footer says how old the spend figures are.

## Configuration

Optional: `$XDG_CONFIG_HOME/ccmoneta/config.toml`, defaulting to `~/.config/ccmoneta/config.toml`. Every key is optional, and an unknown key is an error, so a typo does not silently leave a default in place.

```toml
[cost]
refresh_seconds = 180   # re-gather spend once it is this old
window_days = 30        # days of history, counting today

[limits]
max_age_seconds = 900   # poll the usage endpoint once the cached limits are this old

[health]
max_age_seconds = 900   # re-check status.claude.com and the published version this often
status = true           # check https://status.claude.com
version = true          # compare the installed Claude Code against its release channel

[sync]
refresh_seconds = 600   # sync a host once its last attempt is this old
days = 31               # copy transcripts modified within this many days

[[sync.hosts]]
name = "laptop"                    # label, cache directory, and default ssh destination
ssh = "me@laptop.local"            # optional
remote_dir = "~/.claude/projects"  # optional

[bar]
terminal = "alacritty"  # a click on the i3blocks block opens the dashboard in this
signal = 12             # optional, with program: redraw right after each refresh
program = "i3blocks"    # the bar process sent SIGRTMIN+signal; at most 15 characters
```

- `bar.terminal` defaults to `$TERMINAL`, then `i3-sensible-terminal`, then `x-terminal-emulator`. `i3-sensible-terminal` has its own search order, which may not pick the terminal you use.
- `bar.signal` and `bar.program` must be set together, because a real-time signal terminates a process that does not handle it. The bar also needs the matching `signal=` (i3blocks) or `"signal":` (Waybar) on the block; `ccmoneta install` prints both.
- `health` is quiet by design: the bar block and the status line show a `⚠` marker **only** when Claude is not fully operational or a newer Claude Code has been published. The detail is always in the bar's tooltip, the dashboard's health pane, and `ccmoneta doctor`.
- `health.max_age_seconds` has a floor of 300. Both lookups hit someone else's service, from every machine running this. Setting `status` and `version` to false turns the checks off entirely, and then no surface mentions health at all.

## Counting more than one machine

List the other machines under `[[sync.hosts]]`. Each must be reachable with `ssh` without a password prompt, and have `rsync`. Any running ccmoneta surface then mirrors that machine's recent transcripts into `~/.cache/ccmoneta/hosts/<name>/`, 10 minutes after the last attempt, and includes them in every total.

- Only transcripts modified in the last `sync.days` days are copied, and local copies that have aged out are deleted. An empty listing deletes nothing.
- A failed sync deletes nothing, leaves `last-sync` as it was, and records the reason in `last-error`. `ccmoneta doctor` shows it.
- `ccmoneta sync [host...]` syncs now, in the foreground. A host named on the command line but absent from the config is synced with its name as the ssh destination.
- To keep a mirror current while no surface is running, install the user timer in `contrib/systemd/`:

  ```bash
  cp contrib/systemd/ccmoneta-sync@.service contrib/systemd/ccmoneta-sync@.timer ~/.config/systemd/user/
  systemctl --user daemon-reload
  systemctl --user enable --now ccmoneta-sync@laptop.timer
  ```

  The service runs `%h/.local/bin/ccmoneta`. `cargo install` puts ccmoneta in `~/.cargo/bin` instead, so change `ExecStart` to wherever yours is.

## How the numbers are computed

- **Limits** are per Claude account, so they are shown as reported and never summed across machines.
- **Spend** runs `ccusage daily` once per machine, in parallel, and adds the results, so the per-machine figures always add up to the totals.
- **Per-project spend** comes from ccusage's session report, which is not guaranteed to add up to the daily totals; it can come out higher.
- **Service status** comes from `https://status.claude.com/api/v2/summary.json`: the overall indicator, the `Claude Code` component, any component that is not operational, and the unresolved incidents.
- **The published Claude Code version** is looked up the way Claude Code's own updater does, because where it lives depends on how Claude Code was installed: a native install reads `https://downloads.claude.ai/claude-code-releases/<channel>`, an npm or bun global install runs `npm view` against a pinned registry from your home directory, and a Homebrew install reads its own cask, whose channel is fixed by the cask name rather than by settings. The channel is `autoUpdatesChannel` from Claude Code's settings, defaulting to `latest`, and only ever the literal `stable` or `latest`. Versions are compared numerically, so an install ahead of its channel reads as current rather than behind.
- Today's figure on the bar and status line reads `$12.34` when current, `~$12.34` when over an hour old, `$…` while the first refresh for today runs, and `$?` when refreshing failed.

## Files

Under `$XDG_CACHE_HOME/ccmoneta/`, defaulting to `~/.cache/ccmoneta/`:

| Path | Contents |
|---|---|
| `cost.json` | The spend data set, with the day it covers and when it was gathered |
| `limits.json` | The latest limit windows, and where they came from |
| `health.json` | Claude's service status and the installed-vs-published Claude Code version |
| `hosts/<name>/` | A mirrored machine's transcripts, with `last-sync`, `last-attempt` and `last-error` |
| `jobs/` | Each refresh job's last attempt, failures and backoff |
| `refresh.log` | One line per background refresh, and a note when a cache file does not read |

## Environment variables

| Variable | Effect |
|---|---|
| `XDG_CACHE_HOME`, `XDG_CONFIG_HOME` | Where the cache and the config live |
| `CLAUDE_CONFIG_DIR` | Where `install statusline` and `doctor` look for Claude Code's settings |
| `CCMONETA_CCUSAGE` | The ccusage executable to run |
| `CCMONETA_CLAUDE` | The claude executable to read the installed version from |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` | Claude Code's own setting; when set, the version lookup is skipped, as the built-in updater does |
| `CCMONETA_SSH` | The ssh executable, for sync's listing and rsync's transport |

## Tests

```bash
cargo test
```

The integration tests run the real binary against a throwaway cache, config and home, with a fake ccusage and a fake ssh, so they touch nothing of yours and need no network. The sync tests need `rsync`.

## License

MIT; see [LICENSE](LICENSE).
