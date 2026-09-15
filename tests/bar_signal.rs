//! The optional redraw signal, against a stand-in bar.
//!
//! The stand-in is a copy of bash under a name nothing else on the machine
//! uses, so `pkill -x` can only reach it. It traps SIGRTMIN+11 and records
//! whether it arrived. Everything else runs against a throwaway cache, config
//! and a fake ccusage.

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ccmoneta");
const FAKE_BAR: &str = "ccsig-fakebar";

#[test]
fn a_successful_refresh_signals_the_named_bar() {
    let root = std::env::temp_dir().join(format!("ccmoneta-signal-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for d in ["home/.claude/projects", "cache", "config/ccmoneta"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    fs::write(
        root.join("config/ccmoneta/config.toml"),
        format!("[bar]\nsignal = 11\nprogram = \"{FAKE_BAR}\"\n"),
    )
    .unwrap();
    let ccusage = root.join("fake-ccusage");
    fs::write(
        &ccusage,
        "#!/bin/bash\ncase \"$1\" in daily) printf '{\"daily\":[]}';; session) printf '{\"session\":[]}';; esac\n",
    )
    .unwrap();
    let handler = root.join("handler.sh");
    fs::write(
        &handler,
        "trap 'echo redraw > \"$OUT\"; exit 0' RTMIN+11\nsleep 15 & wait $!\necho 'no signal' > \"$OUT\"\n",
    )
    .unwrap();
    let bar = root.join(FAKE_BAR);
    fs::copy("/bin/bash", &bar).unwrap();
    Command::new("chmod")
        .arg("+x")
        .arg(&ccusage)
        .arg(&bar)
        .status()
        .unwrap();

    let out = root.join("out");
    let mut stand_in = Command::new(&bar)
        .arg(&handler)
        .env("OUT", &out)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let comm = format!("/proc/{}/comm", stand_in.id());
    let start = Instant::now();
    while fs::read_to_string(&comm)
        .map(|s| s.trim().to_string())
        .ok()
        .as_deref()
        != Some(FAKE_BAR)
    {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "stand-in bar did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Let bash install its trap before anything is sent.
    std::thread::sleep(Duration::from_millis(300));

    let status = Command::new(BIN)
        .args(["refresh", "cost", "--force"])
        .env_remove("CLAUDE_CONFIG_DIR")
        .env("HOME", root.join("home"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("CCMONETA_CCUSAGE", &ccusage)
        .status()
        .unwrap();
    assert!(status.success());

    let start = Instant::now();
    while !out.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "stand-in never finished"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = stand_in.kill();
    let _ = stand_in.wait();
    let got = fs::read_to_string(&out).unwrap();
    let _ = fs::remove_dir_all(&root);
    assert_eq!(got.trim(), "redraw");
}
