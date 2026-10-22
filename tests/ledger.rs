//! The cost job's ledger, against a throwaway HOME and cache.
//!
//! Every turn `turn` writes costs $1 at the sandbox's price list, read from a
//! local file, so what was counted can be read straight off the dollars.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_ccmoneta");

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("ccmoneta-ledger-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for d in ["home/.claude/projects/-home-me-code-p", "cache", "config"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(
            root.join("prices.json"),
            r#"{"m": {"input_cost_per_token": 1, "output_cost_per_token": 0}}"#,
        )
        .unwrap();
        Sandbox { root }
    }

    fn transcript(&self, name: &str) -> PathBuf {
        self.root
            .join("home/.claude/projects/-home-me-code-p")
            .join(name)
    }

    fn refresh(&self) -> serde_json::Value {
        self.refresh_with(&format!(
            "file://{}",
            self.root.join("prices.json").display()
        ))
    }

    fn refresh_with(&self, prices_url: &str) -> serde_json::Value {
        let o = Command::new(BIN)
            .args(["refresh", "cost", "--force"])
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CCMONETA_AIMUX_DIR")
            .env("HOME", self.root.join("home"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("CCMONETA_PRICES_URL", prices_url)
            .output()
            .unwrap();
        assert!(o.status.success(), "{o:?}");
        let cost: serde_json::Value =
            serde_json::from_slice(&fs::read(self.cache("cost.json")).unwrap()).unwrap();
        cost["costs"].clone()
    }

    fn cache(&self, rel: &str) -> PathBuf {
        self.root.join("cache/ccmoneta").join(rel)
    }

    fn ledger_len(&self) -> u64 {
        fs::metadata(self.cache("ledger.jsonl")).unwrap().len()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A turn now, worth `dollars` at the sandbox's prices, with the given ids.
fn turn_as(message: &str, request: &str, dollars: u64) -> String {
    format!(
        r#"{{"type":"assistant","timestamp":"{}","requestId":"{request}","message":{{"id":"{message}","model":"m","usage":{{"input_tokens":{dollars},"output_tokens":0}}}}}}"#,
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ")
    ) + "\n"
}

/// A $1 turn now with ids of its own.
fn turn() -> String {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    turn_as(&format!("msg_{n}"), &format!("req_{n}"), 1)
}

fn append(path: &PathBuf, text: &str) {
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

#[test]
fn appended_turns_are_counted_and_a_quiet_run_writes_nothing_to_the_ledger() {
    let sb = Sandbox::new("append");
    let s1 = sb.transcript("s1.jsonl");
    fs::write(&s1, turn() + &turn()).unwrap();
    assert_eq!(sb.refresh()["today"], 2.0);

    let before = sb.ledger_len();
    assert_eq!(sb.refresh()["today"], 2.0);
    assert_eq!(sb.ledger_len(), before, "nothing new, nothing written");

    append(&s1, &turn());
    assert_eq!(sb.refresh()["today"], 3.0);
}

#[test]
fn a_line_still_being_written_waits_for_its_end() {
    let sb = Sandbox::new("partial");
    let s1 = sb.transcript("s1.jsonl");
    let next = turn();
    let (head, tail) = next.split_at(next.len() / 2);
    fs::write(&s1, turn() + head).unwrap();
    assert_eq!(sb.refresh()["today"], 1.0);
    append(&s1, tail);
    assert_eq!(sb.refresh()["today"], 2.0, "read whole once complete");
}

#[test]
fn a_transcript_replaced_under_the_same_path_is_read_again() {
    let sb = Sandbox::new("replaced");
    let s1 = sb.transcript("s1.jsonl");
    fs::write(&s1, turn() + &turn()).unwrap();
    assert_eq!(sb.refresh()["today"], 2.0);
    // As rsync delivers a changed file: a new file in its place, here with
    // a different start, so the tail read before no longer matches.
    let tmp = sb.transcript("s1.jsonl.tmp");
    let five = |n: &str| turn_as(&format!("msg_r{n}"), &format!("req_r{n}"), 5);
    fs::write(&tmp, five("1") + &five("2") + &five("3")).unwrap();
    fs::rename(&tmp, &s1).unwrap();
    // Read on from the old offset instead, it would count 2 + 5.
    assert_eq!(
        sb.refresh()["today"],
        15.0,
        "old records dropped, new counted"
    );
}

#[test]
fn a_deleted_transcript_takes_its_spend_with_it() {
    let sb = Sandbox::new("deleted");
    fs::write(sb.transcript("s1.jsonl"), turn()).unwrap();
    fs::write(sb.transcript("s2.jsonl"), turn() + &turn()).unwrap();
    assert_eq!(sb.refresh()["today"], 3.0);
    fs::remove_file(sb.transcript("s2.jsonl")).unwrap();
    assert_eq!(sb.refresh()["today"], 1.0);
}

#[test]
fn a_reply_copied_into_another_transcript_counts_once_at_its_largest() {
    let sb = Sandbox::new("copies");
    // Claude Code streams a reply as several lines with growing counts, and a
    // resumed session copies the history into a new transcript.
    fs::write(
        sb.transcript("s1.jsonl"),
        turn_as("msg_a", "req_a", 2) + &turn_as("msg_a", "req_a", 5),
    )
    .unwrap();
    assert_eq!(sb.refresh()["today"], 5.0);
    fs::write(
        sb.transcript("s2.jsonl"),
        turn_as("msg_a", "req_a", 7) + &turn_as("msg_b", "req_b", 1),
    )
    .unwrap();
    assert_eq!(sb.refresh()["today"], 8.0);
}

#[test]
fn a_ledger_cut_short_by_a_crash_recovers() {
    let sb = Sandbox::new("crash");
    let s1 = sb.transcript("s1.jsonl");
    fs::write(&s1, turn()).unwrap();
    assert_eq!(sb.refresh()["today"], 1.0);
    append(&sb.cache("ledger.jsonl"), "[1,99,");
    append(&s1, &turn());
    assert_eq!(sb.refresh()["today"], 2.0);
    let text = fs::read_to_string(sb.cache("ledger.jsonl")).unwrap();
    assert!(text.lines().all(|l| !l.starts_with("[1,99,")), "{text}");
}

#[test]
fn a_ledger_mostly_of_dead_lines_is_compacted() {
    let sb = Sandbox::new("compact");
    fs::write(sb.transcript("s1.jsonl"), turn()).unwrap();
    let big: String = (0..12_000).map(|_| turn()).collect();
    fs::write(sb.transcript("s2.jsonl"), big).unwrap();
    assert_eq!(sb.refresh()["today"], 12_001.0);
    let full = sb.ledger_len();
    fs::remove_file(sb.transcript("s2.jsonl")).unwrap();
    assert_eq!(sb.refresh()["today"], 1.0);
    assert!(
        sb.ledger_len() < full / 100,
        "{} bytes left of {full}",
        sb.ledger_len()
    );
    assert_eq!(sb.refresh()["today"], 1.0, "still reads after compaction");
}

#[test]
fn a_failed_price_download_keeps_the_last_list() {
    let sb = Sandbox::new("prices");
    fs::write(sb.transcript("s1.jsonl"), turn()).unwrap();
    assert_eq!(sb.refresh()["today"], 1.0);
    // Two days old, so due for a download, which then fails.
    let status = Command::new("touch")
        .args(["-d", "2 days ago"])
        .arg(sb.cache("prices.json"))
        .status()
        .unwrap();
    assert!(status.success());
    let costs = sb.refresh_with("file:///nonexistent/prices.json");
    assert_eq!(costs["today"], 1.0);
    let log = fs::read_to_string(sb.cache("refresh.log")).unwrap();
    assert!(log.contains("price list not refreshed"), "{log}");
}

#[test]
fn a_model_the_list_does_not_name_is_reported_not_guessed() {
    let sb = Sandbox::new("unpriced");
    fs::write(
        sb.transcript("s1.jsonl"),
        turn() + &turn().replace(r#""model":"m""#, r#""model":"vendor/new""#),
    )
    .unwrap();
    let costs = sb.refresh();
    assert_eq!(costs["today"], 1.0);
    assert_eq!(costs["unpriced"], serde_json::json!(["vendor/new"]));
}
