//! Every usage record in the transcripts, kept in the cache and read
//! incrementally.
//!
//! Claude Code only ever appends to a transcript, so each run reads just the
//! bytes added since the last one. What it finds goes into `ledger.jsonl`, an
//! append-only file: one line per usage record, and one per transcript saying
//! how far it has been read. Nothing is rewritten in steady state; a run
//! writes the records it found and a line per transcript that grew. The file
//! is compacted, rewritten without dead lines, only once those make up most of
//! it.
//!
//! A record holds tokens, not dollars, so a price change or a fix to an
//! account's markers applies to history on the next run without rereading
//! anything. Records are deduplicated when they are added up (see cost.rs),
//! with ccusage's rules, so the figures match what ccusage reported; within
//! one transcript the streamed copies of a reply are collapsed as they are
//! read, which ccusage's rules decide the same way.
//!
//! Which lines count follows ccusage too: a line with `"usage":{` whose
//! message has input and output token counts and a timestamp, minus the ones
//! ccusage refuses (a `null` in a field it requires, an unknown `speed`, an
//! empty id). Agent-progress lines (`data.message.message.usage`) are not
//! read; no current transcript has them.
//!
//! A transcript is recognised by its path and checked by a hash of the last
//! bytes read: when it shrank, or that tail no longer matches (rsync replaces
//! files, so a synced transcript is a new file each time), it is read again
//! from the start and its old records are dropped. One that disappears, or
//! falls out of the window, has its records dropped too.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::prices::Tokens;
use crate::store;

const VERSION: u32 = 1;

/// How many bytes before a transcript's read offset are hashed to recognise it.
const TAIL: u64 = 4096;

pub const SIDECHAIN: u8 = 1;
pub const FAST: u8 = 2;
pub const SPEED: u8 = 4;
/// The message id starts with `msg_` or `resp_`: unique per response, so a
/// record without a request id is deduplicated by session alone.
pub const ANTHROPIC_ID: u8 = 8;

/// One usage record. Ids are 64-bit hashes; 0 means absent.
#[derive(Debug, Clone, PartialEq)]
pub struct Rec {
    pub file: u32,
    /// Byte offset of the line in its transcript. With `file`, the record's
    /// identity and its place in ccusage's reading order.
    pub off: u64,
    /// Epoch milliseconds.
    pub ts: i64,
    pub message: u64,
    pub request: u64,
    pub flags: u8,
    /// As the transcript names it; None when it names none.
    pub model: Option<String>,
    pub tokens: Tokens,
    /// A dollar figure the line carries itself, which ccusage uses as is.
    pub cost_usd: Option<f64>,
    /// The line's `sessionId`, when it differs from the transcript's.
    pub session: Option<String>,
}

/// On disk: a JSON array, which is the compact form.
type RecRow = (
    u32,
    u64,
    i64,
    u64,
    u64,
    u8,
    Option<String>,
    [u64; 5],
    Option<f64>,
    Option<String>,
);

impl Rec {
    fn row(&self) -> RecRow {
        let t = &self.tokens;
        (
            self.file,
            self.off,
            self.ts,
            self.message,
            self.request,
            self.flags,
            self.model.clone(),
            [t.input, t.output, t.cache_5m, t.cache_1h, t.cache_read],
            self.cost_usd,
            self.session.clone(),
        )
    }

    fn from_row(r: RecRow) -> Rec {
        let [input, output, cache_5m, cache_1h, cache_read] = r.7;
        Rec {
            file: r.0,
            off: r.1,
            ts: r.2,
            message: r.3,
            request: r.4,
            flags: r.5,
            model: r.6,
            tokens: Tokens {
                input,
                output,
                cache_5m,
                cache_1h,
                cache_read,
            },
            cost_usd: r.8,
            session: r.9,
        }
    }

    pub fn is(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

/// How far one transcript has been read.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileState {
    #[serde(rename = "f")]
    pub id: u32,
    #[serde(rename = "p")]
    pub path: PathBuf,
    /// The session the path names (see `path_session`).
    #[serde(rename = "s")]
    pub session: String,
    /// Bytes read: always the end of a complete line.
    #[serde(rename = "o")]
    offset: u64,
    /// Hash of up to `TAIL` bytes before `offset`.
    #[serde(rename = "h")]
    tail: u64,
    /// Size and modification time when last read, to skip an unchanged file
    /// without opening it.
    #[serde(rename = "z")]
    size: u64,
    #[serde(rename = "t")]
    mtime_ns: i128,
}

#[derive(Deserialize)]
struct Header {
    v: u32,
}

#[derive(Deserialize)]
struct Gone {
    g: u32,
}

/// The records of every live transcript, after this run's reading.
#[derive(Debug, Default)]
pub struct Ledger {
    pub files: HashMap<u32, FileState>,
    pub records: Vec<Rec>,
}

pub fn path() -> PathBuf {
    store::cache_dir().join("ledger.jsonl")
}

/// FNV-1a, 64 bits: stable across builds and platforms, unlike std's hasher.
/// 0 is kept for "absent".
pub fn hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h.max(1)
}

/// What ccusage takes as a transcript's session, from its path under
/// `projects/`: `<project>/<session>.jsonl`, or the directory above
/// `subagents/` for a subagent's transcript, else the second-to-last part.
pub fn path_session(rel: &Path) -> String {
    let parts: Vec<&str> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if parts.len() == 2
        && let Some(s) = parts[1].strip_suffix(".jsonl").filter(|s| !s.is_empty())
    {
        return s.to_string();
    }
    if parts.len() >= 4 && parts[parts.len() - 2] == "subagents" {
        return parts[parts.len() - 3].to_string();
    }
    parts
        .get(parts.len().saturating_sub(2))
        .copied()
        .unwrap_or("unknown")
        .to_string()
}

// ------------------------------------------------------------------ parsing

#[derive(Deserialize)]
struct Line {
    timestamp: String,
    message: Message,
    version: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "costUSD")]
    cost_usd: Option<f64>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
}

#[derive(Deserialize)]
struct Message {
    usage: Usage,
    model: Option<String>,
    id: Option<String>,
}

#[derive(Deserialize, Clone, Copy)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    speed: Option<Speed>,
    cache_creation: Option<CacheCreation>,
}

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Speed {
    Standard,
    Fast,
}

#[derive(Deserialize, Clone, Copy)]
struct CacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

impl Usage {
    /// With a `cache_creation` split, ccusage takes the two kinds from it and
    /// ignores the total; without one, every write is a five-minute one.
    fn tokens(&self) -> Tokens {
        let (cache_5m, cache_1h) = match self.cache_creation {
            Some(c) => (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens),
            None => (self.cache_creation_input_tokens, 0),
        };
        Tokens {
            input: self.input_tokens,
            output: self.output_tokens,
            cache_5m,
            cache_1h,
            cache_read: self.cache_read_input_tokens,
        }
    }
}

#[derive(Deserialize)]
struct Iterations {
    message: IterMessage,
}

#[derive(Deserialize)]
struct IterMessage {
    usage: IterUsage,
}

#[derive(Deserialize)]
struct IterUsage {
    #[serde(default)]
    iterations: Vec<Iteration>,
}

#[derive(Deserialize)]
struct Iteration {
    #[serde(rename = "type")]
    kind: String,
    model: Option<String>,
    #[serde(flatten)]
    usage: Usage,
}

/// The fields a line is refused for when one of them is `null`, at any depth.
const NOT_NULL: [&str; 11] = [
    "id",
    "cwd",
    "model",
    "speed",
    "costUSD",
    "version",
    "sessionId",
    "requestId",
    "isApiErrorMessage",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
];

/// Whether ccusage refuses the line for a `null` it requires to be set. Only
/// an iteration's `model` may be null. A byte search rules most lines out
/// before anything is parsed.
fn has_refused_null(line: &[u8]) -> bool {
    let candidate = NOT_NULL.iter().any(|k| {
        let needle = format!("\"{k}\":null");
        contains(line, needle.as_bytes())
    });
    if !candidate {
        return false;
    }
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(line) else {
        return false;
    };
    let iterations = root.pointer("/message/usage/iterations");
    walk_nulls(&root, false, iterations)
}

fn walk_nulls(
    v: &serde_json::Value,
    in_iteration: bool,
    iterations: Option<&serde_json::Value>,
) -> bool {
    match v {
        serde_json::Value::Object(map) => map.iter().any(|(k, v)| {
            if v.is_null() && NOT_NULL.contains(&k.as_str()) {
                return !(in_iteration && k == "model");
            }
            walk_nulls(v, false, iterations)
        }),
        serde_json::Value::Array(items) => {
            let is_iterations = iterations.is_some_and(|i| std::ptr::eq(i, v));
            items
                .iter()
                .any(|x| walk_nulls(x, is_iterations, iterations))
        }
        _ => false,
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Epoch milliseconds, for the timestamp shapes ccusage accepts: seconds,
/// optionally with exactly three decimals, then `Z` or an offset.
fn timestamp_ms(s: &str) -> Option<i64> {
    if ![20, 24, 25, 29].contains(&s.len()) {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// `major.minor.patch...`, as ccusage requires of a `version` it is given.
fn semver_like(v: &str) -> bool {
    let mut parts = v.splitn(3, '.');
    let digits =
        |p: Option<&str>| p.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let starts_digit =
        |p: Option<&str>| p.is_some_and(|p| p.as_bytes().first().is_some_and(u8::is_ascii_digit));
    digits(parts.next()) && digits(parts.next()) && starts_digit(parts.next())
}

/// The records on one line, or none: the reply's own usage, then one record per
/// advisor iteration, priced at that iteration's model.
pub fn parse_line(line: &[u8], file: u32, off: u64, file_session: &str) -> Vec<Rec> {
    if !contains(line, b"\"usage\":{") || has_refused_null(line) {
        return Vec::new();
    }
    let Ok(l) = serde_json::from_slice::<Line>(line) else {
        return Vec::new();
    };
    let Some(ts) = timestamp_ms(&l.timestamp) else {
        return Vec::new();
    };
    let empty = |s: &Option<String>| s.as_deref().is_some_and(str::is_empty);
    if l.version.as_deref().is_some_and(|v| !semver_like(v))
        || empty(&l.session_id)
        || empty(&l.request_id)
        || empty(&l.message.id)
        || empty(&l.message.model)
    {
        return Vec::new();
    }
    let session = l.session_id.filter(|s| s != file_session);
    let request = l.request_id.as_deref().map_or(0, |r| hash(r.as_bytes()));
    let mut base = 0;
    if l.is_sidechain == Some(true) {
        base |= SIDECHAIN;
    }
    let id = l.message.id;
    if id
        .as_deref()
        .is_some_and(|i| i.starts_with("msg_") || i.starts_with("resp_"))
    {
        base |= ANTHROPIC_ID;
    }
    let speed_flags = |u: &Usage| match u.speed {
        Some(Speed::Fast) => SPEED | FAST,
        Some(Speed::Standard) => SPEED,
        None => 0,
    };
    let mut out = vec![Rec {
        file,
        off,
        ts,
        message: id.as_deref().map_or(0, |i| hash(i.as_bytes())),
        request,
        flags: base | speed_flags(&l.message.usage),
        model: l.message.model,
        tokens: l.message.usage.tokens(),
        cost_usd: l.cost_usd,
        session: session.clone(),
    }];
    if contains(line, b"\"advisor_message\"")
        && let Ok(it) = serde_json::from_slice::<Iterations>(line)
    {
        let advisors = it
            .message
            .usage
            .iterations
            .into_iter()
            .filter(|i| i.kind == "advisor_message")
            .filter_map(|i| Some((i.model.filter(|m| !m.is_empty())?, i.usage)));
        for (n, (model, usage)) in advisors.enumerate() {
            out.push(Rec {
                file,
                // Inside the parent's line, so unique and right after it.
                off: off + 1 + n as u64,
                ts,
                message: id
                    .as_deref()
                    .map_or(0, |i| hash(format!("{i}:advisor:{n}").as_bytes())),
                request,
                flags: base | speed_flags(&usage),
                model: Some(model),
                tokens: usage.tokens(),
                cost_usd: None,
                session: session.clone(),
            });
        }
    }
    out
}

/// Records in what `input` holds, which starts at offset `start` of the
/// transcript, up to the last complete line; and the offset just past that
/// line. Line by line, so a large transcript is never held in memory whole.
fn parse_from(
    mut input: impl std::io::BufRead,
    start: u64,
    file: u32,
    session: &str,
) -> std::io::Result<(Vec<Rec>, u64)> {
    let mut recs = Vec::new();
    let mut at = start;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = input.read_until(b'\n', &mut line)?;
        // The end, or a line still being written: it is read once complete.
        if n == 0 || line.last() != Some(&b'\n') {
            break;
        }
        if n > 1 {
            recs.extend(parse_line(&line[..n - 1], file, at, session));
        }
        at += n as u64;
    }
    Ok((recs, at))
}

// ------------------------------------------------------------------ the file

/// Two records of one transcript that ccusage would merge: the same reply,
/// request and session, both or neither a sidechain.
#[derive(Hash, PartialEq, Eq)]
struct SameReply {
    file: u32,
    message: u64,
    request: u64,
    session: Option<String>,
    sidechain: bool,
    /// Without a request id, a non-Anthropic message id is only the same reply
    /// at the same moment.
    ts: i64,
}

fn same_reply(r: &Rec) -> Option<SameReply> {
    (r.message != 0).then(|| SameReply {
        file: r.file,
        message: r.message,
        request: r.request,
        session: r.session.clone(),
        sidechain: r.is(SIDECHAIN),
        ts: if r.request == 0 && !r.is(ANTHROPIC_ID) {
            r.ts
        } else {
            0
        },
    })
}

/// Which of two copies of a reply in one transcript ccusage keeps: the one
/// with more tokens, else the one that names its speed, else the first. None
/// when that needs their dollar figures, which only adding up can compare.
fn later_wins(earlier: &Rec, later: &Rec) -> Option<bool> {
    let (a, b) = (earlier.tokens.total(), later.tokens.total());
    if a != b {
        return Some(b > a);
    }
    let same_price = earlier.model == later.model
        && earlier.is(FAST) == later.is(FAST)
        && earlier.cost_usd.is_none()
        && later.cost_usd.is_none();
    same_price.then(|| later.is(SPEED) && !earlier.is(SPEED))
}

struct Loaded {
    files: HashMap<u32, FileState>,
    records: HashMap<(u32, u64), Rec>,
    /// Lines in the file, for deciding when to compact.
    lines: usize,
}

/// Read the ledger. A file of another version, or none, reads as empty; a
/// line cut short by a crash is dropped, and the file truncated to the last
/// complete line, so the next append starts on a line of its own.
fn load(path: &Path) -> Loaded {
    let mut out = Loaded {
        files: HashMap::new(),
        records: HashMap::new(),
        lines: 0,
    };
    let Ok(bytes) = std::fs::read(path) else {
        return out;
    };
    let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    if end < bytes.len()
        && let Ok(f) = std::fs::File::options().write(true).open(path)
    {
        let _ = f.set_len(end as u64);
    }
    let mut lines = bytes[..end]
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty());
    match lines
        .next()
        .and_then(|h| serde_json::from_slice::<Header>(h).ok())
    {
        Some(h) if h.v == VERSION => {}
        _ => return out,
    }
    for line in lines {
        out.lines += 1;
        match line.first() {
            Some(b'[') => {
                if let Ok(row) = serde_json::from_slice::<RecRow>(line) {
                    let r = Rec::from_row(row);
                    out.records.insert((r.file, r.off), r);
                }
            }
            _ => {
                if let Ok(s) = serde_json::from_slice::<FileState>(line) {
                    out.files.insert(s.id, s);
                } else if let Ok(g) = serde_json::from_slice::<Gone>(line) {
                    out.files.remove(&g.g);
                }
            }
        }
    }
    out.records.retain(|(f, _), _| out.files.contains_key(f));
    out
}

fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i128)
}

/// Every `*.jsonl` under `root`, at any depth, not following symlinks, as
/// ccusage reads them.
fn transcripts(root: &Path) -> Vec<(PathBuf, std::fs::Metadata)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(kind) = e.file_type() else {
                continue;
            };
            let path = e.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file()
                && path.extension().is_some_and(|x| x == "jsonl")
                && let Ok(meta) = e.metadata()
            {
                out.push((path, meta));
            }
        }
    }
    out
}

fn tail_hash(file: &mut std::fs::File, offset: u64) -> std::io::Result<u64> {
    let from = offset.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(from))?;
    let mut buf = vec![0; (offset - from) as usize];
    file.read_exact(&mut buf)?;
    Ok(hash(&buf))
}

/// What reading one transcript found.
struct Reading {
    state: FileState,
    records: Vec<Rec>,
}

/// Read a transcript from `state.offset` on; None when it vanished. A file
/// that no longer starts with what was read before is read from the start,
/// under `fresh_id`.
fn read_file(mut state: FileState, meta: &std::fs::Metadata, fresh_id: u32) -> Option<Reading> {
    let mut file = std::fs::File::open(&state.path).ok()?;
    let same = state.offset <= meta.len()
        && tail_hash(&mut file, state.offset).is_ok_and(|h| h == state.tail);
    if !same {
        state.id = fresh_id;
        state.offset = 0;
    }
    file.seek(SeekFrom::Start(state.offset)).ok()?;
    let mut input = std::io::BufReader::with_capacity(1 << 20, file);
    let (records, end) = parse_from(&mut input, state.offset, state.id, &state.session).ok()?;
    let mut file = input.into_inner();
    state.offset = end;
    state.tail = tail_hash(&mut file, end).ok()?;
    state.size = meta.len();
    state.mtime_ns = mtime_ns(meta);
    Some(Reading { state, records })
}

/// Bring the ledger up to date with every transcript under `roots` (each a
/// `projects/` directory) and return its records.
///
/// Transcripts last written before `oldest` (epoch seconds) are not read, and
/// any already in the ledger are dropped: like ccusage, which skips files
/// older than a day before its window.
pub fn update(roots: &[PathBuf], oldest: i64) -> Result<Ledger, String> {
    let path = path();
    let mut loaded = load(&path);
    let fresh_file = loaded.lines == 0 && loaded.files.is_empty();

    let mut seen: Vec<(PathBuf, std::fs::Metadata)> = roots
        .iter()
        .flat_map(|r| transcripts(r))
        .filter(|(_, m)| mtime_ns(m) >= i128::from(oldest) * 1_000_000_000)
        .collect();
    seen.sort_by(|a, b| a.0.cmp(&b.0));

    let by_path: HashMap<PathBuf, u32> = loaded
        .files
        .values()
        .map(|s| (s.path.clone(), s.id))
        .collect();
    let mut next_id = loaded.files.keys().max().map_or(1, |m| m + 1);
    let mut work: Vec<(FileState, std::fs::Metadata, u32)> = Vec::new();
    let mut live: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for (p, meta) in seen {
        let known = by_path.get(&p).and_then(|id| loaded.files.get(id));
        if let Some(s) = known {
            live.insert(s.id);
            if s.size == meta.len() && s.mtime_ns == mtime_ns(&meta) {
                continue;
            }
        }
        let Some(rel) = roots.iter().find_map(|r| p.strip_prefix(r).ok()) else {
            continue;
        };
        let state = known.cloned().unwrap_or_else(|| {
            let id = next_id;
            next_id += 1;
            FileState {
                id,
                path: p.clone(),
                session: path_session(rel),
                offset: 0,
                tail: hash(b""),
                size: 0,
                mtime_ns: 0,
            }
        });
        let fresh = next_id;
        next_id += 1;
        work.push((state, meta, fresh));
    }

    // Read in parallel; the first build reads every transcript in the window.
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(16);
    let chunk = work.len().div_ceil(threads).max(1);
    let reads: Vec<(u32, Option<Reading>)> = std::thread::scope(|s| {
        let handles: Vec<_> = work
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .map(|(st, meta, fresh)| (st.id, read_file(st.clone(), meta, *fresh)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });

    let mut out = String::new();
    if fresh_file {
        out.push_str(&format!("{{\"v\":{VERSION}}}\n"));
    }
    let gone = |out: &mut String, id: u32| out.push_str(&format!("{{\"g\":{id}}}\n"));
    // Dropped: transcripts deleted, or out of the window.
    let dropped: Vec<u32> = loaded
        .files
        .keys()
        .filter(|id| !live.contains(id) && !work.iter().any(|(s, _, _)| s.id == **id))
        .copied()
        .collect();
    for id in dropped {
        gone(&mut out, id);
        loaded.files.remove(&id);
    }

    let mut index: HashMap<SameReply, (u32, u64)> = loaded
        .records
        .values()
        .filter_map(|r| Some((same_reply(r)?, (r.file, r.off))))
        .collect();
    for (old_id, read) in reads {
        let Some(read) = read else {
            // Vanished since the listing.
            if loaded.files.remove(&old_id).is_some() {
                gone(&mut out, old_id);
            }
            continue;
        };
        if read.state.id != old_id && loaded.files.remove(&old_id).is_some() {
            gone(&mut out, old_id);
        }
        for r in read.records {
            let key = same_reply(&r);
            let earlier = key
                .as_ref()
                .and_then(|k| index.get(k))
                .and_then(|at| loaded.records.get(at));
            let stored = match earlier.map(|e| (e.off, later_wins(e, &r))) {
                Some((_, Some(false))) => continue,
                Some((off, Some(true))) => Rec { off, ..r },
                _ => {
                    if let Some(k) = key {
                        index.insert(k, (r.file, r.off));
                    }
                    r
                }
            };
            out.push_str(&serde_json::to_string(&stored.row()).map_err(|e| e.to_string())?);
            out.push('\n');
            loaded.records.insert((stored.file, stored.off), stored);
        }
        out.push_str(&serde_json::to_string(&read.state).map_err(|e| e.to_string())?);
        out.push('\n');
        loaded.files.insert(read.state.id, read.state);
    }
    loaded
        .records
        .retain(|(f, _), _| loaded.files.contains_key(f));

    let live_lines = loaded.records.len() + loaded.files.len();
    if loaded.lines + out.lines().count() > 2 * live_lines + 10_000 {
        compact(&path, &loaded)?;
    } else if !out.is_empty() {
        append(&path, &out)?;
    }

    let mut records: Vec<Rec> = loaded.records.into_values().collect();
    records.sort_by_key(|r| (r.file, r.off));
    Ok(Ledger {
        files: loaded.files,
        records,
    })
}

fn append(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    let mut f = std::fs::File::options()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Rewrite the ledger with only what is live, through a temp file and a rename.
fn compact(path: &Path, l: &Loaded) -> Result<(), String> {
    let mut text = format!("{{\"v\":{VERSION}}}\n");
    let mut files: Vec<&FileState> = l.files.values().collect();
    files.sort_by_key(|s| s.id);
    let mut records: Vec<&Rec> = l.records.values().collect();
    records.sort_by_key(|r| (r.file, r.off));
    for r in records {
        text.push_str(&serde_json::to_string(&r.row()).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    for s in files {
        text.push_str(&serde_json::to_string(s).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    store::write_text(path, &text).map_err(|e| format!("writing {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, req: &str, out: u64, extra: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2027-01-15T08:00:00.000Z","sessionId":"s1","requestId":"{req}","version":"2.1.0","message":{{"id":"{id}","model":"claude-opus-5","usage":{{"input_tokens":10,"output_tokens":{out},"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}{extra}}}"#
        )
    }

    #[test]
    fn a_usage_line_becomes_a_record() {
        let l = line("msg_1", "req_1", 5, "");
        let r = parse_line(l.as_bytes(), 3, 100, "s1");
        assert_eq!(r.len(), 1);
        let r = &r[0];
        assert_eq!((r.file, r.off), (3, 100));
        assert_eq!(r.ts, 1_800_000_000_000);
        assert_eq!(r.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(r.tokens.input, 10);
        assert_eq!(r.tokens.output, 5);
        assert!(r.is(ANTHROPIC_ID));
        assert_eq!(r.session, None, "same session as the file: not repeated");
    }

    #[test]
    fn lines_ccusage_refuses_are_skipped() {
        let refused = [
            // A null where ccusage requires a value, at any depth.
            line("msg_1", "req_1", 5, r#","x":{"cwd":null}"#),
            // A speed it does not know.
            line("msg_1", "req_1", 5, "")
                .replace(r#""input_tokens""#, r#""speed":"turbo","input_tokens""#),
            // An empty id, and a version that is not a version.
            line("", "req_1", 5, ""),
            line("msg_1", "req_1", 5, "").replace("2.1.0", "dev"),
            // Six decimals in the timestamp.
            line("msg_1", "req_1", 5, "").replace(".000Z", ".000000Z"),
            // No output count.
            line("msg_1", "req_1", 5, "").replace(r#","output_tokens":5"#, ""),
        ];
        for l in refused {
            assert!(parse_line(l.as_bytes(), 1, 0, "s1").is_empty(), "{l}");
        }
        // A null elsewhere is fine.
        let ok = line("msg_1", "req_1", 5, r#","stop_sequence":null"#);
        assert_eq!(parse_line(ok.as_bytes(), 1, 0, "s1").len(), 1);
    }

    #[test]
    fn one_hour_cache_writes_come_from_the_split_when_there_is_one() {
        let l = line("msg_1", "req_1", 5, "").replace(
            r#""cache_creation_input_tokens":0"#,
            r#""cache_creation_input_tokens":999,"cache_creation":{"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":4}"#,
        );
        let r = &parse_line(l.as_bytes(), 1, 0, "s1")[0];
        assert_eq!((r.tokens.cache_5m, r.tokens.cache_1h), (3, 4));
    }

    #[test]
    fn advisor_iterations_are_records_of_their_own() {
        let l = line("msg_1", "req_1", 5, "").replace(
            r#""cache_read_input_tokens":0}"#,
            r#""cache_read_input_tokens":0,"iterations":[{"type":"message","model":null,"input_tokens":1,"output_tokens":1},{"type":"advisor_message","model":"claude-fable-5","input_tokens":7,"output_tokens":8}]}"#,
        );
        let r = parse_line(l.as_bytes(), 1, 50, "s1");
        assert_eq!(r.len(), 2, "{l}");
        assert_eq!(r[1].model.as_deref(), Some("claude-fable-5"));
        assert_eq!(r[1].off, 51);
        assert_eq!(r[1].message, hash(b"msg_1:advisor:0"));
        assert_eq!(r[1].tokens.output, 8);
    }

    #[test]
    fn sessions_come_from_paths_as_ccusage_takes_them() {
        assert_eq!(path_session(Path::new("p/abc.jsonl")), "abc");
        assert_eq!(
            path_session(Path::new("p/abc/subagents/agent-1.jsonl")),
            "abc"
        );
        assert_eq!(path_session(Path::new("p/abc/wf/x.jsonl")), "wf");
    }

    #[test]
    fn only_complete_lines_are_read() {
        let body = format!(
            "{}\n{}",
            line("msg_1", "req_1", 5, ""),
            line("msg_2", "req_2", 5, "")
        );
        let (recs, end) = parse_from(body.as_bytes(), 10, 1, "s1").unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(end, 10 + line("msg_1", "req_1", 5, "").len() as u64 + 1);
    }

    #[test]
    fn the_later_copy_of_a_reply_wins_only_with_more_tokens() {
        let parse =
            |out| parse_line(line("msg_1", "req_1", out, "").as_bytes(), 1, 0, "s1").remove(0);
        assert_eq!(later_wins(&parse(5), &parse(9)), Some(true));
        assert_eq!(later_wins(&parse(9), &parse(5)), Some(false));
        assert_eq!(later_wins(&parse(5), &parse(5)), Some(false));
        let mut other = parse(5);
        other.model = Some("claude-sonnet-5".into());
        assert_eq!(later_wins(&parse(5), &other), None, "needs prices");
    }

    #[test]
    fn hashes_are_stable() {
        assert_eq!(hash(b"msg_1"), hash(b"msg_1"));
        assert_ne!(hash(b"msg_1"), hash(b"msg_2"));
        assert_ne!(hash(b""), 0);
    }
}
