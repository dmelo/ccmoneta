# ccmoneta

Claude Code usage limits and spend, on three surfaces (dashboard, status bar block, Claude Code status line). [README.md](README.md) is the user guide, [docs/design.md](docs/design.md) the architecture, [docs/decisions.md](docs/decisions.md) the settled choices and why.

## Branches

- **Every change reaches `main` through a pull request.** Never commit on `main`, and never push to it directly — branch off `main`, push the branch, open the PR, and let CI run on it. This holds for one-line fixes and for documentation as much as for code.
- `main` is the published branch: one squashed commit plus whatever follows it. The GitHub repo is **public**.
- `history` is local only and **must never be pushed**. It holds the full development history, whose commit messages contain the maintainer's real machine names and spend figures. That is exactly what the squash kept off GitHub.
- Never merge `history` into `main`, and never push `--all` or `--mirror`.

## What must not enter the repo

The repo is published, so no commit message, comment, test or doc may contain real host names, home paths, project names or spend figures. Tests use neutral names (`desk`, `laptop`) and example paths (`-home-me-code-project`). Made-up figures like `$12.34` are fine.

## CI

`.github/workflows/ci.yml` runs on every push to `main` and every pull request: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked`, a release build, a check that `rsync` is installed (without it the sync tests skip themselves and the run is green while testing nothing), and a guard that no home path other than `/home/me` is committed. Match a run to the commit's sha rather than reading "the latest run".

## Tests

`cargo test` runs everything; no network and nothing of the user's is touched.

- Integration tests run the real binary against a throwaway `HOME`, `XDG_CACHE_HOME` and `XDG_CONFIG_HOME`, with `CCMONETA_CCUSAGE` pointing at a fake ccusage and `CCMONETA_SSH` at a fake ssh that runs commands against a fake remote home. The sync tests need real `rsync`; they skip themselves without it, so check for a "skipping" line before believing they passed.
- The suite exercises guards, not just happy paths. Before trusting one, break it on purpose in a scratch copy and confirm its test fails. Every guard here was checked that way.
- **After moving or copying this repo, run `cargo clean` first.** Integration tests reach the binary through `CARGO_BIN_EXE_ccmoneta`, an absolute path fixed when the test was compiled; a stale build runs the binary at the old path, which fails with "No such file or directory" or, worse, silently tests a different build.

## Toolchain

rustup's minimal profile has no `rustfmt` or `clippy`: `rustup component add rustfmt clippy`. `cargo fmt --check` exiting non-zero when rustfmt is absent looks exactly like formatting drift. Keep `cargo clippy --all-targets` at zero findings.

## Surfaces and the cache

Every surface reads `$XDG_CACHE_HOME/ccmoneta/` and starts a detached `ccmoneta refresh <job>` when a value is stale; jobs are single-flighted by a lock file each. Nothing in a surface may block on the network or on ccusage. Pricing stays ccusage's job; this repo carries no model price table, deliberately (see docs/decisions.md).

## After changing the binary

A running dashboard keeps executing the build it started from, and cannot spawn refreshes if its binary was replaced (`store::self_exe` handles the replaced-path case, but the old process still runs old code). Reinstall, then restart any open dashboard; `ccmoneta doctor` lists processes running from a replaced binary.
