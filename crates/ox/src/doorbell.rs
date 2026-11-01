//! The Claude Code doorbell for `ox mcp`. SHADOW by default since 2026-10-04: it only records what it would ring;
//! OX_DOORBELL=live turns it on for a seat that asks. Blaze, 2026-10-04: one channel for every seat, the DOT ring (dotd
//! types one line into the seat's terminal, for any agent on any machine); this socket ring is Claude-only and arrives
//! wrapped as "another Claude session sent a message".
//!
//! An `ox mcp` process is a child of exactly one Claude Code session, which exports its inbox socket, token and
//! session id to its children (CLAUDE_CODE_MESSAGING_SOCKET, CLAUDE_CODE_MESSAGING_TOKEN, CLAUDE_CODE_SESSION_ID;
//! https://code.claude.com/docs/en/cross-session-messaging#the-sessions-inbox-socket). So the doorbell is for its own
//! parent session only: no discovery, no PTY, no kernel, and it ends with the session.
//!
//! History: shadow first (Emmy, PR 55 review) until the frame was pinned. Over 24h
//! before cutover, shadow selected every id the Mac relay rang (3 of 3) plus the open asks. Blaze, 2026-10-03: every
//! action to and from Oracle goes through this one MCP, so this replaces the launchd ox-relay and the kernel's
//! room-notifier (which typed into PTYs). A live ring is the auth line then {"type":"user","message":…}; a ring that
//! cannot be sent is not marked rung and goes out on the next look.
//!
//! What rings (parity with the relay it replaces, ~/.claude/hooks/ox-relay.py, 2026-10-01):
//! - now: the server's own derived list of open asks waiting on this seat (`GET /asks?state=open`), and any message
//!   from a person ([`PEOPLE`]);
//! - as one `fyi` line, at most every [`DIGEST_EVERY_MS`] or carried by the next real ring: other messages addressed
//!   to this seat (`to:` or an @mention), by id.
//!
//! New messages are found by the doorbell's own scan, never the read fold: `/channels` says which rooms moved, and each
//! is paged newest first back to where the scan last stood ([`Doorbell::scan`]). The fold lists the oldest unread
//! first, so behind 200 unread messages it would never show a new one (Emmy, review of #65); the scan moves no read
//! mark. What it finds waits in `pending` until it rings, so a ring held back by the gap or the digest loses nothing.
//!
//! What has rung is remembered per SEAT, not per session ([`LEDGER_FILE`], seeded once from the relay's `seen`): a
//! restarted or new session is not rung again for the backlog, which its brief already lists (the first shadow run
//! after a restart re-selected 20 asks from Sep 23 on, 2026-10-01). An ask that resolves is forgotten (only on a
//! complete list); one that stays open is never forgotten. Rings are spaced per session ([`MIN_GAP_MS`]) and budgeted
//! per seat across all its sessions ([`SEAT_MAX`] per [`SEAT_WINDOW_MS`]). A ring names at most [`MAX_ITEMS`] asks.
//! Ringing never marks anything read.

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// At most one ring per session in this window; what arrives meanwhile waits and is coalesced.
pub const MIN_GAP_MS: i64 = 60_000;
/// At most this many rings for one seat, across all its sessions, in [`SEAT_WINDOW_MS`].
pub const SEAT_MAX: usize = 6;
pub const SEAT_WINDOW_MS: i64 = 10 * 60_000;
/// Asks named in one ring; the rest are counted.
pub const MAX_ITEMS: usize = 10;
/// Messages from these seats ring now, addressed or not (the relay's PEOPLE).
pub const PEOPLE: [&str; 2] = ["blaze", "hustler"];
/// Addressed messages that ask nothing ring as one `fyi` line at most this often, unless a real ring carries them.
pub const DIGEST_EVERY_MS: i64 = 30 * 60_000;
/// Message ids remembered (they never reopen); asks are kept while open whatever their number.
pub const MESSAGES_KEPT: usize = 5000;
/// Pages of 50 read per room in one look. A room that moved more is read in later looks from where this one stopped
/// (a gap), so nothing is skipped: a look is bounded, the scan is not.
pub const SCAN_PAGES: usize = 10;
/// Messages kept waiting to ring. Past this the oldest are let go, counted in `overflow` and named in the next ring
/// ("N more not kept: oracle_inbox"), never silently.
pub const PENDING_KEPT: usize = 5000;
/// The seat's doorbell state in shadow mode: what it WOULD have rung, scan positions, pending messages. Kept apart from
/// any real delivery history: at cutover the real doorbell starts from the relay's `seen`, never from a shadow
/// selection that was only logged (Emmy, review of #65).
pub const LEDGER_FILE: &str = "shadow-rung.json";
/// The LIVE doorbell's ledger: what it really rang. A new file, seeded once from the relay's `seen` (never from a shadow
/// selection, Emmy's review of #65, and never from the stale `rung.json` an earlier experiment left on the Mac).
pub const LIVE_LEDGER_FILE: &str = "doorbell.json";
/// The shadow log rotates to `.1` past this size.
pub const LOG_MAX_BYTES: u64 = 1 << 20;

struct Ledger {
    /// Asks rung (or seeded) that are still open, kept whatever their number while they stay open.
    asks: BTreeSet<String>,
    /// Oldest first; the newest [`MESSAGES_KEPT`] are kept.
    messages: Vec<String>,
    /// When the last ring went out with its fyi line.
    digest_ms: i64,
    /// Per room: the arrival seq (`/channels` last_seq) the scan has read up to. A server without last_seq gives its
    /// last_ms instead, kept in `ms_cursors`; then messages of the same millisecond are read again and de-duplicated.
    cursors: std::collections::BTreeMap<String, i64>,
    ms_cursors: std::collections::BTreeMap<String, i64>,
    /// Messages the scan found that have not rung yet (from a person, or addressed to the seat).
    pending: Vec<Value>,
    /// Ranges a look did not finish, read in later looks: (room, read before this seq, back to this watermark).
    gaps: Vec<(String, i64, Mark)>,
    /// Waiting messages let go past [`PENDING_KEPT`] since the last ring; said in the ring.
    overflow: i64,
}

/// Where a room's read stops: at a seq already read (arrival order, the safe watermark: Emmy, review of #65), or, by time
/// (a server without `last_seq`, or state saved by an older ox), at the millisecond the position was taken. That
/// millisecond counts as read: a position is taken at a room's head without reading it, so re-reading the boundary
/// rang heads that were never news (the live upgrade of 2026-10-01 rang a message from July). The cost, a message
/// landing in the same millisecond just after the position was taken, is why seq is the watermark wherever it exists.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mark {
    Seq(i64),
    Ms(i64),
}

impl Mark {
    fn reached(self, it: &Value) -> bool {
        match self {
            Mark::Seq(s) => it["seq"].as_i64().unwrap_or(i64::MAX) <= s,
            Mark::Ms(ms) => it["created_ms"].as_i64().unwrap_or(0) <= ms,
        }
    }
    fn json(self) -> Value {
        match self {
            Mark::Seq(s) => json!({"seq": s}),
            Mark::Ms(ms) => json!({"ms": ms}),
        }
    }
    fn parse(v: &Value) -> Option<Mark> {
        v["seq"].as_i64().map(Mark::Seq).or_else(|| v["ms"].as_i64().map(Mark::Ms))
    }
}

/// One bounded read of a room: newest first from `before` (None = the newest) back to `since`. What qualifies goes to
/// pending. Returns Ok(None) when it reached `since` (or the room's start), Ok(Some(seq)) when it stopped at the page
/// bound and must resume before `seq`, Err(seq_or_none) when a read failed (resume there; None = nothing read).
fn read_back(ledger: &mut Ledger, room: &str, me: &str, before: Option<i64>, since: Mark, get: &dyn Fn(&str) -> Option<Value>) -> Result<Option<i64>, Option<i64>> {
    let mut before = before;
    for _ in 0..SCAN_PAGES {
        let path = format!("/search?channel={}&limit=50{}", crate::urlencode(room), before.map(|b| format!("&before={b}")).unwrap_or_default());
        let Some(page) = get(&path) else { return Err(before) };
        let items = page["items"].as_array().cloned().unwrap_or_default();
        for it in &items {
            if since.reached(it) {
                return Ok(None);
            }
            let from_person = it["author"].as_str().is_some_and(|a| PEOPLE.contains(&a));
            let mine = addressed_to(it, me);
            let id = it["id"].as_str().unwrap_or("");
            if is_message(it) && it["author"].as_str() != Some(me) && (from_person || mine) && !id.is_empty()
                && !ledger.pending.iter().any(|p| p["id"] == it["id"]) {
                ledger.pending.push(json!({"id": id, "channel": room, "seq": it["seq"], "author": it["author"], "created_ms": it["created_ms"],
                    "content": it["content"].as_str().and_then(|c| c.lines().next()).unwrap_or(""), "for_me": mine}));
            }
        }
        match items.last().and_then(|it| it["seq"].as_i64()) {
            Some(seq) if items.len() == 50 => before = Some(seq),
            _ => return Ok(None),
        }
    }
    Ok(before)
}

/// Is this message addressed to `me`: a `to:` list naming it, or an @mention in the text.
fn addressed_to(it: &Value, me: &str) -> bool {
    if me.is_empty() {
        return false;
    }
    let tags = it["tags"].as_array().into_iter().flatten().filter_map(Value::as_str);
    if tags.filter_map(|t| t.strip_prefix("to:")).flat_map(|v| v.split(',')).any(|v| v.trim() == me) {
        return true;
    }
    let text = it["content"].as_str().unwrap_or("");
    let at = format!("@{me}");
    text.match_indices(&at).any(|(i, _)| !text[i + at.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
}

/// A message, not a reaction, an annotation or a State write (the relay's rule: a seen-mark never rings).
fn is_message(it: &Value) -> bool {
    it["kind"].as_str() != Some("state")
        && !it["tags"].as_array().into_iter().flatten().filter_map(Value::as_str).any(|t| t.starts_with("react:") || t.starts_with("annotates:") || t.starts_with("set:"))
}

pub struct Doorbell {
    /// The parent session (CLAUDE_CODE_SESSION_ID; the socket's name only when that is missing).
    pub session: String,
    pub socket: PathBuf,
    dir: PathBuf,
    /// Live: the ring is sent to the parent session's inbox socket. Shadow (the default; OX_DOORBELL=live turns it on): only logged.
    live: bool,
    /// The parent session's CLAUDE_CODE_MESSAGING_TOKEN: the auth line that makes the session accept the ring as its own.
    token: Option<String>,
}

impl Doorbell {
    /// The doorbell for the Claude Code session this process belongs to, if one started it.
    pub fn from_env(home: &Path) -> Option<Doorbell> {
        let socket = PathBuf::from(std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET")?);
        let session = std::env::var("CLAUDE_CODE_SESSION_ID").ok().filter(|s| !s.is_empty());
        let mut bell = Doorbell::new(home, socket, session)?;
        // Shadow unless a seat asks for live (Blaze, 2026-10-04: the DOT ring is the one channel).
        bell.live = std::env::var("OX_DOORBELL").is_ok_and(|m| m == "live");
        bell.token = std::env::var("CLAUDE_CODE_MESSAGING_TOKEN").ok().filter(|t| !t.is_empty());
        Some(bell)
    }

    pub fn new(home: &Path, socket: PathBuf, session: Option<String>) -> Option<Doorbell> {
        // A process id is reused; a session id is not. The socket name is only the fallback.
        let session = session.or_else(|| socket.file_stem()?.to_str().map(|s| format!("socket-{s}")))?;
        let session: String = session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        Some(Doorbell { session, socket, dir: home.join("doorbell"), live: false, token: None })
    }

    fn session_file(&self) -> PathBuf {
        self.dir.join(format!("session-{}.json", self.session))
    }

    fn log_file(&self) -> PathBuf {
        self.dir.join(if self.live { "doorbell.jsonl" } else { "shadow.jsonl" })
    }

    fn ledger_file(&self) -> &'static str {
        if self.live { LIVE_LEDGER_FILE } else { LEDGER_FILE }
    }

    /// Sends one ring to the parent session's inbox socket: the auth line, then the message frame
    /// (code.claude.com/docs/en/cross-session-messaging#the-sessions-inbox-socket). The socket never answers, so success
    /// means the bytes were written; the session's own transcript is the receipt.
    #[cfg(not(unix))]
    fn send(&self, _text: &str) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "a session's inbox is a Unix socket, which this system does not have"))
    }

    #[cfg(unix)]
    fn send(&self, text: &str) -> std::io::Result<()> {
        use std::io::Write;
        let mut s = std::os::unix::net::UnixStream::connect(&self.socket)?;
        s.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
        let mut out = String::new();
        if let Some(t) = &self.token {
            out += &json!({"type": "auth", "token": t}).to_string();
            out.push('\n');
        }
        out += &json!({"type": "user", "message": {"role": "user", "content": text}}).to_string();
        out.push('\n');
        s.write_all(out.as_bytes())?;
        s.shutdown(std::net::Shutdown::Write)
    }

    /// One look at the seat's open asks (the body of `GET /asks?state=open`): the ring this session would get now,
    /// if any. Records the selection.
    /// One look: the seat's open asks (the body of `GET /asks?state=open`) and, through `get` (a GET on the Oracle, None
    /// on failure), the rooms' new messages. Returns the ring this session would get now, if any, and records it.
    pub fn tick(&self, asks: &Value, get: &dyn Fn(&str) -> Option<Value>, now_ms: i64) -> std::io::Result<Option<String>> {
        std::fs::create_dir_all(&self.dir)?;
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(self.dir.join("rung.lock"))?;
        lock.lock()?;
        let me = asks["seat"].as_str().unwrap_or("");
        // A seat's own asks never ring it (the relay's rule): the server can list an ask its author decides or owes a
        // follow-up on, but the author wrote it and already knows (rang twice on 2026-10-03).
        let open: Vec<&Value> = pending(asks).into_iter().filter(|a| a["author"].as_str() != Some(me)).collect();
        let open_ids: BTreeSet<&str> = open.iter().filter_map(|a| a["id"].as_str()).collect();
        // A list the server cut short is partial: an ask missing from it is unknown, not resolved.
        let complete = covers_all(asks);
        let mut ledger = self.ledger()?;
        // A seeded or rung message that is an open ask moves to the asks kept while open: the message history is
        // bounded, and an evicted id must not ring again (Emmy, review of #65).
        let promote: Vec<String> = ledger.messages.iter().filter(|m| open_ids.contains(m.as_str())).cloned().collect();
        ledger.asks.extend(promote);
        // Rung asks that are still open are never evicted; one is forgotten only when a COMPLETE list no longer has it.
        if complete {
            ledger.asks.retain(|id| open_ids.contains(id.as_str()));
        }
        let partial_scan = self.scan(&mut ledger, me, get);
        // What this seat already read is not news (Blaze, 2026-10-03: "you should get one notification for one thing";
        // the fyi listed messages Rocky had read and marked seen). A pending message at or below the seat's read mark in
        // its room is dropped and remembered as handled, so it never rings. A room whose marks cannot be read keeps its
        // messages: a ring too many beats one lost.
        let rooms: BTreeSet<String> = ledger.pending.iter().filter_map(|m| m["channel"].as_str().map(String::from)).collect();
        let mut read_to: std::collections::BTreeMap<String, i64> = Default::default();
        for room in &rooms {
            if let Some(v) = get(&format!("/fold?marks=true&channel={room}")) {
                if let Some(m) = v["marks"].as_array().into_iter().flatten().find(|m| m["agent"].as_str() == Some(me)) {
                    if let Some(seq) = m["mark_seq"].as_i64() { read_to.insert(room.clone(), seq); }
                }
            }
        }
        let already_read = |m: &Value| match (m["channel"].as_str().and_then(|r| read_to.get(r)), m["seq"].as_i64()) {
            (Some(mark), Some(seq)) => seq <= *mark,
            _ => false,
        };
        let read: Vec<String> = ledger.pending.iter().filter(|m| already_read(m)).filter_map(|m| m["id"].as_str().map(String::from)).collect();
        ledger.pending.retain(|m| !already_read(m));
        ledger.messages.extend(read);
        let rung = |id: &str| ledger.asks.contains(id) || ledger.messages.iter().any(|m| m == id);
        let due: Vec<&Value> = open.iter().copied().filter(|a| a["id"].as_str().is_some_and(|id| !rung(id))).collect();
        let waiting: Vec<Value> = ledger.pending.iter().filter(|m| m["id"].as_str().is_some_and(|id| !rung(id) && !open_ids.contains(id))).cloned().collect();
        let people: Vec<&Value> = waiting.iter().filter(|m| m["author"].as_str().is_some_and(|a| PEOPLE.contains(&a))).collect();
        let fyi: Vec<&Value> = waiting.iter().filter(|m| m["for_me"] == true && !m["author"].as_str().is_some_and(|a| PEOPLE.contains(&a))).collect();
        let saved: Value = std::fs::read_to_string(self.session_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
        let last = saved["last_ms"].as_i64().unwrap_or(0);
        let digest_due = (!fyi.is_empty() || ledger.overflow > 0) && now_ms - ledger.digest_ms >= DIGEST_EVERY_MS;
        if (due.is_empty() && people.is_empty() && !digest_due) || now_ms - last < MIN_GAP_MS || !self.seat_budget(now_ms)? {
            ledger.pending = waiting;
            self.save_ledger(&ledger)?;
            return Ok(None);
        }
        let mut text = ring_text(&due, &people, &fyi);
        if ledger.overflow > 0 {
            text = text.replace("\nRead with oracle_thread <id>.", &format!("\n· and {} more for you, not kept: oracle_inbox\nRead with oracle_thread <id>.", ledger.overflow));
        }
        if self.live {
            if let Err(e) = self.send(&text) {
                // Not sent: nothing is marked rung, so the next look tries again.
                ledger.pending = waiting;
                self.save_ledger(&ledger)?;
                drop(lock);
                self.log(&json!({"at_ms": now_ms, "session": self.session, "socket": self.socket, "mode": "live", "delivered": false, "error": e.to_string()}))?;
                return Ok(None);
            }
        }
        let rang: BTreeSet<String> = people.iter().chain(fyi.iter()).filter_map(|m| m["id"].as_str().map(String::from)).collect();
        ledger.asks.extend(due.iter().filter_map(|a| a["id"].as_str().map(String::from)));
        ledger.messages.extend(rang.iter().cloned());
        ledger.pending = waiting.iter().filter(|m| m["id"].as_str().is_some_and(|id| !rang.contains(id))).cloned().collect();
        ledger.digest_ms = now_ms;
        let overflow = std::mem::take(&mut ledger.overflow);
        self.save_ledger(&ledger)?;
        std::fs::write(self.session_file(), json!({"last_ms": now_ms}).to_string())?;
        drop(lock);
        let mut line = json!({"at_ms": now_ms, "session": self.session, "socket": self.socket, "mode": if self.live { "live" } else { "shadow" },
                         "selected": due.iter().map(|a| &a["id"]).collect::<Vec<_>>(), "named": due.len().min(MAX_ITEMS),
                         "people": people.iter().map(|m| &m["id"]).collect::<Vec<_>>(), "fyi": fyi.iter().map(|m| &m["id"]).collect::<Vec<_>>(),
                         "coverage": if complete { "complete" } else { "partial" }, "scan": if partial_scan { "partial" } else { "complete" }, "overflow": overflow,
                         "bytes": text.len(), "text": text});
        // A shadow record only ever says it WOULD deliver; only a live send is recorded as delivered.
        if self.live {
            line["delivered"] = json!(true);
            line["auth"] = json!(self.token.is_some());
        } else {
            line["would_deliver"] = json!(true);
        }
        self.log(&line)?;
        Ok(Some(text))
    }

    /// The doorbell's own scan of new messages: first the gaps earlier looks left, then the rooms whose `/channels`
    /// last_ms passed the scan's position, each paged newest first (`/search?channel=`) back to it. On the first scan every
    /// room starts at now (no history is read). A read that stops at the page bound, or fails, leaves a gap that later
    /// looks finish; a room whose first read fails keeps its position. What is from a person, or addressed to `me`,
    /// goes to `pending`. Returns true when this look left anything unread.
    fn scan(&self, ledger: &mut Ledger, me: &str, get: &dyn Fn(&str) -> Option<Value>) -> bool {
        let mut partial = false;
        // Older ranges first: one bounded read each.
        for (room, before, since) in std::mem::take(&mut ledger.gaps) {
            match read_back(ledger, &room, me, Some(before), since, get) {
                Ok(None) => {}
                Ok(Some(next)) | Err(Some(next)) => {
                    partial = true;
                    ledger.gaps.push((room, next, since));
                }
                Err(None) => {
                    partial = true;
                    ledger.gaps.push((room, before, since));
                }
            }
        }
        let Some(channels) = get("/channels") else { return true };
        let first = ledger.cursors.is_empty() && ledger.ms_cursors.is_empty();
        for c in channels["channels"].as_array().into_iter().flatten() {
            let Some(room) = c["channel"].as_str() else { continue };
            // The room's head now, and the watermark this scan last read to, in the same unit.
            let (head, seen) = match c["last_seq"].as_i64() {
                // A room first read by time (an older ox, or a server before last_seq) moves to seq here: this read goes
                // back to its time position, never to seq 0 (Emmy, review of 8307d72).
                Some(seq) => (Mark::Seq(seq), ledger.cursors.get(room).copied().map(Mark::Seq)
                    .or_else(|| ledger.ms_cursors.get(room).copied().map(Mark::Ms))),
                None => match c["last_ms"].as_i64() {
                    Some(ms) => (Mark::Ms(ms), ledger.ms_cursors.get(room).copied().map(Mark::Ms)),
                    None => continue,
                },
            };
            let set = |ledger: &mut Ledger| match head {
                Mark::Seq(s) => {
                    ledger.ms_cursors.remove(room);
                    ledger.cursors.insert(room.to_owned(), s)
                }
                Mark::Ms(ms) => ledger.ms_cursors.insert(room.to_owned(), ms),
            };
            let since = match seen {
                None if first => {
                    set(ledger);
                    continue;
                }
                None => match head { Mark::Seq(_) => Mark::Seq(0), Mark::Ms(_) => Mark::Ms(0) },
                Some(Mark::Seq(s)) if matches!(head, Mark::Seq(h) if h > s) => Mark::Seq(s),
                Some(Mark::Ms(ms)) if matches!(head, Mark::Seq(_)) => Mark::Ms(ms),
                // Fallback (no last_seq): read when the head's time moved, back to the time position.
                Some(Mark::Ms(ms)) if matches!(head, Mark::Ms(h) if h > ms) => Mark::Ms(ms),
                Some(_) => continue,
            };
            match read_back(ledger, room, me, None, since, get) {
                Ok(None) => {
                    set(ledger);
                }
                // Stopped at the bound, or failed after some pages: the newer part is read, the rest is a gap to finish.
                Ok(Some(before)) | Err(Some(before)) => {
                    partial = true;
                    ledger.gaps.push((room.to_owned(), before, since));
                    set(ledger);
                }
                // Nothing could be read: the position stays, and the next look tries again.
                Err(None) => partial = true,
            }
        }
        ledger.pending.sort_by_key(|m| m["created_ms"].as_i64().unwrap_or(0));
        let over = ledger.pending.len().saturating_sub(PENDING_KEPT);
        ledger.pending.drain(..over);
        ledger.overflow += over as i64;
        partial
    }

    /// What this seat has been rung for, on this machine. Created once from the relay's `seen` (one id per line) and
    /// any per-session records the earlier doorbell left, so the cutover rings nothing the relay already rang.
    fn ledger(&self) -> std::io::Result<Ledger> {
        let path = self.dir.join(self.ledger_file());
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                let list = |k: &str| v[k].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>();
                let map = |k: &str| v[k].as_object().into_iter().flatten().filter_map(|(k, x)| x.as_i64().map(|n| (k.clone(), n))).collect();
                let (cursors, ms_cursors) = if v["watermark"] == "seq" { (map("cursors"), map("ms_cursors")) } else { (Default::default(), map("cursors")) };
                return Ok(Ledger { asks: list("asks").into_iter().collect(), messages: list("messages"), digest_ms: v["digest_ms"].as_i64().unwrap_or(0),
                    cursors, ms_cursors, pending: v["pending"].as_array().cloned().unwrap_or_default(),
                    gaps: v["gaps"].as_array().into_iter().flatten()
                        .filter_map(|g| Some((g[0].as_str()?.to_owned(), g[1].as_i64()?, Mark::parse(&g[2]).or_else(|| g[2].as_i64().map(Mark::Ms))?))).collect(),
                    overflow: v["overflow"].as_i64().unwrap_or(0) });
            }
        }
        let mut messages: Vec<String> = std::fs::read_to_string(self.dir.join("seen")).unwrap_or_default().lines().map(str::trim)
            .filter(|l| !l.is_empty()).map(String::from).collect();
        for e in std::fs::read_dir(&self.dir)?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("session-") && name.ends_with(".json") {
                let v: Value = std::fs::read_to_string(e.path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
                messages.extend(v["delivered"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)));
            }
        }
        Ok(Ledger { asks: BTreeSet::new(), messages, digest_ms: 0, cursors: Default::default(), ms_cursors: Default::default(), pending: vec![], gaps: vec![], overflow: 0 })
    }

    fn save_ledger(&self, l: &Ledger) -> std::io::Result<()> {
        let skip = l.messages.len().saturating_sub(MESSAGES_KEPT);
        let tmp = self.dir.join(format!("{}.tmp", self.ledger_file()));
        std::fs::write(&tmp, json!({"asks": l.asks, "messages": l.messages[skip..], "digest_ms": l.digest_ms, "watermark": "seq", "cursors": l.cursors, "ms_cursors": l.ms_cursors, "pending": l.pending,
            "gaps": l.gaps.iter().map(|(r, b, s)| json!([r, b, s.json()])).collect::<Vec<_>>(), "overflow": l.overflow}).to_string())?;
        std::fs::rename(tmp, self.dir.join(self.ledger_file()))
    }

    /// Take one ring from the seat's budget (shared by every session of this seat on the machine), under a lock.
    fn seat_budget(&self, now_ms: i64) -> std::io::Result<bool> {
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(self.dir.join("seat.lock"))?;
        lock.lock()?;
        let path = self.dir.join("seat.json");
        let v: Value = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
        let mut rings: Vec<i64> = v["rings"].as_array().into_iter().flatten().filter_map(Value::as_i64).filter(|t| now_ms - t < SEAT_WINDOW_MS).collect();
        let ok = rings.len() < SEAT_MAX;
        if ok {
            rings.push(now_ms);
        }
        std::fs::write(&path, json!({"rings": rings}).to_string())?;
        drop(lock);
        Ok(ok)
    }

    fn log(&self, line: &Value) -> std::io::Result<()> {
        use std::io::Write;
        let path = self.log_file();
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > LOG_MAX_BYTES {
            std::fs::rename(&path, path.with_extension("jsonl.1"))?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(f, "{line}")
    }
}

/// The server's `/asks` lists are cut at this many rows (oracle-server asks.rs LIST_CAP), without a flag.
pub const SERVER_LIST_CAP: usize = 200;

/// Does this `/asks` answer list every open ask waiting on the seat? Not when the server says its candidate read was
/// capped, nor when the list is a full page (it may have been truncated). Asks beyond a partial list cannot be seen
/// at all until `/asks` pages: a stated limit of shadow mode, recorded as `coverage: partial` on every selection.
pub fn covers_all(asks: &Value) -> bool {
    asks["capped"] != true && asks["waiting_on_me"].as_array().is_some_and(|a| a.len() < SERVER_LIST_CAP)
}

/// The asks waiting on this seat that the projection says are still open.
pub fn pending(asks: &Value) -> Vec<&Value> {
    asks["waiting_on_me"].as_array().map(|a| a.iter().filter(|r| r["state"] == "open").collect()).unwrap_or_default()
}

/// One coalesced ring: at most [`MAX_ITEMS`] asks by kind, room, author and the server's card line (already bounded
/// by the projection), each with its id to read it by; the rest counted. Then messages from people, by room and id
/// (with the first line when it is short enough to be whole), and one `fyi` line of ids.
pub fn ring_text(due: &[&Value], people: &[&Value], fyi: &[&Value]) -> String {
    let mut out = if due.is_empty() { "Oracle: for you".to_string() } else { format!("Oracle: {} open for you", due.len()) };
    for a in due.iter().take(MAX_ITEMS) {
        out.push_str(&format!("\n· {} · #{} {}: {} ({})", a["ask"].as_str().unwrap_or("?"), a["room"].as_str().unwrap_or("?"),
            a["author"].as_str().unwrap_or("?"), a["card"].as_str().unwrap_or(""), a["id"].as_str().unwrap_or("?")));
    }
    if due.len() > MAX_ITEMS {
        out.push_str(&format!("\n· and {} more: oracle_brief", due.len() - MAX_ITEMS));
    }
    for m in people {
        let first = m["content"].as_str().and_then(|c| c.lines().next()).filter(|l| l.chars().count() <= 160).unwrap_or("");
        out.push_str(&format!("\n· from {} · #{}: {} ({})", m["author"].as_str().unwrap_or("?"), m["channel"].as_str().unwrap_or("?"), first, m["id"].as_str().unwrap_or("?")));
    }
    if !fyi.is_empty() {
        let mut f: Vec<&&Value> = fyi.iter().collect();
        f.sort_by_key(|m| m["created_ms"].as_i64().unwrap_or(0));
        let tail: Vec<String> = f.iter().rev().take(8).rev().map(|m| format!("#{} {} {}", m["channel"].as_str().unwrap_or("?"),
            m["author"].as_str().unwrap_or("?"), m["id"].as_str().unwrap_or("?"))).collect();
        out.push_str(&format!("\nfyi {}: {}", fyi.len(), tail.join(" · ")));
    }
    out.push_str("\nRead with oracle_thread <id>.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, state: &str) -> Value {
        json!({"id": id, "room": "oracle", "author": "emmy", "ask": "review", "state": state, "card": format!("@rocky please review {id}")})
    }
    fn asks(rows: Vec<Value>) -> Value {
        json!({"seat": "rocky", "waiting_on_me": rows, "im_waiting_on": [], "done_today": [], "capped": false})
    }
    fn home(name: &str) -> PathBuf {
        let h = std::env::temp_dir().join(format!("ox-bell-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        h
    }
    fn bell(h: &Path, session: &str) -> Doorbell {
        Doorbell::new(h, PathBuf::from("/tmp/cc-socks/4242.sock"), Some(session.into())).unwrap()
    }
    const T0: i64 = 1_800_000_000_000;
    fn no(_: &str) -> Option<Value> {
        None
    }

    /// A toy Oracle for the scan: rooms of messages, `/channels` and newest-first `/search?channel=…&before=`.
    struct Rooms(std::cell::RefCell<Vec<Value>>, std::cell::Cell<bool>, std::cell::Cell<i64>);
    impl Rooms {
        fn new() -> Rooms {
            Rooms(std::cell::RefCell::new(vec![json!({"id": "SEED", "channel": "oracle", "author": "emmy", "content": "x", "tags": [], "seq": 1, "created_ms": T0})]),
                std::cell::Cell::new(true), std::cell::Cell::new(0))
        }
        fn say(&self, id: &str, author: &str, content: &str, tags: &[&str]) {
            let seq = self.0.borrow().len() as i64 + 1;
            self.say_at(id, author, content, tags, T0 + seq);
        }
        /// A message whose created_ms is given: the same millisecond as another, or an import's older time.
        fn say_at(&self, id: &str, author: &str, content: &str, tags: &[&str], created_ms: i64) {
            let mut v = self.0.borrow_mut();
            let seq = v.len() as i64 + 1;
            v.push(json!({"id": id, "channel": "oracle", "author": author, "content": content, "tags": tags, "seq": seq, "created_ms": created_ms}));
        }
        fn get(&self, path: &str) -> Option<Value> {
            let v = self.0.borrow();
            if path == "/fold?marks=true&channel=oracle" {
                return Some(json!({"channel": "oracle", "marks": [{"agent": "rocky", "mark_seq": self.2.get()}, {"agent": "emmy", "mark_seq": 999}]}));
            }
            if path == "/channels" {
                let last = v.iter().max_by_key(|m| m["seq"].as_i64().unwrap()).unwrap();
                let mut c = json!({"channel": "oracle", "count": v.len(), "last_ms": last["created_ms"], "last_seq": last["seq"]});
                if !self.1.get() {
                    c.as_object_mut().unwrap().remove("last_seq");
                }
                return Some(json!({"channels": [c]}));
            }
            let before = path.split("&before=").nth(1).and_then(|b| b.parse::<i64>().ok()).unwrap_or(i64::MAX);
            let page: Vec<Value> = v.iter().rev().filter(|m| m["seq"].as_i64().unwrap() < before).take(50).cloned().collect();
            Some(json!({"items": page}))
        }
    }
    fn seat(rows: Vec<Value>) -> Value {
        let mut a = asks(rows);
        a["seat"] = json!("rocky");
        a
    }

    #[test]
    fn only_open_asks_waiting_on_the_seat_ring_and_a_ring_is_bounded() {
        let a = asks(vec![row("A", "open"), row("B", "answered"), row("C", "done"), row("D", "open")]);
        let ids: Vec<&str> = pending(&a).iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["A", "D"]);
        let many: Vec<Value> = (0..25).map(|i| row(&format!("X{i}"), "open")).collect();
        let m = asks(many);
        let text = ring_text(&pending(&m), &[], &[]);
        assert_eq!(text.lines().filter(|l| l.starts_with("· review")).count(), MAX_ITEMS);
        assert!(text.contains("· and 15 more: oracle_brief"), "{text}");
    }

    #[test]
    fn an_open_ask_rings_once_per_session_and_is_never_forgotten_while_open() {
        let h = home("forget");
        // 501 open asks: more than any fixed-size memory would keep.
        let rows: Vec<Value> = (0..501).map(|i| row(&format!("OBS-{i:04}"), "open")).collect();
        let a = asks(rows);
        assert!(bell(&h, "s1").tick(&a, &no, T0).unwrap().is_some());
        // Later ticks and a restarted ox mcp for the same session: none of the 501 rings again.
        for k in 1..4 {
            assert_eq!(bell(&h, "s1").tick(&a, &no, T0 + k * 10 * MIN_GAP_MS).unwrap(), None);
        }
        // One resolves (drops out of the open list) and a new one opens: only the new one rings.
        let mut rows: Vec<Value> = (1..501).map(|i| row(&format!("OBS-{i:04}"), "open")).collect();
        rows.push(row("OBS-new", "open"));
        let rung = bell(&h, "s1").tick(&asks(rows), &no, T0 + 50 * MIN_GAP_MS).unwrap().unwrap();
        assert!(rung.starts_with("Oracle: 1 open for you") && rung.contains("(OBS-new)"), "{rung}");
    }

    #[test]
    fn a_partial_list_never_resolves_what_it_left_out() {
        let h = home("capped");
        let capped = |rows: Vec<Value>| { let mut a = asks(rows); a["capped"] = json!(true); a };
        assert!(!covers_all(&capped(vec![row("A", "open")])));
        // A and B ring.
        assert!(bell(&h, "s1").tick(&capped(vec![row("A", "open"), row("B", "open")]), &no, T0).unwrap().is_some());
        // The capped window shifts: A is absent, C appears. Only C rings; A is not taken as resolved.
        let r = bell(&h, "s1").tick(&capped(vec![row("B", "open"), row("C", "open")]), &no, T0 + 2 * MIN_GAP_MS).unwrap().unwrap();
        assert!(r.contains("(C)") && !r.contains("(A)") && !r.contains("(B)"), "{r}");
        // A is back in view, still open: it does not ring again.
        assert_eq!(bell(&h, "s1").tick(&capped(vec![row("A", "open"), row("C", "open")]), &no, T0 + 4 * MIN_GAP_MS).unwrap(), None);
        // A full page without the flag is partial too.
        let full: Vec<Value> = (0..SERVER_LIST_CAP).map(|i| row(&format!("P{i}"), "open")).collect();
        assert!(!covers_all(&asks(full)));
        // Only a complete list that no longer has A forgets it (resolved); then a reopened A rings again.
        assert_eq!(bell(&h, "s1").tick(&asks(vec![row("C", "open")]), &no, T0 + 6 * MIN_GAP_MS).unwrap(), None);
        assert!(bell(&h, "s1").tick(&asks(vec![row("A", "open"), row("C", "open")]), &no, T0 + 8 * MIN_GAP_MS).unwrap().is_some_and(|t| t.contains("(A)")));
        let log = std::fs::read_to_string(h.join("doorbell/shadow.jsonl")).unwrap();
        assert!(log.lines().next().unwrap().contains("\"coverage\":\"partial\""));
    }

    #[test]
    fn rings_are_spaced_per_session_and_budgeted_per_seat_across_sessions() {
        let h = home("budget");
        assert!(bell(&h, "s1").tick(&asks(vec![row("A", "open")]), &no, T0).unwrap().is_some());
        // A new ask inside the session's gap waits.
        assert_eq!(bell(&h, "s1").tick(&asks(vec![row("A", "open"), row("B", "open")]), &no, T0 + 1_000).unwrap(), None);
        // Other sessions of the same seat share one budget: SEAT_MAX rings per window in all.
        let mut rung = 1;
        for s in 2..12 {
            if bell(&h, &format!("s{s}")).tick(&asks(vec![row(&format!("Q{s}"), "open")]), &no, T0 + 2_000).unwrap().is_some() {
                rung += 1;
            }
        }
        assert_eq!(rung, SEAT_MAX, "the seat's budget holds across sessions");
        // Once the window has passed, the seat can be rung again.
        assert!(bell(&h, "s20").tick(&asks(vec![row("Z", "open")]), &no, T0 + SEAT_WINDOW_MS + 3_000).unwrap().is_some());
    }

    #[test]
    fn the_shadow_log_says_would_deliver_and_rotates() {
        let h = home("log");
        let b = bell(&h, "s1");
        b.tick(&asks(vec![row("A", "open")]), &no, T0).unwrap();
        let log = std::fs::read_to_string(h.join("doorbell/shadow.jsonl")).unwrap();
        let v: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!((v["mode"].clone(), v["would_deliver"].clone()), (json!("shadow"), json!(true)));
        assert!(v.get("delivered").is_none(), "a selection is never recorded as a delivery");
        std::fs::write(h.join("doorbell/shadow.jsonl"), vec![b'x'; (LOG_MAX_BYTES + 1) as usize]).unwrap();
        b.tick(&asks(vec![row("A", "open"), row("B", "open")]), &no, T0 + 2 * MIN_GAP_MS).unwrap();
        assert!(h.join("doorbell/shadow.jsonl.1").exists());
        assert!(std::fs::metadata(h.join("doorbell/shadow.jsonl")).unwrap().len() < 10_000);
    }

    #[test]
    fn a_process_id_is_not_a_session_id() {
        let h = home("ids");
        let a = Doorbell::new(&h, PathBuf::from("/tmp/cc-socks/4242.sock"), Some("sess-one".into())).unwrap();
        let b = Doorbell::new(&h, PathBuf::from("/tmp/cc-socks/4242.sock"), Some("sess-two".into())).unwrap();
        assert_ne!(a.session_file(), b.session_file(), "the same reused pid socket, two sessions: two records");
        assert!(a.tick(&asks(vec![row("A", "open")]), &no, T0).unwrap().is_some());
        // What has rung is the seat's: a second session is not rung again for A (its brief lists it), only for news.
        assert_eq!(b.tick(&asks(vec![row("A", "open")]), &no, T0).unwrap(), None);
        assert!(b.tick(&asks(vec![row("A", "open"), row("B", "open")]), &no, T0 + 1).unwrap().is_some_and(|t| t.contains("(B)") && !t.contains("(A)")));
    }

    #[cfg(unix)]
    fn live_bell(h: &Path, socket: PathBuf) -> Doorbell {
        let mut b = Doorbell::new(h, socket, Some("live-1".into())).unwrap();
        b.live = true;
        b.token = Some("tok-123".into());
        b
    }

    #[cfg(unix)]
    #[test]
    fn live_sends_the_auth_line_then_the_ring_to_its_own_session_and_remembers_it_apart_from_shadow() {
        use std::io::Read;
        let h = home("live");
        std::fs::create_dir_all(&h).unwrap();
        let sock = h.join("s.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let open = asks(vec![row("A1", "open")]);
        let bell = live_bell(&h, sock.clone());
        let r = bell.tick(&open, &no, T0).unwrap().unwrap();
        let (mut c, _) = listener.accept().unwrap();
        let mut got = String::new();
        c.read_to_string(&mut got).unwrap();
        let lines: Vec<Value> = got.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2, "{got}");
        assert_eq!(lines[0], json!({"type": "auth", "token": "tok-123"}));
        assert_eq!(lines[1]["type"], "user");
        assert_eq!(lines[1]["message"]["content"].as_str().unwrap(), r);
        assert!(r.contains("(A1)"), "{r}");
        // The live ledger and log are their own files; shadow's are untouched.
        assert!(h.join("doorbell").join(LIVE_LEDGER_FILE).exists() && !h.join("doorbell").join(LEDGER_FILE).exists());
        let log: Value = serde_json::from_str(std::fs::read_to_string(h.join("doorbell/doorbell.jsonl")).unwrap().lines().last().unwrap()).unwrap();
        assert_eq!((log["mode"].clone(), log["delivered"].clone(), log["auth"].clone()), (json!("live"), json!(true), json!(true)));
        // Rung once: the same ask does not ring again.
        assert_eq!(bell.tick(&open, &no, T0 + 2 * MIN_GAP_MS).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_ring_that_cannot_be_sent_is_not_marked_rung_and_goes_out_on_the_next_look() {
        use std::io::Read;
        let h = home("live-retry");
        std::fs::create_dir_all(&h).unwrap();
        let sock = h.join("s.sock");
        let open = asks(vec![row("B1", "open")]);
        // No one listening yet: nothing is sent, and nothing is remembered as rung.
        assert_eq!(live_bell(&h, sock.clone()).tick(&open, &no, T0).unwrap(), None);
        let log = std::fs::read_to_string(h.join("doorbell/doorbell.jsonl")).unwrap();
        assert!(log.contains("\"delivered\":false"), "{log}");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let r = live_bell(&h, sock).tick(&open, &no, T0 + 1).unwrap().unwrap();
        let (mut c, _) = listener.accept().unwrap();
        let mut got = String::new();
        c.read_to_string(&mut got).unwrap();
        assert!(r.contains("(B1)") && got.contains("(B1)"), "{got}");
    }

    #[test]
    fn a_seat_is_never_rung_with_an_ask_it_wrote_itself() {
        let h = home("own");
        let mut mine = row("MINE", "open");
        mine["author"] = json!("rocky");
        let r = bell(&h, "s1").tick(&asks(vec![mine.clone(), row("THEIRS", "open")]), &no, T0).unwrap().unwrap();
        assert!(r.contains("(THEIRS)") && !r.contains("(MINE)"), "{r}");
        // NEGATIVE CONTROL: the same ask from someone else rings.
        let h2 = home("own2");
        mine["author"] = json!("emmy");
        assert!(bell(&h2, "s1").tick(&asks(vec![mine]), &no, T0).unwrap().unwrap().contains("(MINE)"));
    }

    #[test]
    fn the_cutover_rings_nothing_the_relay_already_rang_and_a_restart_rings_no_backlog() {
        let h = home("seed");
        std::fs::create_dir_all(h.join("doorbell")).unwrap();
        std::fs::write(h.join("doorbell/seen"), "OLD-1\nOLD-2\n").unwrap();
        let open = asks(vec![row("OLD-1", "open"), row("OLD-2", "open"), row("NEW", "open")]);
        let r = bell(&h, "s1").tick(&open, &no, T0).unwrap().unwrap();
        assert!(r.contains("(NEW)") && !r.contains("OLD-"), "{r}");
        // Claude Code restarts: a new session, the same backlog. Nothing rings.
        assert_eq!(bell(&h, "s2").tick(&open, &no, T0 + 2 * MIN_GAP_MS).unwrap(), None);
        // Shadow state lives apart from any real delivery history.
        assert!(h.join("doorbell").join(LEDGER_FILE).exists() && !h.join("doorbell/rung.json").exists());
    }

    #[test]
    fn a_seeded_open_ask_is_kept_while_open_however_much_history_follows() {
        // Emmy (review of #65): a relay-seen id that is still an open ask must not ring once the bounded history evicts it.
        let h = home("promote");
        std::fs::create_dir_all(h.join("doorbell")).unwrap();
        let mut seen = String::from("OLD-1\n");
        for i in 0..MESSAGES_KEPT + 1 {
            seen.push_str(&format!("M{i}\n"));
        }
        std::fs::write(h.join("doorbell/seen"), seen).unwrap();
        let open = asks(vec![row("OLD-1", "open")]);
        assert_eq!(bell(&h, "s1").tick(&open, &no, T0).unwrap(), None);
        let saved = std::fs::read_to_string(h.join("doorbell").join(LEDGER_FILE)).unwrap();
        assert!(!saved.contains("\"messages\":[\"OLD-1\""), "the history was trimmed past OLD-1");
        assert_eq!(bell(&h, "s2").tick(&open, &no, T0 + 10 * MIN_GAP_MS).unwrap(), None, "still open, still known");
    }

    #[test]
    fn a_person_rings_now_an_fyi_waits_for_the_digest_or_a_real_ring() {
        let h = home("people");
        let o = Rooms::new();
        let get = |p: &str| o.get(p);
        let none = seat(vec![]);
        // The first scan starts every room at now: history never rings.
        o.say("H1", "blaze", "old", &[]);
        assert_eq!(bell(&h, "s1").tick(&none, &get, T0).unwrap(), None);
        // An addressed status (no ask): the digest is due on the first ring, then waits DIGEST_EVERY_MS.
        o.say("F1", "emmy", "@rocky #59 merged", &["intent:status"]);
        let r = bell(&h, "s1").tick(&none, &get, T0 + 2 * MIN_GAP_MS).unwrap().unwrap();
        assert!(r.contains("fyi 1: #oracle emmy F1"), "{r}");
        o.say("F2", "emmy", "deployed", &["to:rocky,kin"]);
        assert_eq!(bell(&h, "s1").tick(&none, &get, T0 + 4 * MIN_GAP_MS).unwrap(), None, "fyi waits");
        // Blaze speaks (not addressed): rings now and carries the waiting fyi.
        o.say("B1", "blaze", "keep going", &[]);
        let r = bell(&h, "s1").tick(&none, &get, T0 + 6 * MIN_GAP_MS).unwrap().unwrap();
        assert!(r.contains("· from blaze · #oracle: keep going (B1)") && r.contains("fyi 1: #oracle emmy F2"), "{r}");
        // Never: my own message, a reaction, an annotation, a mention of another seat, something for nobody.
        o.say("M1", "rocky", "@rocky note to self", &[]);
        o.say("R1", "blaze", "", &["react:seen"]);
        o.say("A1", "blaze", "", &["annotates:B1", "set:to=emmy"]);
        o.say("X1", "emmy", "@rocky-core and @rockyx", &[]);
        o.say("X2", "emmy", "chatter", &[]);
        assert_eq!(bell(&h, "s1").tick(&none, &get, T0 + 100 * MIN_GAP_MS).unwrap(), None);
    }

    #[test]
    fn a_message_the_seat_already_read_is_not_rung() {
        // Blaze, 2026-10-03: one notification for one thing. Rocky read (and marked seen) messages before the fyi digest
        // went out, and the digest listed them anyway.
        let h = home("read");
        let o = Rooms::new();
        let get = |p: &str| o.get(p);
        assert_eq!(bell(&h, "s1").tick(&seat(vec![]), &get, T0).unwrap(), None);
        o.say("F1", "emmy", "@rocky read before the digest", &[]);
        o.say("F2", "emmy", "@rocky not read yet", &[]);
        o.2.set(2); // rocky's mark covers F1 (seq 2), not F2 (seq 3)
        let r = bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 2 * MIN_GAP_MS).unwrap().unwrap();
        assert!(r.contains("F2") && !r.contains("F1"), "{r}");
        // Control: with no mark, the same kind of message is rung.
        o.2.set(0);
        o.say("F3", "emmy", "@rocky unread", &[]);
        o.say("B3", "blaze", "now", &[]);
        let r = bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 4 * MIN_GAP_MS).unwrap().unwrap();
        assert!(r.contains("F3") && !r.contains("F1"), "F1 stays handled, F3 rings: {r}");
        // A message read after it was found, before the ring, is dropped from the waiting list too.
        o.say("F4", "emmy", "@rocky will be read", &[]);
        assert!(bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 4 * MIN_GAP_MS + 1).unwrap().is_none(), "spaced");
        o.2.set(99);
        o.say("B4", "blaze", "later", &[]);
        let r = bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 8 * MIN_GAP_MS).unwrap();
        assert!(r.as_deref().is_none_or(|t| !t.contains("F4") && !t.contains("B4")), "both read: {r:?}");
    }

    #[test]
    fn news_behind_hundreds_of_unread_messages_is_found() {
        // Emmy (review of #65): the read fold lists the oldest unread first, so it never reached a new message behind
        // 200 unread ones. The scan reads newest first from where it stood.
        let h = home("deep");
        let o = Rooms::new();
        let get = |p: &str| o.get(p);
        assert_eq!(bell(&h, "s1").tick(&seat(vec![]), &get, T0).unwrap(), None);
        o.say("DEEP", "emmy", "@rocky look at this", &[]);
        for i in 0..260 {
            o.say(&format!("N{i}"), "kin", "chatter", &[]);
        }
        o.say("B9", "blaze", "and this", &[]);
        let r = bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 2 * MIN_GAP_MS).unwrap().unwrap();
        assert!(r.contains("(B9)") && r.contains("emmy DEEP"), "{r}");
    }

    #[test]
    fn a_room_that_moved_past_the_page_bound_is_read_whole_over_several_looks() {
        // Emmy (review of 35ef98f): the page bound must bound a look, not skip what is older.
        let h = home("bound");
        let o = Rooms::new();
        let get = |p: &str| o.get(p);
        assert_eq!(bell(&h, "s1").tick(&seat(vec![]), &get, T0).unwrap(), None);
        o.say("OLDEST", "emmy", "@rocky first of many", &[]);
        for i in 0..SCAN_PAGES * 50 + 120 {
            o.say(&format!("N{i}"), "kin", "chatter", &[]);
        }
        o.say("NEWEST", "emmy", "@rocky last", &[]);
        let mut seen = String::new();
        for k in 0..4 {
            if let Some(r) = bell(&h, "s1").tick(&seat(vec![]), &get, T0 + (2 + 40 * k) * MIN_GAP_MS).unwrap() {
                seen.push_str(&r);
            }
        }
        assert!(seen.contains("NEWEST") && seen.contains("OLDEST"), "both ends rang over the looks: {seen}");
        let saved = std::fs::read_to_string(h.join("doorbell").join(LEDGER_FILE)).unwrap();
        assert!(saved.contains("\"gaps\":[]"), "the gap was finished: {saved}");
    }

    #[test]
    fn a_failed_read_moves_no_position() {
        let h = home("fail");
        let o = Rooms::new();
        let up = std::cell::Cell::new(true);
        let get = |p: &str| if up.get() || p == "/channels" { o.get(p) } else { None };
        assert_eq!(bell(&h, "s1").tick(&seat(vec![]), &get, T0).unwrap(), None);
        o.say("LOST?", "emmy", "@rocky while search was down", &[]);
        up.set(false);
        assert_eq!(bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 2 * MIN_GAP_MS).unwrap(), None);
        up.set(true);
        let r = bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 80 * MIN_GAP_MS).unwrap().unwrap();
        assert!(r.contains("LOST?"), "{r}");
    }

    #[test]
    fn waiting_messages_past_the_bound_are_counted_and_said_never_dropped_silently() {
        let h = home("overflow");
        let o = Rooms::new();
        let get = |p: &str| o.get(p);
        assert_eq!(bell(&h, "s1").tick(&seat(vec![]), &get, T0).unwrap(), None);
        // One fyi rings and starts the digest window; inside it, more addressed messages arrive than pending keeps.
        o.say("F-first", "emmy", "@rocky fyi", &[]);
        assert!(bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 2 * MIN_GAP_MS).unwrap().is_some());
        for i in 0..PENDING_KEPT + 600 {
            o.say(&format!("F{i}"), "emmy", "@rocky fyi", &[]);
        }
        for k in 0..((PENDING_KEPT + 600) / (SCAN_PAGES * 50) + 2) as i64 {
            assert_eq!(bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 4 * MIN_GAP_MS + k).unwrap(), None, "the digest waits");
        }
        let r = bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 4 * MIN_GAP_MS + DIGEST_EVERY_MS).unwrap().expect("the digest rings");
        assert!(r.contains(&format!("fyi {PENDING_KEPT}:")), "{r}");
        assert!(r.contains("· and 600 more for you, not kept: oracle_inbox"), "{r}");
    }

    #[test]
    fn the_scan_follows_arrival_order_not_timestamps() {
        // Emmy (review of #65): a message in the same millisecond as the last one read, or an import with an older
        // created_ms at a higher seq, must not be skipped.
        let h = home("seq");
        let o = Rooms::new();
        let get = |p: &str| o.get(p);
        assert_eq!(bell(&h, "s1").tick(&seat(vec![]), &get, T0).unwrap(), None);
        o.say("A", "emmy", "@rocky first", &[]);
        let same_ms = o.0.borrow().last().unwrap()["created_ms"].as_i64().unwrap();
        assert!(bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 2 * MIN_GAP_MS).unwrap().is_some());
        o.say_at("SAME-MS", "blaze", "same millisecond", &[], same_ms);
        let r = bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 4 * MIN_GAP_MS).unwrap().unwrap();
        assert!(r.contains("(SAME-MS)"), "{r}");
        o.say_at("IMPORTED", "blaze", "an older timestamp, a newer seq", &[], T0 - 1_000_000);
        let r = bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 6 * MIN_GAP_MS).unwrap().unwrap();
        assert!(r.contains("(IMPORTED)"), "{r}");
    }

    #[test]
    fn time_positions_move_to_seq_without_reading_history_again() {
        // Emmy (review of 8307d72): state saved by time (an older ox, or before the server sent last_seq) must not
        // fall back to seq 0 when last_seq appears, or old messages from people ring again.
        let h = home("migrate");
        let o = Rooms::new();
        o.1.set(false);
        let get = |p: &str| o.get(p);
        o.say("OLD-BLAZE", "blaze", "said before the doorbell started", &[]);
        assert_eq!(bell(&h, "s1").tick(&seat(vec![]), &get, T0).unwrap(), None);
        o.say("BY-TIME", "blaze", "read by time", &[]);
        assert!(bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 2 * MIN_GAP_MS).unwrap().is_some_and(|r| r.contains("(BY-TIME)")));
        // The server starts sending last_seq; one new message arrives.
        o.1.set(true);
        o.say("AFTER", "blaze", "after the upgrade", &[]);
        let r = bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 4 * MIN_GAP_MS).unwrap().unwrap();
        assert!(r.contains("(AFTER)") && !r.contains("OLD-BLAZE") && !r.contains("(BY-TIME)"), "{r}");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(h.join("doorbell").join(LEDGER_FILE)).unwrap()).unwrap();
        assert!(saved["cursors"]["oracle"].is_i64() && saved["ms_cursors"].get("oracle").is_none(), "{saved}");
        assert_eq!(bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 6 * MIN_GAP_MS).unwrap(), None, "then by seq, nothing again");
    }

    #[test]
    fn a_head_taken_as_a_position_never_rings_when_the_position_moves_to_seq() {
        // Live, 2026-10-01: the first scan took each room's head as its time position without reading it; on the move
        // to seq the boundary was read again, and heads from people rang as news (one from July).
        let h = home("boundary");
        let o = Rooms::new();
        o.1.set(false);
        let get = |p: &str| o.get(p);
        o.say("HEAD", "blaze", "the room's head when the doorbell started", &[]);
        assert_eq!(bell(&h, "s1").tick(&seat(vec![]), &get, T0).unwrap(), None);
        o.1.set(true);
        assert_eq!(bell(&h, "s1").tick(&seat(vec![]), &get, T0 + 2 * MIN_GAP_MS).unwrap(), None, "the head is not news");
    }

    #[test]
    fn against_a_real_oracle_an_addressed_ask_rings_behind_sixty_other_messages() {
        use crate::signed_read_tests::{oracle, sk};
        use oracle_proto::{Envelope, Kind};
        let o = oracle();
        let rocky = o.ox(2);
        let mut mallory = o.ox(3);
        mallory.cfg.handle = "mallory".into();
        mallory.cfg.token = o.mallory_token.clone();
        let say = |text: &str, tags: Vec<String>| {
            let env = Envelope::sign(&sk(3), "svc", "mallory", Kind::Say, text, None, tags, crate::now_ms());
            mallory.post("/observations", &serde_json::to_value(&env).unwrap()).unwrap()["id"].as_str().unwrap().to_string()
        };
        let ask = say("@rocky please act on the deploy", vec!["ask:act".into(), "to:rocky".into()]);
        for i in 0..60 {
            say(&format!("ambient chatter {i}"), vec![]);
        }
        let open = rocky.get("/asks?state=open").unwrap();
        let h = home("real");
        let rung = bell(&h, "s1").tick(&open, &no, crate::now_ms()).unwrap().expect("the addressed ask rings");
        assert!(rung.contains(&format!("({ask})")) && rung.starts_with("Oracle: 1 open for you"), "{rung}");
        // Answered by rocky with a result pointing at it: no longer open, nothing more rings.
        let env = Envelope::sign(&sk(2), "svc", "rocky", Kind::Say, "done: deployed", None, vec!["intent:result".into(), format!("reply:{ask}")], crate::now_ms());
        rocky.post("/observations", &serde_json::to_value(&env).unwrap()).unwrap();
        let open = rocky.get("/asks?state=open").unwrap();
        assert!(pending(&open).is_empty(), "{open}");
    }
}
