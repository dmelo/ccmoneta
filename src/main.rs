//! ccmoneta: Claude Code usage limits and spend, on the terminal, a status bar and
//! Claude Code's own status line.
//!
//! Each surface renders from a shared cache and starts a background refresh
//! when that cache is stale, so any one of them alone shows correct numbers.
//! See docs/design.md.

mod bar;
mod config;
mod cost;
mod doctor;
mod hook;
mod install;
mod limits;
mod refresh;
mod store;
mod sync;
mod tui;

const USAGE: &str = "\
usage: ccmoneta [command]

  (none), tui              the dashboard
  bar [--format FORMAT]    one status bar block; FORMAT is i3blocks (default)
                           or waybar
  hook                     Claude Code statusLine hook; reads its JSON on stdin
  sync [host...]           mirror other machines' transcripts now; hosts come
                           from the config file, or are named here
  refresh <job> [--force]  refresh the cache now; <job> is cost, limits or sync
  install statusline [--force]
                           add the hook to Claude Code's settings
  install i3blocks|waybar  print a block to add to that bar's config
  doctor                   check what ccmoneta depends on, and how to fix it
  help                     show this text
  version                  show the version
";

/// `bar`'s arguments: nothing, `--format X` or `--format=X`.
fn bar_format(rest: &[String]) -> Result<bar::Format, String> {
    let value = match rest {
        [] => return Ok(bar::Format::I3blocks),
        [flag, value] if flag == "--format" => value.as_str(),
        [one] => one
            .strip_prefix("--format=")
            .ok_or_else(|| format!("unexpected argument {one:?}"))?,
        _ => return Err(format!("unexpected arguments {rest:?}")),
    };
    bar::Format::parse(value).ok_or_else(|| format!("unknown bar format {value:?}"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cfg, cfg_error) = config::load();
    let warn = || {
        if let Some(e) = &cfg_error {
            eprintln!("ccmoneta: ignoring config, using defaults: {e}");
        }
    };

    let code = match args.first().map(String::as_str) {
        None | Some("tui") => {
            warn();
            match tui::run(cfg) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("ccmoneta: {e}");
                    1
                }
            }
        }
        Some("bar") => match bar_format(&args[1..]) {
            Ok(format) => bar::run(&cfg, format),
            Err(e) => {
                eprint!("ccmoneta: {e}\n\n{USAGE}");
                2
            }
        },
        Some("hook") => hook::run(&cfg),
        Some("install") => {
            warn();
            install::run(&cfg, &args[1..])
        }
        Some("doctor") => doctor::run(&cfg, cfg_error.as_deref()),
        Some("sync") => {
            warn();
            sync::run_cli(&cfg, &args[1..])
        }
        Some("refresh") => {
            warn();
            let force = args.iter().any(|a| a == "--force");
            match args.get(1).and_then(|j| store::Job::parse(j)) {
                Some(job) => refresh::run(&cfg, job, force),
                None => {
                    eprint!("{USAGE}");
                    2
                }
            }
        }
        Some("help" | "--help" | "-h") => {
            print!("{USAGE}");
            0
        }
        Some("version" | "--version" | "-V") => {
            println!("ccmoneta {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some(other) => {
            eprint!("ccmoneta: unknown command {other:?}\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}
