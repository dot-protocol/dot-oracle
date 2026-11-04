//! The Oracle's MCP tools, written once and served through two doors.
//!
//! `ox mcp` serves them over stdio on the agent's own machine, signing every write with the
//! seat's key. `oracle serve` serves the same tools at `POST /mcp` for harnesses that can only
//! reach a URL — claude.ai connectors, Codex, a phone. Both doors implement [`Door`]; everything
//! a tool does goes through it, so a tool cannot know which door it came in by.
//!
//! The old Oracle's tool names (`oracle_query`, `oracle_about`, `oracle_thread`, `oracle_board`,
//! `oracle_state`, `room_say`) are served here as aliases, so a client whose prompts and habits
//! were formed on that server keeps working the day the URL points here (Blaze, 2026-09-28: "the
//! cutover should work with existing agents using old oracle").

use anyhow::{anyhow, bail, Result};
use oracle_proto::Kind;
use serde_json::{json, Value};
use std::path::PathBuf;

pub mod state;

pub const PROTOCOL: &str = "2025-06-18";
pub const MEMORY_KINDS: [&str; 4] = ["decision", "lesson", "preference", "commitment"];

/// A room's State (`room-context/1`): its fields, and how a revision is built. Shared by `ox state --set` and the
/// `oracle_room_state` tool, so the CLI and the MCP write exactly the same signed revision.
pub const STATE_FIELDS: [&str; 8] = [
    "purpose",
    "goal",
    "current",
    "scope",
    "constraints",
    "questions",
    "next",
    "acceptance",
];
pub const CONTINUATION_FIELDS: [&str; 2] = ["continued_in", "continued_from"];
/// A room's archive mark (docs/ARCHIVE.md): why the room is out of search; empty or absent means live.
pub const ARCHIVED_FIELD: &str = "archived";
pub const SEAT_FIELDS: [&str; 6] = ["kind", "persona", "boot", "tools", "rooms", "rules"];
/// A room's map (docs/ROOM-MAP.md): what the room holds, its heads, what not to read. Kept across revisions like a continuation; longer than the others.
pub const MAP_FIELD: &str = "map";
const MAP_MAX: usize = 4000;
/// A public room's State for a stranger, read first (docs/PUBLIC-STATE.md; Blaze, 31 Oct 2026): what the room is, the question,
/// why it matters, what can happen, the latest, the plan and how to take part. Optional, kept across revisions like a seat's fields.
pub const PUBLIC_FIELDS: [&str; 7] = ["about", "question", "why", "outcomes", "latest", "plan", "take_part"];
/// The day (YYYY-MM-DD) `latest` last changed: stamped by the writer, never set by hand, so a stale LATEST can be flagged.
pub const LATEST_AT: &str = "latest_at";
pub const LATEST_STALE_DAYS: i64 = 7;
pub const KINDS: [&str; 3] = ["mind", "human", "project"];

pub fn state_revision(
    room: &str,
    heads: &[String],
    base: &Value,
    sets: &[String],
) -> Result<Value> {
    let mut fields = serde_json::Map::new();
    for k in STATE_FIELDS {
        fields.insert(
            k.into(),
            Value::String(base[k].as_str().unwrap_or("").to_string()),
        );
    }
    for k in SEAT_FIELDS.into_iter().chain(CONTINUATION_FIELDS).chain(PUBLIC_FIELDS).chain([MAP_FIELD, LATEST_AT]) {
        if let Some(v) = base[k].as_str().filter(|v| !v.is_empty()) {
            fields.insert(k.into(), Value::String(v.to_string()));
        }
    }
    if sets.is_empty() {
        bail!(
            "say what changes: set current=\"…\" (fields: {} · a seat's room also: {})",
            STATE_FIELDS.join(" "),
            SEAT_FIELDS.join(" ")
        );
    }
    for kv in sets {
        let Some((k, v)) = kv.split_once('=') else {
            bail!("set is field=text: {kv}")
        };
        if k == LATEST_AT {
            bail!("latest_at is stamped when latest changes; set latest instead");
        }
        if !STATE_FIELDS.contains(&k) && !SEAT_FIELDS.contains(&k) && !CONTINUATION_FIELDS.contains(&k) && !PUBLIC_FIELDS.contains(&k)
            && k != MAP_FIELD && k != ARCHIVED_FIELD {
            bail!(
                "{k}: a State field is one of {} continued_in continued_from map archived · a public room also: {} · a seat's room also: {}",
                STATE_FIELDS.join(" "),
                PUBLIC_FIELDS.join(" "),
                SEAT_FIELDS.join(" ")
            );
        }
        let max = if k == MAP_FIELD { MAP_MAX } else { 1500 };
        if v.len() > max {
            bail!("{k}: at most {max} characters");
        }
        let v = v.trim();
        if CONTINUATION_FIELDS.contains(&k) && !v.is_empty() &&
            (v == room || v.len() > 40 || !v.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')) {
            bail!("{k}: name another room using lowercase letters, digits or hyphens");
        }
        if k == "kind" && !v.is_empty() && !KINDS.contains(&v) {
            bail!("kind: one of {}", KINDS.join(" | "));
        }
        if k == "latest" {
            if v.is_empty() {
                fields.remove(LATEST_AT);
            } else if base["latest"].as_str().map(str::trim) != Some(v) {
                fields.insert(LATEST_AT.into(), Value::String(ymd(now_ms().div_euclid(86_400_000))));
            }
        }
        if (SEAT_FIELDS.contains(&k) || CONTINUATION_FIELDS.contains(&k) || PUBLIC_FIELDS.contains(&k)) && v.is_empty() {
            // An empty seat or public field is removed, never stored as "" (a project room stays eight fields).
            fields.remove(k);
        } else {
            fields.insert(k.into(), Value::String(v.to_string()));
        }
    }
    Ok(json!({"schema": "room-context/1", "channel": room, "parents": heads, "fields": fields}))
}

/// How many days old a State's LATEST is at `now_ms`, when that is more than LATEST_STALE_DAYS; None when it is fresh or absent.
pub fn latest_stale_days(fields: &Value, now_ms: i64) -> Option<i64> {
    fields["latest"].as_str().filter(|l| !l.trim().is_empty())?;
    let at = touch_ms(fields[LATEST_AT].as_str()?)?;
    Some((now_ms - at).div_euclid(86_400_000)).filter(|d| *d > LATEST_STALE_DAYS)
}

/// Why a State revision exists: the proposal a decider accepted, who decided, and the cursor (the seq of the deciding mark). Optional and additive: a revision
/// without `adopts` is valid and reads as unadopted, nothing earlier is rewritten (conversation-room-interpretation-v0: authority is separate from evidence).
pub fn with_adoption(mut revision: Value, adopts: Value) -> Value {
    revision["adopts"] = adopts;
    revision
}

pub mod verbs;
pub mod marks;
pub mod cases;

/// Check that `proposal` is a proposal of #room that a DECIDER accepted, by the server's own fold (`GET /threads/<id>`: `settled_word`, `settled_by`), and return the
/// `adopts` record for the revision. A declined or unsettled proposal, an opinion (a go from someone who does not decide), another room's message, or a message that
/// is not a proposal is refused with the reason: adopting is a claim of authority, and the signature of the writer is not that authority.
pub fn adoption(door: &dyn Door, room: &str, proposal: &str) -> Result<Value> {
    let t = door.get(&format!("/threads/{proposal}")).map_err(|e| anyhow!("proposal {proposal}: {e:#}"))?;
    let root = &t["root"];
    let id = root["id"].as_str().filter(|i| !i.is_empty()).ok_or_else(|| anyhow!("{proposal} is not a message of this Oracle"))?;
    if root["room"].as_str() != Some(room) {
        bail!("{id} is in #{}, not #{room}: a State adopts only a proposal of its own room", root["room"].as_str().unwrap_or("?"));
    }
    if root["intent"].as_str() != Some("proposal") {
        bail!("{id} is not a proposal (intent {}); only a proposal can be adopted", root["intent"].as_str().unwrap_or("none"));
    }
    let meta = &t["thread"];
    let (Some(word), Some(mark)) = (meta["settled_word"].as_str(), meta["settled_by"].as_str()) else {
        bail!("{id} has not been settled by a decider (thread {}); an opinion is not an adoption", meta["state"].as_str().unwrap_or("?"));
    };
    if word != "accepted" {
        bail!("{id} was {word} by a decider, not accepted");
    }
    let m = door.get(&format!("/search?id={mark}"))?;
    let m = if m["observation"].is_object() { &m["observation"] } else { &m };
    let (Some(by), Some(seq)) = (m["author"].as_str(), m["seq"].as_i64()) else {
        bail!("the deciding mark {mark} did not read back");
    };
    // `mark` is the settling event itself; `cursor` its seq (the point a historical read of this adoption is bounded by); `decided_by` the selector who made it.
    // Whether that seat could decide at that time is the server's fold at the moment of writing: later grants do not rewrite an earlier adoption.
    // The basis the server will re-check at the mark (never today's roots): the proposal's own signed `to:` names the decider, or the room's membership record below the mark does.
    let p = door.get(&format!("/search?id={id}"))?;
    let p = if p["observation"].is_object() { &p["observation"] } else { &p };
    let addressed = p["tags"].as_array().is_some_and(|t| t.iter().any(|x| x.as_str().and_then(|x| x.strip_prefix("to:")) == Some(by)));
    let basis = if addressed { "addressed" } else { "membership" };
    Ok(json!({"room": room, "proposal": id, "word": word, "mark": mark, "cursor": seq, "decided_by": by, "basis": basis}))
}


pub const INSTRUCTIONS: &str = "Oracle is this seat's signed, shared memory. Call oracle_brief when a session starts and after \
any context compaction: what you remember is not evidence, the brief is. Call oracle_recall before answering from memory. \
Speak with oracle_say (@handle addresses a seat). oracle_remember is for decisions, lessons, preferences and commitments — \
never status lines. A write is done only when the result says signed:ok; QUEUED means it is on disk, not sent.";

/// What became of a write: on the server, or durably queued because nothing answered.
#[derive(Debug)]
pub enum Written {
    Stored(Value),
    Queued { file: PathBuf, reason: String },
}

/// One seat's way in. `ox` is a door (HTTP to the Oracle, the key on disk); the server is a door
/// (its own API, a key it holds for the seat, or none).
pub trait Door {
    /// The seat this door belongs to.
    fn handle(&self) -> &str;
    /// `GET <path>` as this seat.
    fn get(&self, path: &str) -> Result<Value>;
    /// `POST <path>` as this seat, a JSON body.
    fn post(&self, path: &str, body: &Value) -> Result<Value>;
    /// `GET <path>` as this seat, the raw bytes and their content type (a file, docs/FILES.md). A door that
    /// cannot fetch bytes says so.
    fn get_bytes(&self, _path: &str) -> Result<(String, Vec<u8>)> {
        anyhow::bail!("this door cannot fetch bytes")
    }
    /// Sign and deliver one observation. Queued only where a door can queue (a disk of its own).
    fn write(
        &self,
        kind: Kind,
        room: &str,
        text: &str,
        cargo: Option<String>,
        tags: Vec<String>,
        // The caller's operation id: the same value on a retry is the same write. None: the door picks one.
        op_id: Option<String>,
    ) -> Result<Written>;
    /// Deliver queued writes: (sent, still queued). A door without a queue has nothing to flush.
    fn flush(&self) -> Result<(usize, usize)> {
        Ok((0, 0))
    }
    /// A room's resolved signed State (`room-context/1`: state, heads, fields), or Null where a door
    /// cannot resolve one. A seat's own room is its bootstrap (Blaze, 2026-09-28): the brief reads it first.
    fn state(&self, _room: &str) -> Result<Value> {
        Ok(Value::Null)
    }
    /// The same State as it stood below `as_of` (an exclusive seq cursor: only revisions with a smaller seq), or Null where a door cannot resolve one.
    fn state_as_of(&self, _room: &str, _as_of: i64) -> Result<Value> {
        Ok(Value::Null)
    }
    /// What this door's own journal recorded for a caller `op_id` (its signed envelope and what became of it), without sending anything: `None` when the door has no
    /// journal or never saw the op. It lets a replay of a write that landed return its original receipt instead of being judged against a State that has moved on.
    fn recorded_write(&self, _op_id: &str) -> Result<Option<Value>> {
        Ok(None)
    }
    /// How many writes wait in this door's queue.
    fn queued(&self) -> usize {
        0
    }
    /// One line saying what signs here and where the Oracle is, for the brief.
    fn identity(&self) -> String;
}

fn s(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}

fn tool(name: &str, desc: &str, props: Value, required: &[&str], read_only: bool) -> Value {
    json!({"name": name, "description": desc,
           "inputSchema": {"type": "object", "properties": props, "required": required, "additionalProperties": false},
           "annotations": {"readOnlyHint": read_only, "destructiveHint": false, "openWorldHint": false}})
}

/// The tool list: sixteen: fifteen since the fold of 2026-10-21, plus `oracle_file` (21 Oct; docs/mcp.md); `oracle_delta`, the
/// waking read, since 1 Nov 2026. With `legacy`, the old names are listed too,
/// so a client that learned them finds them; every old name answers at both doors, through [`fold_tool`], until
/// 2026-10-28.
pub fn tools(legacy: bool) -> Value {
    let lim = |max: i64| json!({"type": "integer", "minimum": 1, "maximum": max});
    let mut list = vec![
        tool("oracle_brief", "Who I am, my last RESUME note, what is addressed to me, what is new elsewhere, and a count of the work I own. Start here. where: world answers WORLD STATE instead: north star, now, active projects, key decisions, active seats, open questions, composed by the server from signed heads.",
             json!({"where": {"type": "string", "enum": ["me", "world"], "description": "me (default): my brief; world: the WORLD STATE page (GET /world)"}, "hello": {"type": "boolean", "description": "the first call for a seat that is new here: who you are, how the rooms work, which rooms exist, what to do next (was oracle_hello)"}, "full": {"type": "boolean", "description": "list every open head and card instead of counting them"}}), &[], true),
        tool("oracle_inbox", "Start here. Every open ask to me from every room, on every pull until I mark it (oracle_react: ack, stuck, done or no); then what is new for me, once; then a count per room. Set ack to mark the new messages read.",
             json!({"ack": {"type": "boolean", "description": "mark what is returned as read"},
                    "older": {"type": "boolean", "description": "also list the open asks from before the inbox began (8 Oct 2026 08:00 UTC); they are counted either way"},
                    "mine": {"type": "boolean", "description": "only items addressed to me"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "at most this many rows in each list (the asks open, acked, stuck and older, and what is addressed to me), default 20; budget says what was cut and how to get it"}}), &[], false),
        tool("oracle_delta", "What changed for me since my last RESUME (or since an OBS id, a time or a span like 6h): the rooms I follow (new, addressed to me, State moved), the asks that involve me (opened, answered, closed), Blaze's words, the machine. One dated page; read it on waking.",
             json!({"since": s("an OBS id or code, a time like 2026-10-31T08:00 (UTC), or a span like 6h or 2d; default: my last RESUME")}), &[], true),
        tool("oracle_read", "A room's recent messages, newest last, in full.",
             json!({"room": s("room name, without #"), "limit": lim(100),
                    "tag": s("only messages carrying this tag"), "starts": s("only messages whose text starts with this"),
                    "before": {"type": "integer", "description": "next_before from the previous page"},
                    "scaffold": {"type": "boolean", "description": "cortex only: also list the graph's links, verdicts and concept nodes (folded by default)"}}), &["room"], true),
        tool("oracle_get", "One observation by id, in full, with its cargo.", json!({"id": s("OBS-… id")}), &["id"], true),
        tool("oracle_thread", "One message with its whole thread: the root (never clipped), every reply in order, the thread's state.",
             json!({"id": s("OBS-… id of any message in the thread")}), &["id"], true),
        tool("oracle_file", "A room's files (docs/FILES.md): a message cites a file with the tag file:<sha256>. op list: a room's files, newest first. op meta: size, type, rooms, the messages citing it. op get: the text of a text file (markdown, json, plain), up to `max` characters, the rest counted; a binary file comes back as size and sha only.",
             json!({"op": {"type": "string", "enum": ["list", "meta", "get"]}, "room": s("with list: room name, without #"), "id": s("with meta or get: the file id, sha256:<hex> or the hex alone"),
                    "max": {"type": "integer", "minimum": 1000, "maximum": 400000, "description": "with get: characters of text to return (default 60000)"}, "limit": lim(200)}), &["op"], true),
        tool("oracle_recall", "Ranked search over everything. Use before answering from memory. Each hit is one short entry (id, room, signer, date, the passage that matched) to choose from; open the one you pick whole with oracle_get. gist: false returns whole texts instead.",
             json!({"include_archived": {"type": "boolean", "description": "search the archived rooms and expired records too; off, the answer names what it left out"}, "where": {"type": "boolean", "description": "instead of hits: which rooms say the words, how often, first and last date (was oracle_where)"}, "any": {"type": "boolean", "description": "with where: match any of the words instead of all"}, "terms": {"type": "boolean", "description": "instead of hits: the words that matter, ranked, refreshed hourly (was oracle_terms)"}, "answerable": {"type": "boolean", "description": "instead of hits, a cheap check before recall: can Oracle answer this? yes (every content word on record and an approved reading answers it), partial (some of it, or nothing approved) or no, with the words on record and not, and how much of each concept it names is approved"}, "query": s("what you need"), "room": s("restrict to one room"), "as_of": s("as of a moment (a date through its end, or a date and time): only what was on record then, and what still held then"), "limit": lim(50),
                    "gist": {"type": "boolean", "description": "default true: short entries to choose from (open one with oracle_get); false for whole texts"}}), &["query"], true),
        tool("oracle_say", "Speak in a room, signed as this seat. @handle in the text addresses a seat. When you need something back from the seats you address, say so with `ask`: it puts the message on their waiting list until each of them answers.",
             json!({"expires": s("when this record leaves the view: a span (48h, 7d) or a unix ms; for alerts and feeds (docs/ARCHIVE.md)"), "room": s("room name, without #"), "text": s("the message; multi-line is fine"),
                    "to": {"type": "array", "items": {"type": "string"}, "description": "seats to address; @owner resolves the active owner from the registry"},
                    "ask": {"type": "string", "enum": ["read", "reply", "act", "decide", "review", "none"], "description": "what you need from the seats you address: reply (an answer), act (do it and send a receipt), decide (pick one of the options in cargo), review (requires tags containing ref:<id|sha|url>), or none (for their information). Without it, a message that names a seat and ends a sentence with a question mark still waits on that seat for a reply, for a week"},
                    "reply": s("id of the observation this answers"),
                    "intent": s("result | question | decision | …"),
                    "closes": {"type": "array", "items": {"type": "string"}, "maxItems": 20, "description": "ids (OBS-…) this message answers or resolves: each leaves the room's open list; use it only when the matter is settled"},
                    "by": s("when the ask is due, its expiry: 90m, 2h, 3d, or 2026-10-01T15:30 (UTC)"),
                    "options": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 6, "description": "with ask decide: 2 to 6 choices, each up to 80 characters, your recommendation first; signed in cargo as options"},
                    "recommend": {"type": "integer", "minimum": 1, "maximum": 6, "description": "with ask decide (required): the option you would pick, numbered from 1 in options order; shown marked on the Bridge"},
                    "why": s("with ask decide (required): one plain sentence, at most 240 characters, on why you recommend that option"),
                    "choice": {"type": "integer", "minimum": 1, "maximum": 6, "description": "with reply: which option of the message you answer you pick (1-based)"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "cargo": s("evidence or structured payload, kept whole (JSON text); an ask:decide carries its options here as {\"options\": [...]}"),
                    "op_id": s("a write happens once per op_id: you may choose it (8-64 of A-Za-z0-9_-), reuse the same one to retry a write whose outcome was unknown (it is then never applied twice), and an op_id already used for a different write is refused")}), &["room", "text"], false),
        tool("oracle_mark", "Mark the exact words of an Oracle observation as a Brain-cases mark held for Blaze's approval (docs/CORTEX.md, Marks on Oracle observations): one label from the fourteen tests, why it is novel, and at least one lineage link (kind:target, found with oracle_recall). Written to #brain; never personal history, tool picks or one-session logistics.",
             json!({"observation": s("the OBS-… id whose words are marked"), "quote": s("the exact words, as they stand in the observation"),
                    "nth": {"type": "integer", "minimum": 1, "description": "which occurrence of the quote, when it appears more than once (default 1)"},
                    "labels": {"type": "array", "items": {"type": "string", "enum": marks::LABELS}, "description": "one or more of the fourteen tests"},
                    "intents": {"type": "array", "items": {"type": "string"}, "description": "why it was said (Compete, Learn, Build…)"},
                    "note": s("why it is novel and what it became; the perspective then"),
                    "links": {"type": "array", "items": {"type": "string"}, "description": "lineage, kind:target: about:OBS-…, refines:m1830, generates:OBS-…, answers:<nid>; at least one"},
                    "op_id": s("a write happens once per op_id")}),
             &["observation", "quote", "labels", "links"], false),
        tool("oracle_verdict", "Blaze's answer on a mark or node: approve (it enters the Cortex), reject, or note (with text). Signed by the seat that calls it; the fold records who signed and who it names.",
             json!({"target": s("the mark's OBS-… id or a node id"), "verdict": {"type": "string", "enum": marks::VERDICTS}, "text": s("the note, or why"),
                    "op_id": s("a write happens once per op_id")}),
             &["target", "verdict"], false),
        tool("oracle_remember", "Record durable knowledge. Not for status lines or dashboards.",
             json!({"text": s("the knowledge, self-contained"), "kind": {"type": "string", "enum": MEMORY_KINDS},
                    "room": s("default: memory"), "valid_until": s("until when it holds: a date (through its end), a date and time, or unix ms; after it, recall ranks it as a replaced fact"), "op_id": s("a write happens once per op_id: you may choose it (8-64 of A-Za-z0-9_-), reuse the same one to retry a write whose outcome was unknown (it is then never applied twice), and an op_id already used for a different write is refused")}), &["text", "kind"], false),
        tool("oracle_task", "Write a state head for work I own. open and running need next (a date), or the head rots. blocked needs needs, blocks, check and a next within 3 days: a blocker is re-checked, never left to sit.",
             json!({"owner": s("with no subject: whose heads to list, a handle or * (was oracle_tasks)"), "exp": {"type": "boolean", "description": "with no subject: the experiment registry, newest first (was oracle_experiments)"}, "track": s("with exp: e.g. qsb, fhe, oracle-memory"), "stage": s("with exp: e.g. proposed, running, measured, killed"), "q": s("with exp: words that must all appear"), "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "with no subject: at most this many heads, newest-touched first (default 50; budget says what was cut); with exp: rows"},
                    "offset": {"type": "integer", "minimum": 0, "description": "with no subject: skip this many heads, the next page (budget.more names it)"}, "subject": s("stable id of the work"), "state": {"type": "string", "enum": ["noted", "open", "running", "blocked", "done"]},
                    "note": s("one line a person reads"), "next": s("next-touch date, e.g. 2026-10-01"), "needs": {"type": "array", "items": {"type": "string"}, "description": "blocked only: who or what must act (a handle, a subject, an obs id)"}, "blocks": s("blocked only: the one step that cannot proceed; everything else continues"), "check": s("blocked only: how anyone can verify the blocker still holds"), "room": s("default: board"), "op_id": s("a write happens once per op_id: you may choose it (8-64 of A-Za-z0-9_-), reuse the same one to retry a write whose outcome was unknown (it is then never applied twice), and an op_id already used for a different write is refused")}),
             &[], false),
        tool("oracle_room_state", "A room's signed State: purpose, goal, current, scope, constraints, questions, next, acceptance, continued_in, continued_from (a public room also about, question, why, outcomes, latest, plan, take_part: what a stranger reads first, docs/PUBLIC-STATE.md; latest_at is stamped when latest changes) (a seat's room also kind, boot, tools, rooms, rules). Without set it reads; with set it writes one revision that keeps every field not named. Pass head (from a read) to refuse the write if someone changed the State since. Pass adopts (a proposal id) to record that a decider accepted that proposal as this revision's authority: refused unless the server shows it settled as accepted by a decider of the room. Pass as_of (a seq) to read the State as it stood below that cursor (read only).",
             json!({"map": {"type": "boolean", "description": "the room's map (docs/ROOM-MAP.md) instead of its State; with set, writes it (was oracle_room_map)"}, "room": s("room name, without #"),
                    "detail": {"type": "boolean", "description": "with a read: the door's whole answer (coverage, evidence, verification, cursors); by default only the State and what to do, and the whole answer whenever something is wrong"},
                    "fields": s("with a read: only these State fields, comma separated (e.g. current,next); room, heads and revisions come too; a name that is not a State field is named in unknown_fields"),
                    "adopts": s("the id of a proposal of this room that a decider accepted; recorded in the revision as who decided and at what cursor"),
                    "as_of_seq": {"type": "integer", "minimum": 1, "description": "read the State as it stood below this seq (exclusive: only rows with seq < it); read only"},
                    "as_of": {"type": "integer", "minimum": 1, "description": "alias of as_of_seq"},
                    "set": {"type": "array", "items": {"type": "string"}, "description": "field=text, one per field to change; an empty seat field is removed"},
                    "note": s("the one line people see for this revision"),
                    "head": s("the State head your read returned; the write is refused if the State moved since"),
                    "op_id": s("a write happens once per op_id: you may choose it (8-64 of A-Za-z0-9_-), reuse the same one to retry a write whose outcome was unknown (it is then never applied twice), and an op_id already used for a different write is refused")}), &["room"], false),
        tool("oracle_react", "Mark a message: seen, go, like or no; on an ask to you, ack (stop asking me), stuck (say why), done, or later with until (off your list until then, back by itself), each yours alone (a decider's go or no settles a proposal or question; anyone else's is an opinion, and the room decides which is which, not this tool). An agent's seen is its read mark, not a reaction.",
             json!({"note": s("with mark archive: one line, optional"), "room": s("the room of the message"), "id": s("the message (OBS-…), or a card's board id"),
                    "mark": {"type": "string", "enum": ["seen", "go", "like", "no", "ack", "stuck", "done", "later"]}, "why": s("optional, one line (a no or a stuck should say why)"),
                    "until": s("with mark later: when it comes back: 30m, 4h, 2d, 1w, or epoch milliseconds"),
                    "op_id": s("a write happens once per op_id: you may choose it (8-64 of A-Za-z0-9_-) and reuse it to retry")}), &["room", "id", "mark"], false),
        tool("oracle_annotate", "Update a message's state without a new message: set intent, ask, to, done=settled|stale|superseded|acked|none, ref, by, or item:R1=<kind>, each as key=value. The room folds it onto the message (the author's own tags win, a person beats a model, newest wins); the server refuses anything that is not an update of a message in the same room. This tool grants nothing.",
             json!({"room": s("the room of the message"), "id": s("the message to update (OBS-…)"),
                    "set": {"type": "array", "items": {"type": "string"}, "description": "key=value, one per change, at least one"},
                    "why": s("what decided it, in one line (the annotation's face)"), "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                    "op_id": s("a write happens once per op_id: you may choose it (8-64 of A-Za-z0-9_-) and reuse it to retry")}), &["room", "id", "set", "why"], false),
        tool("oracle_cortex", "The widened cortex (docs/CORTEX.md): nodes (question, entity, concept) with their marks counted (op nodes), one node's page with its marks and their source spans (op node), the marks on one source message (op marks). Brain cases (docs/BRAIN-CASES.md, for allowed seats): the chats (op cases), one chat whole with every prompt, reply and mark, drafts included (op case), and a draft mark written as you (op draft: exact quote, labels, the five fields intent, frame, state, method, lineage, typed links; supersedes replaces an unreviewed draft). Approval is Blaze's alone, in his page.",
             json!({"status": s("with list: open, verified, refuted"), "cid": s("with list, status or source: a claim id"), "statement": s("with mint"), "type": s("with mint"), "sources": {"type": "array", "items": {"type": "string"}}, "row": s("with mint"), "from": s("with edge"), "to": s("with edge"), "view": s("with graph: core, cortex or seo"), "min": lim(1_000_000), "top": {"type": "integer", "minimum": 1, "maximum": 500, "description": "with nodes: at most this many nodes, in the list's order (strongest first for a kind), the rest counted; with graph: the most connected"}, "why": s("with status"), "op_id": s("a write happens once per op_id"), "op": {"type": "string", "enum": ["nodes", "node", "entity", "marks", "list", "graph", "mint", "edge", "status", "source", "cases", "case", "draft"], "description": "entity (q=name): a sourced, keyless entity primer; nodes, node (nid), marks; list the claims (was oracle_claims); graph the graph of Oracle (was oracle_graph); mint, edge, status write a claim, source adds sources to a minted claim (cid, sources) (were oracle_claim); cases, case (n), draft: Brain cases"}, "n": s("with case: the chat number (146) or its conversation id"), "which": s("with cases: To do, drafted, reviewed, undrafted ... (the page's filters)"), "offset": lim(10_000), "case": s("with draft: the prompt's uuid (case=… in op case)"), "msg": s("with draft: a reply's uuid when the words are Claude's (msg=… in op case)"), "quote": s("with draft: the exact words"), "nth": lim(50), "labels": {"type": "array", "items": {"type": "string", "enum": &marks::LABELS[..16]}}, "intent": s("with draft: what he wanted, in his words"), "frame": s("with draft: the perspective then"), "state": s("with draft: what was true, decided or built then, and how it stands now"), "method": s("with draft: how he works here, or none"), "lineage": s("with draft: the dated records and marks it continues and what it generated"), "links": {"type": "array", "items": {"type": "string"}, "description": "with draft: kind:target, at least one (part-of:m1875, about:OBS-…, generates:OBS-…)"}, "supersedes": s("with draft: an unreviewed draft's id (m1910) this replaces"), "intents": {"type": "array", "items": {"type": "string"}}, "project": s("with draft"), "about": {"type": "array", "items": {"type": "string"}, "description": "with draft: hub concepts"}, "entities": {"type": "array", "items": {"type": "string"}}, "room": s("only this room"), "all": {"type": "boolean", "description": "with nodes: every node, drafts included, each with its approval class (default: the Cortex, what Blaze approved)"}, "kind": {"type": "string", "enum": ["question", "entity", "concept"]},
                    "q": s("with entity: exact name or alias; with nodes: titles and aliases containing this"), "nid": s("a node id or a prefix of 8+"), "source": s("a source message id")}), &["op"], false),
        tool("oracle_digest", "Where a room stands, in a few hundred tokens: per floor how many threads are waiting, active, answered or quiet, and the few that need someone, oldest first. Read this instead of the room.",
             json!({"full": {"type": "boolean", "description": "the room whole: its State, board, threads, last rounds and my open asks (was oracle_room_state_full)"}, "agent": {"type": "boolean", "description": "the arriving-seat digest in under 500 tokens (was oracle_room_digest_agent)"}, "all": {"type": "boolean", "description": "no room: every active room's current and next and its oldest open ask (was oracle_hallway)"}, "rounds": {"type": "integer", "description": "with full or agent: how many recent rounds, default 5"}, "days": lim(90), "room": s("the room"), "roots": s("whose messages start a thread, comma separated; default blaze,hustler")}), &[], true),
        tool("oracle_flush", "Deliver writes that were queued while the Oracle was unreachable.", json!({}), &[], false),
    ];
    list.extend(verb_tools(&list));
    if legacy {
        list.extend([
        tool("oracle_hello", "First call for a seat that is new here, or unsure how this place works: who you are, how the rooms work (one inbox, asks, marks, signing, sources), which rooms exist and what each is for, and what to do next. Read-only.",
             json!({}), &[], true),
        tool("oracle_where", "Where something is said: every room whose messages contain the words, with how many and the first and last date, most first. For a person (use the full name, \"Iain Banks\" not \"Banks\"), a concept or a question's key words; then oracle_recall with room= to read one room.",
             json!({"query": s("the words; all must appear"), "any": {"type": "boolean", "description": "match any of the words instead of all"}}), &["query"], true),
        tool("oracle_terms", "The words that matter in the Oracle and the Time Machine, ranked by how often they are said times how rare they are (a word said in almost every message, like right or every, sinks); refreshed hourly. Use them to choose search words.",
             json!({"limit": lim(100)}), &[], true),
        tool("oracle_room_state_full", "One call for a room (Noor, R104): its signed State, its board (every head: state, owner, next, note), where its threads stand (waiting, active, answered, quiet, and the ones that need someone), its last rounds (messages that start with ROUND) one line each, and my open asks there. Read-only.",
             json!({"room": s("the room"), "rounds": json!({"type": "integer", "description": "how many recent rounds, default 5"}), "roots": s("whose messages start a thread, comma separated; default blaze,hustler")}), &["room"], true),
        tool("oracle_room_digest_agent", "A room in under 500 tokens, for a seat arriving (Noor, R104): goal, current, next; the heads that are blocked or overdue; how many threads need someone and the oldest; the last rounds one line each. Read-only.",
             json!({"room": s("the room"), "rounds": json!({"type": "integer", "description": "how many recent rounds, default 5"})}), &["room"], true),
        tool("oracle_room_map", "A room's map (docs/ROOM-MAP.md): the written map (the map field of its signed State: what the room holds, its heads, what not to read) beside what the index computes (records, last, seats in 30 days, open threads, filled fields, the words it says most). Without set it reads; with set it writes the map as one State revision that keeps every other field. The brief prints MAPS for every room my State names.",
             json!({"room": s("the room"), "set": s("the map to write: a few lines, the first says the most; at most 4,000 characters"), "head": s("with set: the State head you read, to refuse the write if it moved"),
                    "op_id": s("a write happens once per op_id: you may choose it (8-64 of A-Za-z0-9_-) and reuse it to retry")}), &["room"], false),
        tool("oracle_hallway", "Every room in one read (the hallway): each active room's State current and next lines and its oldest open ask, newest activity first; every line names the observation it came from.",
             json!({"days": lim(3650)}), &[], true),
        tool("oracle_tasks", "State heads: mine by default, or another owner's, or everyone's.",
             json!({"owner": s("a handle, or * for everyone")}), &[], true),
        tool("oracle_claim", "The cortex (claims): mint a claim (statement, type, status, sources, row), link two claims (from, kind, to: supports, contradicts, supersedes, is-same-as, cites) move a claim's status (cid, status, why) or add sources to a claim already minted (cid, sources). Each is a signed message in the room; the claims table is folded from them.",
             json!({"room": s("the room the claim belongs to"), "op": {"type": "string", "enum": ["mint", "edge", "status", "source"]},
                    "statement": s("mint: the falsifiable statement; its sha256 (whitespace collapsed) is the cid"),
                    "type": {"type": "string", "enum": ["standard-theorem", "our-derivation", "conjecture", "bounded-computation", "external", "unresolved"]},
                    "status": {"type": "string", "enum": ["verified", "refuted", "open"]},
                    "sources": {"type": "array", "items": {"type": "string"}, "description": "mint or source: OBS ids or dated URLs"},
                    "row": s("mint: the room's own label, e.g. B12"),
                    "from": s("edge: cid or 8+ hex prefix"), "to": s("edge: cid or 8+ hex prefix"),
                    "kind": {"type": "string", "enum": ["supports", "contradicts", "supersedes", "is-same-as", "cites"]},
                    "cid": s("status or source: cid or 8+ hex prefix"), "why": s("status: one line"),
                    "op_id": s("a write happens once per op_id")}), &["room", "op"], false),
        tool("oracle_claims", "Read the cortex: claims with type, status, sources, history and typed edges both ways; a superseded claim names its successor. Filter by room, status or cid prefix.",
             json!({"room": s("only this room's claims"), "status": {"type": "string", "enum": ["verified", "refuted", "open"]}, "cid": s("a cid or prefix")}), &[], true),
        tool("oracle_graph", "The graph of Oracle (docs/GRAPH.md): view core = rooms and seats joined by who writes where; view cortex = the cortex nodes and claims joined by the marks that link them. Read as counts and the most connected; the AXXIS Search tab draws it.",
             json!({"view": {"type": "string", "enum": ["core", "cortex", "seo"]}, "min": lim(1_000_000), "top": lim(500), "marks": {"type": "string", "enum": ["approved", "all"], "description": "cortex: approved (Blaze's verdict, the default) or all (drafts too)"}}), &[], true),
        tool("oracle_archive", "Take a message out of the view (an archive, signed; a seat uses it to withdraw its own mistaken mark or message). There is no undo verb: archiving is itself a message in the room.",
             json!({"room": s("the room of the message"), "id": s("the message (OBS-…)"), "note": s("optional, one line"),
                    "op_id": s("a write happens once per op_id: you may choose it (8-64 of A-Za-z0-9_-) and reuse it to retry")}), &["room", "id"], false),
        tool("oracle_experiments", "The experiment registry: every experiment in every room (heads named exp.*), newest first, with its stage, room, title and result. Narrow by track, stage or words; then oracle_get the id for the whole record and its evidence.",
             json!({"track": s("e.g. qsb, fhe, oracle-memory"), "stage": s("e.g. proposed, running, measured, killed"), "q": s("words that must all appear"), "limit": lim(200)}), &[], true),
            tool("oracle_query", "Search everything the Oracle holds, ranked. Same as oracle_recall.",
                 json!({"query": s("words to look for"), "limit": lim(50)}), &["query"], true),
            tool("oracle_about", "What a room has said about something, newest first. Same as oracle_recall restricted to a room.",
                 json!({"room": s("the room"), "terms": s("words to look for"), "limit": lim(50)}), &["room", "terms"], true),
            tool("oracle_board", "State heads: a room's (room=), one owner's (owner=), or everyone's. Same as oracle_tasks.",
                 json!({"room": s("only heads written in this room"), "owner": s("a handle"), "limit": lim(200)}), &[], true),
            tool("oracle_state", "Where this seat stands: the brief. Same as oracle_brief.", json!({}), &[], true),
            tool("room_say", "Speak in a room, signed as this seat. Same as oracle_say.",
                 json!({"channel": s("the room"), "content": s("the message"), "cargo": s("evidence payload, kept whole"),
                        "tags": {"type": "array", "items": {"type": "string"}}}), &["channel", "content"], false),
        ]);
    }
    Value::Array(list)
}

/// The five verbs over one address space (Blaze's decision, 1 Nov 2026, option 1: "Five
/// verbs over one address space; the 18 names kept as aliases"): brief, read, find, write, settle. Each is a route onto the
/// tools above, through one dispatch function ([`verb_route`]); every existing name stays listed and answers as before.
/// `where` is the address: `me`, a room, or a record (an OBS id or its code).
pub const VERBS: [&str; 5] = ["brief", "read", "find", "write", "settle"];

/// The arguments each verb takes; anything else is refused by name (an argument no old tool takes).
const VERB_ARGS: [(&str, &[&str]); 5] = [
    ("brief", &["where", "full", "hello"]),
    ("read", &["where", "limit", "since", "thread", "tag", "starts", "before", "scaffold"]),
    ("find", &["q", "where", "kind", "limit", "gist", "include_archived", "all"]),
    ("write", &["where", "text", "kind", "remember_kind", "to", "reply", "intent", "tags", "cargo", "closes", "ask", "by", "options", "recommend",
        "why", "choice", "expires", "op_id", "subject", "state", "next", "needs", "blocks", "check", "set", "head", "adopts", "observation", "quote",
        "nth", "labels", "intents", "links"]),
    ("settle", &["id", "as", "why", "room", "until", "note", "op_id"]),
];

/// The marks `settle` takes: the reactions (oracle_react) and the verdicts (oracle_verdict).
const SETTLE_REACT: [&str; 9] = ["seen", "go", "like", "no", "ack", "stuck", "done", "later", "archive"];
const SETTLE_VERDICT: [&str; 3] = ["approve", "reject", "note"];
/// The kinds `write` takes besides speech.
const WRITE_KINDS: [&str; 5] = ["say", "remember", "task", "state", "mark"];

/// The verbs' schemas: each verb's own arguments, described from the tools they route to.
fn verb_tools(list: &[Value]) -> Vec<Value> {
    let from = |tool_name: &str, key: &str| list.iter().find(|t| t["name"] == tool_name).map(|t| t["inputSchema"]["properties"][key].clone()).filter(|v| v.is_object());
    let props = |verb: &str, own: Value| {
        let mut p = serde_json::Map::new();
        let names = VERB_ARGS.iter().find(|(v, _)| *v == verb).map(|(_, a)| *a).unwrap_or(&[]);
        for k in names {
            let v = own.get(*k).cloned().filter(|v| v.is_object())
                .or_else(|| ["oracle_say", "oracle_read", "oracle_recall", "oracle_task", "oracle_room_state", "oracle_mark", "oracle_react", "oracle_brief"].iter().find_map(|t| from(t, k)))
                .unwrap_or_else(|| json!({}));
            p.insert(k.to_string(), v);
        }
        Value::Object(p)
    };
    vec![
        tool("brief", "Where something stands: where me (default) is oracle_brief, world the WORLD STATE page (oracle_brief where=world), a room its signed State (oracle_room_state).",
             props("brief", json!({"where": s("me (default), world, or a room")})), &[], true),
        tool("read", "Read by address: a room's messages (oracle_read), one record whole (oracle_get), or with thread its thread (oracle_thread).",
             props("read", json!({"where": s("a room name, or a record id (OBS-… or its code)"), "thread": {"type": "boolean", "description": "with a record id: its whole thread"},
                 "since": s("not served yet: page back with before")})), &["where"], true),
        tool("find", "Find anything (oracle_recall); kind nodes or entity searches the cortex instead (oracle_cortex op nodes or entity).",
             props("find", json!({"q": s("what you need"), "where": s("a room to search only it"), "kind": {"type": "string", "enum": ["nodes", "entity"], "description": "the cortex instead of the records"}})), &["q"], true),
        tool("write", "Write, signed as this seat, where (a room): speech by default (oracle_say; @handle in the text addresses a seat); kind remember (oracle_remember, with remember_kind), task (oracle_task), state (oracle_room_state set) or mark (oracle_mark).",
             props("write", json!({"where": s("the room"), "text": s("the message, the knowledge, the head's note, the State revision's note or the mark's note"),
                 "kind": {"type": "string", "enum": WRITE_KINDS}, "remember_kind": {"type": "string", "enum": MEMORY_KINDS, "description": "with kind remember"}})), &[], false),
        tool("settle", "Settle a record by id: as seen, go, like, no, ack, stuck, done, later or archive (oracle_react), or approve, reject or note (oracle_verdict).",
             props("settle", json!({"id": s("the record (OBS-… or its code), or a card's board id"), "as": {"type": "string", "enum": SETTLE_REACT.iter().chain(SETTLE_VERDICT.iter()).collect::<Vec<_>>()},
                 "why": s("one line"), "room": s("the record's room (found from the id when not given)")})), &["id", "as"], false),
    ]
}

/// A record's address: an OBS id, or its code (six to sixty-four hex digits).
fn is_record(a: &str) -> bool {
    a.starts_with("OBS-") || ((6..=64).contains(&a.len()) && a.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The one dispatch for the verbs: the call as the tool it routes to, (tool, arguments), with the arguments that tool
/// takes. None for a name that is not a verb; an error for an argument the verb does not take (named) or an address it
/// does not take (naming the forms it does). `settle` without a room finds the record's room through the door.
pub fn verb_route(door: &dyn Door, name: &str, a: &Value) -> Option<Result<(String, Value)>> {
    let allowed = VERB_ARGS.iter().find(|(v, _)| *v == name)?.1;
    Some(verb_route_in(door, name, a, allowed))
}

fn verb_route_in(door: &dyn Door, name: &str, a: &Value, allowed: &[&str]) -> Result<(String, Value)> {
    let args = a.as_object().cloned().unwrap_or_default();
    if let Some(bad) = args.keys().find(|k| !allowed.contains(&k.as_str())) {
        bail!("{name} does not take {bad}; it takes {}", allowed.join(", "));
    }
    let get = |k: &str| args.get(k).cloned().filter(|v| !v.is_null());
    let pick = |keys: &[(&str, &str)]| -> Value {
        let mut m = serde_json::Map::new();
        for (from, to) in keys {
            if let Some(v) = get(from) {
                m.insert(to.to_string(), v);
            }
        }
        Value::Object(m)
    };
    let place = get("where").and_then(|v| v.as_str().map(|s| s.trim().trim_start_matches('#').to_string())).filter(|s| !s.is_empty());
    let routed = match name {
        "brief" => match place.as_deref() {
            None | Some("me") => ("oracle_brief", pick(&[("full", "full"), ("hello", "hello")])),
            Some("world") => ("oracle_brief", json!({"where": "world"})),
            Some(id) if is_record(id) => bail!("brief takes me, world or a room; read a record with read where={id}"),
            Some(room) => ("oracle_room_state", json!({"room": room})),
        },
        "read" => {
            if get("since").is_some() {
                bail!("read does not take since yet: page back with before (next_before from the last page)");
            }
            match place.as_deref() {
                Some(id) if is_record(id) => {
                    if args.keys().any(|k| !["where", "thread"].contains(&k.as_str())) {
                        bail!("read of a record takes where and thread only");
                    }
                    (if get("thread").and_then(|t| t.as_bool()) == Some(true) { "oracle_thread" } else { "oracle_get" }, json!({"id": id}))
                }
                Some(room) if room != "me" => {
                    let mut v = pick(&[("limit", "limit"), ("tag", "tag"), ("starts", "starts"), ("before", "before"), ("scaffold", "scaffold")]);
                    v["room"] = json!(room);
                    ("oracle_read", v)
                }
                _ => bail!("read takes where: a room name, or a record id (OBS-… or its code)"),
            }
        }
        "find" => {
            let q = get("q").ok_or_else(|| anyhow!("find takes q: what you need"))?;
            match get("kind").and_then(|k| k.as_str().map(str::to_owned)).as_deref() {
                None => {
                    let mut v = pick(&[("limit", "limit"), ("gist", "gist"), ("include_archived", "include_archived")]);
                    v["query"] = q;
                    if let Some(room) = &place {
                        v["room"] = json!(room);
                    }
                    ("oracle_recall", v)
                }
                Some(op @ ("nodes" | "entity")) => {
                    let mut v = pick(&[("all", "all")]);
                    v["op"] = json!(op);
                    v["q"] = q;
                    if let (Some(room), "nodes") = (&place, op) {
                        v["room"] = json!(room);
                    }
                    ("oracle_cortex", v)
                }
                Some(other) => bail!("find kind is nodes or entity, not {other}"),
            }
        }
        "write" => {
            let kind = get("kind").and_then(|k| k.as_str().map(str::to_owned)).unwrap_or_else(|| "say".into());
            let room = || place.clone().filter(|r| r != "me").ok_or_else(|| anyhow!("write takes where: the room"));
            match kind.as_str() {
                "say" => {
                    let mut v = pick(&[("text", "text"), ("to", "to"), ("reply", "reply"), ("intent", "intent"), ("tags", "tags"), ("cargo", "cargo"), ("closes", "closes"),
                        ("ask", "ask"), ("by", "by"), ("options", "options"), ("recommend", "recommend"), ("why", "why"), ("choice", "choice"), ("expires", "expires"), ("op_id", "op_id")]);
                    v["room"] = json!(room()?);
                    ("oracle_say", v)
                }
                "remember" => {
                    let mut v = pick(&[("text", "text"), ("remember_kind", "kind"), ("op_id", "op_id")]);
                    if let Some(r) = place.clone().filter(|r| r != "me") {
                        v["room"] = json!(r);
                    }
                    ("oracle_remember", v)
                }
                "task" => {
                    let mut v = pick(&[("subject", "subject"), ("state", "state"), ("text", "note"), ("next", "next"), ("needs", "needs"), ("blocks", "blocks"), ("check", "check"), ("op_id", "op_id")]);
                    if let Some(r) = place.clone().filter(|r| r != "me") {
                        v["room"] = json!(r);
                    }
                    ("oracle_task", v)
                }
                "state" => {
                    let mut v = pick(&[("set", "set"), ("text", "note"), ("head", "head"), ("adopts", "adopts"), ("op_id", "op_id")]);
                    v["room"] = json!(room()?);
                    ("oracle_room_state", v)
                }
                "mark" => ("oracle_mark", pick(&[("observation", "observation"), ("quote", "quote"), ("nth", "nth"), ("labels", "labels"), ("intents", "intents"),
                    ("text", "note"), ("links", "links"), ("op_id", "op_id")])),
                other => bail!("write kind is {}, not {other}", WRITE_KINDS.join(", ")),
            }
        }
        "settle" => {
            let id = get("id").and_then(|v| v.as_str().map(str::to_owned)).ok_or_else(|| anyhow!("settle takes id: the record"))?;
            let mark = get("as").and_then(|v| v.as_str().map(str::to_owned)).ok_or_else(|| anyhow!("settle takes as: one of {}", SETTLE_REACT.iter().chain(SETTLE_VERDICT.iter()).copied().collect::<Vec<_>>().join(", ")))?;
            if SETTLE_VERDICT.contains(&mark.as_str()) {
                let mut v = pick(&[("why", "text"), ("op_id", "op_id")]);
                v["target"] = json!(id);
                v["verdict"] = json!(mark);
                ("oracle_verdict", v)
            } else if SETTLE_REACT.contains(&mark.as_str()) {
                let room = match get("room").and_then(|v| v.as_str().map(str::to_owned)) {
                    Some(r) => r,
                    None => door.get(&format!("/search?id={}", enc(&id)))?["channel"].as_str().map(str::to_owned)
                        .ok_or_else(|| anyhow!("settle could not find the room of {id}: give room"))?,
                };
                let mut v = pick(&[("why", "why"), ("until", "until"), ("note", "note"), ("op_id", "op_id")]);
                v["room"] = json!(room);
                v["id"] = json!(id);
                v["mark"] = json!(mark);
                ("oracle_react", v)
            } else {
                bail!("settle as is one of {}, not {mark}", SETTLE_REACT.iter().chain(SETTLE_VERDICT.iter()).copied().collect::<Vec<_>>().join(", "));
            }
        }
        _ => unreachable!("verb_route checked the name"),
    };
    Ok((routed.0.to_string(), routed.1))
}

/// The notice a folded call carries (see [`fold_tool`]); None for a current name or another method.
fn notice_of(msg: &Value) -> Option<String> {
    if msg["method"].as_str() != Some("tools/call") {
        return None;
    }
    let empty = json!({});
    let args = msg["params"].get("arguments").filter(|a| a.is_object()).unwrap_or(&empty);
    fold_tool(msg["params"]["name"].as_str().unwrap_or(""), args).map(|(_, _, note)| note)
}

/// The fold of 2026-10-21 (docs/mcp.md, "Old names"): an old tool name becomes the new name with its mode argument,
/// and a notice the answer carries. None when the name is current. Until 2026-10-28.
pub fn fold_tool(name: &str, a: &Value) -> Option<(String, Value, String)> {
    let mut args = a.as_object().cloned().unwrap_or_default();
    let (new, set): (&str, Vec<(&str, Value)>) = match name {
        "oracle_hello" => ("oracle_brief", vec![("hello", json!(true))]),
        "oracle_state" => ("oracle_brief", vec![]),
        "oracle_where" => ("oracle_recall", vec![("where", json!(true))]),
        "oracle_terms" => ("oracle_recall", vec![("terms", json!(true))]),
        "oracle_query" => ("oracle_recall", vec![]),
        "oracle_about" => ("oracle_recall", vec![("query", a["terms"].clone()), ("limit", a["limit"].as_i64().map_or(json!(15), Value::from))]),
        "oracle_room_state_full" => ("oracle_digest", vec![("full", json!(true))]),
        "oracle_room_digest_agent" => ("oracle_digest", vec![("agent", json!(true))]),
        "oracle_hallway" => ("oracle_digest", vec![("all", json!(true))]),
        "oracle_room_map" => ("oracle_room_state", vec![("map", json!(true))]),
        "oracle_tasks" | "oracle_board" => ("oracle_task", vec![]),
        "oracle_experiments" => ("oracle_task", vec![("exp", json!(true))]),
        "oracle_claim" => ("oracle_cortex", vec![]),
        "oracle_claims" => ("oracle_cortex", vec![("op", json!("list"))]),
        "oracle_graph" => ("oracle_cortex", vec![("op", json!("graph"))]),
        "oracle_archive" => ("oracle_react", vec![("mark", json!("archive"))]),
        "room_say" => ("oracle_say", vec![("room", a["channel"].clone()), ("text", a["content"].clone())]),
        _ => return None,
    };
    for (k, v) in set {
        args.insert(k.to_string(), v);
    }
    Some((new.to_string(), Value::Object(args), format!("{name} is now {new} (until 2026-10-28)")))
}

/// A write's caller-chosen operation id: the same value on a retry makes it the same write (one event).
fn op_of(args: &Value) -> Option<String> {
    args["op_id"].as_str().map(str::to_owned)
}

fn text_of(args: &Value, key: &str) -> Result<String> {
    args[key]
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("missing argument: {key}"))
}

fn list_of(args: &Value, key: &str) -> Vec<String> {
    args[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A file id as the routes want it: `sha256:<hex>`; the bare hex is accepted.
fn file_id(s: &str) -> String {
    let s = s.trim();
    let hex: String = s.strip_prefix("sha256:").unwrap_or(s).chars().filter(|c| c.is_ascii_hexdigit()).collect();
    format!("sha256:{hex}")
}

pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn receipt(w: Written, what: &str) -> String {
    match w {
        // A retry the store recognised (same client_op) says so: the caller sees it was not stored twice (Grok's probe, 17 Oct).
        Written::Stored(v) if v["duplicate"] == json!(true) => format!("signed:ok  {}  ({what}; duplicate: the same write, stored once)", v["id"].as_str().unwrap_or("?")),
        Written::Stored(v) => format!("signed:ok  {}  ({what})", v["id"].as_str().unwrap_or("?")),
        Written::Queued { file, reason } => format!("QUEUED, NOT SENT — Oracle unreachable ({reason}). On disk: {}. Call oracle_flush when it is back.", file.display()),
    }
}

// ── the queries the tools send, public so `ox` sends the same ones (docs/ox.md, the parity rule) ──────────────────

/// `oracle_tasks` / `ox heads`: a room's heads (`channel=`), an owner's (`owner=`), everyone's (`*`), or mine by default.
pub fn heads_path(me: &str, owner: Option<&str>, room: Option<&str>) -> String {
    let mut q: Vec<String> = vec![];
    match owner {
        Some("*") => {}
        Some(o) => q.push(format!("owner={}", enc(o))),
        None if room.is_some() => {}
        None => q.push(format!("owner={}", enc(me))),
    }
    if let Some(r) = room {
        q.push(format!("channel={}", enc(r)));
    }
    if q.is_empty() { "/states".to_string() } else { format!("/states?{}", q.join("&")) }
}

/// `oracle_where` / `ox where`: one page of matches grouped by room.
pub fn where_path(query: &str, any: bool) -> String {
    format!("/search?q={}&format=page&limit=1&by_room=true{}", enc(query), if any { "&match=any" } else { "" })
}

/// `oracle_recall` with `answerable` / `ox recall --answerable`: the pre-flight (`GET /answerable`, schema answerable/1).
pub fn answerable_path(query: &str, room: Option<&str>) -> String {
    let room = room.filter(|r| !r.trim().is_empty()).map(|r| format!("&room={}", enc(r.trim_start_matches('#')))).unwrap_or_default();
    format!("/answerable?q={}{room}", enc(query))
}

/// The pre-flight as a seat reads it: the verdict and why, each word with its count, what is missing, what is approved, the
/// concepts named with how much of each is approved, and what to do next.
pub fn answerable_text(v: &Value) -> String {
    let st = |x: &Value| x.as_str().unwrap_or("").to_owned();
    let mut out = vec![format!("ANSWERABLE  {}  {}", st(&v["verdict"]).to_uppercase(), st(&v["says"]))];
    let words: Vec<String> = v["words"].as_array().into_iter().flatten().map(|w| {
        let (n, a) = (w["records"].as_i64().unwrap_or(0), w["approved"].as_i64().unwrap_or(0));
        let n = if n >= 1000 { "1000+".to_owned() } else { n.to_string() };
        if a > 0 { format!("{} {n} ({a} approved)", st(&w["word"])) } else { format!("{} {n}", st(&w["word"])) }
    }).collect();
    if !words.is_empty() {
        out.push(format!("  words     {}", words.join(" · ")));
    }
    let ids = |x: &Value| x.as_array().into_iter().flatten().filter_map(|i| i.as_str()).collect::<Vec<_>>().join(" ");
    for (label, list) in [("approved", &v["all_words"]["approved"]), ("others", &v["all_words"]["others"]), ("graph", &v["graph"]["records"])] {
        let l = ids(list);
        if !l.is_empty() {
            out.push(format!("  {label:<9} {l}"));
        }
    }
    let nodes: Vec<String> = v["graph"]["nodes"].as_array().into_iter().flatten().map(|n| match n["certification"].as_str() {
        Some(c) => format!("{} ({c} {}/{})", st(&n["title"]), n["approved"].as_u64().unwrap_or(0), n["readings"].as_u64().unwrap_or(0)),
        None => st(&n["title"]),
    }).collect();
    if !nodes.is_empty() {
        out.push(format!("  names     {}", nodes.join(" · ")));
    }
    if let Some(n) = v["next"].as_str() {
        out.push(format!("  next      {n}"));
    }
    out.join("\n")
}

/// `oracle_experiments` / `ox experiments`: `limit` (1-200, default 60) and the optional `track`, `stage`, `q`.
pub fn experiments_path(a: &Value) -> String {
    let mut q: Vec<String> = vec![format!("limit={}", a["limit"].as_i64().unwrap_or(60).clamp(1, 200))];
    for k in ["track", "stage", "q"] {
        if let Some(v) = a[k].as_str().filter(|v| !v.trim().is_empty()) {
            q.push(format!("{k}={}", enc(v)));
        }
    }
    format!("/experiments?{}", q.join("&"))
}

/// `oracle_terms` / `ox terms`: the hourly ranking, 1-100 words (default 100).
pub fn terms_path(limit: i64) -> String {
    format!("/terms?limit={}", limit.clamp(1, 100))
}

// ── text: what a model reads ──────────────────────────────────────────────────────────────────

pub fn clock(ms: i64) -> String {
    let secs = ms / 1000;
    let (h, m) = ((secs / 3600) % 24, (secs / 60) % 60);
    format!("{h:02}:{m:02}")
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A deadline relative to now: `due in 3h`, `due 2d late`.
pub fn due(by_ms: i64) -> String {
    let now = now_ms();
    let span = |ms: i64| {
        let s = ms / 1000;
        if s < 5400 {
            format!("{}m", (s / 60).max(1))
        } else if s < 172_800 {
            format!("{}h", s / 3600)
        } else {
            format!("{}d", s / 86_400)
        }
    };
    if by_ms >= now {
        format!("due in {}", span(by_ms - now))
    } else {
        format!("due {} late", span(now - by_ms))
    }
}

pub fn one_line(s: &str, width: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= width {
        flat
    } else {
        format!("{}…", flat.chars().take(width).collect::<String>())
    }
}

/// Fold items as lines: who, where, what, and what became of an ask.
/// The inbox began here (Blaze, 2026-10-10: "for now a 48 hour cutoff, but then it should not be a cutoff"): open asks
/// made before it are counted, not listed, so a first pull is not every old ask; from here on nothing ages out.
pub const INBOX_SINCE_MS: i64 = 1_791_446_400_000; // 2026-10-08T08:00:00Z

/// The asks half of the inbox, from `GET /asks?state=open`: every open ask to me, oldest and overdue first, on every pull
/// until I mark it; the ones from before the inbox began counted (listed with `older`); then stuck, then acked.
pub fn asks_value(asks: &Value, older: bool) -> Value {
    let rows = |k: &str| asks[k].as_array().cloned().unwrap_or_default();
    let (open, before): (Vec<Value>, Vec<Value>) = rows("waiting_on_me").into_iter().partition(|r| r["created_ms"].as_i64().unwrap_or(0) >= INBOX_SINCE_MS);
    json!({"open": open, "older_count": before.len(), "older": if older { json!(before) } else { json!([]) },
           "stuck": rows("stuck"), "acked": rows("acked"), "capped": asks["capped"] == true})
}

/// How many asks a wake lists before it counts the rest by room (Blaze, 25 Oct: token efficiency). Measured that day:
/// 190 open asks made `ox brief` ~16k tokens and `ox inbox --mine` ~14k on every wake; the newest few carry the work.
pub const ASKS_SHOWN: usize = 12;

/// The rest of a list, counted by room: "+ 40 more: #astra 22 · #hustler 12 · #axxis 6 — ox asks --mine lists every one".
fn rest_by_room(rest: &[Value], room_key: &str) -> String {
    if rest.is_empty() {
        return String::new();
    }
    let mut by: std::collections::BTreeMap<String, usize> = Default::default();
    for r in rest {
        *by.entry(r[room_key].as_str().unwrap_or("?").to_owned()).or_default() += 1;
    }
    let mut parts: Vec<(String, usize)> = by.into_iter().collect();
    parts.sort_by_key(|p| std::cmp::Reverse(p.1));
    let parts: Vec<String> = parts.into_iter().map(|(k, n)| format!("#{k} {n}")).collect();
    format!("  + {} more: {} — ox asks --mine lists every one\n", rest.len(), parts.join(" · "))
}

fn asks_render(a: &Value, now: i64) -> String {
    let line = |r: &Value| {
        let marks: Vec<String> = r["marks"].as_object().map(|m| m.iter().map(|(k, v)| format!("{k} {}", v.as_str().unwrap_or(""))).collect()).unwrap_or_default();
        format!(
            "  {}  #{}  from {}  {}  {}{}{}  {}\n",
            r["id"].as_str().unwrap_or(""),
            r["room"].as_str().unwrap_or(""),
            r["author"].as_str().unwrap_or(""),
            ago(now, r["created_ms"].as_i64().unwrap_or(now)),
            r["ask"].as_str().unwrap_or(""),
            if r["overdue"] == true { " OVERDUE" } else if r["state"] == "answered" { " (another seat answered; yours still open)" } else { "" },
            if marks.is_empty() { String::new() } else { format!(" [{}]", marks.join(", ")) },
            r["card"].as_str().unwrap_or(""),
        )
    };
    if let Some(e) = a["error"].as_str() {
        return format!("ASKS TO ME · could not be read: {e}\n");
    }
    let list = |k: &str| a[k].as_array().cloned().unwrap_or_default();
    // How many a list held before the answer's limit cut it (`counts`, inbox_value); a list never cut is its own length.
    let total = |k: &str, shown: usize| a["counts"][k].as_u64().map_or(shown, |n| n as usize).max(shown);
    let unlisted = |total: usize, shown: usize| if total > shown {
        format!("  + {} more not in this answer: a larger limit (at most 200) lists them; ox asks --mine lists every one\n", total - shown)
    } else { String::new() };
    // Open means nobody has answered it; an ask another seat answered is still yours, but it is listed apart and not
    // counted as open (Jobs, 2026-10-10: "the header calls it open").
    let (answered, open): (Vec<Value>, Vec<Value>) = list("open").into_iter().partition(|r| r["state"] == "answered");
    let (n_open, n_answered) = (total("open_unanswered", open.len()), total("open_answered", answered.len()));
    let mut out = format!("ASKS TO ME · {n_open} open, every room. Each comes back on every pull until you mark it: react ack, stuck, done or no.\n");
    for r in open.iter().take(ASKS_SHOWN) {
        out += &line(r);
    }
    out += &rest_by_room(open.get(ASKS_SHOWN..).unwrap_or(&[]), "room");
    out += &unlisted(n_open, open.len());
    if n_answered > 0 {
        out += &format!("ANSWERED BY ANOTHER SEAT, yours still open · {n_answered}\n");
        for r in answered.iter().take(ASKS_SHOWN / 2) {
            out += &line(r);
        }
        out += &rest_by_room(answered.get(ASKS_SHOWN / 2..).unwrap_or(&[]), "room");
        out += &unlisted(n_answered, answered.len());
    }
    let n = a["older_count"].as_u64().unwrap_or(0);
    if n > 0 {
        let older = list("older");
        if older.is_empty() {
            out += &format!("  + {n} older open asks from before 8 Oct 08:00 UTC: inbox with older lists them\n");
        } else {
            out += &format!("OLDER · {n} open from before 8 Oct 08:00 UTC\n");
            for r in &older {
                out += &line(r);
            }
            out += &unlisted(n as usize, older.len());
        }
    }
    for (k, head) in [("stuck", "STUCK"), ("acked", "ACKED, not done"), ("later", "LATER, comes back by itself")] {
        let l = list(k);
        let n = total(k, l.len());
        if n > 0 {
            out += &format!("{head} · {n}\n");
            for r in &l {
                out += &line(r);
            }
            out += &unlisted(n, l.len());
        }
    }
    if a["capped"] == true {
        out += "  (text shows a bounded preview; structured asks contain the found rows; the candidate scan was capped, so older asks may be missing)\n";
    }
    out
}

/// How this place works, said once to a seat that arrives (Grok's onboarding: "after
/// sign-in there's no first-run: my State was empty and I had to discover rooms, norms and tools myself").
pub const NORMS: &[&str] = &[
    "Start every session with oracle_brief; before you stop, write what you did and what is next in your own room (oracle_say, tag resume).",
    "One inbox: oracle_inbox lists what is addressed to you in every room, newest first. An open ask comes back on every pull until you mark it with oracle_react: ack (seen, on it), stuck, done, or no.",
    "Speak with oracle_say. @handle addresses a seat; set ask (reply, act, decide, review) when you need something back, and it waits on them until they answer.",
    "Before you answer from memory, look: oracle_recall (gist: true gives short entries to choose from), then oracle_get for one record whole and oracle_thread for its conversation.",
    "oracle_remember is for decisions, lessons, preferences and commitments; never for status lines.",
    "Every write is signed as you. A write is done only when the result says signed:ok; QUEUED means it is on disk, not sent.",
    "Name your sources: an observation id, a link, or a measurement. What you recall is not evidence until you have checked it.",
    "A message comes whole or not at all: a long one is listed by its header and id; open it with oracle_get.",
    "Facts with a status and links live in the cortex: oracle_claims reads them; oracle_claim mints one or links it (supports, contradicts, supersedes, is-same-as, cites).",
];

/// The first-run read: who you are, the norms, the rooms (from the hallway) and your open asks.
pub fn hello_text(door: &dyn Door) -> Result<String> {
    let who = door.get("/whoami")?;
    let handle = who["handle"].as_str().unwrap_or("?").to_string();
    let now = who["server_ms"].as_i64().unwrap_or_else(now_ms);
    let mut out = format!("HELLO {handle} · now {} (Oracle clock, UTC){}\n", oracle_proto::iso_ms(now), local_suffix(&who["local"], now));
    let state = door.state(&handle).unwrap_or(Value::Null);
    match state["fields"]["current"].as_str().or(state["fields"]["purpose"].as_str()) {
        Some(line) => out += &format!("YOU      seat {handle}; your room is #{handle}; its State says: {}\n", line.chars().take(240).collect::<String>()),
        None => out += &format!("YOU      seat {handle}; your room is #{handle}, and it has no State yet. Write one so every session of yours starts from it: oracle_room_state with room \"{handle}\", purpose (what you are for), current (where you stand), next.\n"),
    }
    out += "HOW THIS PLACE WORKS\n";
    for (i, n) in NORMS.iter().enumerate() {
        out += &format!("  {}. {n}\n", i + 1);
    }
    match door.get("/hallway?days=14") {
        Ok(h) => {
            let rooms = h["rooms"].as_array().cloned().unwrap_or_default();
            out += &format!("ROOMS active in the last 14 days ({}), newest first: read any; post where your work belongs\n", rooms.len());
            for r in rooms.iter().take(30) {
                let line = r["purpose"].as_str().or(r["current"].as_str()).map(|l| l.chars().take(140).collect::<String>()).unwrap_or_else(|| "(no State: nothing signed says what it is for)".into());
                out += &format!("  #{:<14} {line}\n", r["room"].as_str().unwrap_or("?"));
            }
            if rooms.len() > 30 {
                out += &format!("  … {} more: oracle_hallway\n", rooms.len() - 30);
            }
        }
        Err(e) => out += &format!("ROOMS could not be read: {e}\n"),
    }
    let open = door.get("/asks?state=open").ok().and_then(|a| a["waiting_on_me"].as_array().map(|w| w.len()));
    out += &format!("NEXT     {}oracle_brief, then oracle_inbox.\n", open.map(|n| format!("{n} asks wait on you. ")).unwrap_or_default());
    Ok(out)
}

/// The hallway as text: one block per room, newest activity first; a room without a State says so.
pub fn hallway_text(v: &Value, now: i64) -> String {
    let rooms = v["rooms"].as_array().cloned().unwrap_or_default();
    let mut out = format!("HALLWAY · {} rooms active in {} days\n", rooms.len(), v["days"]);
    for r in &rooms {
        out += &format!("#{}  {} messages · last {}\n", r["room"].as_str().unwrap_or("?"), r["messages"], ago(now, r["last_ms"].as_i64().unwrap_or(now)));
        match r["current"].as_str() {
            Some(c) => out += &format!("  current  {}\n", c.chars().take(200).collect::<String>()),
            None => out += "  current  (no State: nothing signed says where this room stands)\n",
        }
        if let Some(n) = r["next"].as_str() {
            out += &format!("  next     {}\n", n.chars().take(160).collect::<String>());
        }
        let a = &r["oldest_open_ask"];
        if a.is_object() {
            out += &format!("  asks     {} open; oldest {} from {} {}: {}\n", r["open_asks"], a["id"].as_str().unwrap_or(""), a["author"].as_str().unwrap_or(""),
                ago(now, a["at_ms"].as_i64().unwrap_or(now)), a["card"].as_str().unwrap_or(""));
        } else {
            out += "  asks     none open\n";
        }
    }
    out
}

/// The claims read as text: one block per claim, its edges by short cid, the rejected writes said.
/// The cortex reads as text (`oracle_cortex`, `ox cortex nodes|node|marks`): one line per node or mark, ids in full.
/// The first `n` nodes of a `/cortex` answer, in its own order (ranked by approved readings for a kind, newest first
/// otherwise), saying what was cut (Rocky's live pass, 1 Nov 2026: `oracle_cortex op=nodes q=oracle top=5` answered 11
/// nodes; top bounded only the graph). `count` stays what matched, `shown` and `cut` say what this answer holds. The same
/// cut on the server and in the MCP, so the MCP bounds an answer from a server that does not know `top`; cutting twice
/// changes nothing.
pub fn cortex_keep_top(v: &mut Value, n: usize) {
    let total = v["count"].as_u64().map(|c| c as usize);
    let Some(nodes) = v["nodes"].as_array_mut() else { return };
    let matched = total.unwrap_or(nodes.len()).max(nodes.len());
    nodes.truncate(n);
    let shown = nodes.len();
    v["count"] = json!(matched);
    v["top"] = json!(n);
    v["shown"] = json!(shown);
    v["cut"] = json!(matched - shown);
}

pub fn cortex_text(op: &str, v: &Value) -> String {
    if op == "entity" { return v["primer"].as_str().unwrap_or("Entity primer unavailable").to_string(); }
    let mut out = String::new();
    let mark_line = |m: &Value| {
        let src = &m["source"];
        let labels: Vec<&str> = m["labels"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let intents: Vec<&str> = m["intents"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        format!("{}  [{}]{}{}  {}:{}-{}  by {}{}{}\n", m["id"].as_str().unwrap_or("?"), labels.join(" + "),
                if intents.is_empty() { String::new() } else { format!(" · {}", intents.join(", ")) },
                m["project"].as_str().map(|p| format!(" · {p}")).unwrap_or_default(),
                src["message"].as_str().unwrap_or("?"), src["start"], src["end"], m["by"].as_str().unwrap_or("?"),
                m["verdict"]["verdict"].as_str().map(|v| format!(" · {v}")).unwrap_or_default(),
                m["superseded_by"].as_str().map(|n| format!(" · superseded by {n}")).unwrap_or_default())
    };
    match op {
        "nodes" => {
            out.push_str(&format!("{} nodes{} · {} approved observations here, {} in the whole cortex{}\n", v["count"], if v["all"] == true { " (all, drafts included)" } else { " in the Cortex" },
                v["approved_observations"].as_u64().unwrap_or(0), v["approved_observations_total"].as_u64().unwrap_or(0),
                if v["rejected"].as_i64().unwrap_or(0) > 0 { format!(" · {} writes rejected", v["rejected"]) } else { String::new() }));
            if let Some(cut) = v["cut"].as_u64().filter(|c| *c > 0) {
                out.push_str(&format!("top {} shown, {cut} more: raise top (at most 500) or narrow with q, kind or room\n", v["shown"]));
            }
            // Why each write was rejected, grouped (the server lists every one under rejected_why; the count alone said nothing).
            let mut why: std::collections::BTreeMap<String, Vec<String>> = Default::default();
            for r in v["rejected_why"].as_array().into_iter().flatten() {
                why.entry(r["why"].as_str().unwrap_or("?").to_string()).or_default().push(r["id"].as_str().unwrap_or("?").to_string());
            }
            for (w, ids) in &why {
                out.push_str(&format!("  rejected · {} · {}: {}{}\n", ids.len(), w, ids.iter().take(3).cloned().collect::<Vec<_>>().join(", "), if ids.len() > 3 { ", …" } else { "" }));
            }
            for n in v["nodes"].as_array().into_iter().flatten() {
                // What it holds first: Blaze's approved readings linked to it; the old claim marks only where there are any.
                let readings = n["readings"].as_u64().map(|r| format!(" · {r} approved reading{}", if r == 1 { "" } else { "s" })).unwrap_or_default();
                let claims = n["marks"].as_u64().filter(|m| *m > 0).map(|m| format!(" · {m} claim marks ({} approved, {} rejected)", n["approved"], n["rejected"])).unwrap_or_default();
                let status = n["status"].as_str().map(|s| format!(" · {s}")).unwrap_or_default();
                out.push_str(&format!("{}  {:<8} {}{readings}{claims}{status}\n", &n["nid"].as_str().unwrap_or("?")[..12.min(n["nid"].as_str().unwrap_or("").len())],
                    n["kind"].as_str().unwrap_or("?"), n["title"].as_str().unwrap_or("?")));
                if let Some(verdict) = n["verdict"].as_str() {
                    let class = n["approval"]["class"].as_str().map(|c| format!(", {c}")).unwrap_or_default();
                    out.push_str(&format!("  node review: {verdict}{class}\n"));
                }
                if let Some(by) = n["superseded_by"].as_str() {
                    out.push_str(&format!("  superseded by {by}\n"));
                }
            }
        }
        "node" => {
            let n = &v["node"];
            out.push_str(&format!("{} {}  {}\n", n["kind"].as_str().unwrap_or("?"), n["nid"].as_str().unwrap_or("?"), n["title"].as_str().unwrap_or("?")));
            if let Some(a) = n["aliases"].as_array().filter(|a| !a.is_empty()) {
                out.push_str(&format!("also: {}\n", a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" · ")));
            }
            if let Some(record) = n["minted_by"].as_str() { out.push_str(&format!("record: {record}\n")); }
            if let Some(quote) = n["quote"].as_str() { out.push_str(&format!("source quote: {quote}\n")); }
            if let Some(message) = n["source"]["message"].as_str() {
                out.push_str(&format!("source: {message}:{}-{} · chat {} · attributed speaker {}\n",
                    n["source"]["start"], n["source"]["end"], n["source"]["chat"].as_str().unwrap_or("unknown"), n["speaker"].as_str().unwrap_or("unknown")));
            }
            if let Some(note) = n["note"].as_str() {
                out.push_str(&format!("drafter interpretation ({}): {note}\n", n["drafted_by"].as_str().unwrap_or("unknown")));
            }
            let intents: Vec<&str> = n["intents"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let labels: Vec<&str> = n["labels"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            out.push_str(&format!("labels: {} · intents: {} · project: {}\n", labels.join(", "), intents.join(", "), n["project"].as_str().unwrap_or("unknown")));
            if let Some(verdict) = n["verdict"]["verdict"].as_str() {
                out.push_str(&format!("recorded review: {verdict} · by {} · signed by {} · {}\n", n["verdict"]["by"].as_str().unwrap_or("unknown"), n["verdict"]["signed_by"].as_str().unwrap_or("unknown"), n["verdict"]["message"].as_str().unwrap_or("unknown")));
            }
            out.push_str(&format!("state: {}\n", n["status"]["verdict"].as_str().unwrap_or("not recorded")));
            for (side, target) in [("out", "to"), ("in", "from")] {
                let links: Vec<&Value> = n[side].as_array().into_iter().flatten().collect();
                for link in links.iter().take(20) {
                    out.push_str(&format!("{side}: {} {} · {}\n", link["kind"].as_str().unwrap_or("?"), link[target].as_str().unwrap_or("?"), link["message"].as_str().unwrap_or("?")));
                }
                if links.len() > 20 { out.push_str(&format!("{side}: {} more links; use --json for all\n", links.len() - 20)); }
            }
            for m in v["marks"].as_array().into_iter().flatten() {
                out.push_str(&mark_line(m));
            }
        }
        _ => {
            out.push_str(&format!("{} marks\n", v["count"]));
            for m in v["marks"].as_array().into_iter().flatten() {
                out.push_str(&mark_line(m));
            }
        }
    }
    out
}

/// The graph as text (`oracle_graph`, `ox graph`): counts by kind, then the twelve most connected, with what joins them.
pub fn graph_text(v: &Value) -> String {
    let nodes = v["nodes"].as_array().cloned().unwrap_or_default();
    let edges = v["edges"].as_array().cloned().unwrap_or_default();
    let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
    for n in &nodes { *kinds.entry(n["kind"].as_str().unwrap_or("?").to_owned()).or_default() += 1; }
    let mut degree: std::collections::BTreeMap<String, i64> = Default::default();
    for e in &edges {
        for k in ["source", "target"] { *degree.entry(e[k].as_str().unwrap_or("").to_owned()).or_default() += e["weight"].as_i64().unwrap_or(1); }
    }
    let label = |id: &str| nodes.iter().find(|n| n["id"] == id).and_then(|n| n["label"].as_str()).unwrap_or(id).to_owned();
    let mut out = format!("{} graph: {} nodes ({}), {} edges\n", v["view"].as_str().unwrap_or("?"), nodes.len(),
        kinds.iter().map(|(k, n)| format!("{n} {k}")).collect::<Vec<_>>().join(", "), edges.len());
    let mut top: Vec<(&String, &i64)> = degree.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    for (id, d) in top.into_iter().take(12) { out.push_str(&format!("  {d:>6}  {}\n", label(id))); }
    out
}

pub fn claims_text(v: &Value) -> String {
    let short = |c: &Value| c.as_str().map(|x| x.chars().take(12).collect::<String>()).unwrap_or_default();
    let mut out = format!("CLAIMS · {}\n", v["count"]);
    for c in v["claims"].as_array().into_iter().flatten() {
        out += &format!("{} [{} · {}]{}{} #{} {}\n  {}\n", short(&c["cid"]), c["type"].as_str().unwrap_or(""), c["status"].as_str().unwrap_or(""),
            c["row"].as_str().map(|r| format!(" row {r}")).unwrap_or_default(),
            if c["superseded_by"].is_string() { format!(" SUPERSEDED by {}", short(&c["superseded_by"])) } else { String::new() },
            c["room"].as_str().unwrap_or(""), c["minted_by"].as_str().unwrap_or(""), c["statement"].as_str().unwrap_or(""));
        for (k, arrow) in [("out", "→"), ("in", "←")] {
            for e in c[k].as_array().into_iter().flatten() {
                out += &format!("    {arrow} {} {} ({})\n", e["kind"].as_str().unwrap_or(""), short(&e["cid"]), e["by"].as_str().unwrap_or(""));
            }
        }
        if let Some(src) = c["sources"].as_array().filter(|a| !a.is_empty()) {
            out += &format!("    sources: {}\n", src.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
        }
    }
    let rej = v["rejected"].as_array().cloned().unwrap_or_default();
    if !rej.is_empty() {
        out += &format!("REJECTED WRITES · {}\n", rej.len());
        for r in &rej {
            out += &format!("  {}: {}\n", r["id"].as_str().unwrap_or(""), r["why"].as_str().unwrap_or(""));
        }
    }
    out
}

pub fn items_text(items: &[Value], width: usize) -> String {
    let mut out = String::new();
    for it in items {
        let me = if it["for_me"].as_bool() == Some(true) {
            "→"
        } else {
            " "
        };
        // The tail is the outcome: what was asked, whether it is overdue, who answered.
        let mut tail = String::new();
        match it["ask"].as_str() {
            Some("reply") | Some("act") if it["answered_by"].is_null() => {
                tail += &format!("  · ask: {}", it["ask"].as_str().unwrap_or(""));
                if let Some(b) = it["by_ms"].as_i64() {
                    tail += &format!(" {}", due(b));
                }
                if it["overdue"].as_bool() == Some(true) {
                    tail += " · OVERDUE";
                }
            }
            _ => {}
        }
        if let Some(a) = it["answered_by"].as_str() {
            tail += &format!("  · answered {}", a.rsplit('-').next().unwrap_or(a));
        }
        // Read before you repeat (2026-09-30): the subject this message names has moved since it was written.
        for st in it["stale"].as_array().into_iter().flatten() {
            tail += &format!(
                "  · NOW {} is {} ({})",
                st["subject"].as_str().unwrap_or(""),
                st["state"].as_str().unwrap_or(""),
                st["owner"].as_str().unwrap_or("")
            );
        }
        out += &format!(
            "{me} {}  #{:<12} {:<12} {}{tail}\n",
            clock(it["created_ms"].as_i64().unwrap_or(0)),
            it["channel"].as_str().unwrap_or(""),
            it["author"].as_str().unwrap_or(""),
            one_line(it["content"].as_str().unwrap_or(""), width)
        );
    }
    out
}

/// One observation, whole: header line, text, tags, cargo.
pub fn full(o: &Value) -> String {
    let mut out = format!(
        "{}  #{}  {}  {}\n{}\n",
        o["id"].as_str().unwrap_or(""),
        o["channel"].as_str().unwrap_or(""),
        o["author"].as_str().unwrap_or(""),
        clock(o["created_ms"].as_i64().unwrap_or(0)),
        o["content"].as_str().unwrap_or("")
    );
    let tags: Vec<&str> = o["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
        .unwrap_or_default();
    if !tags.is_empty() {
        out += &format!("tags: {}\n", tags.join(" "));
    }
    if let Some(c) = o["cargo"].as_str() {
        out += &format!("cargo: {c}\n");
    }
    out
}

/// The digest as lines: cheaper for a model to read than the JSON it came from.
/// The experiment registry as lines: subject · stage · room — title: result (evidence n), newest first.
/// A day number since 1970-01-01 as YYYY-MM-DD (civil from days, Howard Hinnant; the inverse of `touch_ms`'s).
fn ymd(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `oracle_where`: the rooms a search's words are said in, from `/search?by_room=true`.
pub fn where_text(query: &str, v: &Value) -> String {
    let rooms = v["rooms"].as_array().cloned().unwrap_or_default();
    if rooms.is_empty() {
        return format!("\"{query}\" is not said in any room — try other words or any=true before concluding there is no record");
    }
    let day = |ms: i64| ymd(ms.div_euclid(86_400_000));
    let total: i64 = rooms.iter().filter_map(|r| r["count"].as_i64()).sum();
    let mut out = vec![format!("\"{query}\": {total} messages in {} rooms, most first", rooms.len())];
    for r in &rooms {
        let (first, last) = (day(r["first_ms"].as_i64().unwrap_or(0)), day(r["last_ms"].as_i64().unwrap_or(0)));
        let span = if first == last { first } else { format!("{first} to {last}") };
        out.push(format!("#{}  {}  {span}", r["room"].as_str().unwrap_or(""), r["count"].as_i64().unwrap_or(0)));
    }
    out.join("\n")
}

pub fn experiments_text(v: &Value) -> String {
    let items = v["items"].as_array().cloned().unwrap_or_default();
    let mut out = format!("{} of {} experiments in the registry.\n", items.len(), v["total"].as_u64().unwrap_or(0));
    for e in &items {
        let d = &e["data"];
        let text = |k: &str| d[k].as_str().unwrap_or("").trim().to_string();
        let result = text("result");
        out.push_str(&format!(
            "{} · {} · #{} · {} — {}{} [{}]\n",
            e["subject"].as_str().unwrap_or("?"),
            if text("stage").is_empty() { e["state"].as_str().unwrap_or("?").to_string() } else { text("stage") },
            e["room"].as_str().unwrap_or("?"),
            if text("title").is_empty() { e["note"].as_str().unwrap_or("").to_string() } else { text("title") },
            if result.is_empty() { "no result yet".to_string() } else { result.chars().take(200).collect() },
            d["evidence"].as_array().map(|a| format!(" (evidence {})", a.len())).unwrap_or_default(),
            e["id"].as_str().unwrap_or("")
        ));
    }
    out
}

/// The word ranking (head `search.terms`, written hourly by `oracle terms`, docs/TERMS.md; Blaze 2026-10-08: "make it
/// available to the room whenever they search Oracle"): one line per word, best first.
pub fn terms_text(head: &Value, limit: i64) -> String {
    // Either GET /terms ({top, documents, generated_at}) or the head search.terms (the same in its cargo's data).
    let cargo: Value = head["observation"]["cargo"].as_str().and_then(|c| serde_json::from_str(c).ok()).unwrap_or(Value::Null);
    let d = if head["top"].is_array() { head } else { &cargo["data"] };
    let Some(top) = d["top"].as_array() else {
        return "No word ranking yet (head search.terms is not set).".into();
    };
    let mut out = format!(
        "The {} words that matter in {} messages, ranked {} (weight = times said x rarity; common words sink).\n",
        top.len().min(limit as usize),
        d["documents"].as_i64().unwrap_or(0),
        d["generated_at"].as_str().unwrap_or("?")
    );
    for (i, t) in top.iter().take(limit as usize).enumerate() {
        out.push_str(&format!("{}. {}  weight {}  in {} messages\n", i + 1, t[0].as_str().unwrap_or("?"), t[1], t[2]));
    }
    out
}

/// One line of a message: id, date, its first line cut to `n` characters.
fn round_line(m: &Value, n: usize) -> String {
    let first = m["content"].as_str().unwrap_or("").lines().next().unwrap_or("").trim().to_string();
    let cut: String = first.chars().take(n).collect();
    let more = if first.chars().count() > n { "…" } else { "" };
    let day: String = oracle_proto::iso_ms(m["created_ms"].as_i64().unwrap_or(0)).chars().take(10).collect();
    format!("{} · {} · {cut}{more}", m["id"].as_str().unwrap_or("?"), day)
}

/// A room's rounds: its messages that start with "ROUND", newest first.
fn rounds_of(door: &dyn Door, room: &str, n: i64) -> Result<Vec<Value>> {
    let v = door.get(&format!("/search?channel={}&limit={}&starts={}", enc(room), n.clamp(1, 50), enc("ROUND")))?;
    Ok(v["items"].as_array().cloned().unwrap_or_default())
}

/// A head is overdue when its next touch (YYYY-MM-DD…) is before today, and it is still open or running.
fn head_overdue(h: &Value, today: &str) -> bool {
    let live = matches!(h["state"].as_str().unwrap_or(""), "open" | "running" | "blocked");
    let next = h["next_touch"].as_str().unwrap_or("");
    live && next.len() >= 10 && &next[..10] < today
}

/// oracle_room_state_full: the State, the board, the digest, the last rounds and my open asks, in one read.
fn room_state_full(door: &dyn Door, a: &Value) -> Result<String> {
    let room = text_of(a, "room")?;
    let n = a["rounds"].as_i64().unwrap_or(5);
    let roots = a["roots"].as_str().unwrap_or("blaze,hustler");
    let section = |title: &str, body: Result<String>| format!("== {title} ==\n{}\n", body.unwrap_or_else(|e| format!("(unavailable: {e})")));
    let state = room_state_typed(door, &json!({"room": room})).map(|t| t.text);
    let board = tasks(door, None, Some(&room), &json!({}));
    let digest = door.get(&format!("/threads?view=digest&channel={}&roots={}", enc(&room), enc(roots))).map(|d| digest_text(&d));
    let rounds = rounds_of(door, &room, n).map(|r| if r.is_empty() { "no rounds".into() } else { r.iter().map(|m| round_line(m, 160)).collect::<Vec<_>>().join("\n") });
    let asks = door.get(&format!("/asks?state=open&room={}", enc(&room))).map(|v| {
        let items: Vec<String> = ["waiting_on_me", "i_wait_on", "items", "asks"].iter()
            .flat_map(|k| v[*k].as_array().cloned().unwrap_or_default())
            .map(|x| format!("{} · {} · {}", x["id"].as_str().unwrap_or("?"), x["author"].as_str().or(x["from"].as_str()).unwrap_or("?"),
                             x["card"].as_str().or(x["text"].as_str()).unwrap_or("").chars().take(120).collect::<String>()))
            .collect();
        if items.is_empty() { "none".into() } else { items.join("\n") }
    });
    Ok([section("STATE", state), section("BOARD", board), section("WHERE IT STANDS", digest),
        section(&format!("LAST {n} ROUNDS"), rounds), section(&format!("MY OPEN ASKS IN #{room}"), asks)].concat())
}

/// oracle_room_digest_agent: a room for a seat arriving, under 500 tokens.
fn room_digest_agent(door: &dyn Door, a: &Value) -> Result<String> {
    let room = text_of(a, "room")?;
    let n = a["rounds"].as_i64().unwrap_or(5).clamp(1, 10);
    let today: String = oracle_proto::iso_ms(now_ms()).chars().take(10).collect();
    let cut = |s: &str, k: usize| -> String { let c: String = s.chars().take(k).collect(); if s.chars().count() > k { c + "…" } else { c } };
    let mut out = format!("#{room} for a seat arriving\n");
    match door.state(&room) {
        Ok(st) if !st.is_null() => {
            let f = &st["fields"];
            let mut any = false;
            for k in ["goal", "current", "next"] {
                if let Some(v) = f[k].as_str().filter(|v| !v.is_empty()) {
                    out += &format!("{k}: {}\n", cut(v, 220));
                    any = true;
                }
            }
            if !any { out += "State: no signed State yet\n"; }
        }
        Ok(_) => out += "State: this door cannot read it\n",
        Err(e) => out += &format!("State: unavailable ({e})\n"),
    }
    match door.get(&format!("/states?channel={}", enc(&room))) {
        Ok(v) => {
            let heads = v["heads"].as_array().cloned().unwrap_or_default();
            let blocked: Vec<String> = heads.iter().filter(|h| h["state"] == "blocked").map(|h| format!("{} ({})", h["subject"].as_str().unwrap_or("?"), h["owner"].as_str().unwrap_or("?"))).collect();
            let late: Vec<String> = heads.iter().filter(|h| head_overdue(h, &today)).map(|h| format!("{} ({}, next {})", h["subject"].as_str().unwrap_or("?"), h["owner"].as_str().unwrap_or("?"), &h["next_touch"].as_str().unwrap_or("")[..10])).collect();
            let live = heads.iter().filter(|h| matches!(h["state"].as_str().unwrap_or(""), "open" | "running" | "blocked")).count();
            out += &format!("board: {live} live heads · {} blocked · {} overdue\n", blocked.len(), late.len());
            for b in blocked.iter().take(3) { out += &format!("  blocked: {b}\n"); }
            for l in late.iter().take(3) { out += &format!("  overdue: {l}\n"); }
        }
        Err(e) => out += &format!("board: unavailable ({e})\n"),
    }
    match door.get(&format!("/threads?view=digest&channel={}&roots={}", enc(&room), enc("blaze,hustler"))) {
        Ok(d) => {
            let needs = d["needs"].as_array().cloned().unwrap_or_default();
            out += &format!("needs someone: {} of {}\n", needs.len(), d["needs_of"]);
            if let Some(o) = needs.first() {
                out += &format!("  oldest: {}h {} · {}\n", o["h"], o["by"].as_str().unwrap_or("?"), cut(o["t"].as_str().unwrap_or(""), 120));
            }
        }
        Err(e) => out += &format!("threads: unavailable ({e})\n"),
    }
    match rounds_of(door, &room, n) {
        Ok(r) if !r.is_empty() => { out += "last rounds:\n"; for m in &r { out += &format!("  {}\n", round_line(m, 90)); } }
        Ok(_) => out += "last rounds: none\n",
        Err(e) => out += &format!("last rounds: unavailable ({e})\n"),
    }
    Ok(out)
}

pub fn digest_text(d: &Value) -> String {
    let mut out = format!(
        "#{} · {} threads of {} roots",
        d["room"].as_str().unwrap_or("?"),
        d["threads"],
        d["of"]
    );
    for f in d["floors"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "\n  {:<12} waiting {} · active {} · answered {} · quiet {}",
            f["fl"].as_str().unwrap_or("?"),
            f["wait"],
            f["act"],
            f["ans"],
            f["quiet"]
        ));
    }
    let needs = d["needs"].as_array().cloned().unwrap_or_default();
    out.push_str(&format!(
        "\nneeds someone: {} of {}",
        needs.len(),
        d["needs_of"]
    ));
    for n in &needs {
        out.push_str(&format!(
            "\n  {}h {} [{}] {} · {} replies · {}",
            n["h"],
            n["by"].as_str().unwrap_or("?"),
            n["fl"].as_str().unwrap_or("?"),
            n["t"].as_str().unwrap_or(""),
            n["re"],
            n["id"].as_str().unwrap_or("")
        ));
    }
    out
}

/// A thread as lines: its state, then the root whole, then every reply whole.
pub fn thread_text(t: &Value) -> String {
    let mut out = format!(
        "THREAD  {} · {} · floor {} · {} replies\n",
        t["card"]["status"].as_str().unwrap_or("?"),
        t["state"].as_str().unwrap_or("?"),
        t["floor"]["name"].as_str().unwrap_or("?"),
        t["card"]["replies"]
    );
    for (label, key) in [("OUTCOME", "outcome"), ("SUMMARY", "summary")] {
        if let Some(text) = t[key]["text"].as_str() {
            out += &format!("{label}  {} — {}\n", t[key]["author"].as_str().unwrap_or("?"), text);
        }
    }
    out += "ROOT\n";
    out += &full(&t["root"]);
    for r in t["replies"].as_array().into_iter().flatten() {
        out += "REPLY\n";
        out += &full(r);
    }
    out
}

/// A head's `next_touch` ("2026-09-29" or "2026-08-05T16:57:46Z") as milliseconds since the epoch, UTC.
/// None for anything else — a date that cannot be read is never overdue.
pub fn touch_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = s.split_once('T').unwrap_or((s, ""));
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    if d.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    // days from civil (Howard Hinnant), valid for the proleptic Gregorian calendar
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut secs = 0i64;
    if !time.is_empty() {
        let t = time.trim_end_matches('Z');
        let mut p = t.split(':').map(|x| x.parse::<i64>().ok());
        let (h, mi) = (p.next()??, p.next()??);
        let sec = p.next().flatten().unwrap_or(0);
        if !(0..24).contains(&h) || !(0..60).contains(&mi) {
            return None;
        }
        secs = h * 3600 + mi * 60 + sec;
    }
    Some((days * 86_400 + secs) * 1000)
}

/// How long a blocked head may sit before it must be re-checked (Blaze, 30 Oct 2026: "there is no signing issue now in dot
/// terminal in browser, why is that a blocker on building anything, our system for this is broken, we need to build proper
/// processes"): dot-vault-credentials sat blocked from 22 to 30 Oct on a reason that had stopped being true, with no date,
/// no test and the whole lane frozen behind one detail.
pub const BLOCKED_RECHECK_DAYS: i64 = 3;

/// A blocked head names who or what must act (`needs`), the one step that cannot proceed (`blocks`: the rest of the
/// work continues), how anyone can verify the blocker still holds (`check`), and a re-check date at most
/// BLOCKED_RECHECK_DAYS away. Other states pass through untouched; their fields are refused so a stale `needs` is not
/// carried on a head that is no longer blocked.
pub fn blocked_rule(cargo: &mut Value, needs: &[String], blocks: Option<&str>, check: Option<&str>, now: i64) -> Result<()> {
    let blocked = cargo["state"].as_str() == Some("blocked");
    if !blocked {
        if !needs.is_empty() || blocks.is_some() || check.is_some() {
            bail!("needs, blocks and check belong to a blocked head only");
        }
        return Ok(());
    }
    let needs: Vec<&str> = needs.iter().map(|n| n.trim()).filter(|n| !n.is_empty()).collect();
    if needs.is_empty() || needs.iter().any(|n| n.len() > 120) {
        bail!("a blocked head names who or what must act: --needs <handle|subject|obs id> (repeatable)");
    }
    let blocks = blocks.map(str::trim).filter(|b| !b.is_empty() && b.len() <= 300)
        .ok_or_else(|| anyhow!("a blocked head names the one step that cannot proceed: --blocks \"…\" (the rest of the work continues)"))?;
    let check = check.map(str::trim).filter(|c| !c.is_empty() && c.len() <= 300)
        .ok_or_else(|| anyhow!("a blocked head says how to verify the blocker still holds: --check \"…\""))?;
    let next = cargo["next_touch"].as_str().unwrap_or("");
    let due = touch_ms(next).ok_or_else(|| anyhow!("a blocked head needs --next <date>, at most {BLOCKED_RECHECK_DAYS} days away: the day it is re-checked"))?;
    let due_end = if next.contains('T') { due } else { due + 86_400_000 - 1 };
    if due_end < now {
        bail!("--next {next} is already past: a blocker is re-checked from today");
    }
    if due - now > BLOCKED_RECHECK_DAYS * 86_400_000 {
        bail!("--next {next} is more than {BLOCKED_RECHECK_DAYS} days away: re-check a blocker within {BLOCKED_RECHECK_DAYS} days");
    }
    cargo["needs"] = json!(needs);
    cargo["blocks"] = json!(blocks);
    cargo["check"] = json!(check);
    Ok(())
}

/// How long past its `next_touch` a head is ("3h", "16d"), or None while the clock has not passed.
pub fn overdue(next_touch: &str, now: i64) -> Option<String> {
    let t = touch_ms(next_touch)?;
    // A bare date is due at the end of that day, not at its midnight.
    let t = if next_touch.contains('T') { t } else { t + 86_400_000 - 1 };
    let s = (now - t) / 1000;
    if s <= 0 {
        return None;
    }
    Some(if s < 5400 {
        format!("{}m", (s / 60).max(1))
    } else if s < 172_800 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    })
}

/// The seat section of a brief: a mind's or a human's own room State, boot first, one line per field
/// that is set. Empty for a project room (kind absent or `project`).
pub fn seat_text(handle: &str, state: &Value) -> String {
    let fields = &state["fields"];
    let kind = fields["kind"].as_str().unwrap_or("");
    if !fields.is_object() || kind.is_empty() || kind == "project" {
        return String::new();
    }
    let head = state["heads"].as_array().and_then(|h| h.first()).and_then(|h| h.as_str()).unwrap_or("?");
    let mut out = format!("ME         #{handle} · {kind} · State {head}\n");
    for (label, key) in [
        ("PERSONA", "persona"),   // who the mind is and how it speaks, before anything it must do (Blaze, 18 Oct 2026: "you have no character")
        ("BOOT", "boot"),
        ("TOOLS", "tools"),
        ("ROOMS", "rooms"),
        ("RULES", "rules"),
        ("CURRENT", "current"),
        ("NEXT", "next"),
        ("QUESTIONS", "questions"),
    ] {
        if let Some(v) = fields[key].as_str().map(str::trim).filter(|v| !v.is_empty()) {
            out.push_str(&format!("{label:<11}{v}\n"));
        }
    }
    out
}

/// A card list, one line each: status, room, id, how long it has been in that status, title, and who it waits on.
/// `whose` shows the owner (for a list that is not the reader's own).
pub fn cards_text(v: &Value, whose: bool) -> String {
    let now = v["now_ms"].as_i64().unwrap_or_else(now_ms);
    let age = |ms: i64| {
        let m = (now - ms).max(0) / 60_000;
        if m < 60 { format!("{m}m") } else if m < 48 * 60 { format!("{}h", m / 60) } else { format!("{}d", m / 1440) }
    };
    let mut out = String::new();
    for c in v["cards"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let status = c["status"].as_str().unwrap_or("?");
        let flag = if c["conflict"] == true { " CONFLICT" } else if c["effective_status"].as_str().is_some_and(|e| e != status) { " HELD" } else { "" };
        out.push_str(&format!("  {status:<11} #{:<8} {}  {}  {}{flag}", c["room"].as_str().unwrap_or("?"), c["id"].as_str().unwrap_or("?"),
            age(c["since_ms"].as_i64().unwrap_or(now)), one_line(c["title"].as_str().unwrap_or(""), 60)));
        if whose { out.push_str(&format!("  · {}", c["owner"].as_str().unwrap_or("?"))); }
        if let Some(u) = c["unblock"].as_str().filter(|u| !u.is_empty()) { out.push_str(&format!("  ← {}", one_line(u, 80))); }
        out.push('\n');
    }
    out
}

/// The asks this seat opened that haven't ended (docs/ASKS-AND-EVENTS.md "Whoever opens an ask closes it"), from
/// `/asks`' `im_waiting_on`: answered ones to close (oldest first), open ones past 24 h (still needed?), and the rest.
pub fn my_asks(asks: &Value, now: i64) -> Value {
    let row = |r: &Value| json!({"id": r["id"], "room": r["room"], "ask": r["ask"], "state": r["state"],
        "held_by": r["held_by"], "age": ago(now, r["created_ms"].as_i64().unwrap_or(now)),
        "created_ms": r["created_ms"], "card": r["card"], "gap": r["gap"]});
    let mut rows: Vec<Value> = asks["im_waiting_on"].as_array().cloned().unwrap_or_default();
    rows.sort_by_key(|r| r["created_ms"].as_i64().unwrap_or(0));
    let day = now - 86_400_000;
    let answered: Vec<Value> = rows.iter().filter(|r| r["state"] == "answered").map(row).collect();
    let open: Vec<&Value> = rows.iter().filter(|r| r["state"] != "answered").collect();
    let old: Vec<Value> = open.iter().filter(|r| r["created_ms"].as_i64().unwrap_or(now) < day).map(|r| row(r)).collect();
    let mut fresh: Vec<Value> = open.iter().filter(|r| r["created_ms"].as_i64().unwrap_or(now) >= day).map(|r| row(r)).collect();
    fresh.reverse();
    json!({"answered": answered, "old": old, "open": fresh, "capped": asks["capped"] == true})
}

/// MY ASKS as text. `limit` lines per list (None: all), each list saying how many it left out.
pub fn my_asks_text(m: &Value, limit: Option<usize>) -> String {
    let n = |k: &str| m[k].as_array().map_or(0, Vec::len);
    let (a, o, f) = (n("answered"), n("old"), n("open"));
    if a + o + f == 0 {
        return String::new();
    }
    let mut out = format!("MY ASKS    {} open, {a} answered: close each when it is settled (ox close <id>; all: ox asks --mine)\n", o + f);
    for (key, title) in [("answered", "answered, close them"), ("old", "open past 24 h, still needed?"), ("open", "open")] {
        let rows = m[key].as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            continue;
        }
        out += &format!("  {title} · {}\n", rows.len());
        let shown = limit.unwrap_or(rows.len()).min(rows.len());
        for r in &rows[..shown] {
            let held = r["held_by"].as_array().map(|h| h.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")).filter(|h| !h.is_empty()).unwrap_or_else(|| "·".into());
            out += &format!("    {}  #{:<9} {:<11} {:<8} {}\n", r["id"].as_str().unwrap_or("?"), r["room"].as_str().unwrap_or("?"),
                held, r["age"].as_str().unwrap_or(""), one_line(r["card"].as_str().unwrap_or(""), 70));
            out += &gap_render(&r["gap"], "      ");
        }
        if shown < rows.len() {
            out += &format!("    … {} more: ox asks --mine\n", rows.len() - shown);
        }
    }
    out
}

/// The footer's line for the seat's own asks; None when it has none open or answered.
pub fn my_asks_nudge(m: &Value) -> Option<String> {
    let n = |k: &str| m[k].as_array().map_or(0, Vec::len);
    let (open, answered) = (n("old") + n("open"), n("answered"));
    (open + answered > 0).then(|| format!("· mine: {open} open, {answered} answered, close them: ox asks --mine"))
}

/// The same line from the count answer (`GET /asks?state=open&count=1`, schema asks-count/1), which carries the numbers
/// and no rows (Rocky, 1 Nov 2026: the footer fetched every open ask whole to print this one line, and timed out).
/// None for an answer that is not a count answer, so a caller can tell an older server from a seat with nothing open.
pub fn my_asks_count_nudge(c: &Value) -> Option<Option<String>> {
    if c["schema"] != "asks-count/1" {
        return None;
    }
    let total = c["im_waiting_on"].as_u64().unwrap_or(0);
    let answered = c["im_waiting_on_answered"].as_u64().unwrap_or(0).min(total);
    let open = total - answered;
    Some((total > 0).then(|| format!("· mine: {open} open, {answered} answered, close them: ox asks --mine")))
}

/// How to tell the owner, printed under NOW on every wake (Blaze, 3 Nov 2026: "why do you have to look up the card
/// format every time ... that should be part of your core brief"). The same flags `tell_problems` checks; a decide is
/// the only ask the Bridge turns into a card today (dotd reads waiting_on_me), so the check is that its card landed.
const TELL_BLOCK: &str = "TELL       the owner reads the Bridge, never this terminal; a decide is the only ask that becomes a card\n  \
decide   ox tell \"<head, at most 40 words>\" --ask decide --option \"<what happens>\" --option \"<what happens>\" --recommend <n> --why \"<one sentence>\" --by <date> --affects \"<who>: <how>\" --absent \"<who>: <why>\" --owed \"<what it owes>\" --room <room>\n  \
rules    2 to 6 options of at most 80 characters each, in the owner's words; --why at most 240; one question a message\n  \
answer   the same, plus --reply <their OBS id> in its room; a new topic is top level in your own room\n  \
check    signed:ok <id>, then the card <id>#ask is in the Bridge store (bridge/events.sqlite, event_id) within a minute\n";

/// The brief a session starts with: who I am, my last RESUME note, what is addressed to me, what is new, and the work I own counted.
pub fn brief_text(door: &dyn Door) -> Result<String> {
    brief_with(door, false)
}

/// The seat's own handover: the newest message it wrote in its own room tagged `resume`, whole. None when it never wrote one.
/// Read newest first by tag (never ranked search: BM25 order can restore an older handover). `whole` is false when only the
/// list preview could be read.
fn last_resume(door: &dyn Door, handle: &str) -> Option<(Value, bool)> {
    let page = door.get(&format!("/search?channel={}&tag=resume&limit=20", enc(handle))).ok()?;
    let hit = page["items"].as_array()?.iter()
        .find(|i| i["author"].as_str() == Some(handle) && i["tags"].as_array().is_some_and(|t| t.iter().any(|t| t == "resume")))?.clone();
    let id = hit["id"].as_str()?;
    match door.get(&format!("/search?id={}", enc(id))).ok().filter(|o| o["content"].is_string()) {
        Some(o) => Some((o, true)),
        None => Some((hit, false)),
    }
}

/// A record the server verified when it was written (envelope 2: a native signed write; an import is envelope 1 and
/// was never verified here), by this seat, in its own room. The server checks every native signature before it stores
/// the row (docs/IDENTITY.md "Trust"), so envelope 2 is the boundary; a non-empty `sig` string alone is not.
fn signed_by_seat(rec: &Value, handle: &str) -> bool {
    rec["author"].as_str() == Some(handle) && rec["channel"].as_str() == Some(handle) && rec["envelope"].as_i64() == Some(2)
}

/// Whether `rec` can vouch for a crew entry `handle`: a State revision (cargo `room-context/1`, tag `room-context:v1`)
/// that the seat itself wrote in its own room and the server verified (envelope 2). Anything else is not its State.
pub fn crew_source_ok(rec: &Value, handle: &str) -> std::result::Result<(), String> {
    if !signed_by_seat(rec, handle) {
        return Err(format!("not a server-verified record of {handle} in #{handle}"));
    }
    let cargo: Value = match &rec["cargo"] { Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null), v => v.clone() };
    let tagged = rec["tags"].as_array().is_some_and(|t| t.iter().any(|t| t == "room-context:v1"));
    if cargo["schema"] != "room-context/1" || !tagged {
        return Err(format!("not a State revision of {handle} (room-context/1)"));
    }
    Ok(())
}

/// Bounds of an identity head (docs/IDENTITY.md): whole means whole within these, and a head past them is refused at
/// write and at read, never cut.
pub const IDENTITY_MAX_BYTES: usize = 32_768;
pub const IDENTITY_FIELD_MAX: usize = 4_000;
pub const IDENTITY_PERSONA_MAX: usize = 16_384;
pub const IDENTITY_CREW_MAX: usize = 40;
pub const IDENTITY_PRINCIPLES_MAX: usize = 20;
pub const IDENTITY_FIELDS: [&str; 4] = ["purpose", "partner", "mission", "authority"];

/// The cited ids of an identity head's `refs`, per field (docs/IDENTITY.md). A cite is an id or, as a read prints it,
/// `{id, by}` (the label is recomputed on write); any other shape is refused, never dropped.
pub fn identity_ref_ids(refs: &Value) -> Result<Vec<(String, Vec<String>)>, String> {
    let Some(given) = refs.as_object() else {
        return if refs.is_null() { Ok(vec![]) } else { Err("refs must be an object of field: [ids]".into()) };
    };
    let mut out = vec![];
    for (k, ids) in given {
        if !IDENTITY_FIELDS.contains(&k.as_str()) { return Err(format!("refs.{k}: refs are for {IDENTITY_FIELDS:?}")); }
        let list = ids.as_array().ok_or_else(|| format!("refs.{k} must be a list of ids"))?;
        let mut v = vec![];
        for c in list {
            v.push(c.as_str().or_else(|| c["id"].as_str()).ok_or_else(|| format!("refs.{k}: {c} is not an id"))?.to_owned());
        }
        out.push((k.clone(), v));
    }
    Ok(out)
}

/// Why a head breaks the bounds (empty when it does not).
pub fn identity_bounds(h: &Value) -> Vec<String> {
    let mut out = vec![];
    let total = h.to_string().len();
    if total > IDENTITY_MAX_BYTES { out.push(format!("the head is {total} bytes, over {IDENTITY_MAX_BYTES}")); }
    for k in IDENTITY_FIELDS {
        if let Some(n) = h[k].as_str().map(str::len).filter(|n| *n > IDENTITY_FIELD_MAX) { out.push(format!("{k} is {n} bytes, over {IDENTITY_FIELD_MAX}")); }
    }
    if let Some(n) = h["persona_text"].as_str().map(str::len).filter(|n| *n > IDENTITY_PERSONA_MAX) { out.push(format!("the persona is {n} bytes, over {IDENTITY_PERSONA_MAX}")); }
    if let Some(n) = h["crew"].as_array().map(Vec::len).filter(|n| *n > IDENTITY_CREW_MAX) { out.push(format!("{n} crew entries, over {IDENTITY_CREW_MAX}")); }
    if let Some(n) = h["principles"].as_array().map(Vec::len).filter(|n| *n > IDENTITY_PRINCIPLES_MAX) { out.push(format!("{n} principles, over {IDENTITY_PRINCIPLES_MAX}")); }
    out
}

/// A seat's identity head (docs/IDENTITY.md): the newest server-verified `identity` message in its own room, within
/// bounds, its `identity-head/1` cargo. The persona shown is the copy embedded in the head, and only when it matches the
/// head's hash; the source record is compared to it and a missing or changed source makes the copy STALE.
pub fn last_identity(door: &dyn Door, handle: &str) -> Option<Value> {
    let page = door.get(&format!("/search?channel={}&tag=identity&limit=20", enc(handle))).ok()?;
    let mut problems: Vec<String> = vec![];
    for hit in page["items"].as_array()?.iter().filter(|i| i["tags"].as_array().is_some_and(|t| t.iter().any(|t| t == "identity"))) {
        let id = hit["id"].as_str()?;
        let rec = door.get(&format!("/search?id={}", enc(id))).ok().unwrap_or(hit.clone());
        if !signed_by_seat(&rec, handle) {
            problems.push(format!("{id} is not a server-verified write by {handle} in #{handle}; ignored"));
            continue;
        }
        let cargo: Value = match &rec["cargo"] {
            Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
            v => v.clone(),
        };
        if cargo["schema"] != "identity-head/1" || cargo["seat"].as_str() != Some(handle) {
            problems.push(format!("{id} is not an identity-head/1 for {handle}; ignored"));
            continue;
        }
        let over = identity_bounds(&cargo);
        if !over.is_empty() {
            problems.push(format!("{id} is over its bounds ({}); refused, not cut", over.join("; ")));
            continue;
        }
        let mut stale = false;
        let embedded = cargo["persona_text"].as_str().unwrap_or("");
        let verified = !embedded.is_empty() && cargo["persona_sha256"].as_str() == Some(oracle_proto::sha_hex(embedded.as_bytes()).as_str());
        let persona = if !verified {
            problems.push("the embedded persona does not match its hash; no persona is shown".into());
            Value::Null
        } else {
            let pid = cargo["persona_ref"].as_str().unwrap_or("?");
            match door.get(&format!("/search?id={}", enc(pid))).ok().filter(|p| p["content"].is_string()) {
                None => { stale = true; problems.push(format!("the persona source {pid} could not be read; showing the copy verified when the head was written")); }
                Some(p) if !signed_by_seat(&p, handle) || !p["tags"].as_array().is_some_and(|t| t.iter().any(|t| t.as_str().is_some_and(|t| t.starts_with("persona:")))) => {
                    stale = true;
                    problems.push(format!("the persona source {pid} is not a server-verified persona:vN record of {handle}; showing the verified copy"));
                }
                Some(p) if p["content"].as_str() != Some(embedded) => { stale = true; problems.push(format!("the persona source {pid} has changed since the head was written; showing the verified copy, write a new head")); }
                Some(_) => {}
            }
            json!({"id": pid, "text": embedded, "stale": stale})
        };
        return Some(json!({"id": id, "at_ms": rec["created_ms"], "head": cargo, "persona": persona, "problems": problems}));
    }
    if problems.is_empty() { None } else { Some(json!({"head": Value::Null, "problems": problems})) }
}

/// The fold pages (Blaze, 1 Nov 2026: "the brief prints the fold pages whole after the head,
/// no schema change"): a seat's server-verified doc/1 records in its own room tagged `identity:fold`, oldest batch first,
/// printed whole after WHO I AM; at most FOLD_MAX_BYTES in all, the rest counted and named, never cut mid-page.
pub const FOLD_MAX_BYTES: usize = 65_536;
pub fn fold_pages(door: &dyn Door, handle: &str) -> Value {
    let Ok(page) = door.get(&format!("/search?channel={}&tag=identity%3Afold&limit=50", enc(handle))) else { return Value::Null; };
    let mut problems: Vec<String> = vec![];
    let mut pages: Vec<Value> = vec![];
    let mut replaced = std::collections::HashSet::<String>::new();
    for hit in page["items"].as_array().into_iter().flatten().filter(|i| i["tags"].as_array().is_some_and(|t| t.iter().any(|t| t == "identity:fold"))) {
        let id = hit["id"].as_str().unwrap_or("?").to_string();
        let rec = door.get(&format!("/search?id={}", enc(&id))).ok().unwrap_or(hit.clone());
        if !signed_by_seat(&rec, handle) {
            problems.push(format!("{id} is not a server-verified write by {handle} in #{handle}; ignored"));
            continue;
        }
        let cargo: Value = match &rec["cargo"] { Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null), v => v.clone() };
        let Some(md) = cargo["markdown"].as_str().filter(|_| cargo["schema"] == "doc/1") else {
            problems.push(format!("{id} is not a doc/1 page; ignored"));
            continue;
        };
        for t in rec["tags"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if let Some(old) = t.strip_prefix("supersedes:") { replaced.insert(old.to_string()); }
        }
        pages.push(json!({"id": id, "title": cargo["title"].as_str().unwrap_or(""), "at_ms": rec["created_ms"], "bytes": md.len(), "markdown": md}));
    }
    // A revised page (`ox say --supersedes <old>`, as WHO I AM v2.1 replaced v2 on 1 Nov 2026) replaces the one it names;
    // only a verified page of this seat can, so a forged record never hides a real one.
    pages.retain(|p| !replaced.contains(p["id"].as_str().unwrap_or("")));
    pages.sort_by_key(|p| p["at_ms"].as_i64().unwrap_or(0));
    let (mut kept, mut cut, mut used) = (Vec::<Value>::new(), Vec::<Value>::new(), 0usize);
    for p in pages {
        let n = p["bytes"].as_u64().unwrap_or(0) as usize;
        if used + n <= FOLD_MAX_BYTES { used += n; kept.push(p); } else { cut.push(json!({"id": p["id"], "title": p["title"], "bytes": n})); }
    }
    json!({"pages": kept, "bytes": used, "cut": cut, "problems": problems})
}

/// The FOLD block after WHO I AM: each page whole with its id and title; what was cut, named; the problems, named. No
/// fold records, no block.
pub fn fold_render(f: &Value) -> String {
    let mut out = String::new();
    let Some(pages) = f["pages"].as_array() else { return out; };
    let cut: Vec<&Value> = f["cut"].as_array().into_iter().flatten().collect();
    let problems: Vec<&str> = f["problems"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if pages.is_empty() && cut.is_empty() && problems.is_empty() { return out; }
    let kb = |b: u64| (b + 512) / 1024;
    out += &format!("FOLD       {} {}, {} KB: what my owner approved, folded into my head and cited by it (ox get <id> for one)\n",
        pages.len(), if pages.len() == 1 { "page" } else { "pages" }, kb(f["bytes"].as_u64().unwrap_or(0)));
    for p in problems { out += &format!("  ! {p}\n"); }
    for p in pages {
        out += &format!("  ── {} · {}\n", p["id"].as_str().unwrap_or("?"), p["title"].as_str().unwrap_or(""));
        for line in p["markdown"].as_str().unwrap_or("").lines() { out += "  "; out += line; out += "\n"; }
    }
    if !cut.is_empty() {
        out += &format!("  cut: {} more {} over the {} KB cap, read whole with ox get: {}\n", cut.len(), if cut.len() == 1 { "page" } else { "pages" }, FOLD_MAX_BYTES / 1024,
            cut.iter().map(|p| format!("{} ({} KB)", p["id"].as_str().unwrap_or("?"), kb(p["bytes"].as_u64().unwrap_or(0)))).collect::<Vec<_>>().join(", "));
    }
    out
}

/// The citations of a field, each labelled by who wrote the cited record (checked when the head was written). A
/// citation is a record, not a verification of the field's text: the field stays self-declared (Emmy).
fn sources_line(refs: &Value) -> Option<String> {
    let rs = refs.as_array().filter(|r| !r.is_empty())?;
    Some(rs.iter().map(|r| format!("{} ({})", r["id"].as_str().unwrap_or("?"), match r["by"].as_str() {
        Some("owner") => "a Blaze record".to_string(), Some("self") => "my record".to_string(), Some(o) => format!("by {o}"), None => "?".into() })).collect::<Vec<_>>().join(" · "))
}

/// The WHO I AM block: problems first, then every field whole with its sources, then the verified persona whole.
pub fn identity_render(i: &Value, now_ms: i64, handle: &str) -> String {
    identity_render_with(i, now_ms, handle, true)
}

/// An identity's problems, one line each, except the same refusal about several records, which is one line naming them all
/// (Blaze, 31 Oct 2026: three identical "is not a server-verified write" lines in his brief).
fn problem_lines(problems: &Value) -> String {
    let all: Vec<&str> = problems.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let mut groups: Vec<(String, Vec<&str>)> = vec![];
    let mut out = String::new();
    for p in &all {
        match p.split_once(" is not a server-verified write") {
            Some((id, rest)) => match groups.iter_mut().find(|(r, _)| r == rest) {
                Some((_, ids)) => ids.push(id),
                None => groups.push((rest.to_string(), vec![id])),
            },
            None => out += &format!("  ! {p}\n"),
        }
    }
    for (rest, ids) in groups {
        out += &if ids.len() == 1 { format!("  ! {} is not a server-verified write{rest}\n", ids[0]) } else { format!("  ! {} records are not server-verified writes{rest}: {}\n", ids.len(), ids.join(", ")) };
    }
    out
}

/// The same block; `whole_persona` false prints the persona as one line (id, state, size) instead of its text.
pub fn identity_render_with(i: &Value, now_ms: i64, handle: &str, whole_persona: bool) -> String {
    let Some(h) = i["head"].as_object() else {
        let mut out = format!("WHO I AM   no identity head for {handle} (docs/IDENTITY.md): ox identity --write head.json\n");
        out += &problem_lines(&i["problems"]);
        return out;
    };
    let at = i["at_ms"].as_i64().unwrap_or(now_ms);
    let mut out = format!("WHO I AM   {} · identity {} · written {}\n", h.get("name").and_then(Value::as_str).unwrap_or(handle), i["id"].as_str().unwrap_or("?"), ago(now_ms, at));
    out += &problem_lines(&i["problems"]);
    let refs = h.get("refs").cloned().unwrap_or(Value::Null);
    for k in IDENTITY_FIELDS {
        if let Some(v) = h.get(k).and_then(Value::as_str).filter(|v| !v.trim().is_empty()) {
            out += &format!("  {:<10}{}\n", k, v.trim());
            // Descriptive only: never an authorization input. Self-declared until the owner signs a policy for the seat.
            out += &format!("            self-declared{}\n", sources_line(&refs[k]).map(|l| format!(" · citations: {l}")).unwrap_or_else(|| " · no citations".into()));
        }
    }
    if let Some(ps) = h.get("principles").and_then(Value::as_array).filter(|a| !a.is_empty()) {
        out += "  principles\n";
        for p in ps {
            out += &format!("    - {}\n", p.as_str().unwrap_or("").trim());
        }
    }
    if let Some(cs) = h.get("crew").and_then(Value::as_array).filter(|a| !a.is_empty()) {
        let (verified, others): (Vec<&Value>, Vec<&Value>) = cs.iter().partition(|c| c["source"]["by"] == "state");
        if !verified.is_empty() {
            out += "  crew (each cites its own server-verified State)\n";
            for c in verified {
                out += &format!("    {:<13}{} [{}]\n", c["handle"].as_str().unwrap_or("?"), c["who"].as_str().unwrap_or("").trim(), c["source"]["id"].as_str().unwrap_or("?"));
            }
        }
        if !others.is_empty() {
            out += "  associates (self-reported by me, unverified)\n";
            for c in others {
                out += &format!("    {:<13}{}\n", c["handle"].as_str().unwrap_or("?"), c["who"].as_str().unwrap_or("").trim());
            }
        }
    }
    if let Some(t) = i["persona"]["text"].as_str() {
        let state = if i["persona"]["stale"] == true { "STALE copy" } else { "verified" };
        if whole_persona {
            out += &format!("  persona   {} (whole, {state})\n{}\n", i["persona"]["id"].as_str().unwrap_or("?"), t.trim_end());
        } else {
            out += &format!("  persona   {} ({state}, {} chars; whole: ox identity, or ox brief --full)\n", i["persona"]["id"].as_str().unwrap_or("?"), t.trim_end().chars().count());
        }
    }
    out
}

/// The first `keep` lines of a text, then one line saying how many more there are (the brief folds long notes).
pub fn fold_lines(text: &str, keep: usize, whole_hint: &str) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    if lines.len() <= keep { return lines.join("\n"); }
    format!("{}\n  … {} more lines; whole: {whole_hint}", lines[..keep].join("\n"), lines.len() - keep)
}

/// The Bridge contract (docs/BRIDGE-CONTRACT.md): why a message to the owner cannot go out (empty when it can).
pub const TELL_HEAD_WORDS: usize = 40;
pub const TELL_OPTION_CHARS: usize = 80;
/// The longest --why: one sentence, read aloud after the options.
pub const TELL_WHY_CHARS: usize = 240;

/// What a decide says in its own words after the head, so voice and every reader get the choice without parsing cargo:
/// the options numbered, the recommended one marked, and why.
pub fn tell_choice_text(options: &[String], recommend: usize, why: &str) -> String {
    let list = options.iter().enumerate()
        .map(|(i, o)| format!("{}) {o}{}", i + 1, if i + 1 == recommend { " (recommended)" } else { "" }))
        .collect::<Vec<_>>().join("\n");
    format!("Options:\n{list}\nI recommend {recommend}: {}", why.trim())
}

/// Puts the asker's pick into a decide's cargo (`recommendation: {option, label, why}`) and returns the words that say the
/// same in the text, for voice and for readers that ignore cargo. Refuses a pick outside the options or a missing why.
/// The words for a decide whose cargo already carries `options` and `recommendation`: the same text add_recommendation writes,
/// so voice and readers that ignore cargo always hear the pick the Bridge marks (Emmy's review of e1428bb).
pub fn recommendation_words(cargo: &Value) -> Option<String> {
    let options: Vec<String> = cargo["options"].as_array()?.iter().filter_map(|o| o.as_str().map(str::to_owned)).collect();
    let n = cargo["recommendation"]["option"].as_u64()? as usize;
    let why = cargo["recommendation"]["why"].as_str()?;
    (n >= 1 && n <= options.len()).then(|| tell_choice_text(&options, n, why))
}

/// Appends a decide's canonical choice words unless that exact block is already in the text.
pub fn with_choice_words(text: &str, words: &str) -> String {
    if text.contains(words) { text.to_owned() } else { format!("{text}\n{words}") }
}

pub fn add_recommendation(cargo: &mut Value, recommend: usize, why: &str) -> anyhow::Result<String> {
    let options: Vec<String> = cargo["options"].as_array().map(|a| a.iter().filter_map(|o| o.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let why = why.trim();
    if recommend == 0 || recommend > options.len() { anyhow::bail!("recommend {recommend}: pick one of the {} options, numbered from 1", options.len()); }
    if why.is_empty() || why.chars().count() > TELL_WHY_CHARS { anyhow::bail!("why: one plain sentence, 1 to {TELL_WHY_CHARS} characters"); }
    let obj = cargo.as_object_mut().ok_or_else(|| anyhow::anyhow!("a decide needs a JSON object cargo"))?;
    obj.insert("recommendation".into(), json!({"option": recommend, "label": options[recommend - 1], "why": why}));
    Ok(tell_choice_text(&options, recommend, why))
}

pub fn tell_problems(head: &str, ask: &str, options: &[String], by: Option<&str>, recommend: Option<usize>, why: Option<&str>) -> Vec<String> {
    let mut out = vec![];
    let why = why.map(str::trim).filter(|w| !w.is_empty());
    let words = head.split_whitespace().count();
    if words == 0 { out.push("the head is empty".into()); }
    if words > TELL_HEAD_WORDS { out.push(format!("the head is {words} words, over {TELL_HEAD_WORDS}: put the detail in --detail or the room")); }
    match ask {
        "decide" => {
            if !(2..=6).contains(&options.len()) { out.push(format!("a decide needs 2 to 6 --option, got {}", options.len())); }
            for o in options.iter().filter(|o| o.chars().count() > TELL_OPTION_CHARS) { out.push(format!("option over {TELL_OPTION_CHARS} characters: {o:?}")); }
            if by.is_none_or(|b| b.trim().is_empty()) { out.push("a decide needs --by (when it is due)".into()); }
            // Blaze, 23 Oct: "you are saying decision in bridge but I get no recommendations and options". A decide
            // without the seat's own pick hands him the thinking; it names one option and says why.
            match recommend {
                None => out.push("a decide needs --recommend <n>: the option you would pick, numbered from 1".into()),
                Some(n) if n == 0 || n > options.len() => out.push(format!("--recommend {n}: pick one of the {} options, numbered from 1", options.len())),
                Some(_) => {}
            }
            match why {
                None => out.push("a decide needs --why: one plain sentence on why you recommend that option".into()),
                Some(w) if w.chars().count() > TELL_WHY_CHARS => out.push(format!("--why is over {TELL_WHY_CHARS} characters")),
                Some(_) => {}
            }
        }
        "read" | "none" => {
            if !options.is_empty() { out.push("--option belongs to --ask decide".into()); }
            if recommend.is_some() || why.is_some() { out.push("--recommend and --why belong to --ask decide".into()); }
        }
        other => out.push(format!("--ask {other:?}: use decide, read or none")),
    }
    out
}

/// The gap (docs/GAP.md): who a decide affects, who was not at the table and why, what is owed to them.
pub const GAP_ENTRIES_MAX: usize = 12;
pub const GAP_TEXT_CHARS: usize = 240;

/// The cargo `gap` of a decide from its flags, or why it cannot go out. Any other ask carries no gap and takes no flags.
pub fn tell_gap(ask: &str, affects: &[String], absent: &[String], owed: Option<&str>) -> Result<Option<Value>, Vec<String>> {
    let owed = owed.map(str::trim).filter(|o| !o.is_empty());
    if ask != "decide" {
        return if affects.is_empty() && absent.is_empty() && owed.is_none() { Ok(None) } else { Err(vec!["--affects, --absent and --owed belong to --ask decide".into()]) };
    }
    let mut out: Vec<String> = vec![];
    if affects.is_empty() || absent.is_empty() {
        out.push("who is not at this table? a decide needs --affects \"<who>: <how>\" and --absent \"<who>: <why>\" or --absent none (docs/GAP.md)".into());
    }
    if owed.is_none() { out.push("a decide needs --owed: what it owes those it affects who had no voice (docs/GAP.md)".into()); }
    for (flag, n) in [("affects", affects.len()), ("absent", absent.len())] {
        if n > GAP_ENTRIES_MAX { out.push(format!("{n} --{flag}, over {GAP_ENTRIES_MAX}")); }
    }
    for t in affects.iter().chain(absent).map(String::as_str).chain(owed).filter(|t| t.chars().count() > GAP_TEXT_CHARS) {
        out.push(format!("over {GAP_TEXT_CHARS} characters: {t:?}"));
    }
    let pair = |flag: &str, second: &str, s: &str, out: &mut Vec<String>| match s.split_once(':').map(|(a, b)| (a.trim(), b.trim())) {
        Some((who, what)) if !who.is_empty() && !what.is_empty() => Some((who.to_owned(), what.to_owned())),
        _ => { out.push(format!("--{flag} {s:?}: write it as \"<who>: <{second}>\"")); None }
    };
    let mut aff = vec![];
    for a in affects {
        // `@OBS-…` at the end cites where that party spoke.
        let (text, asked) = match a.trim().rsplit_once(' ') {
            Some((t, id)) if id.starts_with("@OBS-") => (t, Some(id[1..].to_owned())),
            _ => (a.trim(), None),
        };
        if let Some((who, how)) = pair("affects", "how", text, &mut out) { aff.push(json!({"who": who, "how": how, "asked": asked})); }
    }
    let mut abs = vec![];
    if absent.len() == 1 && absent[0].trim() == "none" {
        for a in aff.iter().filter(|a| a["asked"].is_null()) {
            out.push(format!("--absent none says everyone affected was asked: cite where {} spoke with @OBS-… on its --affects", a["who"].as_str().unwrap_or("?")));
        }
    } else {
        for a in absent {
            if a.trim() == "none" { out.push("--absent none stands alone: it says nobody is missing".into()); continue; }
            if let Some((who, why)) = pair("absent", "why", a, &mut out) { abs.push(json!({"who": who, "why": why})); }
        }
    }
    if out.is_empty() { Ok(Some(json!({"affects": aff, "absent": abs, "owed": owed}))) } else { Err(out) }
}

/// The gap as lines under an ask (docs/GAP.md); empty when the ask carries none.
pub fn gap_render(g: &Value, indent: &str) -> String {
    if !g.is_object() { return String::new(); }
    let list = |k: &str, why: &str| g[k].as_array().map(|a| a.iter().map(|e| {
        let cite = e["asked"].as_str().map(|i| format!(", asked {i}")).unwrap_or_default();
        format!("{} ({}{cite})", e["who"].as_str().unwrap_or("?"), e[why].as_str().unwrap_or(""))
    }).collect::<Vec<_>>().join(" · ")).unwrap_or_default();
    let absent = match list("absent", "why") { s if s.is_empty() => "none: everyone affected was asked".to_owned(), s => s };
    format!("{indent}affects {}\n{indent}absent  {absent}\n{indent}owed    {}\n", list("affects", "how"), g["owed"].as_str().unwrap_or("?"))
}

/// Bounds of a procedure (docs/PROCEDURES.md).
pub const PROCEDURE_MAX_BYTES: usize = 32_768;
pub const PROCEDURE_STEPS_MAX: usize = 40;

/// Why a procedure cannot be written or shown (empty when it can).
pub fn procedure_problems(p: &Value) -> Vec<String> {
    let mut out = vec![];
    let name = p["name"].as_str().unwrap_or("");
    if name.is_empty() || !name.split('-').all(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())) {
        out.push(format!("name {name:?} must be lower-case words joined by -"));
    }
    for k in ["title", "when"] {
        if p[k].as_str().is_none_or(|v| v.trim().is_empty()) { out.push(format!("{k} is required")); }
    }
    match p["steps"].as_array() {
        Some(s) if !s.is_empty() && s.iter().all(|x| x["do"].as_str().is_some_and(|d| !d.trim().is_empty())) => {
            if s.len() > PROCEDURE_STEPS_MAX { out.push(format!("{} steps, over {PROCEDURE_STEPS_MAX}", s.len())); }
        }
        _ => out.push("steps: at least one, each with a `do`".into()),
    }
    let n = p.to_string().len();
    if n > PROCEDURE_MAX_BYTES { out.push(format!("{n} bytes, over {PROCEDURE_MAX_BYTES}")); }
    out
}

fn procedure_cargo(rec: &Value) -> Value {
    match &rec["cargo"] { Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null), v => v.clone() }
}

/// Every procedure on record, newest version of each name first: name, version, author, when, title, id. Only
/// server-verified (envelope 2) records written in their author's own room count.
pub fn procedure_list(door: &dyn Door) -> Result<Vec<Value>> {
    let page = door.get("/search?tag=procedure&limit=200")?;
    let mut out: Vec<Value> = vec![];
    for r in page["items"].as_array().into_iter().flatten() {
        let author = r["author"].as_str().unwrap_or("");
        if r["envelope"].as_i64() != Some(2) || r["channel"].as_str() != Some(author) { continue; }
        let c = procedure_cargo(r);
        let Some(name) = c["name"].as_str().filter(|_| c["schema"] == "procedure/1") else { continue };
        if out.iter().any(|o| o["name"] == name) { continue; }
        out.push(json!({"name": name, "version": c["version"], "author": author, "created_ms": r["created_ms"], "title": c["title"], "id": r["id"]}));
    }
    Ok(out)
}

/// The newest server-verified version of one procedure, whole, with its record.
pub fn procedure_get(door: &dyn Door, name: &str) -> Result<Option<Value>> {
    let page = door.get(&format!("/search?tag={}&limit=50", enc(&format!("procedure:{name}"))))?;
    for hit in page["items"].as_array().into_iter().flatten() {
        let id = hit["id"].as_str().unwrap_or("");
        let r = door.get(&format!("/search?id={}", enc(id))).ok().filter(|r| r["cargo"].is_string() || r["cargo"].is_object()).unwrap_or(hit.clone());
        let author = r["author"].as_str().unwrap_or("");
        if r["envelope"].as_i64() != Some(2) || r["channel"].as_str() != Some(author) { continue; }
        let c = procedure_cargo(&r);
        if c["schema"] != "procedure/1" || c["name"].as_str() != Some(name) || !procedure_problems(&c).is_empty() { continue; }
        return Ok(Some(json!({"id": id, "author": author, "created_ms": r["created_ms"], "procedure": c})));
    }
    Ok(None)
}

/// A procedure as text: head, when, every step with its command and check, verify, rollback, refs. Whole.
pub fn procedure_render(p: &Value, now_ms: i64) -> String {
    let c = &p["procedure"];
    let mut out = format!("PROCEDURE  {} v{} · by {} · written {} · {}\n  {}\n", c["name"].as_str().unwrap_or("?"), c["version"].as_i64().unwrap_or(1),
        p["author"].as_str().unwrap_or("?"), ago(now_ms, p["created_ms"].as_i64().unwrap_or(now_ms)), p["id"].as_str().unwrap_or("?"), c["title"].as_str().unwrap_or(""));
    out += &format!("  when     {}\n  steps\n", c["when"].as_str().unwrap_or(""));
    for (n, s) in c["steps"].as_array().into_iter().flatten().enumerate() {
        out += &format!("    {}. {}\n", n + 1, s["do"].as_str().unwrap_or("").trim());
        if let Some(cmd) = s["cmd"].as_str().filter(|c| !c.trim().is_empty()) {
            for line in cmd.trim_end().lines() { out += &format!("       $ {line}\n"); }
        }
        if let Some(ch) = s["check"].as_str().filter(|c| !c.trim().is_empty()) { out += &format!("       check: {}\n", ch.trim()); }
    }
    for k in ["verify", "rollback"] {
        if let Some(v) = c[k].as_str().filter(|v| !v.trim().is_empty()) { out += &format!("  {k:<9}{}\n", v.trim()); }
    }
    if let Some(r) = c["refs"].as_array().filter(|r| !r.is_empty()) {
        out += &format!("  refs     {}\n", r.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" · "));
    }
    out
}

/// How many LAW lines a brief shows; `--full` shows every one (3 Nov 2026: 292 approved readings are ~11k tokens whole).
pub const LAW_BRIEF_TOP: usize = 25;

/// The kinds a canon rule can be (docs/CANON.md), in the order `ox canon` prints them.
pub const CANON_KINDS: [&str; 7] = ["law", "authority", "safety", "bridge", "work", "craft", "domain"];
/// The kinds the brief prints whole: what a woken seat must never get wrong.
pub const CANON_BRIEF_KINDS: [&str; 3] = ["law", "authority", "safety"];
/// Bounds of a canon (docs/CANON.md).
pub const CANON_MAX_RULES: usize = 300;
pub const CANON_MAX_BYTES: usize = 98_304;
pub const CANON_RULE_TEXT_MAX: usize = 1_000;

fn kebab_ok(s: &str) -> bool {
    !s.is_empty() && s.split('-').all(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
}

/// Why a canon cannot be written or shown (empty when it can).
pub fn canon_problems(c: &Value) -> Vec<String> {
    let mut out = vec![];
    let ids_ok = |v: &Value| v.as_array().is_some_and(|a| a.iter().all(|x| x.as_str().is_some_and(|s| s.starts_with("OBS-"))));
    match c["rules"].as_array() {
        Some(rules) if !rules.is_empty() => {
            if rules.len() > CANON_MAX_RULES { out.push(format!("{} rules, over {CANON_MAX_RULES}", rules.len())); }
            let mut seen: Vec<&str> = vec![];
            for (n, r) in rules.iter().enumerate() {
                let id = r["id"].as_str().unwrap_or("");
                let at = if id.is_empty() { format!("rule {}", n + 1) } else { format!("rule {id}") };
                if !kebab_ok(id) { out.push(format!("{at}: id must be lower-case words joined by -")); }
                if seen.contains(&id) { out.push(format!("{at}: id used twice")); }
                seen.push(id);
                let kind = r["kind"].as_str().unwrap_or("");
                if !CANON_KINDS.contains(&kind) { out.push(format!("{at}: kind {kind:?} is not one of {}", CANON_KINDS.join(", "))); }
                if kind == "domain" && r["scope"].as_str().is_none_or(|s| s.trim().is_empty()) { out.push(format!("{at}: a domain rule names its scope")); }
                match r["text"].as_str().map(str::trim) {
                    Some(t) if !t.is_empty() => if t.len() > CANON_RULE_TEXT_MAX { out.push(format!("{at}: text is {} bytes, over {CANON_RULE_TEXT_MAX}", t.len())); },
                    _ => out.push(format!("{at}: text is required")),
                }
                if !(ids_ok(&r["sources"]) && r["sources"].as_array().is_some_and(|a| !a.is_empty())) { out.push(format!("{at}: sources must be at least one OBS- id")); }
                if !r["supersedes"].is_null() && !ids_ok(&r["supersedes"]) { out.push(format!("{at}: supersedes must be OBS- ids")); }
            }
            for (n, o) in c["open"].as_array().into_iter().flatten().enumerate() {
                if o["question"].as_str().is_none_or(|q| q.trim().is_empty()) { out.push(format!("open {}: question is required", n + 1)); }
            }
        }
        _ => out.push("rules: at least one".into()),
    }
    let n = c.to_string().len();
    if n > CANON_MAX_BYTES { out.push(format!("{n} bytes, over {CANON_MAX_BYTES}")); }
    out
}

/// Every record id a canon cites (sources, supersedes, open asks), each once, in order.
pub fn canon_cited_ids(c: &Value) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut add = |v: &Value| for x in v.as_array().into_iter().flatten().filter_map(Value::as_str) {
        if !out.iter().any(|o| o == x) { out.push(x.to_string()); }
    };
    for r in c["rules"].as_array().into_iter().flatten() { add(&r["sources"]); add(&r["supersedes"]); }
    for o in c["open"].as_array().into_iter().flatten() { if o["ask"].is_string() { add(&json!([o["ask"]])); } }
    out
}

/// A seat's canon (docs/CANON.md): the newest server-verified `canon/1` it wrote in its own room, as
/// {id, created_ms, canon}; records that are not (an import, another room, malformed) are listed under `problems` and
/// skipped. None when the seat never wrote one.
pub fn last_canon(door: &dyn Door, handle: &str) -> Option<Value> {
    let page = door.get(&format!("/search?channel={}&tag=canon&limit=20", enc(handle))).ok()?;
    let mut problems: Vec<String> = vec![];
    for hit in page["items"].as_array()?.iter().filter(|i| i["tags"].as_array().is_some_and(|t| t.iter().any(|t| t == "canon"))) {
        let id = hit["id"].as_str()?;
        let rec = door.get(&format!("/search?id={}", enc(id))).ok().unwrap_or(hit.clone());
        if !signed_by_seat(&rec, handle) { problems.push(format!("{id} is not a server-verified write by {handle} in #{handle}; ignored")); continue; }
        let c = procedure_cargo(&rec);
        if c["schema"] != "canon/1" { problems.push(format!("{id} is not a canon/1; ignored")); continue; }
        let p = canon_problems(&c);
        if !p.is_empty() { problems.push(format!("{id} refused ({})", p.join("; "))); continue; }
        return Some(json!({"id": id, "author": handle, "created_ms": rec["created_ms"].as_i64().or(hit["created_ms"].as_i64()), "canon": c, "problems": problems}));
    }
    (!problems.is_empty()).then(|| json!({"problems": problems}))
}

/// Rule counts by kind, in CANON_KINDS order: "law 6 · authority 9 · …".
fn canon_counts(c: &Value) -> String {
    CANON_KINDS.iter().filter_map(|k| {
        let n = c["rules"].as_array().map_or(0, |r| r.iter().filter(|x| x["kind"] == *k).count());
        (n > 0).then(|| format!("{k} {n}"))
    }).collect::<Vec<_>>().join(" · ")
}

/// A canon as text, grouped by kind (or one kind), every rule whole with its sources; then what is still open.
pub fn canon_render(p: &Value, kind: Option<&str>, now_ms: i64) -> String {
    let c = &p["canon"];
    let rules = c["rules"].as_array().cloned().unwrap_or_default();
    let mut out = format!("CANON  v{} · by {} · written {} · {} · {} rules · {}\n", c["version"].as_i64().unwrap_or(1), p["author"].as_str().unwrap_or("?"),
        ago(now_ms, p["created_ms"].as_i64().unwrap_or(now_ms)), p["id"].as_str().unwrap_or("?"), rules.len(), canon_counts(c));
    for k in CANON_KINDS.iter().filter(|k| kind.is_none_or(|w| w == **k)) {
        let of: Vec<&Value> = rules.iter().filter(|r| r["kind"] == *k).collect();
        if of.is_empty() { continue; }
        out += &format!("{}\n", k.to_uppercase());
        for r in of {
            let scope = r["scope"].as_str().map(|s| format!(" [{s}]")).unwrap_or_default();
            out += &format!("  {}{scope}: {}\n", r["id"].as_str().unwrap_or("?"), r["text"].as_str().unwrap_or("").trim());
            let ids = |v: &Value| v.as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(" · ");
            let mut src = format!("      sources {}", ids(&r["sources"]));
            if r["supersedes"].as_array().is_some_and(|a| !a.is_empty()) { src += &format!(" · supersedes {}", ids(&r["supersedes"])); }
            out += &format!("{src}\n");
        }
    }
    if kind.is_none() {
        for o in c["open"].as_array().into_iter().flatten() {
            out += &format!("OPEN  {}{}\n", o["question"].as_str().unwrap_or("").trim(), o["ask"].as_str().map(|a| format!(" · asked {a}")).unwrap_or_default());
        }
    }
    out
}

/// How many lines of the RESUME note the brief prints before folding it.
pub const RESUME_BRIEF_LINES: usize = 12;

/// The brief's CANON block: one line (version, counts, open); with `full`, the law, authority and safety rules whole.
fn canon_brief(v: &Value, now_ms: i64, full: bool) -> String {
    if v.is_null() { return "CANON      none on record yet (docs/CANON.md): ox canon --write file.json\n".into(); }
    let Some(c) = v.get("canon").filter(|c| c.is_object()) else {
        return format!("CANON      could not be read: {}\n", v["problems"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join("; "));
    };
    let open = c["open"].as_array().map_or(0, |o| o.len());
    let mut out = format!("CANON      v{} · written {} · {} · {} rules · {}{} · whole: ox canon{}\n", c["version"].as_i64().unwrap_or(1),
        ago(now_ms, v["created_ms"].as_i64().unwrap_or(now_ms)), v["id"].as_str().unwrap_or("?"), c["rules"].as_array().map_or(0, |r| r.len()), canon_counts(c),
        if open > 0 { format!(" · {open} open") } else { String::new() }, if full { "" } else { " · law, authority, safety: ox canon --kind law, or ox brief --full" });
    for r in c["rules"].as_array().into_iter().flatten().filter(|r| full && CANON_BRIEF_KINDS.iter().any(|k| r["kind"] == *k)) {
        out += &format!("  {:<10} {} [{}]\n", r["kind"].as_str().unwrap_or(""), r["text"].as_str().unwrap_or("").trim(), r["id"].as_str().unwrap_or(""));
    }
    for o in c["open"].as_array().into_iter().flatten() {
        out += &format!("  {:<10} {}{}\n", "open", o["question"].as_str().unwrap_or("").trim(), o["ask"].as_str().map(|a| format!(" · asked {a}")).unwrap_or_default());
    }
    out
}

/// The Now head's fields (docs/NOW.md), in the order the brief prints them.
pub const NOW_FIELDS: [&str; 10] = ["task", "why", "room", "repo", "checkout", "branch", "head", "last", "next", "waiting"];
/// A Now head older than this is printed as stale.
pub const NOW_STALE_MS: i64 = 2 * 3_600_000;

/// A seat's Now head (docs/NOW.md): the newest message it wrote in its own room tagged `now`, with its `now/1` cargo.
/// Read newest first by tag, never ranked; the cargo from the record itself. None when it never wrote one.
pub fn last_now(door: &dyn Door, handle: &str) -> Option<Value> {
    let page = door.get(&format!("/search?channel={}&tag=now&limit=20", enc(handle))).ok()?;
    let hit = page["items"].as_array()?.iter()
        .find(|i| i["author"].as_str() == Some(handle) && i["tags"].as_array().is_some_and(|t| t.iter().any(|t| t == "now")))?.clone();
    let id = hit["id"].as_str()?;
    let rec = door.get(&format!("/search?id={}", enc(id))).ok().unwrap_or(hit.clone());
    let cargo = match &rec["cargo"] {
        Value::String(s) => serde_json::from_str(s).ok()?,
        v if v.is_object() => v.clone(),
        _ => return None,
    };
    (cargo["schema"] == "now/1").then(|| json!({"id": id, "at_ms": rec["created_ms"].as_i64().or(hit["created_ms"].as_i64()), "now": cargo}))
}

/// A new Now head from the previous one: `sets` are `field=text`, `clears` are field names; every other field is kept.
/// A field outside NOW_FIELDS, a set without `=` or an empty result is refused.
pub fn now_merge(prev: Option<&Value>, sets: &[String], clears: &[String], at_ms: i64) -> Result<Value> {
    let mut n = prev.cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
    for s in sets {
        let (k, v) = s.split_once('=').ok_or_else(|| anyhow::anyhow!("--set wants field=text, got {s:?}"))?;
        let k = k.trim();
        if !NOW_FIELDS.contains(&k) {
            anyhow::bail!("{k:?} is not a Now field; the fields are {}", NOW_FIELDS.join(", "));
        }
        n[k] = json!(v.trim());
    }
    for k in clears {
        if !NOW_FIELDS.contains(&k.as_str()) {
            anyhow::bail!("{k:?} is not a Now field; the fields are {}", NOW_FIELDS.join(", "));
        }
        n.as_object_mut().map(|o| o.remove(k.as_str()));
    }
    if !NOW_FIELDS.iter().any(|k| n[*k].as_str().is_some_and(|v| !v.is_empty())) {
        anyhow::bail!("a Now head needs at least one field; start with --set task=\"…\" --set next=\"…\"");
    }
    n["schema"] = json!("now/1");
    n["at"] = json!(at_ms);
    Ok(n)
}

/// The NOW block of the brief and of `ox now`: the head's age, then each field it has, whole.
pub fn now_render(n: &Value, now_ms: i64, handle: &str) -> String {
    let Some(head) = n["now"].as_object() else {
        return "NOW        none written. Write it first and at every step: ox now --set task=\"…\" --set next=\"…\" (docs/NOW.md)\n".to_string();
    };
    let at = n["at_ms"].as_i64().unwrap_or(now_ms);
    let stale = now_ms - at > NOW_STALE_MS;
    let mut out = format!("NOW        {}{} · written {} · {}\n", if stale { "STALE · " } else { "" }, handle, ago(now_ms, at), n["id"].as_str().unwrap_or("?"));
    for k in NOW_FIELDS {
        if let Some(v) = head.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()) {
            out += &format!("  {k:<9}{v}\n");
        }
    }
    if stale {
        out += "  update it before anything else: ox now --set last=\"…\" --set next=\"…\"\n";
    }
    out
}

/// A moment as an agent reads it next to the Oracle clock: minutes or hours ago within two days, else the UTC date.
pub fn ago(now_ms: i64, at_ms: i64) -> String {
    let m = (now_ms - at_ms).max(0) / 60_000;
    if m < 120 {
        format!("{m}m ago")
    } else if m < 48 * 60 {
        format!("{}h ago", m / 60)
    } else {
        oracle_proto::iso_ms(at_ms)[..10].to_string()
    }
}

/// The brief as one typed object: what the MCP tool returns as `structuredContent` (what Claude Code hands the model),
/// what a UI renders, and what `brief_render` prints. Every time in it is relative to `now`, the Oracle's clock.
/// `full` lists every open head and card; otherwise they are counted (Blaze, 2026-10-01: every line of a brief is paid
/// for again after each compaction).
pub fn brief_value(door: &dyn Door, full: bool) -> Result<Value> {
    let who = door.get("/whoami")?;
    let handle = who["handle"].as_str().unwrap_or("?").to_string();
    let now = who["server_ms"].as_i64().unwrap_or_else(now_ms);
    let state = door.state(&handle).unwrap_or(Value::Null);
    // The maps of the rooms my State names (docs/ROOM-MAP.md), one read; an unreadable /maps is shown, never silently nothing.
    let maps = match state["fields"]["rooms"].as_str().map(rooms_named).filter(|r| !r.is_empty()) {
        Some(rooms) => match door.get(&format!("/maps?rooms={}", enc(&rooms.join(",")))) {
            Ok(v) => json!(v["rooms"].as_array().into_iter().flatten().map(|m| json!({"room": m["room"], "line": m["line"], "written": !m["written"].is_null()})).collect::<Vec<_>>()),
            Err(e) => json!({"error": e.to_string()}),
        },
        None => Value::Null,
    };
    let now_head = last_now(door, &handle).unwrap_or(Value::Null);
    let identity_head = last_identity(door, &handle).unwrap_or(Value::Null);
    // The fold pages, whole, only when there is a head to fold into (docs/IDENTITY.md "The fold").
    let identity_fold = if identity_head["head"].is_object() { fold_pages(door, &handle) } else { Value::Null };
    // The Bridge cadence (docs/BRIDGE-CONTRACT.md): this seat's newest message addressed to the owner.
    let bridge = if handle == "blaze" { Value::Null } else {
        door.get("/search?tag=to%3Ablaze&limit=50").ok()
            .and_then(|p| p["items"].as_array().and_then(|a| a.iter().find(|i| i["author"].as_str() == Some(handle.as_str())).cloned()))
            .map(|i| json!({"id": i["id"], "at_ms": i["created_ms"]})).unwrap_or(json!({"none": true}))
    };
    // The procedures on record, named (docs/PROCEDURES.md); an unreadable list is said, never silently none.
    let procedures = match procedure_list(door) {
        Ok(l) => json!(l.iter().map(|p| json!({"name": p["name"], "version": p["version"], "author": p["author"]})).collect::<Vec<_>>()),
        Err(e) => json!({"error": e.to_string()}),
    };
    // The rule canon (docs/CANON.md): this seat's newest verified canon/1.
    let canon = last_canon(door, &handle).unwrap_or(Value::Null);
    let resume = last_resume(door, &handle).map(|(r, whole)| json!({"id": r["id"], "written": ago(now, r["created_ms"].as_i64().unwrap_or(now)),
        "whole": whole, "text": r["content"].as_str().unwrap_or("").trim_end()}));
    // What is asked of this seat: the server's derived list (open and answered-but-not-done), never a page of raw messages.
    // An unreadable /asks is an error the brief shows, never zero asks.
    let (asks, asks_error) = match door.get("/asks?state=open") {
        Ok(v) => (v, Value::Null),
        Err(e) => (Value::Null, json!(e.to_string())),
    };
    // The same rules as the inbox (Jobs' audit, 2026-10-10: the brief said 31 where the inbox said 1): asks from
    // before the inbox began are counted, not listed; an ask another seat answered is listed apart, not as open.
    let inbox_asks = asks_value(&asks, false);
    let rows: Vec<Value> = inbox_asks["open"].as_array().cloned().unwrap_or_default().iter().map(|r| json!({
        "id": r["id"], "room": r["room"], "from": r["author"], "ask": r["ask"], "state": r["state"],
        "age": ago(now, r["created_ms"].as_i64().unwrap_or(now)), "overdue": r["overdue"] == true, "card": r["card"], "gap": r["gap"]})).collect();
    let fold = door.get("/fold?limit=1").unwrap_or(Value::Null);
    let elsewhere: serde_json::Map<String, Value> = fold["pending"].as_object().cloned().unwrap_or_default();
    let total: i64 = elsewhere.values().filter_map(Value::as_i64).sum();
    // A seat scoped to its rooms may not read the heads (the seat-scope guard refuses GET /states); its brief is still a
    // brief: the heads are said to be closed to it, never the whole brief refused (Dot, 4 Nov 2026: `ox brief` was a 403).
    let (heads, heads_error): (Vec<Value>, Value) = match door.get(&format!("/states?owner={}", enc(&handle))) {
        Ok(v) => (v["heads"].as_array().cloned().unwrap_or_default().into_iter().filter(|h| h["state"].as_str() != Some("done")).collect(), Value::Null),
        Err(e) => (vec![], json!(format!("{e:#}"))),
    };
    let late = heads.iter().filter(|h| overdue(h["next_touch"].as_str().unwrap_or("—"), now).is_some()).count();
    let cards = door.get(&format!("/cards?owner={}&status=open", enc(&handle))).ok().filter(|c| c["cards"].is_array());
    let mut by: Vec<(String, usize)> = vec![];
    for st in cards.iter().flat_map(|c| c["cards"].as_array().cloned().unwrap_or_default()).map(|c| c["status"].as_str().unwrap_or("?").to_string()) {
        match by.iter_mut().find(|(s, _)| *s == st) { Some(e) => e.1 += 1, None => by.push((st, 1)) }
    }
    let order = |s: &str| ["in_progress", "blocked", "review", "queue"].iter().position(|o| *o == s).unwrap_or(9);
    by.sort_by_key(|(s, _)| order(s));
    let waiting = door.get(&format!("/cards?waiting_on={}", enc(&handle))).ok().filter(|c| c["cards"].is_array());
    let mut work = json!({"heads": heads.len(), "overdue": late, "cards": by.iter().map(|(s, n)| json!([s, n])).collect::<Vec<_>>()});
    if !heads_error.is_null() {
        work["error"] = heads_error;
    }
    if full {
        work["head_list"] = json!(heads.iter().map(|h| json!({"subject": h["subject"], "state": h["state"], "next": h["next_touch"],
            "overdue": overdue(h["next_touch"].as_str().unwrap_or("—"), now)})).collect::<Vec<_>>());
        work["card_list"] = cards.clone().unwrap_or(Value::Null);
    }
    // What Blaze approved and what waits on his verdict, counted on the server (Rocky's live pass, 1 Nov 2026: the brief read
    // none of the cortex). Asked only of a server that says it counts (whoami features), so an older one is never asked for
    // its whole node list; an unreadable count is said, never zero.
    let cortex = if who["features"].as_array().is_some_and(|f| f.iter().any(|x| x == "cortex_count")) {
        match door.get("/cortex?count=1") {
            Ok(c) if c["schema"] == "cortex-count/1" => json!({"approved_nodes": c["approved_nodes"], "pending_marks": c["pending_marks"]}),
            Ok(_) => json!({"error": "the server's count was not a cortex count"}),
            Err(e) => json!({"error": format!("{e:#}")}),
        }
    } else {
        json!({"unavailable": "this Oracle does not count its cortex yet (GET /cortex?count=1, 1 Nov 2026)"})
    };
    // The law every seat wakes with (Noor, 3 Nov 2026): what Blaze approved as Rule, Profile or
    // Preference, one line each, most linked first; the brief shows LAW_BRIEF_TOP, --full every line. Asked only of a server that
    // serves it (whoami features); an unreadable list is said, never none.
    let law = if who["features"].as_array().is_some_and(|f| f.iter().any(|x| x == "cortex_law")) {
        match door.get(&format!("/cortex/law?top={}", if full { 10_000 } else { LAW_BRIEF_TOP })) {
            Ok(l) if l["schema"] == "cortex-law/1" => l,
            Ok(_) => json!({"error": "the server's answer was not cortex-law/1"}),
            Err(e) => json!({"error": format!("{e:#}")}),
        }
    } else {
        Value::Null
    };
    Ok(json!({
        "now": oracle_proto::iso_ms(now), "now_ms": now, "local": local_of(&who, now),
        "seat": handle, "identity": door.identity(),
        "state": state,
        "now_head": now_head,
        "identity_head": identity_head,
        "identity_fold": identity_fold,
        "bridge": bridge,
        "procedures": procedures,
        "canon": canon,
        "resume": resume,
        "maps": maps,
        "mine": if asks.is_null() { Value::Null } else { my_asks(&asks, now) },
        "asks": {"rows": rows, "older": inbox_asks["older_count"], "capped": asks["capped"] == true, "error": asks_error, "read_whole": "oracle_thread <id>"},
        "elsewhere": {"total": total, "rooms": elsewhere},
        "work": work,
        "waiting_on_you": waiting.unwrap_or(Value::Null),
        "cortex": cortex,
        "law": law,
        "outbox": door.queued(),
    }))
}

/// The owner's local clock for an answer: the server's (`/whoami` carries it from ORACLE_LOCAL_OFFSET), else this
/// process's own setting. Rocky, 1 Nov 2026: the MCP brief said `local: null` while /health, /whoami and `ox now` showed
/// +05:30, because `ox mcp` runs on the seat's machine, where ORACLE_LOCAL_OFFSET is not set, and read only its own env.
pub fn local_of(who: &Value, now: i64) -> Value {
    who["local"].as_str().map(str::to_owned).or_else(|| oracle_proto::local_clock_env(now)).map_or(Value::Null, Value::from)
}

/// An Oracle instant as a reader needs it: "31 Oct 05:44Z" (the year only when it is not this one). The typed object keeps
/// the exact ISO time; the text spends five words less on every clock (Blaze, 31 Oct 2026).
/// ` · local 10:50 (+05:30)` after a clock line: what the record carries, else the server's own setting, else nothing
/// (Blaze, 1 Nov 2026: the Oracle needs a UTC clock plus a local clock).
pub fn local_suffix(carried: &Value, now: i64) -> String {
    match carried.as_str().map(str::to_owned).or_else(|| oracle_proto::local_clock_env(now)) {
        Some(l) => format!(" · local {l}"),
        None => String::new(),
    }
}

pub fn short_at(iso: &str) -> String {
    const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (Some(y), Some(m), Some(d), Some(hm)) = (iso.get(..4), iso.get(5..7), iso.get(8..10), iso.get(11..16)) else { return iso.to_string() };
    let Some(mon) = m.parse::<usize>().ok().and_then(|m| MON.get(m.wrapping_sub(1))) else { return iso.to_string() };
    let this_year = oracle_proto::iso_ms(now_ms()).get(..4).map(str::to_owned);
    let day = d.trim_start_matches('0');
    if this_year.as_deref() == Some(y) { format!("{day} {mon} {hm}Z") } else { format!("{day} {mon} {y} {hm}Z") }
}

/// What the MCP hands the model for a brief (Blaze, 31 Oct 2026: the brief's structured object was 1.5 MB, 86% of it the
/// State's evidence, every revision's signed envelope, so Claude Code could not take it at all). The State as the model
/// needs it, the canon and the persona by id (whole: ox canon, ox identity), the asks and my asks capped with their
/// counts, the RESUME cut with its id to read it whole. `full: true` keeps the whole object.
pub fn brief_compact(b: &Value) -> Value {
    let mut v = b.clone();
    v["state"] = compact_state(&b["state"]);
    if b["canon"].is_object() {
        v["canon"] = json!({"id": b["canon"]["id"], "author": b["canon"]["author"], "created_ms": b["canon"]["created_ms"],
            "rules": b["canon"]["canon"]["rules"].as_array().map(Vec::len), "whole": "ox canon"});
    }
    if b["identity_head"].is_object() {
        v["identity_head"] = json!({"id": b["identity_head"]["id"], "at_ms": b["identity_head"]["at_ms"],
            "persona": b["identity_head"]["head"]["persona"].as_str().or(b["identity_head"]["persona"]["id"].as_str()),
            "problems": b["identity_head"]["problems"], "whole": "ox identity"});
    }
    if let Some(rows) = b["asks"]["rows"].as_array().filter(|r| r.len() > BRIEF_ASKS) {
        v["asks"]["rows"] = json!(rows[..BRIEF_ASKS]);
        v["asks"]["more"] = json!(rows.len() - BRIEF_ASKS);
    }
    for key in ["answered", "old", "open"] {
        if let Some(rows) = b["mine"][key].as_array().filter(|r| r.len() > 10) {
            v["mine"][key] = json!(rows[..10]);
            v["mine"][format!("{key}_more")] = json!(rows.len() - 10);
        }
    }
    if let Some(t) = b["resume"]["text"].as_str().filter(|t| t.chars().count() > 4000) {
        v["resume"]["text"] = json!(format!("{} …", t.chars().take(4000).collect::<String>()));
        v["resume"]["cut"] = json!(true);
    }
    v
}

/// How many asks the brief lists, in its object and its text, unless `full` (Rocky's live pass, 1 Nov 2026 07:15Z: the MCP
/// brief carried 20 ask rows; the inbox is where they are read). The rest is counted (`asks.more`, and by room in the text).
pub const BRIEF_ASKS: usize = 10;

/// The brief as text, from the typed object.
pub fn brief_render(b: &Value, full: bool) -> String {
    let mut out = String::new();
    let handle = b["seat"].as_str().unwrap_or("?");
    let now = b["now_ms"].as_i64().unwrap_or_else(now_ms);
    out += &format!("ox brief · {handle} · now {} (Oracle clock){}\n", short_at(b["now"].as_str().unwrap_or("?")), local_suffix(&b["local"], now));
    out += &format!("IDENTITY   {handle} · {}\n", b["identity"].as_str().unwrap_or(""));
    // What I am doing this minute comes before everything else (docs/NOW.md).
    out += &now_render(&b["now_head"], now, handle);
    out += TELL_BLOCK;
    // Who I am, whole, right after what I am doing (docs/IDENTITY.md); then the State without its one-line persona.
    let has_identity = b["identity_head"]["head"].is_object();
    if !b["identity_head"].is_null() {
        out += &identity_render_with(&b["identity_head"], now, handle, full);
        out += &fold_render(&b["identity_fold"]);
    }
    if b["bridge"]["none"] == true {
        out += "BRIDGE     nothing to @blaze on record: every answer to him is ox tell \"<head>\" (docs/BRIDGE-CONTRACT.md)\n";
    } else if let Some(at) = b["bridge"]["at_ms"].as_i64() {
        let late = now - at > 30 * 60_000;
        out += &format!("BRIDGE     last to @blaze {} · {}{}\n", ago(now, at), b["bridge"]["id"].as_str().unwrap_or("?"),
            if late { " · OVERDUE: tell him where things stand (ox tell)" } else { "" });
    }
    match &b["procedures"] {
        Value::Array(ps) if !ps.is_empty() => out += &format!("PROCEDURES {} on record, read one whole before doing it: ox procedure <name> · {}\n", ps.len(),
            ps.iter().map(|p| format!("{} v{}", p["name"].as_str().unwrap_or("?"), p["version"].as_i64().unwrap_or(1))).collect::<Vec<_>>().join(" · ")),
        Value::Array(_) => out += "PROCEDURES none on record yet (docs/PROCEDURES.md): ox procedure --write file.json\n",
        v if v["error"].is_string() => out += &format!("PROCEDURES could not be read: {}\n", v["error"].as_str().unwrap_or("")),
        _ => {}
    }
    if b.get("canon").is_some() { out += &canon_brief(&b["canon"], now, full); }
    let state = &b["state"];
    match state["state"].as_str() {
        Some("alive") if has_identity => {
            let mut s = state.clone();
            s["fields"].as_object_mut().map(|f| f.remove("persona"));
            out += &seat_text(handle, &s);
        }
        Some("alive") => out += &seat_text(handle, state),
        Some("degraded") => out += &format!("ME         #{handle} State DEGRADED ({} heads, {} bad) — ox state {handle}\n",
            state["heads"].as_array().map_or(0, |h| h.len()), state["bad"].as_array().map_or(0, |x| x.len())),
        Some("empty") => out += &format!("ME         #{handle} has no State yet — ox state {handle} --set kind=mind --set boot=\"…\"\n"),
        _ => {}
    }
    if let Some(ms) = b["maps"].as_array().filter(|m| !m.is_empty()) {
        out += "MAPS       what my rooms hold, one line each (docs/ROOM-MAP.md); the whole map: ox map <room>\n";
        for m in ms {
            out += &format!("  {}\n", m["line"].as_str().unwrap_or("?"));
        }
    } else if let Some(e) = b["maps"]["error"].as_str() {
        out += &format!("MAPS       could not be read: {e}\n");
    }
    if let Some(r) = b["resume"].as_object() {
        // Folded unless --full: after a compaction the harness summary already carries this note whole (measured
        // 2026-10-20: the RESUME was read three times on one wake, ~9k tokens of it waste).
        let text = r["text"].as_str().unwrap_or("");
        let id = r["id"].as_str().unwrap_or("?");
        out += &format!("RESUME     {id} · written {}{}\n{}\n", r["written"].as_str().unwrap_or("?"),
            if r["whole"] == false { " · PREVIEW only, the whole note was unreadable — oracle_get <id>" } else { "" },
            if full { text.to_string() } else { fold_lines(text, RESUME_BRIEF_LINES, &format!("ox get {id}, or ox brief --full")) });
    }
    let rows = b["asks"]["rows"].as_array().cloned().unwrap_or_default();
    if let Some(e) = b["asks"]["error"].as_str() {
        out += &format!("ASKS FOR ME UNREADABLE — {e}\n");
    } else {
        let open = rows.iter().filter(|r| r["state"] != "answered").count();
        let older = b["asks"]["older"].as_u64().unwrap_or(0);
        out += &format!("ASKS FOR ME ({open} open{}{}{}) — read one whole with oracle_thread <id>\n",
            match rows.len() - open { 0 => String::new(), n => format!(", {n} answered by another seat") },
            if older > 0 { format!(", {older} from before 8 Oct: ox inbox --older") } else { String::new() },
            if b["asks"]["capped"] == true { "; asks older than the newest 500 per kind may be missing" } else { "" });
    }
    let mut rows = rows;
    rows.sort_by_key(|r| r["state"] == "answered");
    let shown = if full { rows.len() } else { rows.len().min(BRIEF_ASKS) };
    let rest = rows.split_off(shown);
    for r in &rows {
        out += &format!("  {:<6} #{:<9} {:<12} {:<9}{} {}  ({})\n", r["ask"].as_str().unwrap_or("?"), r["room"].as_str().unwrap_or("?"),
            r["from"].as_str().unwrap_or("?"), r["age"].as_str().unwrap_or(""),
            if r["overdue"] == true { " OVERDUE" } else if r["state"] == "answered" { " answered" } else { "" },
            r["card"].as_str().unwrap_or(""), r["id"].as_str().unwrap_or("?"));
        out += &gap_render(&r["gap"], "           ");
    }
    out += &rest_by_room(&rest, "room");
    if b["mine"].is_object() {
        out += &my_asks_text(&b["mine"], if full { None } else { Some(5) });
    }
    if let Some(rooms) = b["elsewhere"]["rooms"].as_object().filter(|m| !m.is_empty()) {
        let parts: Vec<String> = rooms.iter().map(|(k, v)| format!("#{k} {v}")).collect();
        out += &format!("ELSEWHERE  {} new · {}\n", b["elsewhere"]["total"], parts.join(" · "));
    }
    let w = &b["work"];
    let cards: Vec<String> = w["cards"].as_array().into_iter().flatten().map(|c| format!("{} {}", c[0].as_str().unwrap_or("?"), c[1])).collect();
    let card_total: i64 = w["cards"].as_array().into_iter().flatten().filter_map(|c| c[1].as_i64()).sum();
    if full {
        out += "MY OPEN WORK\n";
        for h in w["head_list"].as_array().into_iter().flatten() {
            out += &format!("  [{}] {}  next: {}{}\n", h["state"].as_str().unwrap_or(""), h["subject"].as_str().unwrap_or(""),
                h["next"].as_str().unwrap_or("—"), h["overdue"].as_str().map(|d| format!("  OVERDUE {d}")).unwrap_or_default());
        }
        if w["card_list"]["cards"].is_array() {
            out += &format!("YOUR CARDS ({card_total})\n");
            out += &cards_text(&w["card_list"], false);
        }
    } else if let Some(e) = w["error"].as_str() {
        out += &format!("WORK       heads not readable here ({e}) · {card_total} cards ({})\n", cards.join(" · "));
    } else {
        out += &format!("WORK       {} open heads ({} overdue) · {card_total} cards ({}) — ox brief --full lists them\n",
            w["heads"], w["overdue"], cards.join(" · "));
    }
    let c = &b["cortex"];
    if c["approved_nodes"].is_u64() {
        out += &format!("CORTEX     {} approved nodes · {} marks waiting on Blaze's verdict — oracle_cortex op=nodes top=10\n", c["approved_nodes"], c["pending_marks"]);
    } else if let Some(e) = c["error"].as_str() {
        out += &format!("CORTEX     could not be counted: {e}\n");
    }
    let law = &b["law"];
    if let Some(lines) = law["lines"].as_array() {
        let (total, shown) = (law["total"].as_u64().unwrap_or(0), lines.len() as u64);
        out += &format!("LAW        {total} readings Blaze approved as Rule, Profile or Preference{}\n",
            if shown < total { format!(" · the {shown} most linked here; every one: ox brief --full") } else { String::new() });
        for l in lines {
            let code = l["code"].as_str().unwrap_or("?");
            out += &format!("  - {} ({})\n", l["line"].as_str().unwrap_or(""), &code[code.len().saturating_sub(12)..]);
        }
    } else if let Some(e) = law["error"].as_str() {
        out += &format!("LAW        could not be read: {e}\n");
    }
    if b["waiting_on_you"]["cards"].is_array() {
        let n = b["waiting_on_you"]["cards"].as_array().map_or(0, |a| a.len());
        out += &format!("WAITING ON YOU ({n})\n");
        out += &cards_text(&b["waiting_on_you"], true);
    }
    if b["outbox"].as_u64().unwrap_or(0) > 0 {
        out += &format!("OUTBOX     {} queued write(s) — call oracle_flush\n", b["outbox"]);
    }
    let _ = now;
    out
}

/// The brief as text.
pub fn brief_with(door: &dyn Door, full: bool) -> Result<String> {
    Ok(brief_render(&brief_value(door, full)?, full))
}

/// Text given whole in one inbox call, in bytes. A message that does not fit is listed by its header and size, with its
/// id to read it whole: whole or none, never cut (Blaze), and bounded (Emmy: urgent items within a budget, lossless fetch).
pub const INBOX_TEXT_BUDGET: usize = 6_000;

/// What an inbox call marks read.
pub enum Ack {
    No,
    /// This page, stopping before any addressed message listed without its text.
    Page,
    /// Exactly this mark map (one an earlier answer returned).
    Marks(Value),
}

/// The inbox as one typed object (the MCP structuredContent): the Oracle clock; what is addressed to the seat, whole;
/// everything else counted per room (Blaze, 2026-10-01: whole text or none, and a room named once, not per line).
/// `ack` marks exactly the page this answer was built from.
pub fn inbox_value(door: &dyn Door, limit: i64, ack: Ack, older: bool) -> Result<Value> {
    let who = door.get("/whoami")?;
    let now = who["server_ms"].as_i64().unwrap_or_else(now_ms);
    let rows_max = limit.clamp(1, 200) as usize;
    // An asks read that fails is said, never silently an empty list.
    let mut asks = match door.get("/asks?state=open") {
        Ok(a) => asks_value(&a, older),
        Err(e) => json!({"error": format!("{e:#}")}),
    };
    let (ask_rows, ask_cut) = if asks["error"].is_null() { asks_keep(&mut asks, rows_max) } else { (0, 0) };
    let older_unlisted = if older { 0 } else { asks["older_count"].as_u64().unwrap_or(0) as usize };
    let v = fold_or_rooms(door, limit, &who)?;
    let scoped = v["scoped"] == true;
    let items = v["items"].as_array().cloned().unwrap_or_default();
    let (page_mine, rest): (Vec<Value>, Vec<Value>) = items.into_iter().partition(|i| i["for_me"] == true);
    // What is addressed to me comes first and newest first, whether or not this page reached it (Jobs' audit,
    // 2026-10-10: a backlog paged oldest-first hid what was written to him today); then the page's own, once each.
    let unread: Vec<Value> = v["unread_for_me"].as_array().cloned().unwrap_or_default();
    let mut mine: Vec<Value> = unread.clone();
    mine.extend(page_mine.into_iter().filter(|i| !unread.iter().any(|u| u["id"] == i["id"])));
    // The limit bounds what is addressed to me too (Rocky's live pass, 1 Nov 2026); the rest is counted and, below, never marked read.
    let mine_total = mine.len();
    let mine_cut = mine.split_off(rows_max.min(mine.len()));
    let tag = |i: &Value, p: &str| i["tags"].as_array().into_iter().flatten().filter_map(Value::as_str).find_map(|t| t.strip_prefix(p).map(str::to_owned));
    let mut spent = 0usize;
    // Every addressed message on this page is listed (Emmy, #59: a cap here plus a page-wide ack lost the rest).
    let whole: Vec<Value> = mine.iter().map(|i| {
        let text = i["content"].as_str().unwrap_or("");
        // The first always comes whole; after it, whole while the budget lasts, else none (never a cut).
        let fits = spent == 0 || spent + text.len() <= INBOX_TEXT_BUDGET;
        if fits {
            spent += text.len();
        }
        json!({"id": i["id"], "room": i["channel"], "author": i["author"], "age": ago(now, i["created_ms"].as_i64().unwrap_or(now)),
               "intent": tag(i, "intent:"), "ask": tag(i, "ask:"), "bytes": text.len(), "text": if fits { json!(text) } else { Value::Null }})
    }).collect();
    // Per room, in the order rooms first appear: count, who wrote, and how many are routine status (not asked of anyone).
    let mut rooms: Vec<(String, Value)> = vec![];
    for i in &rest {
        let room = i["channel"].as_str().unwrap_or("?").to_string();
        let idx = rooms.iter().position(|(r, _)| *r == room).unwrap_or_else(|| {
            rooms.push((room.clone(), json!({"room": room, "count": 0, "routine": 0, "authors": {}, "newest": i["id"]})));
            rooms.len() - 1
        });
        let e = &mut rooms[idx].1;
        e["count"] = json!(e["count"].as_i64().unwrap_or(0) + 1);
        if matches!(tag(i, "intent:").as_deref(), Some("status" | "receipt")) && tag(i, "ask:").as_deref().is_none_or(|a| a == "none") {
            e["routine"] = json!(e["routine"].as_i64().unwrap_or(0) + 1);
        }
        let a = i["author"].as_str().unwrap_or("?").to_string();
        e["authors"][&a] = json!(e["authors"][&a].as_i64().unwrap_or(0) + 1);
    }
    let pending: i64 = v["pending"].as_object().map(|m| m.values().filter_map(Value::as_i64).sum()).unwrap_or(0);
    // What "read" may commit: the page's marks, except that in a room where an addressed message was listed without
    // its text, the mark stops just before it, so it is delivered whole next time instead of being marked read unseen.
    let mut marks = v["next_marks"].clone();
    for (m, i) in whole.iter().zip(mine.iter()) {
        if m["text"].is_null() {
            if let (Some(room), Some(seq)) = (i["channel"].as_str(), i["seq"].as_i64()) {
                let cur = marks[room].as_i64().unwrap_or(i64::MAX);
                marks[room] = json!(cur.min(seq - 1));
            }
        }
    }
    // An addressed message the limit left out is not marked read either: a room's mark that would pass it stops before it.
    for i in &mine_cut {
        if let (Some(room), Some(seq)) = (i["channel"].as_str(), i["seq"].as_i64()) {
            if let Some(cur) = marks[room].as_i64() {
                marks[room] = json!(cur.min(seq - 1));
            }
        }
    }
    let mut more: Vec<String> = vec![];
    if ask_cut > 0 {
        more.push(format!("limit: up to 200 lists more of the asks ({ask_cut} not listed); ox asks --mine lists every one"));
    }
    if older_unlisted > 0 {
        more.push(format!("older: true lists the {older_unlisted} open asks from before 8 Oct 08:00 UTC"));
    }
    if !mine_cut.is_empty() {
        more.push(format!("limit: {mine_total} lists every message addressed to you here ({} not listed; ack marks read only up to the first of them)", mine_cut.len()));
    }
    let acked = match &ack {
        // A scoped seat has no read marks to move (GET and POST /fold are not open to it): nothing is marked read.
        _ if scoped => false,
        Ack::No => false,
        Ack::Page => {
            door.post("/fold", &json!({"marks": marks}))?;
            true
        }
        // An explicit mark map is posted exactly as given: it is what the caller read, not this newer page.
        Ack::Marks(given) => {
            door.post("/fold", &json!({"marks": given}))?;
            true
        }
    };
    Ok(json!({
        "now": oracle_proto::iso_ms(now), "now_ms": now, "local": local_of(&who, now),
        "asks": asks,
        "new": pending, "page": mine_total + rest.len(), "has_more": v["has_more"] == true,
        "for_me": whole,
        "rooms": rooms.into_iter().map(|(_, e)| e).collect::<Vec<_>>(),
        "marks": marks, "acked": acked,
        // What a scoped page is, said plainly (Emmy's review of e76f1aa): recent history of the seat's own rooms, bounded per
        // room; a reply or annotation takes a task off it but does not complete the task. Not a complete ledger of pending work.
        "scoped": if scoped { json!(format!("history of your rooms, the last {} records of each: a task you replied to leaves this page but is not thereby done; this is not a complete list of pending work", limit.clamp(1, 200))) } else { Value::Null },
        "budget": budget(ask_rows + whole.len(), ask_cut + older_unlisted + mine_cut.len(), (!more.is_empty()).then(|| more.join("; "))),
    }))
}

/// The fold page, or for a seat scoped to its rooms (the seat-scope guard refuses `/fold`; Dot, 4 Nov 2026) the same shape
/// built only from what it may read: its rooms (`/rooms/mine`) and their records (`/search?channel=`). A record names it
/// (`to:` or `mention:`) and it has not spoken in that room since: it is for it. Nothing is opened on the server; there are
/// no marks, so `scoped` says so and the caller never acks.
fn fold_or_rooms(door: &dyn Door, limit: i64, who: &Value) -> Result<Value> {
    match door.get(&format!("/fold?limit={limit}")) {
        Ok(v) => Ok(v),
        Err(e) if format!("{e:#}").contains("is scoped to its rooms") => {
            let me = who["handle"].as_str().unwrap_or("").to_string();
            let rooms = door.get("/rooms/mine")?;
            let mut items: Vec<Value> = vec![];
            let mut more_rows = false;
            for r in rooms["rooms"].as_array().into_iter().flatten().filter_map(|r| r["room"].as_str()) {
                let page = door.get(&format!("/search?channel={}&limit={}", enc(r), limit.clamp(1, 200)))?;
                more_rows |= !page["next_before"].is_null();
                let mut got: Vec<Value> = page["items"].as_array().cloned().unwrap_or_default();
                got.sort_by_key(|i| i["created_ms"].as_i64().unwrap_or(0));
                // What I have answered: a record of mine that replies to it. Any other word of mine (a progress line, a diary) is
                // not an answer and leaves an addressed task listed (Emmy's review of b46d543). An annotation is not on a room page
                // (the search page lists no annotate records; Dot's review), so it cannot take a task off; a tag may carry an @.
                let tags_of = |i: &Value| i["tags"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<String>>();
                let answered: std::collections::BTreeSet<String> = got.iter().filter(|i| i["author"].as_str() == Some(me.as_str()))
                    .flat_map(|i| tags_of(i).into_iter().filter_map(|t| ["reply:", "re:", "annotates:"].iter().find_map(|p| t.strip_prefix(p).map(str::to_owned))))
                    .collect();
                let last_mine = got.iter().filter(|i| i["author"].as_str() == Some(me.as_str())).filter_map(|i| i["created_ms"].as_i64()).max().unwrap_or(0);
                for mut i in got {
                    let named = tags_of(&i).iter().any(|t| ["to:", "mention:"].iter().any(|p| t.strip_prefix(p).is_some_and(|v| v.trim_start_matches('@') == me)));
                    let id = i["id"].as_str().unwrap_or("").to_string();
                    let after = i["created_ms"].as_i64().unwrap_or(0) > last_mine;
                    // Mine is never listed; addressed to me, listed until I answer it; the rest only since my last word.
                    if i["author"].as_str() == Some(me.as_str()) || (named && answered.contains(&id)) || (!named && !after) {
                        continue;
                    }
                    if i["channel"].is_null() {
                        i["channel"] = json!(r);
                    }
                    i["for_me"] = json!(named);
                    items.push(i);
                }
            }
            // NEW counts what this page lists per room (there are no read marks to count from; Dot's check, 4 Nov 2026: "0 new"
            // beside 13 listed records said nothing).
            let mut pending = serde_json::Map::new();
            for i in &items {
                let r = i["channel"].as_str().unwrap_or("?").to_string();
                let n = pending.get(&r).and_then(Value::as_i64).unwrap_or(0) + 1;
                pending.insert(r, json!(n));
            }
            Ok(json!({"items": items, "unread_for_me": [], "pending": pending, "next_marks": {}, "has_more": more_rows, "scoped": true}))
        }
        Err(e) => Err(e),
    }
}

/// How many inbox rows an MCP call answers when it names no limit (Rocky's live pass, 1 Nov 2026 07:15Z: `oracle_inbox
/// mine=true limit=5` answered 86 KB, 200 open asks whole; the limit bounded only the fold's page).
pub const INBOX_LIMIT: i64 = 20;

/// The ask lists of an inbox cut to `max` rows each (open, acked, stuck, older), newest first as the server sends them;
/// `counts` keeps each list's length before the cut, and open's answered and unanswered, so the text still counts right.
/// Returns (rows kept, rows cut).
pub fn asks_keep(asks: &mut Value, max: usize) -> (usize, usize) {
    let open = asks["open"].as_array().cloned().unwrap_or_default();
    let answered = open.iter().filter(|r| r["state"] == "answered").count();
    asks["counts"] = json!({"open_answered": answered, "open_unanswered": open.len() - answered});
    let (mut kept, mut cut) = (0, 0);
    for k in ["open", "acked", "stuck", "older"] {
        if let Some(rows) = asks[k].as_array_mut() {
            let n = rows.len();
            rows.truncate(max);
            kept += rows.len();
            cut += n - rows.len();
            asks["counts"][k] = json!(n);
        }
    }
    (kept, cut)
}

/// `oracle_inbox` as the MCP answers it: `limit` (default 20) bounds every list, and `mine` leaves out the per-room
/// counts of what is not addressed to me (as `ox inbox --mine` does).
pub fn inbox_mcp(door: &dyn Door, a: &Value, ack: Ack) -> Result<Value> {
    let mut v = inbox_value(door, a["limit"].as_i64().unwrap_or(INBOX_LIMIT), ack, a["older"].as_bool() == Some(true))?;
    if a["mine"].as_bool() == Some(true) {
        v["rooms"] = json!([]);
    }
    Ok(v)
}

/// The inbox as text, from the typed object.
/// WORLD STATE (`GET /world`, schema world/1; Blaze, 1 Nov 2026, via Noor; Noor's schema):
/// the page the server composes from signed heads. An Oracle without the route is
/// told so by name, never answered with an empty world.
pub fn world_value(door: &dyn Door) -> Result<Value> {
    match door.get("/world") {
        Ok(v) if v["schema"] == "world/1" => Ok(v),
        Ok(_) => bail!("this Oracle's /world did not answer a world/1 page; it is older than the world page (oracle-core, 1 Nov 2026): update the server"),
        Err(e) if e.to_string().contains("(404)") || e.to_string().to_lowercase().contains("not found") => {
            bail!("this Oracle does not serve the world page (GET /world, world/1): it is older than oracle-core of 1 Nov 2026; update the server")
        }
        Err(e) => Err(e),
    }
}

/// The ceiling of the world page's text, in bytes (Noor's schema: under 1,500 tokens).
pub const WORLD_TEXT_MAX: usize = 6_000;
/// One line of the world page, at most this many bytes, cut at a character with an ellipsis.
const WORLD_LINE: usize = 92;

fn world_fit(s: &str) -> String {
    let cut = |s: &str, max: usize| {
        if s.len() <= max {
            return s.to_string();
        }
        let mut end = max.saturating_sub(3);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    };
    if s.len() <= WORLD_LINE {
        return s.to_string();
    }
    // A line's record code is never cut: the text before it is.
    match s.rsplit_once(" · ").filter(|(_, c)| c.len() == 12 && c.chars().all(|x| x.is_ascii_hexdigit())) {
        Some((body, code)) => format!("{} · {code}", cut(body, WORLD_LINE - code.len() - 3)),
        None => cut(s, WORLD_LINE),
    }
}

/// A record id as its code: the twelve hex that name it on every door (`ox get <code>`).
fn code_of(id: &Value) -> String {
    id.as_str().map(|i| i.rsplit('-').next().unwrap_or(i).to_string()).unwrap_or_else(|| "?".into())
}

/// The world page as text: six sections, each line with its record's code, each section ending with how many more and
/// where the rest is, then one footer with the clock, the page's bytes and the next addresses.
pub fn world_render(v: &Value) -> String {
    let now = v["now_ms"].as_i64().unwrap_or_else(now_ms);
    let when = |ms: &Value| ms.as_i64().map(|m| ago(now, m)).unwrap_or_default();
    let mut out = String::new();
    let mut line = |s: String| {
        out += &world_fit(&s);
        out.push('\n');
    };
    let more = |n: &Value, rest: &Value| format!("  + {} more · {}", n.as_u64().unwrap_or(0), rest.as_str().unwrap_or(""));
    let ns = &v["north_star"];
    line("NORTH STAR".into());
    match ns["canon"].as_object() {
        Some(c) => line(format!("  canon v{} · {} rules ({} open) · #{} · {}", c["version"], c["rules"], c["open"], c["room"].as_str().unwrap_or("?"), code_of(&c["id"]))),
        None => line("  canon: none on record".into()),
    }
    for i in ns["identities"].as_array().into_iter().flatten() {
        line(format!("  {}: {} · {}", i["seat"].as_str().unwrap_or("?"), i["purpose"].as_str().unwrap_or(""), code_of(&i["id"])));
    }
    line(more(&ns["more"], &ns["rest"]));
    let nw = &v["now_section"];
    line("NOW".into());
    for st in nw["steps"].as_array().into_iter().flatten() {
        line(format!("  {} {} doing: {}{} · {}", st["plan"].as_str().unwrap_or("?"), st["step"].as_str().unwrap_or("?"), st["title"].as_str().unwrap_or(""),
            st["owner"].as_str().map(|o| format!(" ({o})")).unwrap_or_default(), code_of(&st["id"])));
    }
    line(more(&nw["more"], &nw["rest"]));
    match nw["queue"].as_object() {
        Some(q) => {
            line(format!("  queue #{} by {} {} · {}", q["room"].as_str().unwrap_or("?"), q["by"].as_str().unwrap_or("?"), when(&q["at_ms"]), code_of(&q["id"])));
            for (n, it) in q["items"].as_array().into_iter().flatten().enumerate() {
                line(format!("    {}. {} · {}", n + 1, it["subject"].as_str().unwrap_or("?"), it["title"].as_str().unwrap_or("")));
            }
            if q["more"].as_u64().unwrap_or(0) > 0 {
                line(format!("    + {} more in the queue · ox get {}", q["more"], code_of(&q["id"])));
            }
        }
        None => line("  queue: no queue/1 on record".into()),
    }
    for (key, title) in [("projects", "ACTIVE PROJECTS, running or blocked, newest touched first"), ("decisions", "KEY DECISIONS, last 7 days, newest first"),
                         ("seats", "ACTIVE SEATS"), ("questions", "OPEN QUESTIONS for the owner: decide, unanswered, oldest first")] {
        let sec = &v[key];
        line(title.into());
        for r in sec["items"].as_array().into_iter().flatten() {
            line(match key {
                "projects" => format!("  #{} {} · {} · {} · next {} · {} · {}", r["room"].as_str().unwrap_or("?"), r["subject"].as_str().unwrap_or("?"), r["state"].as_str().unwrap_or("?"),
                    r["owner"].as_str().unwrap_or("?"), r["next"].as_str().unwrap_or("—"), r["note"].as_str().unwrap_or(""), code_of(&r["id"])),
                "decisions" => format!("  {} · {} · {}", oracle_proto::iso_ms(r["at_ms"].as_i64().unwrap_or(0)).get(..10).unwrap_or("?"), r["text"].as_str().unwrap_or(""), code_of(&r["id"])),
                "seats" => format!("  {}{} · {} · {} · {}", r["seat"].as_str().unwrap_or("?"), if r["degraded"] == true { " (State degraded)" } else { "" },
                    r["current"].as_str().unwrap_or(""), when(&r["at_ms"]), code_of(&r["id"])),
                _ => format!("  {} · {} · from {} {}", code_of(&r["id"]), r["head"].as_str().unwrap_or(""), r["from"].as_str().unwrap_or("?"), when(&r["since_ms"])),
            });
        }
        line(more(&sec["more"], &sec["rest"]));
    }
    // The footer says the page's own size: computed with its own digits included.
    let head = format!("WORLD STATE · composed {} (Oracle clock){}\n", short_at(v["now"].as_str().unwrap_or("?")), local_suffix(&v["local"], now));
    let tail = |n: usize| format!("composed at {} · {n} bytes · next: {}\n", v["now"].as_str().unwrap_or("?"),
        v["next"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(" · "));
    let mut n = head.len() + out.len() + tail(0).len();
    n = head.len() + out.len() + tail(n).len();
    format!("{head}{out}{}", tail(n))
}

/// `oracle_brief`'s `where`: me (the default) or world; anything else is refused, naming the forms.
pub fn brief_where(a: &Value) -> Result<&'static str> {
    match a["where"].as_str().map(str::trim) {
        None | Some("") | Some("me") => Ok("me"),
        Some("world") => Ok("world"),
        Some(other) => bail!("oracle_brief where is me (the default) or world, not {other}; a room's State is brief where=<room> (the brief verb), a record is read where=<id>"),
    }
}

/// The waking read (`GET /delta`, schema delta/1; Blaze, 1 Nov 2026: food for an agent is the diff between the world's
/// last known state and its current state): what changed for this seat since `since` (an OBS id or code, a time, a span
/// like `6h`), or since its last RESUME. The MCP tool `oracle_delta` and `ox delta` both print `delta_render` of this.
pub fn delta_value(door: &dyn Door, since: Option<&str>) -> Result<Value> {
    let since = since.map(str::trim).filter(|s| !s.is_empty());
    let path = match since {
        Some(s) => format!("/delta?since={}", enc(s)),
        None => "/delta".to_string(),
    };
    door.get(&path)
}

/// `delta_value` as one dated page: ROOMS, ASKS, WORDS, MACHINE, every line naming the record it came from.
pub fn delta_render(v: &Value) -> String {
    let now = v["now_ms"].as_i64().unwrap_or_else(now_ms);
    let str_of = |x: &Value| x.as_str().unwrap_or("?").to_string();
    let when = |x: &Value| x.as_i64().map(|ms| ago(now, ms)).unwrap_or_default();
    let since = &v["since"];
    let point = match since["kind"].as_str() {
        Some("resume") => format!("my last RESUME {}", str_of(&since["id"])),
        Some("record") => format!("record {}", str_of(&since["id"])),
        _ => str_of(&since["given"]),
    };
    let iso = since["iso"].as_str().unwrap_or("?");
    let mut out = format!("DELTA      {} · since {point} ({}, {}) · now {} (Oracle clock, UTC)
", str_of(&v["seat"]),
        iso.get(..16).unwrap_or(iso), when(&since["ms"]), str_of(&v["now"]));
    let line = |r: &Value| format!("{}  {:<10} {:<8} {}", str_of(&r["id"]), str_of(&r["author"]), when(&r["ms"]), one_line(r["line"].as_str().unwrap_or(""), 80));
    // ROOMS
    let rooms = v["rooms"].as_array().cloned().unwrap_or_default();
    let quiet = v["quiet"].as_array().map_or(0, Vec::len);
    out += &format!("ROOMS      {} changed, {quiet} quiet
", rooms.len());
    for r in &rooms {
        let n = r["new"].as_i64().unwrap_or(0);
        let mut facts = vec![format!("{n}{} new", if r["capped"] == true { "+" } else { "" })];
        if let Some(m) = r["to_me"].as_i64().filter(|m| *m > 0) {
            facts.push(format!("{m} to me"));
        }
        if r["state"]["changed"] == true {
            facts.push(format!("State changed ({})", str_of(&r["state"]["head"])));
        }
        let why: Vec<String> = r["why"].as_array().into_iter().flatten().filter_map(|w| w.as_str().map(str::to_owned)).collect();
        out += &format!("  #{:<12} {}  [{}]
", str_of(&r["room"]), facts.join(" · "), why.join(","));
        for m in r["to_me_lines"].as_array().into_iter().flatten() {
            out += &format!("      {}
", line(m));
        }
    }
    if quiet > 0 {
        let names: Vec<String> = v["quiet"].as_array().into_iter().flatten().filter_map(|q| q.as_str().map(|q| format!("#{q}"))).collect();
        out += &format!("  quiet: {}
", names.join(" "));
    }
    // ASKS
    let a = &v["asks"];
    let n = |k: &str| a[k].as_array().map_or(0, Vec::len);
    out += &format!("ASKS       {} opened, {} answered, {} closed{}
", n("opened"), n("answered"), n("closed"),
        if a["capped"] == true { " (the asks read hit its cap: older asks may be missing)" } else { "" });
    for (k, label) in [("opened", "opened"), ("answered", "answered"), ("closed", "closed")] {
        for r in a[k].as_array().into_iter().flatten() {
            let held: Vec<String> = r["held_by"].as_array().into_iter().flatten().filter_map(|h| h.as_str().map(str::to_owned)).collect();
            out += &format!("  {label:<9} {}  #{:<10} {} → {}  {}
", str_of(&r["id"]), str_of(&r["room"]), str_of(&r["author"]),
                if held.is_empty() { "·".to_string() } else { held.join(",") }, one_line(r["card"].as_str().unwrap_or(""), 70));
        }
    }
    // WORDS
    let words = v["words"].as_array().cloned().unwrap_or_default();
    let counts: Vec<String> = words.iter().map(|w| format!("#{} {} new", str_of(&w["room"]), w["new"].as_i64().unwrap_or(0))).collect();
    out += &format!("WORDS      {}
", if counts.is_empty() { "no word room I may read".to_string() } else { counts.join(" · ") });
    for w in &words {
        for i in w["items"].as_array().into_iter().flatten() {
            out += &format!("  #{:<8} {}
", str_of(&w["room"]), line(i));
        }
    }
    // MACHINE
    let m = &v["machine"];
    out += &match m["started_ms"].as_i64() {
        Some(ms) if m["restarted_since"] == true => format!("MACHINE    oracle-core restarted since: running since {} ({})
", str_of(&m["started"]), ago(now, ms)),
        Some(ms) => format!("MACHINE    oracle-core not restarted since: running since {} ({})
", str_of(&m["started"]), ago(now, ms)),
        None => "MACHINE    oracle-core start time unavailable
".to_string(),
    };
    let mine = m["mine"].as_array().cloned().unwrap_or_default();
    out += &format!("  mine     {} record{} since
", m["mine_since"].as_i64().unwrap_or(0), if m["mine_since"] == 1 { "" } else { "s" });
    for r in &mine {
        out += &format!("    #{:<9} {}
", str_of(&r["room"]), line(r));
    }
    out
}

pub fn inbox_render(b: &Value) -> String {
    let mut out = format!("inbox · now {} (Oracle clock, UTC){}\n", b["now"].as_str().unwrap_or("?"), local_suffix(&b["local"], b["now_ms"].as_i64().unwrap_or_else(now_ms)));
    out += &asks_render(&b["asks"], b["now_ms"].as_i64().unwrap_or_else(now_ms));
    out += &format!("NEW · {} new, {} on this page{}\n", b["new"], b["page"], if b["has_more"] == true { ", more after it" } else { "" });
    if let Some(note) = b["scoped"].as_str() {
        out += &format!("SCOPED · {note}\n");
    }
    let mine = b["for_me"].as_array().cloned().unwrap_or_default();
    if !mine.is_empty() {
        out += &format!("FOR YOU ({}), newest first, whole or by header\n", mine.len());
        for m in &mine {
            let kind = [m["intent"].as_str().map(str::to_owned), m["ask"].as_str().filter(|a| *a != "none").map(|a| format!("ask {a}"))]
                .into_iter().flatten().collect::<Vec<_>>().join(" · ");
            out += &format!("── #{} · {} · {}{} · {}\n", m["room"].as_str().unwrap_or("?"), m["author"].as_str().unwrap_or("?"), m["age"].as_str().unwrap_or(""),
                if kind.is_empty() { String::new() } else { format!(" · {kind}") }, m["id"].as_str().unwrap_or("?"));
            match m["text"].as_str() {
                Some(t) => out += &format!("{}\n", t.trim_end()),
                None => out += &format!("   ({} bytes, past this answer's budget: oracle_get {})\n", m["bytes"], m["id"].as_str().unwrap_or("?")),
            }
        }
    }
    let rooms = b["rooms"].as_array().cloned().unwrap_or_default();
    if !rooms.is_empty() {
        out += "ELSEWHERE, per room (read them whole with oracle_read <room>)\n";
        for r in &rooms {
            let authors: Vec<String> = r["authors"].as_object().map(|m| m.iter().map(|(a, n)| format!("{a} {n}")).collect()).unwrap_or_default();
            out += &format!("  #{:<12} {:>3}  {}{}\n", r["room"].as_str().unwrap_or("?"), r["count"], authors.join(", "),
                match r["routine"].as_i64() { Some(n) if n > 0 => format!("  ({n} routine status)"), _ => String::new() });
        }
    }
    if b["acked"] == true {
        out += "marked read (up to anything listed without its text)\n";
    }
    out
}

/// Text given whole in one read, thread or recall answer, in bytes; past it a message is listed by its header and size,
/// with its id to fetch it whole (whole or none, never cut).
pub const READ_TEXT_BUDGET: usize = 12_000;
/// The tags a reader needs (what a message points at and how it is typed); signatures and storage marks are left out.
const LINK_TAGS: [&str; 10] = ["reply:", "re:", "closes:", "supersedes:", "ref:", "subject:", "rev:", "verdict:", "annotates:", "in_reply_to:"];

/// One message as an agent reads it: where, who, exactly when (Oracle-received time, UTC) and how long ago, what it is,
/// whom it asks, what it points at, and its text whole or not at all. Cargo whole only when `cargo` is true; otherwise
/// its size, to fetch with oracle_get.
/// The server-derived fields a message record keeps when the server sends them: lifecycle, answers, settling, history,
/// and where it sits (seq, replies, envelope, signed).
pub const MSG_STATE_FIELDS: [&str; 23] = ["seq", "kind", "envelope", "signed", "verification", "state", "answered_by", "superseded_by",
    "supersedes", "history", "overdue", "by_ms", "expires_ms", "settled", "settled_by", "settled_at", "resolved_by", "archived_by",
    "stale", "replies", "classified_by", "score", "approved"];

pub fn msg_value(o: &Value, now: i64, whole: bool, cargo: bool) -> Value {
    let tags: Vec<&str> = o["tags"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let first = |p: &str| tags.iter().find_map(|t| t.strip_prefix(p)).map(str::to_owned);
    let at = o["created_ms"].as_i64().unwrap_or(now);
    let text = o["content"].as_str().unwrap_or("");
    let mut m = json!({
        "id": o["id"], "room": o["channel"], "author": o["author"],
        "at": oracle_proto::iso_ms(at), "age": ago(now, at),
        "intent": first("intent:"), "ask": first("ask:").filter(|a| a != "none"),
        "to": tags.iter().filter_map(|t| t.strip_prefix("to:")).collect::<Vec<_>>(),
        "links": tags.iter().filter(|t| LINK_TAGS.iter().any(|p| t.starts_with(p))).collect::<Vec<_>>(),
        // Every signed tag, beside the links picked out of them (Pythia, 1 Nov 2026: "oracle_get and oracle_read don't show a
        // message's tags"; a reader filtering by tag could not see the tag it filtered on).
        "tags": tags,
        "bytes": text.len(), "text": if whole { json!(text) } else { Value::Null },
    });
    // Server-derived state and provenance, by name: presentation must not turn an answered, superseded or annotated
    // message back into an apparently new request (Emmy, 44ffaed), and the record's shape is this list, not whatever
    // the server adds next (every field here is paid for on every read).
    for key in MSG_STATE_FIELDS {
        if let Some(v) = o.get(key).filter(|v| !v.is_null()) {
            m[key] = v.clone();
        }
    }
    // The server's classification beats the tags it was read from; "none" asks nothing.
    for key in ["intent", "ask"] {
        if let Some(v) = o[key].as_str() {
            m[key] = if key == "ask" && v == "none" { Value::Null } else { json!(v) };
        }
    }
    if cargo {
        for key in ["tags", "sig", "pubkey"] {
            if let Some(v) = o.get(key) {
                m[key] = v.clone();
            }
        }
    }
    if let Some(c) = o["cargo"].as_str() {
        m["cargo_bytes"] = json!(c.len());
        if cargo {
            m["cargo"] = json!(c);
        }
    }
    if let Some(a) = o["authored_ms"].as_i64() {
        m["authored"] = json!(oracle_proto::iso_ms(a));
    }
    // Paid for on every read (31 Oct 2026): a null or an empty list says nothing, and an age that is only the date repeats `at`.
    if let Some(map) = m.as_object_mut() {
        // `tags` stays even when empty: a record without tags says so, it does not look like an older answer without the field.
        map.retain(|k, v| k == "tags" || !(v.is_null() || v.as_array().is_some_and(Vec::is_empty)));
        let at = map.get("at").and_then(Value::as_str).map(|a| a.get(..10).unwrap_or(a).to_string());
        if at.is_some() && map.get("age").and_then(Value::as_str).map(str::to_string) == at {
            map.remove("age");
        }
    }
    m
}

/// Where a search hit matched, exactly as the server reported it (`match_in`: content | cargo | tags, `match_pointer` the RFC 6901 pointer of the cargo field, `match_span`
/// UTF-8 byte offsets in that field's own text), plus a one-line hint a reader can use at once. A field the server did not send is NOT invented: a hit with no match
/// fields (meaning-only) or `match_in: null` carries no location and no hint. Never adds a span the backend did not give.
pub fn match_location(hit: &Value) -> (serde_json::Map<String, Value>, Option<String>) {
    let mut out = serde_json::Map::new();
    for key in ["match_in", "match_pointer", "match_span"] {
        if let Some(v) = hit.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    let span = hit["match_span"].as_object().and_then(|s| Some((s.get("start")?.as_u64()?, s.get("end")?.as_u64()?)));
    let bytes = span.map(|(s, e)| format!(" bytes {s}-{e}")).unwrap_or_default();
    let hint = match hit["match_in"].as_str() {
        Some("content") => Some(format!("matched in the message text{bytes}")),
        Some("cargo") => Some(match hit["match_pointer"].as_str() {
            Some(p) => format!("matched in cargo {p}{bytes}"),
            None => "matched in cargo (the field was not located)".to_string(),
        }),
        Some("tags") => Some("matched in tags only".to_string()),
        _ => None,
    };
    (out, hint)
}

/// Messages under one text budget: the first always whole, then whole while the budget lasts, else by header.
pub fn msgs_value(list: &[Value], now: i64, budget: usize) -> Vec<Value> {
    let mut spent = 0usize;
    list.iter().map(|o| {
        let len = o["content"].as_str().map_or(0, str::len);
        let whole = spent == 0 || spent + len <= budget;
        if whole {
            spent += len;
        }
        msg_value(o, now, whole, false)
    }).collect()
}

/// One message as text: a header line, then the text whole, or its size and how to fetch it.
pub fn msg_render(m: &Value) -> String {
    let mut head = format!("── #{} · {} · {} ({})", m["room"].as_str().unwrap_or("?"), m["author"].as_str().unwrap_or("?"),
        m["at"].as_str().unwrap_or("?"), m["age"].as_str().unwrap_or(""));
    if let Some(i) = m["intent"].as_str() {
        head += &format!(" · {i}");
    }
    if let Some(a) = m["ask"].as_str() {
        head += &format!(" · ask {a}");
    }
    let to: Vec<&str> = m["to"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !to.is_empty() {
        head += &format!(" · to {}", to.join(","));
    }
    // Open is what every live message is; only a state that changes how to read it is printed.
    if let Some(state) = m["state"].as_str().filter(|s| *s != "open") {
        head += &format!(" · {state}");
    }
    if let Some(answer) = m["answered_by"].as_str() { head += &format!(" · answered by {answer}"); }
    if m["approved"].as_bool() == Some(true) { head += " · ✓ approved"; }
    head += &format!(" · {}\n", m["id"].as_str().unwrap_or("?"));
    let links: Vec<&str> = m["links"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !links.is_empty() {
        head += &format!("   {}\n", links.join(" "));
    }
    // The other tags, once: what the header already says (intent, ask, to) and the links above are not repeated.
    let rest: Vec<&str> = m["tags"].as_array().into_iter().flatten().filter_map(Value::as_str)
        .filter(|t| !links.contains(t) && !["intent:", "ask:", "to:"].iter().any(|p| t.starts_with(p))).collect();
    if !rest.is_empty() {
        head += &format!("   tags: {}\n", rest.join(" "));
    }
    match m["text"].as_str() {
        Some(t) => head += &format!("{}\n", t.trim_end()),
        None => head += &format!("   ({} bytes, past this answer's budget: oracle_get {})\n", m["bytes"], m["id"].as_str().unwrap_or("?")),
    }
    match (m["cargo"].as_str(), m["cargo_bytes"].as_u64()) {
        (Some(c), _) => head += &format!("cargo: {c}\n"),
        (None, Some(n)) => head += &format!("   cargo: {n} bytes (oracle_get {})\n", m["id"].as_str().unwrap_or("?")),
        _ => {}
    }
    head
}

/// The Oracle's clock for an answer: its `now`, or this machine's when the Oracle does not say.
fn oracle_now(door: &dyn Door) -> i64 {
    door.get("/whoami").ok().and_then(|w| w["server_ms"].as_i64()).unwrap_or_else(now_ms)
}

/// A typed answer for the read tools: the object (structuredContent) and its text.
#[derive(Debug)]
pub struct Typed {
    pub value: Value,
    pub text: String,
}

/// oracle_read, oracle_get, oracle_thread, oracle_recall (and its old names) as typed answers. None for other tools.
pub fn typed_read(door: &dyn Door, name: &str, a: &Value) -> Option<Result<Typed>> {
    let run = || -> Result<Typed> {
        let now = oracle_now(door);
        let clock = format!("now {} (Oracle clock, UTC){}", oracle_proto::iso_ms(now), local_suffix(&Value::Null, now));
        Ok(match name {
            "oracle_read" => {
                let room = text_of(a, "room")?;
                let mut path = format!("/search?channel={}&limit={}", enc(&room), a["limit"].as_i64().unwrap_or(20));
                for k in ["tag", "starts"] {
                    if let Some(x) = a[k].as_str() {
                        path += &format!("&{k}={}", enc(x));
                    }
                }
                if let Some(before) = a["before"].as_i64() { path += &format!("&before={before}"); }
                if a["scaffold"].as_bool() == Some(true) { path += "&scaffold=true"; }
                let page = door.get(&path)?;
                let mut items = page["items"].as_array().cloned().unwrap_or_default();
                items.reverse();
                let msgs = msgs_value(&items, now, READ_TEXT_BUDGET);
                let mut text = format!("#{room} · {} messages, oldest first · {clock}\n{}", msgs.len(), msgs.iter().map(msg_render).collect::<String>());
                // #cortex is read as its readings; the graph's links, verdicts and concepts are counted, one call away.
                if let Some(n) = page["folded"]["scaffold"].as_i64().filter(|n| *n > 0) {
                    text += &format!("{n} graph records folded (links, verdicts, concepts): oracle_read room={room} scaffold=true\n");
                }
                if let Some(before) = page["next_before"].as_i64() {
                    text += &format!("Older messages: oracle_read room={room} before={before}\n");
                }
                Typed { value: json!({"now": oracle_proto::iso_ms(now), "room": room, "messages": msgs,
                    "next_before": page["next_before"], "scanned": page["scanned"], "folded": page["folded"]["scaffold"]}), text }
            }
            "oracle_get" => {
                let o = door.get(&format!("/search?id={}", enc(&text_of(a, "id")?)))?;
                let m = msg_value(&o, now, true, true);
                Typed { text: format!("{clock}\n{}", msg_render(&m)), value: json!({"now": oracle_proto::iso_ms(now), "message": m}) }
            }
            "oracle_file" => {
                let op = text_of(a, "op")?;
                match op.as_str() {
                    "list" => {
                        let room = text_of(a, "room")?;
                        let limit = a["limit"].as_i64().unwrap_or(50).clamp(1, 200);
                        let v = door.get(&format!("/files?room={}&limit={limit}", enc(&room)))?;
                        let files = v["files"].as_array().cloned().unwrap_or_default();
                        let mut text = format!("FILES in #{room} · {} · {clock}\n", files.len());
                        for f in &files {
                            text += &format!("  {}  {}  {} bytes  {}\n", f["id"].as_str().unwrap_or("?"), f["content_type"].as_str().unwrap_or("?"), f["size"], f["name"].as_str().unwrap_or(""));
                        }
                        Typed { value: json!({"now": oracle_proto::iso_ms(now), "room": room, "files": files}), text }
                    }
                    "meta" => {
                        let id = file_id(&text_of(a, "id")?);
                        let v = door.get(&format!("/files/{id}/meta"))?;
                        Typed { text: format!("FILE {id} · {} · {} bytes · rooms {} · cited by {}\n{clock}", v["content_type"].as_str().unwrap_or("?"), v["size"], v["rooms"], v["cited_by"]), value: v }
                    }
                    "get" => {
                        let id = file_id(&text_of(a, "id")?);
                        let max = a["max"].as_u64().unwrap_or(60_000).clamp(1_000, 400_000) as usize;
                        let (ctype, bytes) = door.get_bytes(&format!("/files/{id}"))?;
                        let sha = oracle_proto::sha_hex(&bytes);
                        let textual = ctype.starts_with("text/") || ctype.contains("json") || ctype.contains("markdown") || ctype.contains("xml") || ctype.contains("yaml");
                        if !textual {
                            return Ok(Typed { text: format!("FILE {id} · {ctype} · {} bytes · sha256 {sha} · not text: read it with ox file get\n{clock}", bytes.len()),
                                value: json!({"now": oracle_proto::iso_ms(now), "id": id, "content_type": ctype, "size": bytes.len(), "sha256": sha, "text": Value::Null}) });
                        }
                        let whole = String::from_utf8_lossy(&bytes).into_owned();
                        let chars = whole.chars().count();
                        let text: String = whole.chars().take(max).collect();
                        let more = chars.saturating_sub(max);
                        let head = format!("FILE {id} · {ctype} · {} bytes · {chars} chars{} · {clock}\n", bytes.len(), if more > 0 { format!(" · first {max} shown, {more} more (raise max)") } else { String::new() });
                        Typed { text: format!("{head}{text}"), value: json!({"now": oracle_proto::iso_ms(now), "id": id, "content_type": ctype, "size": bytes.len(), "sha256": sha, "chars": chars, "shown": chars.min(max), "text": text}) }
                    }
                    other => anyhow::bail!("op must be list, meta or get, not {other}"),
                }
            }
            "oracle_thread" => {
                let t = door.get(&format!("/threads?id={}", enc(&text_of(a, "id")?)))?;
                let mut list = vec![t["root"].clone()];
                list.extend(t["replies"].as_array().cloned().unwrap_or_default());
                let msgs = msgs_value(&list, now, READ_TEXT_BUDGET);
                let summary = json!({"status": t["card"]["status"], "state": t["state"], "floor": t["floor"]["name"], "replies": t["card"]["replies"],
                    "outcome": t["outcome"], "summary": t["summary"]});
                let mut text = format!("THREAD {} · {} · floor {} · {} replies · {clock}\n", t["card"]["status"].as_str().unwrap_or("?"),
                    t["state"].as_str().unwrap_or("?"), t["floor"]["name"].as_str().unwrap_or("?"), t["card"]["replies"]);
                for (label, key) in [("OUTCOME", "outcome"), ("SUMMARY", "summary")] {
                    if let Some(x) = t[key]["text"].as_str() {
                        text += &format!("{label} {} — {x}\n", t[key]["author"].as_str().unwrap_or("?"));
                    }
                }
                text += &msgs.iter().map(msg_render).collect::<String>();
                Typed { value: json!({"now": oracle_proto::iso_ms(now), "thread": summary, "messages": msgs}), text }
            }
            "oracle_recall" | "oracle_query" | "oracle_about" => {
                let (q, room) = match name {
                    "oracle_about" => (text_of(a, "terms")?, Some(text_of(a, "room")?)),
                    "oracle_query" => (text_of(a, "query")?, None),
                    _ => (text_of(a, "query")?, a["room"].as_str().map(str::to_owned)),
                };
                let limit = a["limit"].as_i64().unwrap_or(if name == "oracle_about" { 15 } else { 10 });
                let v = door.get(&format!("/search?q={}&limit={limit}{}", enc(&q), room.as_ref().map(|r| format!("&channel={}", enc(r))).unwrap_or_default()))?;
                let hits = v["hits"].as_array().cloned().unwrap_or_default();
                // Gists are the default for oracle_recall (2026-10-10, 95 questions, one caller, all searches with meaning:
                // picks from gists 51 right first vs 47 from whole texts, at about a third of the bytes); gist: false for whole texts.
                // The server's word on whether anything is on record at all (Round 50 ask 6); `found` goes unsaid.
                let prior = v["prior"].clone();
                let prior_line = prior["says"].as_str().filter(|_| prior["verdict"] != "found").map(|s| format!("{s}\n")).unwrap_or_default();
                if a["gist"] != false && name == "oracle_recall" {
                    let engine = v["engine"].as_str().unwrap_or("").to_string();
                    let gists: Vec<Value> = hits.iter().map(gist_of).collect();
                    let text = if gists.is_empty() {
                        format!("no hits for \"{q}\" · {clock}\n{}", if prior_line.is_empty() { "Try other words before concluding there is no record.\n".to_string() } else { prior_line.clone() })
                    } else {
                        format!("{} hits for \"{q}\"{} · {clock} · pick one, open it whole with oracle_get <id>\n{prior_line}{}", gists.len(),
                            if engine.is_empty() { String::new() } else { format!(" ({engine})") },
                            gists.iter().enumerate().map(|(i, g)| gist_render(i + 1, g)).collect::<String>())
                    };
                    return Ok(Typed { value: json!({"now": oracle_proto::iso_ms(now), "query": q, "room": room, "engine": engine, "prior": prior, "gists": gists}), text });
                }
                let mut msgs = msgs_value(&hits, now, READ_TEXT_BUDGET);
                for (m, h) in msgs.iter_mut().zip(&hits) {
                    m["score"] = h["score"].clone();
                    if h["edges"].is_object() {
                        m["edges"] = h["edges"].clone();
                    }
                    // Where the words matched, verbatim from the backend (cargo hits carry the pointer and the byte span of the ORIGINAL field).
                    let (loc, hint) = match_location(h);
                    for (k, v) in loc {
                        m[k] = v;
                    }
                    if let Some(hint) = hint {
                        m["match_hint"] = json!(hint);
                    }
                }
                let engine = v["engine"].as_str().unwrap_or("").to_string();
                let text = if msgs.is_empty() {
                    format!("no hits for \"{q}\" · {clock}\nTry other words before concluding there is no record.\n")
                } else {
                    format!("{} hits for \"{q}\"{} · {clock}\n{}", msgs.len(), if engine.is_empty() { String::new() } else { format!(" ({engine})") },
                        msgs.iter().map(|m| format!("{:.2} {}{}", m["score"].as_f64().unwrap_or(0.0), msg_render(m),
                            m["match_hint"].as_str().map(|h| format!("   [{h}]\n")).unwrap_or_default())).collect::<String>())
                };
                let text = format!("{prior_line}{text}");
                Typed { value: json!({"now": oracle_proto::iso_ms(now), "query": q, "room": room, "engine": engine, "prior": prior, "hits": msgs}), text }
            }
            _ => unreachable!(),
        })
    };
    let mode = name == "oracle_recall" && (a["where"].as_bool() == Some(true) || a["terms"].as_bool() == Some(true));
    (matches!(name, "oracle_read" | "oracle_get" | "oracle_thread" | "oracle_recall" | "oracle_query" | "oracle_about" | "oracle_file") && !mode).then(run)
}

/// The characters of a gist's passage: enough to judge a hit, small enough that ten fit in one screen.
pub const GIST_CHARS: usize = 240;

/// One search hit as a short entry to choose from (Round 66: a caller picking from
/// the top 10 put the right answer first 11 more times in 95 than the server's order): id, room, signer, date, what
/// replaced it, how many copies it stands for, and the passage that matched (the matched span with some context when
/// the server located it in the text, else the opening), on one line.
pub fn gist_of(h: &Value) -> Value {
    let content = h["content"].as_str().unwrap_or("");
    let span = (h["match_in"] == "content").then(|| h["match_span"].as_object().and_then(|s| Some((s.get("start")?.as_u64()? as usize, s.get("end")?.as_u64()? as usize)))).flatten();
    let floor = |i: usize| (0..=i.min(content.len())).rev().find(|j| content.is_char_boundary(*j)).unwrap_or(0);
    let (from, lead) = match span {
        Some((start, _)) if start > 80 => (floor(start - 80), "… "),
        _ => (0, ""),
    };
    // Start on a word, not inside one ("… xxis.world"): skip to the next space when one is near.
    let from = if from > 0 { content[from..].find(char::is_whitespace).filter(|k| *k < 40).map_or(from, |k| from + k) } else { from };
    let passage: String = content[from..].split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = passage.chars().take(GIST_CHARS).collect();
    let more = if cut.chars().count() < passage.chars().count() { " …" } else { "" };
    let mut g = json!({"id": h["id"], "room": h["channel"], "author": h["author"],
        "date": h["created_ms"].as_i64().map(|ms| oracle_proto::iso_ms(ms)[..10].to_string()),
        "passage": format!("{lead}{cut}{more}"), "bytes": content.len()});
    // The row (docs/SEARCH.md, S1): class, title, clean snippet, thread, author kind, synthetic, when the server sends them.
    for k in ["class", "title", "snippet", "author_kind"] {
        if let Some(v) = h[k].as_str() { g[k] = json!(v); }
    }
    if h["thread"].is_object() { g["thread"] = h["thread"].clone(); }
    if h["synthetic"] == true { g["synthetic"] = json!(true); }
    if let Some(snip) = h["snippet"].as_str().filter(|s| !s.is_empty()) { g["passage"] = json!(snip); }
    if let Some(n) = h["superseded_by"].as_str() {
        g["superseded_by"] = json!(n);
    }
    if let Some(c) = h["same_as"].as_array().filter(|c| !c.is_empty()) {
        g["copies"] = json!(c.len());
    }
    if let Some(l) = h["like"].as_array().filter(|l| !l.is_empty()) {
        g["like"] = json!(l);
    }
    if h["edges"].is_object() {
        g["edges"] = h["edges"].clone();
    }
    // Approved by Blaze (Brain cases): marked, with its labels, and its note as the passage, the reading he approved in
    // a few lines (31 Oct 2026: approved readings are "the first most important things that show up in Oracle").
    if h["approved"] == true {
        g["approved"] = json!(true);
        if let Some(l) = h["brain"]["labels"].as_array().filter(|l| !l.is_empty()) {
            g["labels"] = json!(l);
        }
        if let Some(n) = h["brain"]["note"].as_str().filter(|n| !n.trim().is_empty()) {
            let n: String = n.split_whitespace().collect::<Vec<_>>().join(" ");
            let cut: String = n.chars().take(GIST_CHARS).collect();
            g["passage"] = json!(if cut.chars().count() < n.chars().count() { format!("{cut} …") } else { cut });
        }
    }
    if let Some(w) = h["words"].as_array().filter(|w| w.len() == 2) {
        g["words"] = json!(w);
    }
    if let Some(c) = h["cited_by"]["count"].as_u64().filter(|c| *c > 0) {
        g["cited_by"] = json!(c);
    }
    // This object is what Claude Code hands the model (Blaze, 31 Oct 2026, "inspect the index and state tools": the snippet
    // repeated the passage and the title repeated both). Each fact once: the snippet is the passage; a title that is the
    // passage's opening, the plain message class, a model author and a thread nobody answered say nothing.
    if let Some(o) = g.as_object_mut() {
        o.remove("snippet");
        let passage = o.get("passage").and_then(Value::as_str).unwrap_or("").trim_start_matches("… ").to_string();
        let head = |x: &str| x.trim_end_matches('…').trim().chars().take(30).collect::<String>();
        let echo = |t: &str| { let (t, p) = (head(t), head(&passage)); t.is_empty() || (!p.is_empty() && (passage.contains(&t) || t.contains(&p))) };
        if o.get("title").and_then(Value::as_str).is_some_and(echo) {
            o.remove("title");
        }
        if o.get("class").and_then(Value::as_str) == Some("message") {
            o.remove("class");
        }
        if o.get("author_kind").and_then(Value::as_str) != Some("person") {
            o.remove("author_kind");
        }
        if o.get("thread").is_some_and(|t| t["replies"].as_u64().unwrap_or(0) == 0 && t["answered"] != true && t["reply_to"].is_null()) {
            o.remove("thread");
        }
    }
    g
}

/// A hit's links on one line (Round 50 ask 7): the ids it points at, and how many point back, with the newest of each.
fn edge_line(e: &Value) -> String {
    let mut out = Vec::new();
    if let Some(r) = e["reply_to"].as_str() {
        out.push(format!("answers {r}"));
    }
    for (key, word) in [("answered_by", "answered by"), ("replies", "replies"), ("cited_by", "cited by"), ("supersedes", "corrects"), ("refs", "refs")] {
        if let Some(l) = e[key].as_array().filter(|l| !l.is_empty()) {
            let newest = l.last().and_then(Value::as_str).unwrap_or("?");
            out.push(if l.len() == 1 { format!("{word} {newest}") } else { format!("{word} {} (newest {newest})", l.len()) });
        }
    }
    if let Some(n) = e["marks"].as_u64().filter(|n| *n > 0) {
        out.push(format!("{n} marks"));
    }
    if out.is_empty() { String::new() } else { format!("   ↳ {}\n", out.join(" · ")) }
}

/// One line of thread state: replies, answered, what it answers (docs/SEARCH.md).
fn thread_words(t: &Value) -> String {
    if !t.is_object() { return String::new(); }
    let mut w = Vec::new();
    match t["replies"].as_u64().unwrap_or(0) { 0 => {}, 1 => w.push("1 reply".to_string()), n => w.push(format!("{n} replies")) }
    if t["answered"] == true { w.push("answered".into()); }
    if let Some(r) = t["reply_to"].as_str() { w.push(format!("answers {r}")); }
    if w.is_empty() { String::new() } else { format!(" · {}", w.join(", ")) }
}

/// The row: `n. [class] title · #room · author (person|model) · date · thread · copies · superseded`, then the snippet.
pub fn gist_render(n: usize, g: &Value) -> String {
    // A message's title is the opening of its own text: printed once, as the passage (31 Oct 2026, every hit said its
    // first words twice). Heads, pages, claims and files keep the title that names them.
    let passage = g["passage"].as_str().unwrap_or("").trim_start_matches("… ");
    // The title is the text's opening when either holds the first 30 characters of the other (a passage cut at the match
    // starts later than the title: "No. But, so…" against "… But, so…").
    let head = |x: &str| x.trim_end_matches('…').trim().chars().take(30).collect::<String>();
    let opening = |t: &str| { let (t, p) = (head(t), head(passage)); !t.is_empty() && !p.is_empty() && (passage.contains(&t) || t.contains(&p)) };
    let plain = g["class"].as_str().is_none_or(|c| c == "message") || g["title"].as_str().is_some_and(opening);
    let class = g["class"].as_str().filter(|_| !plain).map(|c| format!("[{c}] ")).unwrap_or_default();
    let title = g["title"].as_str().filter(|t| !t.is_empty() && !plain).map(|t| format!("{t} · ")).unwrap_or_default();
    let mark = if g["approved"] == true {
        let labels: Vec<&str> = g["labels"].as_array().map(|l| l.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        format!("✓ approved{} · ", if labels.is_empty() { String::new() } else { format!(" ({})", labels.join(", ")) })
    } else {
        String::new()
    };
    let words = g["words"].as_array().and_then(|w| Some(format!(" · {}/{} words", w.first()?.as_u64()?, w.get(1)?.as_u64()?))).unwrap_or_default();
    let words = format!("{words}{}", g["cited_by"].as_u64().map(|c| format!(" · cited by {c} approved")).unwrap_or_default());
    let who = match g["author_kind"].as_str() { Some("person") => " (person)", _ => "" };
    let like = g["like"].as_array().map(|l| format!(" · +{} like it", l.len())).unwrap_or_default();
    format!("{n}. {mark}{class}{title}{} · #{} · {}{who} · {}{words}{}{}{}{like}\n   {}\n", g["id"].as_str().unwrap_or("?"), g["room"].as_str().unwrap_or("?"),
        g["author"].as_str().unwrap_or("?"), g["date"].as_str().unwrap_or("?"), thread_words(&g["thread"]),
        g["superseded_by"].as_str().map(|s| format!(" · superseded by {s}")).unwrap_or_default(),
        g["copies"].as_u64().map(|c| format!(" · +{c} copies")).unwrap_or_default(), g["passage"].as_str().unwrap_or(""))
        + &edge_line(&g["edges"])
}

// ── the tools ────────────────────────────────────────────────────────────────────────────────

fn recall(door: &dyn Door, query: &str, room: Option<&str>, limit: i64) -> Result<String> {
    recall_in(door, query, room, limit, false, None)
}

/// `expires:` from a span (`48h`, `7d`, `90m`) or a unix ms, as a unix ms (docs/ARCHIVE.md).
pub fn expires_ms(spec: &str, now_ms: i64) -> Result<i64> {
    let s = spec.trim();
    if let Ok(ms) = s.parse::<i64>() {
        anyhow::ensure!(ms > now_ms, "expires {s} is in the past");
        return Ok(ms);
    }
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = n.parse().map_err(|_| anyhow!("expires is a span (48h, 7d, 90m) or a unix ms, not {s}"))?;
    let ms = match unit {
        "m" => n * 60_000,
        "h" => n * 3_600_000,
        "d" => n * 86_400_000,
        _ => bail!("expires is a span (48h, 7d, 90m) or a unix ms, not {s}"),
    };
    anyhow::ensure!(ms > 0, "expires must be a positive span");
    Ok(now_ms + ms)
}

fn recall_in(door: &dyn Door, query: &str, room: Option<&str>, limit: i64, include_archived: bool, as_of: Option<&str>) -> Result<String> {
    let v = door.get(&format!(
        "/search?q={}&limit={limit}{}{}{}",
        enc(query),
        room.map(|r| format!("&channel={}", enc(r))).unwrap_or_default(),
        if include_archived { "&include_archived=true" } else { "" },
        as_of.filter(|a| !a.trim().is_empty()).map(|a| format!("&as_of={}", enc(a))).unwrap_or_default()
    ))?;
    let hits = v["hits"].as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        return Ok("no hits — try other words before concluding there is no record".into());
    }
    Ok(hits
        .iter()
        .map(|h| {
            format!(
                "{:.2}  {}  #{} {} — {}",
                h["score"].as_f64().unwrap_or(0.0),
                h["id"].as_str().unwrap_or(""),
                h["channel"].as_str().unwrap_or(""),
                h["author"].as_str().unwrap_or(""),
                one_line(h["content"].as_str().unwrap_or(""), 300)
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn heads_text(heads: &[Value]) -> String {
    if heads.is_empty() {
        return "no state heads".into();
    }
    heads
        .iter()
        .map(|h| {
            format!(
                "[{}] {}  owner {}  next {}{}{}",
                h["state"].as_str().unwrap_or(""),
                h["subject"].as_str().unwrap_or(""),
                h["owner"].as_str().unwrap_or(""),
                h["next_touch"].as_str().unwrap_or("—"),
                h["channel"].as_str().map(|c| format!("  #{c}")).unwrap_or_default(),
                h["note"].as_str().filter(|n| !n.is_empty()).map(|n| format!("\n    {n}")).unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What an answer left out and how to get it (Blaze's law COMPRESS, DON'T EXTEND; Rocky's live pass, 1 Nov 2026 07:15Z):
/// `rows` given, `cut` left out, `more` the call that brings them (null when nothing was cut).
pub fn budget(rows: usize, cut: usize, more: Option<String>) -> Value {
    json!({"rows": rows, "cut": cut, "more": if cut > 0 { more.map_or(Value::Null, Value::from) } else { Value::Null }})
}

/// How many heads the list form of `oracle_task` answers when no `limit` is given (Rocky's live pass, 1 Nov 2026 07:15Z:
/// `oracle_task owner=rocky limit=5` answered 129 KB, all 727 of rocky's heads; limit was read only by the registry).
pub const TASK_LIST_LIMIT: usize = 50;

/// The list form of `oracle_task`: an owner's heads (mine by default, `*` everyone's), or a room's, newest-touched first
/// (the head's record time), at most `limit` from `offset`, with the budget that says what was cut and the next page.
pub fn tasks_value(door: &dyn Door, owner: Option<&str>, room: Option<&str>, limit: usize, offset: usize) -> Result<Value> {
    // A room's heads are the server's to select (`channel=`); an owner's too; a room alone is everyone's in it.
    let mut heads = door.get(&heads_path(door.handle(), owner, room))?["heads"].as_array().cloned().unwrap_or_default();
    // Stable: heads touched in the same millisecond keep the server's order (by subject).
    heads.sort_by_key(|h| std::cmp::Reverse(h["at_ms"].as_i64().unwrap_or(0)));
    let total = heads.len();
    let page: Vec<Value> = heads.into_iter().skip(offset).take(limit).collect();
    let (shown, next) = (page.len(), offset.saturating_add(page.len()));
    let more = if next < total {
        Some(format!("offset: {next} for the next {}", limit.min(total - next)))
    } else {
        (offset > 0).then(|| "offset: 0 from the newest".to_string())
    };
    Ok(json!({"owner": owner.unwrap_or(door.handle()), "room": room, "total": total, "offset": offset, "order": "newest-touched first",
        "heads": page, "budget": budget(shown, total - shown, more)}))
}

/// `tasks_value` as text: one head a line, then what was cut and how to get it.
pub fn tasks_render(v: &Value) -> String {
    let mut out = heads_text(v["heads"].as_array().map(Vec::as_slice).unwrap_or(&[]));
    if let Some(cut) = v["budget"]["cut"].as_u64().filter(|c| *c > 0) {
        out += &format!("\n+ {cut} more heads not shown ({} of {}, newest-touched first){}", v["budget"]["rows"], v["total"],
            v["budget"]["more"].as_str().map(|m| format!(": {m}")).unwrap_or_default());
    }
    out
}

/// The list form's `limit` and `offset` from the tool's arguments.
fn task_page(a: &Value) -> (usize, usize) {
    (a["limit"].as_u64().map_or(TASK_LIST_LIMIT, |l| l.clamp(1, 200) as usize), a["offset"].as_u64().unwrap_or(0) as usize)
}

fn tasks(door: &dyn Door, owner: Option<&str>, room: Option<&str>, a: &Value) -> Result<String> {
    let (limit, offset) = task_page(a);
    Ok(tasks_render(&tasks_value(door, owner, room, limit, offset)?))
}

/// Resolve the human owner from active registry keys, never from a product-specific handle.
pub fn owner_handle(door: &dyn Door) -> Result<String> {
    owner_from_registry(&door.get("/keys")?)
}

fn owner_from_registry(registry: &Value) -> Result<String> {
    let keys = registry["keys"].as_array().ok_or_else(|| anyhow!("owner registry is missing keys"))?;
    let mut owners = std::collections::BTreeSet::new();
    for key in keys {
        if key["role"] == "owner" && key.get("revoked_ms") == Some(&Value::Null) {
            let handle = key["handle"].as_str().filter(|h| !h.is_empty())
                .ok_or_else(|| anyhow!("owner registry has no handle"))?;
            owners.insert(handle.to_owned());
        }
    }
    if owners.len() != 1 { bail!("owner registry must identify exactly one active owner; found {}", owners.len()); }
    Ok(owners.into_iter().next().unwrap())
}

/// @owner is a recipient shortcut, resolved before signing. Explicit handles need no registry read.
pub fn resolve_recipients(door: &dyn Door, mut tags: Vec<String>) -> Result<Vec<String>> {
    if tags.iter().any(|t| t == "to:@owner") {
        let owner = owner_handle(door)?;
        for tag in &mut tags {
            if tag == "to:@owner" { *tag = format!("to:{owner}"); }
        }
        let mut seen = std::collections::HashSet::new();
        tags.retain(|tag| seen.insert(tag.clone()));
    }
    Ok(tags)
}

fn say(door: &dyn Door, room: &str, text: &str, cargo: Option<String>, tags: Vec<String>, op_id: Option<String>) -> Result<String> {
    Ok(receipt(
        door.write(Kind::Say, room, text, cargo, resolve_recipients(door, tags)?, op_id)?,
        &format!("#{room}"),
    ))
}

/// Relative times belong to the original operation, not to each delivery attempt.
/// Keep the anchor in the journaled envelope so process restarts recover it too.
/// The ordinary write digest still checks every field, including changed deadlines.
fn say_clock(door: &dyn Door, args: &Value, tags: &mut Vec<String>) -> Result<i64> {
    const PREFIX: &str = "mcp-time:";
    anyhow::ensure!(!tags.iter().any(|t| t.starts_with(PREFIX)), "mcp-time is reserved for the operation journal");
    let now = now_ms();
    let Some(op) = op_of(args) else { return Ok(now) };
    if let Some(record) = door.recorded_write(&op)? {
        let stored = record["envelope"]["tags"].as_array().ok_or_else(|| anyhow!("op_id {op} has unreadable journal tags"))?;
        let clocks: Vec<&str> = stored.iter().filter_map(Value::as_str).filter_map(|t| t.strip_prefix(PREFIX)).collect();
        match clocks.as_slice() {
            [] => return Ok(now), // Legacy record: preserve its payload shape; never guess its original relative time.
            [clock] => {
                let clock: i64 = clock.parse().ok().filter(|n| *n > 0).ok_or_else(|| anyhow!("op_id {op} has an invalid time anchor"))?;
                tags.push(format!("{PREFIX}{clock}"));
                return Ok(clock);
            }
            _ => bail!("op_id {op} has multiple time anchors"),
        }
    }
    if args["by"].is_string() || args["expires"].is_string() {
        tags.push(format!("{PREFIX}{now}"));
    }
    Ok(now)
}

/// Run one tool. A failure is text the model reads, returned as `Err` so the door marks it.
/// Cargo given as JSON text, checked to be JSON, kept whole.
fn cargo_of(a: &Value) -> Result<Option<String>> {
    match &a["cargo"] {
        Value::Null => Ok(None),
        Value::String(c) => {
            serde_json::from_str::<Value>(c).map_err(|e| anyhow!("cargo is JSON text: {e}"))?;
            Ok(Some(c.clone()))
        }
        other => Ok(Some(other.to_string())),
    }
}

/// `as_of_seq` (exclusive) with `as_of` as its alias. Any present, non-null value that is not a positive integer is refused (it must never read as "latest"),
/// and both given and different is a mistake worth refusing, not a guess.
fn cursor_arg(a: &Value) -> Result<Option<i64>> {
    let one = |k: &str| -> Result<Option<i64>> {
        match &a[k] {
            Value::Null => Ok(None),
            v => match v.as_i64() {
                Some(n) if n >= 1 => Ok(Some(n)),
                _ => bail!("{k} must be a positive integer seq (exclusive), not {v}"),
            },
        }
    };
    match (one("as_of_seq")?, one("as_of")?) {
        (Some(x), Some(y)) if x != y => bail!("as_of_seq {x} and as_of {y} disagree; as_of is only an alias of as_of_seq"),
        (x, y) => Ok(x.or(y)),
    }
}

/// The receipt of a write this door already recorded for `op`, only when the request now is the SAME request. The journal fingerprints every write as
/// sha256(JSON[kind, room, text, cargo, tags]) and refuses an op_id reused for a different digest; recovery rebuilds the request the same way, from the RECORDED
/// revision's own parents and fields (never today's State), applies the requested `set`s through the same `state_revision`, renders the face from `note` (or the
/// default) and carries the recorded adoption only when `adopts` names that same proposal (absent means absent), and recovers the receipt only if the digests are
/// equal. A changed note, a changed, added or removed `adopts`, other sets or another head each give a different digest and are refused. Normalisation is exactly the
/// journal's: two requests that build the identical payload are the same request. `None`: the record never settled, so the ordinary path decides. A recorded refusal
/// replays as a refusal.
fn recovered_receipt(rec: &Value, op: &str, room: &str, sets: &[String], head: Option<&str>, note: Option<&str>, adopts: Option<&str>) -> Result<Recovery> {
    if rec["outcome"]["ok"] == json!(false) {
        bail!("{}", rec["outcome"]["refused"].as_str().unwrap_or("NOT written: refused earlier"));
    }
    // `stored`: the receipt is on record. Null: the journal never learned the outcome (the reply was lost, or the process died): the write may or may not have landed.
    let stored = rec["outcome"]["stored"].as_object();
    if stored.is_none() && !rec["outcome"].is_null() {
        return Ok(Recovery::No);
    }
    let different = || anyhow!("op_id {op} was already used for a different write (another note, adopts, fields, head or room); use a new op_id for a new write");
    let (Some(digest), Some(recorded)) = (rec["digest"].as_str(), rec["envelope"]["cargo"].as_str().and_then(|c| serde_json::from_str::<Value>(c).ok())) else {
        bail!("op_id {op} has a record whose original request cannot be recovered; refusing rather than answering with a receipt that may be another write's");
    };
    if rec["envelope"]["channel"].as_str() != Some(room) || recorded["schema"] != "room-context/1" || recorded["channel"] != room {
        return Err(different());
    }
    let parents: Vec<String> = recorded["parents"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).ok_or_else(different)?;
    if head.is_some_and(|h| parents != [h]) {
        return Err(different());
    }
    let recorded_adopts = recorded.get("adopts").filter(|a| a.is_object()).cloned();
    match (adopts, &recorded_adopts) {
        (None, None) => {}
        (Some(p), Some(a)) if a["proposal"].as_str() == Some(p) => {}
        _ => return Err(different()),
    }
    let mut rev = state_revision(room, &parents, &recorded["fields"], sets)?;
    if let Some(a) = recorded_adopts {
        rev = with_adoption(rev, a);
    }
    let face = note.map(str::to_owned).unwrap_or_else(|| format!("State updated: {}", one_line(rev["fields"]["current"].as_str().unwrap_or(""), 120)));
    let tags = vec!["room-context:v1".to_string(), "intent:status".to_string()];
    let candidate = oracle_proto::sha_hex(json!(["say", room, face, Some(rev.to_string()), tags]).to_string().as_bytes());
    if candidate != digest {
        return Err(different());
    }
    let Some(stored) = stored else {
        // Same request, outcome unknown: it goes again as the SAME journaled write (same envelope, same client_op). The server answers a delivered one as its duplicate and
        // judges one that never landed by its own guard, so the head preflight (which the first attempt may itself have moved) is not the judge here. The face is the one
        // the digest was computed over (untrimmed), not the trimmed text the envelope stores.
        return Ok(Recovery::Resend { face, cargo: rev.to_string() });
    };
    let id = stored.get("id").and_then(Value::as_str).unwrap_or("?");
    let value = json!({"room": room, "written": true, "queued": false, "id": id, "seq": stored.get("seq"), "duplicate": true, "replayed": true, "parents": parents});
    Ok(Recovery::Receipt(Typed { value, text: format!("signed:ok  {id}  (#{room} State, recovered: this op_id was already written; nothing was sent again)") }))
}

/// Send a recorded, identical State write again exactly as journaled (the digest's own face, cargo and tags) under the same op_id, so the door re-delivers its recorded envelope.
fn resend_recorded(door: &dyn Door, room: &str, op: &str, face: String, cargo: String) -> Result<Typed> {
    let tags = vec!["room-context:v1".to_string(), "intent:status".to_string()];
    let w = door.write(Kind::Say, room, &face, Some(cargo), tags, Some(op.to_string()))?;
    let value = match &w {
        Written::Stored(v) => json!({"room": room, "written": true, "queued": false, "id": v["id"], "seq": v["seq"], "duplicate": v["duplicate"], "replayed": true}),
        Written::Queued { reason, .. } => json!({"room": room, "written": false, "queued": true, "reason": reason, "replayed": true}),
    };
    Ok(Typed { value, text: receipt(w, &format!("#{room} State, same write sent again under op_id {op}")) })
}

/// What the door's journal says about a State write being asked for again.
enum Recovery {
    /// Nothing recorded (or not settled in a way this can use): the ordinary path decides.
    No,
    /// Recorded and identical: its receipt, nothing sent.
    Receipt(Typed),
    /// Recorded, identical, outcome unknown: send the same journaled write again and let the server's dedup and guard answer.
    Resend { face: String, cargo: String },
}

/// Where a look back starts (`ox delta --since`, `oracle_delta`): `30m`, `6h`, `2d` or `1w` back from `now`, or what
/// `parse_by` reads as a time (a unix time, `2026-10-31[T08:00]` UTC; seconds and beyond are dropped). An OBS id is the
/// caller's to resolve.
pub fn parse_since_at(s: &str, now: i64) -> Result<i64> {
    let s = s.trim();
    for (suffix, unit) in [("m", 60_000), ("h", 3_600_000), ("d", 86_400_000), ("w", 7 * 86_400_000)] {
        if let Some(n) = s.strip_suffix(suffix).and_then(|n| n.parse::<i64>().ok()) {
            return Ok(now - n * unit);
        }
    }
    let at = parse_by_at(s, now).map_err(|_| anyhow::anyhow!("since is an OBS id, a time like 2026-10-31T08:00 (UTC) or a span like 6h, 2d"))?;
    if at > now {
        anyhow::bail!("since {s} is in the future");
    }
    Ok(at)
}

/// When an ask is due: `90m`, `2h`, `3d`, a unix time (s or ms), or `2026-10-01[T15:30]` UTC. Shared by `ox say --by` and
/// `oracle_say` `by`, so a seat on the MCP sets the same expiry the CLI does (Bobby and Taylor, 18 Oct).
pub fn parse_by(s: &str) -> Result<i64> {
    parse_by_at(s, now_ms())
}

fn parse_by_at(s: &str, now: i64) -> Result<i64> {
    if let Some(n) = s.strip_suffix('m').and_then(|n| n.parse::<i64>().ok()) {
        return Ok(now + n * 60_000);
    }
    if let Some(n) = s.strip_suffix('h').and_then(|n| n.parse::<i64>().ok()) {
        return Ok(now + n * 3_600_000);
    }
    if let Some(n) = s.strip_suffix('d').and_then(|n| n.parse::<i64>().ok()) {
        return Ok(now + n * 86_400_000);
    }
    if let Ok(n) = s.parse::<i64>() {
        return Ok(if n < 100_000_000_000 { n * 1000 } else { n });
    }
    // A date, `2026-10-01` or `2026-10-01T15:30` (UTC), without pulling in a calendar crate.
    let (date, time) = s.split_once('T').unwrap_or((s, "00:00"));
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let mut t = time.trim_end_matches('Z').split(':').map(|x| x.parse::<i64>());
    match (d.next(), d.next(), d.next(), t.next(), t.next()) {
        (Some(Ok(y)), Some(Ok(mo)), Some(Ok(da)), Some(Ok(h)), Some(Ok(mi)))
            if (1..=12).contains(&mo) && (1..=31).contains(&da) && h < 24 && mi < 60 =>
        {
            // Days since the epoch (proleptic Gregorian), the civil-from-days algorithm.
            let (y2, m2) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
            let era = y2.div_euclid(400);
            let yoe = y2 - era * 400;
            let doy = (153 * m2 + 2) / 5 + da - 1;
            let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
            let days = era * 146_097 + doe - 719_468;
            Ok((days * 86_400 + h * 3_600 + mi * 60) * 1000)
        }
        _ => anyhow::bail!("--by must be like 2h, 3d, 2026-10-01 or 2026-10-01T15:30, not {s}"),
    }
}

/// The rooms a seat's State names in its `rooms` field: every `#room` token, in order, once, at most 12 (docs/ROOM-MAP.md).
pub fn rooms_named(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tok in text.split(char::is_whitespace) {
        let Some(t) = tok.trim_start_matches(['(', '[', '"', '\'']).strip_prefix('#') else { continue };
        let name: String = t.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
        if out.len() == 12 {
            break;
        }
    }
    out
}

/// A room's map as text: the line, the written map whole, then the index facts the line left out.
pub fn room_map_text(v: &Value) -> String {
    let room = v["room"].as_str().unwrap_or("?");
    let mut out = format!("{}\n", v["line"].as_str().unwrap_or("?"));
    if let Some(w) = v["written"].as_object() {
        let text = w["text"].as_str().unwrap_or("");
        let rest: Vec<&str> = text.lines().skip_while(|l| l.trim().is_empty()).skip(1).collect();
        for l in rest {
            out += &format!("  {l}\n");
        }
        out += &format!("  — map {} by {}{}\n", w["head"].as_str().unwrap_or("?"), w["by"].as_str().unwrap_or("?"),
            w["at_ms"].as_i64().map(|t| format!(" · {}", &oracle_proto::iso_ms(t)[..10])).unwrap_or_default());
    } else {
        out += &format!("  write one: ox map {room} --set \"what the room holds · its heads · what not to read\"\n");
    }
    let c = &v["computed"];
    let mut facts = Vec::new();
    if let Some(f) = c["first_ms"].as_i64() {
        facts.push(format!("first {}", &oracle_proto::iso_ms(f)[..10]));
    }
    if let Some(fs) = c["state_fields"].as_array().filter(|f| !f.is_empty()) {
        facts.push(format!("State fields filled: {}", fs.iter().filter_map(|f| f.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if let Some(ws) = c["words"].as_array().filter(|w| !w.is_empty()) {
        facts.push(format!("words by count: {}", ws.iter().map(|w| format!("{} {}", w["word"].as_str().unwrap_or("?"), w["n"])).collect::<Vec<_>>().join(", ")));
    }
    if !facts.is_empty() {
        out += &format!("  {}\n", facts.join(" · "));
    }
    out
}

/// oracle_room_map: read a room's map, or write it (one State revision through the same guard as oracle_room_state).
fn room_map_call(door: &dyn Door, a: &Value) -> Result<String> {
    let room = text_of(a, "room")?;
    match a["set"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(text) => Ok(room_state(door, &room, &[format!("map={text}")], Some("Map updated"), a["head"].as_str(), None, None, op_of(a))?.text),
        None => Ok(room_map_text(&door.get(&format!("/rooms/{}/map", enc(&room)))?)),
    }
}

/// oracle_room_state as a typed answer: the structured State (or the write's receipt) beside the readable text.
fn room_state_typed(door: &dyn Door, a: &Value) -> Result<Typed> {
    let room = text_of(a, "room")?;
    let pick = a["fields"].as_str().map(field_names).filter(|n| !n.is_empty());
    if pick.is_some() && !list_of(a, "set").is_empty() {
        bail!("fields picks what a read returns; a write (set) answers with its receipt");
    }
    let mut t = room_state(door, &room, &list_of(a, "set"), a["note"].as_str(), a["head"].as_str(), a["adopts"].as_str(), cursor_arg(a)?, op_of(a))?;
    let read = list_of(a, "set").is_empty();
    if read && a["detail"].as_bool() != Some(true) {
        t.value = compact_state(&t.value);
    }
    // A room with no State still has an index: say what it holds, so an arriving agent is not left with nothing (31 Oct 2026, #illuminati).
    if read && t.value["heads"].as_array().is_some_and(Vec::is_empty) {
        if let Ok(line) = room_map_call(door, &json!({"room": room})) {
            if let Some(first) = line.lines().next().filter(|l| !l.trim().is_empty()) {
                t.value["index"] = json!(first.trim());
                t.text += &format!("index: {}\n", first.trim());
            }
        }
    }
    if let (true, Some(names)) = (read, pick) {
        t = pick_fields(&t, &names);
    }
    Ok(t)
}

/// `fields: "current,next"` as names: trimmed, once each, in the order given.
fn field_names(s: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for n in s.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if !out.iter().any(|o| o == n) {
            out.push(n.to_string());
        }
    }
    out
}

/// A name the State vocabulary knows (`state_revision` accepts it, or the writer stamps it).
fn is_state_field(k: &str) -> bool {
    STATE_FIELDS.contains(&k) || SEAT_FIELDS.contains(&k) || CONTINUATION_FIELDS.contains(&k) || PUBLIC_FIELDS.contains(&k)
        || [MAP_FIELD, LATEST_AT, ARCHIVED_FIELD].contains(&k)
}

/// Only the named fields of a State read (Rocky's live pass, 1 Nov 2026 07:15Z: oracle_room_state returned every field with
/// no way to pick; the #rocky State answered 20 KB for a caller that wanted `next`). The room, its heads and revision count
/// come too, and the state word; a known field that is not set is null, a name the State does not know is listed in
/// `unknown_fields`. A State that is not trustworthy or not complete says so (read it without fields for the diagnosis).
fn pick_fields(t: &Typed, names: &[String]) -> Typed {
    let v = &t.value;
    let mut fields = serde_json::Map::new();
    let mut unknown: Vec<String> = vec![];
    for n in names {
        match v["fields"].get(n.as_str()) {
            Some(x) => {
                fields.insert(n.clone(), x.clone());
            }
            None if is_state_field(n) => {
                fields.insert(n.clone(), Value::Null);
            }
            None => unknown.push(n.clone()),
        }
    }
    let mut out = json!({"room": v["room"], "state": v["state"], "heads": v["heads"], "revisions": v["revisions"], "fields": fields});
    let mut text = t.text.lines().next().map(|l| format!("{l}\n")).unwrap_or_default();
    for k in ["trustworthy", "complete"] {
        if v[k].as_bool() == Some(false) {
            out[k] = json!(false);
            text += &format!("NOT {}: read it without fields for the diagnosis\n", if k == "complete" { "COMPLETE" } else { "TRUSTWORTHY" });
        }
    }
    if v["index"].is_string() {
        out["index"] = v["index"].clone();
    }
    for (k, x) in names.iter().filter_map(|n| fields.get(n.as_str()).map(|x| (n, x))) {
        text += &match x.as_str() {
            Some(s) if !s.is_empty() => format!("{k}: {s}\n"),
            _ => format!("{k}: (not set)\n"),
        };
    }
    if !unknown.is_empty() {
        text += &format!("unknown fields: {} (a State's fields are {}; a seat's also {})\n", unknown.join(", "), STATE_FIELDS.join(" "), SEAT_FIELDS.join(" "));
        out["unknown_fields"] = json!(unknown);
    }
    Typed { value: out, text }
}

/// The State a model needs (Blaze, 31 Oct 2026: an unset State came back as 1.6 KB of trust plumbing): the room, its head,
/// the fields that are set and what to do. The door's diagnostics (coverage, evidence, client verification, cursors) stay
/// whole when something is wrong (untrustworthy, incomplete) and when the caller asks with `detail: true`.
fn compact_state(v: &Value) -> Value {
    let healthy = v["trustworthy"].as_bool() != Some(false) && v["complete"].as_bool() != Some(false) && v["coverage"]["sql_complete"].as_bool() != Some(false)
        && v["bad"].as_array().is_none_or(Vec::is_empty) && v["unknown"].as_array().is_none_or(Vec::is_empty);
    if !healthy {
        return v.clone();
    }
    // The typed contract stays (Bobby and Emmy, 3 Oct: a harness tells an empty State from an unparsed answer by these).
    let mut out = json!({"room": v["room"], "state": v["state"], "heads": v["heads"], "revisions": v["revisions"], "complete": v["complete"],
        "trustworthy": v["trustworthy"], "as_of_seq": v["as_of_seq"]});
    let fields: serde_json::Map<String, Value> = v["fields"].as_object().into_iter().flatten()
        .filter(|(_, x)| x.as_str().is_some_and(|s| !s.is_empty()) || x.as_array().is_some_and(|a| !a.is_empty()))
        .map(|(k, x)| (k.clone(), x.clone())).collect();
    if !fields.is_empty() {
        out["fields"] = Value::Object(fields);
    }
    if v["adoptions"].as_object().is_some_and(|a| !a.is_empty()) {
        out["adoptions"] = v["adoptions"].clone();
    }
    let o = &v["orientation"];
    if o.is_object() {
        out["orientation"] = json!({"status": o["status"], "action": o["action"], "missing_fields": o["missing_fields"]});
        if o["missing_fields"].as_array().is_some_and(Vec::is_empty) {
            out["orientation"].as_object_mut().map(|m| m.remove("missing_fields"));
        }
    }
    out
}

/// Read a room's State, or write one revision of it (the same revision `ox state <room> --set` writes).
#[allow(clippy::too_many_arguments)]
fn room_state(door: &dyn Door, room: &str, sets: &[String], note: Option<&str>, head: Option<&str>, adopts: Option<&str>, as_of: Option<i64>, op_id: Option<String>) -> Result<Typed> {
    if as_of.is_some() && (!sets.is_empty() || adopts.is_some() || head.is_some()) {
        bail!("a State at a cursor is read only: write against the current head");
    }
    if adopts.is_some() && sets.is_empty() {
        bail!("adopts says why a revision exists: say what the revision changes too (set current=\"...\")");
    }
    // A replay of a write that already landed (same caller op_id) is answered from the door's own journal BEFORE the head is judged: the State has moved on since,
    // so the preflight below would refuse the very write it is being asked to recover. This is receipt recovery, not a way round the guard: nothing is sent, and an
    // op_id that recorded a DIFFERENT write is refused, never answered with the other write's receipt.
    if let (false, Some(op)) = (sets.is_empty(), op_id.as_deref()) {
        if let Some(rec) = door.recorded_write(op)? {
            match recovered_receipt(&rec, op, room, sets, head, note, adopts)? {
                Recovery::Receipt(t) => return Ok(t),
                Recovery::Resend { face, cargo } => return resend_recorded(door, room, op, face, cargo),
                Recovery::No => {}
            }
        }
    }
    let st = match as_of {
        Some(c) => door.state_as_of(room, c)?,
        None => door.state(room)?,
    };
    if st.is_null() {
        bail!("this door cannot read a room's State{}; use the seat's own ox mcp", if as_of.is_some() { " at a cursor" } else { "" });
    }
    let heads: Vec<String> = st["heads"].as_array().into_iter().flatten().filter_map(|h| h.as_str().map(str::to_owned)).collect();
    let bad: Vec<Value> = st["bad"].as_array().cloned().unwrap_or_default();
    let unknown: Vec<Value> = st["unknown"].as_array().cloned().unwrap_or_default();
    // Trust and coverage are different failures and are told apart: a door that does not say (the older client read) is judged by `bad`/`complete` alone.
    let trustworthy = st["trustworthy"].as_bool().unwrap_or(bad.is_empty() && unknown.is_empty());
    let sql_complete = st["coverage"]["sql_complete"].as_bool().or(st["complete"].as_bool()).unwrap_or(true);
    if sets.is_empty() {
        let at = as_of.map(|c| format!(" as of seq {c}")).unwrap_or_default();
        let mut out = match heads.len() {
            0 => format!("#{room} has no signed State yet{at}\n"),
            1 => format!("#{room} State head {} · {} revision(s){at}\n", heads[0], st["revisions"]),
            n => format!("#{room} State has {n} concurrent heads ({}); a write merges them{at}\n", heads.join(", ")),
        };
        if !sql_complete {
            out += "WARNING: the State listing was cut short; this head may not be the newest\n";
        }
        if !trustworthy {
            out += &format!("NOT TRUSTWORTHY: {} revision(s) do not verify and {} could not be judged; this State is shown for diagnosis, not to be written on\n", bad.len(), unknown.len());
            for b in bad.iter().chain(unknown.iter()) {
                out += &format!("  {}: {}\n", b["id"].as_str().unwrap_or("?"), b["why"].as_str().unwrap_or("?"));
            }
        }
        for h in &heads {
            let a = &st["adoptions"][h.as_str()];
            if a.is_object() {
                out += &format!("adopted: proposal {} accepted by {} ({}; mark {} at seq {})\n", a["proposal"].as_str().unwrap_or("?"), a["decided_by"].as_str().unwrap_or("?"), a["basis"].as_str().unwrap_or("basis unstated"), a["mark"].as_str().unwrap_or("?"), a["cursor"]);
            } else {
                out += "adopted: no (unadopted revision)\n";
            }
        }
        for (k, v) in st["fields"].as_object().into_iter().flatten() {
            if let Some(v) = v.as_str().filter(|v| !v.is_empty()) {
                out += &format!("{k}: {v}\n");
            }
        }
        // The structured answer is the door's whole answer (coverage, trust, head times, evidence, diagnostics), from the same response the text was made from.
        let mut value = st.clone();
        value["orientation"] = state::orientation(&st);
        out += &state::orientation_text(&value["orientation"]);
        value["room"] = json!(room);
        value["heads"] = json!(heads);
        if value["as_of_seq"].is_null() {
            value["as_of_seq"] = json!(as_of);
        }
        if value["trustworthy"].is_null() {
            value["trustworthy"] = json!(trustworthy);
        }
        return Ok(Typed { value, text: out });
    }
    // Only a WRITE refuses on a State it cannot trust, and it says which failure it is.
    if !trustworthy {
        bail!("#{room} State is not trustworthy: {} revision(s) do not verify, {} could not be judged. Read it (without set) for the diagnosis and reconcile it before writing.", bad.len(), unknown.len());
    }
    if !sql_complete {
        bail!("#{room} State could not be read completely (the listing was cut short); refusing to write on a partial chain");
    }
    // Fail closed on contradictory flags: an explicit complete:false stops a write even when coverage and trust both say fine.
    if st["complete"].as_bool() == Some(false) {
        bail!("#{room} State says it is not complete; refusing to write (read it again, without set, for the diagnosis)");
    }
    if let Some(h) = head {
        if heads.len() != 1 || heads[0] != h {
            bail!("#{room} State moved since your read (now {}): read it again, then write", if heads.is_empty() { "empty".into() } else { heads.join(", ") });
        }
    }
    let mut rev = state_revision(room, &heads, &st["fields"], sets)?;
    if let Some(p) = adopts {
        rev = with_adoption(rev, adoption(door, room, p)?);
    }
    let face = note.map(str::to_owned).unwrap_or_else(|| format!("State updated: {}", one_line(rev["fields"]["current"].as_str().unwrap_or(""), 120)));
    let w = door.write(Kind::Say, room, &face, Some(rev.to_string()), vec!["room-context:v1".into(), "intent:status".into()], op_id)?;
    let value = match &w {
        Written::Stored(v) => json!({"room": room, "written": true, "queued": false, "id": v["id"], "seq": v["seq"], "duplicate": v["duplicate"], "parents": heads}),
        Written::Queued { reason, .. } => json!({"room": room, "written": false, "queued": true, "reason": reason, "parents": heads}),
    };
    let text = receipt(w, &format!("#{room} State ← {}", if heads.is_empty() { "new".into() } else { heads.join(", ") }));
    Ok(Typed { value, text })
}

pub fn call(door: &dyn Door, name: &str, a: &Value) -> Result<String> {
    if let Some(routed) = verb_route(door, name, a) {
        let (tool, args) = routed?;
        return call(door, &tool, &args);
    }
    Ok(match name {
        // The modes of the fold (docs/mcp.md): a flag picks what was its own tool; they come first so the guard wins.
        "oracle_recall" if a["where"].as_bool() == Some(true) => call(door, "oracle_where", a)?,
        "oracle_recall" if a["terms"].as_bool() == Some(true) => call(door, "oracle_terms", a)?,
        "oracle_recall" if a["answerable"].as_bool() == Some(true) => {
            let query = text_of(a, "query")?;
            answerable_text(&door.get(&answerable_path(&query, a["room"].as_str()))?)
        }
        "oracle_task" if a["exp"].as_bool() == Some(true) => experiments_text(&door.get(&experiments_path(a))?),
        "oracle_task" if a["subject"].as_str().is_none_or(|s| s.trim().is_empty()) => tasks(door, a["owner"].as_str(), a["room"].as_str(), a)?,
        "oracle_react" if a["mark"].as_str() == Some("archive") => call(door, "oracle_archive", a)?,
        "oracle_cortex" if matches!(a["op"].as_str(), Some("mint") | Some("edge") | Some("status") | Some("source")) => call(door, "oracle_claim", a)?,
        "oracle_cortex" if a["op"].as_str() == Some("list") => call(door, "oracle_claims", a)?,
        "oracle_cortex" if a["op"].as_str() == Some("graph") => call(door, "oracle_graph", a)?,
        "oracle_digest" if a["full"].as_bool() == Some(true) => room_state_full(door, a)?,
        "oracle_digest" if a["agent"].as_bool() == Some(true) => room_digest_agent(door, a)?,
        "oracle_digest" if a["all"].as_bool() == Some(true) || a["room"].as_str().is_none_or(|r| r.trim().is_empty()) => {
            hallway_text(&door.get(&format!("/hallway?days={}", a["days"].as_i64().unwrap_or(14)))?, now_ms())
        }
        "oracle_brief" | "oracle_state" if brief_where(a)? == "world" => world_render(&world_value(door)?),
        "oracle_brief" | "oracle_state" => brief_with(door, a["full"].as_bool() == Some(true))?,
        "oracle_delta" => delta_render(&delta_value(door, a["since"].as_str())?),
        // The one inbox (inbox_value): the same object the MCP serves and `ox inbox` prints.
        "oracle_inbox" => {
            let ack = if a["ack"].as_bool() == Some(true) { Ack::Page } else { Ack::No };
            inbox_render(&inbox_mcp(door, a, ack)?)
        }
        "oracle_read" => {
            let mut path = format!(
                "/search?channel={}&limit={}",
                enc(&text_of(a, "room")?),
                a["limit"].as_i64().unwrap_or(20)
            );
            for k in ["tag", "starts"] {
                if let Some(x) = a[k].as_str() {
                    path += &format!("&{k}={}", enc(x));
                }
            }
            let mut items = door.get(&path)?["items"].as_array().cloned().unwrap_or_default();
            items.reverse();
            if items.is_empty() {
                "nothing here".into()
            } else {
                items.iter().map(full).collect::<Vec<_>>().join("\n")
            }
        }
        "oracle_get" => full(&door.get(&format!("/search?id={}", enc(&text_of(a, "id")?)))?),
        "oracle_thread" => thread_text(&door.get(&format!("/threads?id={}", enc(&text_of(a, "id")?)))?),
        "oracle_recall" => recall_in(door, &text_of(a, "query")?, a["room"].as_str(), a["limit"].as_i64().unwrap_or(10), a["include_archived"].as_bool().unwrap_or(false), a["as_of"].as_str())?,
        "oracle_query" => recall(door, &text_of(a, "query")?, None, a["limit"].as_i64().unwrap_or(10))?,
        "oracle_about" => recall(door, &text_of(a, "terms")?, Some(&text_of(a, "room")?), a["limit"].as_i64().unwrap_or(15))?,
        "oracle_say" => {
            let mut tags = list_of(a, "tags");
            let clock = say_clock(door, a, &mut tags)?;
            if let Some(e) = a["expires"].as_str() {
                tags.push(format!("expires:{}", expires_ms(e, clock)?));
            }
            tags.extend(list_of(a, "to").into_iter().map(|h| format!("to:{h}")));
            tags.extend(a["reply"].as_str().map(|id| format!("reply:{id}")));
            tags.extend(a["intent"].as_str().map(|i| format!("intent:{i}")));
            if let Some(k) = a["ask"].as_str() {
                if !["read", "reply", "act", "decide", "review", "none"].contains(&k) {
                    bail!("ask must be read, reply, act, decide, review or none");
                }
                if !tags.iter().any(|t| t.starts_with("ask:")) {
                    tags.push(format!("ask:{k}"));
                }
            }
            // The Bridge's verbs on the MCP (parity with `ox say`): closes, by, options, choice.
            for id in list_of(a, "closes") {
                if !id.starts_with("OBS-") || id.len() > 80 || id.chars().any(char::is_whitespace) {
                    bail!("closes takes observation ids (OBS-…): {id}");
                }
                tags.push(format!("closes:{id}"));
            }
            if let Some(b) = a["by"].as_str() {
                tags.push(format!("by:{}", parse_by_at(b, clock)?));
            }
            let mut cargo = cargo_of(a)?;
            let options = list_of(a, "options");
            if !options.is_empty() {
                if a["ask"].as_str() != Some("decide") {
                    bail!("options are for ask decide");
                }
                let distinct: std::collections::HashSet<&String> = options.iter().collect();
                if !(2..=6).contains(&options.len()) || distinct.len() != options.len() || options.iter().any(|o| o.trim().is_empty() || o.chars().count() > 80) {
                    bail!("options: 2 to 6 distinct choices, each 1 to 80 characters");
                }
                let mut c: Value = match cargo.as_deref() { Some(raw) => serde_json::from_str(raw).map_err(|_| anyhow::anyhow!("options need a JSON object cargo"))?, None => json!({}) };
                let obj = c.as_object_mut().ok_or_else(|| anyhow::anyhow!("options need a JSON object cargo"))?;
                if obj.contains_key("options") {
                    bail!("cargo already has options; use options or the cargo, not both");
                }
                obj.insert("options".into(), json!(options));
                cargo = Some(c.to_string());
            } else if a["ask"].as_str() == Some("decide") && !cargo.as_deref().is_some_and(|c| c.contains("\"options\"")) {
                bail!("ask decide needs 2 to 6 options");
            }
            let mut text = text_of(a, "room").and(text_of(a, "text"))?;
            if a["ask"].as_str() == Some("decide") {
                let mut c: Value = cargo.as_deref().and_then(|raw| serde_json::from_str(raw).ok()).unwrap_or_else(|| json!({}));
                let words = if c.get("recommendation").is_none() {
                    let n = a["recommend"].as_u64().ok_or_else(|| anyhow::anyhow!("ask decide needs recommend: the option you would pick, numbered from 1"))? as usize;
                    let w = add_recommendation(&mut c, n, a["why"].as_str().unwrap_or(""))?;
                    cargo = Some(c.to_string());
                    w
                } else {
                    recommendation_words(&c).ok_or_else(|| anyhow::anyhow!("cargo.recommendation must name one of the options and say why"))?
                };
                text = with_choice_words(&text, &words);
            }
            if let Some(n) = a["choice"].as_i64() {
                if a["reply"].as_str().is_none() {
                    bail!("choice picks an option of the message you reply to: add reply");
                }
                tags.push(format!("choice:{n}"));
            }
            say(door, &text_of(a, "room")?, &text, cargo, tags, op_of(a))?
        }
        "room_say" => say(
            door,
            &text_of(a, "channel")?,
            &text_of(a, "content")?,
            a["cargo"].as_str().map(str::to_string),
            list_of(a, "tags"),
            op_of(a),
        )?,
        "oracle_mark" => {
            let observation = text_of(a, "observation")?;
            let found = door.get(&format!("/search?id={}", enc(&observation)))?;
            let content = marks::content_of(&found).ok_or_else(|| anyhow!("{observation}: no such observation, or not readable by this seat"))?;
            let labels = list_of(a, "labels");
            let m = marks::mark_message(&observation, &content, &text_of(a, "quote")?, a["nth"].as_u64().unwrap_or(1) as usize, &labels, &list_of(a, "intents"), a["note"].as_str(), &list_of(a, "links"), None)?;
            let id = match door.write(Kind::Say, marks::ROOM, &m.face, Some(m.cargo.to_string()), m.tags, op_of(a))? {
                Written::Stored(v) => v["id"].as_str().unwrap_or("?").to_owned(),
                w @ Written::Queued { .. } => return Ok(receipt(w, "mark queued; its links are not written until it is stored: call oracle_flush, then oracle_mark again with the same op_id")),
            };
            let mut lines = vec![format!("signed:ok  {id}  (mark, chars {}..{} of {observation}, labels {}; held for Blaze's verdict)", m.start, m.end, labels.join("+"))];
            for (kind, to) in &m.links {
                let (face, cargo, tags) = marks::link_message(&id, kind, to);
                match door.write(Kind::Say, marks::ROOM, &face, Some(cargo.to_string()), tags, None) {
                    Ok(w) => lines.push(receipt(w, &format!("link {kind} -> {to}"))),
                    Err(e) => lines.push(format!("link {kind} -> {to}  NOT written: {e}")),
                }
            }
            lines.join("\n")
        }
        "oracle_verdict" => {
            let (face, cargo, tags) = marks::verdict_message(&text_of(a, "target")?, &text_of(a, "verdict")?, a["text"].as_str(), None)?;
            say(door, marks::ROOM, &face, Some(cargo.to_string()), tags, op_of(a))?
        }
        "oracle_remember" => {
            let kind = text_of(a, "kind")?;
            if !MEMORY_KINDS.contains(&kind.as_str()) {
                bail!("kind must be one of {MEMORY_KINDS:?}. Status lines and dashboards are not knowledge.");
            }
            let room = a["room"].as_str().unwrap_or("memory");
            let mut tags = vec![format!("memory:{kind}")];
            if let Some(v) = a["valid_until"].as_str().filter(|v| !v.trim().is_empty()) {
                let ms = oracle_proto::parse_when(v).ok_or_else(|| anyhow::anyhow!("valid_until {v:?}: a date (2026-12-31), a date and time, or unix ms"))?;
                tags.push(format!("valid-until:{ms}"));
            }
            receipt(
                door.write(Kind::Memory, room, &text_of(a, "text")?, None, tags, op_of(a))?,
                &format!("remembered, {kind}"),
            )
        }
        "oracle_task" => {
            let (subject, state) = (text_of(a, "subject")?, text_of(a, "state")?);
            let mut cargo = json!({"subject": subject, "state": state, "owner": door.handle()});
            if let Some(n) = a["next"].as_str() {
                cargo["next_touch"] = json!(n);
            }
            let needs: Vec<String> = a["needs"].as_array().map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
            blocked_rule(&mut cargo, &needs, a["blocks"].as_str(), a["check"].as_str(), now_ms())?;
            let room = a["room"].as_str().unwrap_or("board");
            receipt(
                door.write(Kind::State, room, &text_of(a, "note")?, Some(cargo.to_string()), vec![format!("subject:{subject}")], op_of(a))?,
                &format!("{subject} → {state}"),
            )
        }
        "oracle_digest" => {
            let room = a["room"].as_str().ok_or_else(|| anyhow!("room is required"))?;
            let roots = a["roots"].as_str().unwrap_or("blaze,hustler");
            digest_text(&door.get(&format!("/threads?view=digest&channel={}&roots={}", enc(room), enc(roots)))?)
        }
        "oracle_room_state_full" => room_state_full(door, a)?,
        "oracle_room_digest_agent" => room_digest_agent(door, a)?,
        "oracle_where" => {
            let query = text_of(a, "query")?;
            where_text(&query, &door.get(&where_path(&query, a["any"].as_bool().unwrap_or(false)))?)
        }
        "oracle_experiments" => {
            experiments_text(&door.get(&experiments_path(a))?)
        }
        "oracle_terms" => {
            let limit = a["limit"].as_i64().unwrap_or(100).clamp(1, 100);
            // The hourly ranking itself; the head is the fallback for a server without /terms.
            let first = door.get(&terms_path(limit));
            match &first {
                Ok(v) if v["top"].as_array().is_some_and(|t| !t.is_empty()) => terms_text(v, limit),
                _ => match door.get("/states?subject=search.terms") {
                    Ok(h) => terms_text(&h, limit),
                    // A new Oracle has neither the hourly ranking nor its head yet: that is an answer ("none yet, and why"),
                    // not a failure. Anything other than not-found (unreachable, refused) is still an error.
                    Err(e) if e.to_string().contains("(404)") || e.to_string().contains("not found") => format!(
                        "no word ranking yet ({}). It is built hourly from the messages; ask again later.",
                        first.as_ref().ok().and_then(|v| v["error"].as_str()).unwrap_or("the server has none")
                    ),
                    Err(e) => return Err(e),
                },
            }
        }
        "oracle_room_state" => room_state_typed(door, a)?.text,
        "oracle_room_map" => room_map_call(door, a)?,
        "oracle_react" => {
            let (room, id) = (text_of(a, "room")?, text_of(a, "id")?);
            let mark = text_of(a, "mark")?;
            let (face, tags) = match a["until"].as_str() {
                Some(u) if mark == "later" => { let now = now_ms(); verbs::later(&id, verbs::until_of(u, now)?, now, a["why"].as_str())? }
                Some(_) => anyhow::bail!("until belongs to mark later"),
                None => verbs::reaction(&mark, &id, a["why"].as_str())?,
            };
            receipt(door.write(Kind::Say, &room, &face, None, tags, op_of(a))?, &format!("{mark} on {id}"))
        }
        "oracle_annotate" => {
            let (room, id) = (text_of(a, "room")?, text_of(a, "id")?);
            let tags = verbs::annotation(&id, &list_of(a, "set"), a["confidence"].as_f64())?;
            let why = text_of(a, "why")?;
            if why.trim().len() > 1200 {
                bail!("why says in one line what decided it (1-1200 characters)");
            }
            receipt(door.write(Kind::Say, &room, why.trim(), None, tags, op_of(a))?, &format!("annotates {id}"))
        }
        "oracle_claim" => {
            let (room, op) = (text_of(a, "room")?, text_of(a, "op")?);
            let (face, tags, cargo) = verbs::claim(&op, a)?;
            receipt(door.write(Kind::Say, &room, &face, Some(cargo.to_string()), tags, op_of(a))?, &format!("claim {op}"))
        }
        "oracle_hello" => hello_text(door)?,
        "oracle_hallway" => hallway_text(&door.get(&format!("/hallway?days={}", a["days"].as_i64().unwrap_or(14)))?, now_ms()),
        "oracle_claims" => {
            let mut q = vec![];
            for k in ["room", "status", "cid"] {
                if let Some(v) = a[k].as_str() {
                    q.push(format!("{k}={}", enc(v)));
                }
            }
            claims_text(&door.get(&format!("/claims?{}", q.join("&")))?)
        }
        "oracle_graph" => {
            let view = a["view"].as_str().unwrap_or("cortex");
            let min = a["min"].as_i64().map(|m| format!("&min={m}")).unwrap_or_default();
            let marks = a["marks"].as_str().map(|m| format!("&marks={}", enc(m))).unwrap_or_default();
            let top = a["top"].as_i64().map(|t| format!("&top={t}")).unwrap_or_default();
            graph_text(&door.get(&format!("/graph?view={}{min}{marks}{top}", enc(view)))?)
        }
        "oracle_cortex" => {
            let op = text_of(a, "op")?;
            let mut q = vec![];
            for k in ["room", "kind", "q", "source"] {
                if let Some(v) = a[k].as_str() {
                    q.push(format!("{k}={}", enc(v)));
                }
            }
            if a["all"].as_bool() == Some(true) {
                q.push("all=1".into());
            }
            if op == "cases" || op == "case" {
                return Ok(cases::text(&op, &door.get(&cases::path(&op, a)?)?));
            }
            if op == "draft" {
                return Ok(cases::text("draft", &door.post("/cortex/drafts", &cases::draft(a)?)?));
            }
            let top = a["top"].as_u64().filter(|_| op == "nodes").map(|t| t.clamp(1, 500) as usize);
            if let Some(t) = top {
                q.push(format!("top={t}"));
            }
            let path = match op.as_str() {
                "nodes" => format!("/cortex?{}", q.join("&")),
                "node" => format!("/cortex/node/{}", enc(&text_of(a, "nid")?)),
                "entity" => format!("/cortex/entity/{}", enc(&text_of(a, "q")?)),
                "marks" => format!("/cortex/marks?{}", q.join("&")),
                _ => return Err(anyhow::anyhow!("op is nodes, node, entity, marks, cases, case or draft")),
            };
            let mut v = door.get(&path)?;
            if let Some(t) = top {
                cortex_keep_top(&mut v, t);
            }
            cortex_text(&op, &v)
        }
        "oracle_archive" => {
            let (room, id) = (text_of(a, "room")?, text_of(a, "id")?);
            let (face, tags, cargo) = verbs::archive(&room, &id, a["note"].as_str())?;
            receipt(door.write(Kind::Say, &room, &face, Some(cargo.to_string()), tags, op_of(a))?, &format!("archived {id}"))
        }
        "oracle_tasks" => tasks(door, a["owner"].as_str(), None, a)?,
        "oracle_board" => tasks(door, a["owner"].as_str(), a["room"].as_str(), a)?,
        "oracle_flush" => {
            let (sent, left) = door.flush()?;
            format!("flushed {sent}, still queued {left}")
        }
        other => bail!("unknown tool: {other}"),
    })
}

/// One JSON-RPC request in, at most one response out. Notifications (no id) get none.
/// `legacy` lists the old Oracle's tool names; the aliases answer either way.
pub fn handle(door: &dyn Door, msg: &Value, legacy: bool) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let result = match msg["method"].as_str().unwrap_or("") {
        "initialize" => Ok(json!({
            "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "oracle", "title": format!("Oracle · {}", door.handle()), "version": env!("CARGO_PKG_VERSION")},
            "instructions": INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools(legacy)})),
        "tools/call" => {
            let empty = json!({});
            let args = msg["params"]
                .get("arguments")
                .filter(|a| a.is_object())
                .unwrap_or(&empty);
            // A failing tool is a result the model can read and act on, not a protocol error.
            let given = msg["params"]["name"].as_str().unwrap_or("");
            // A verb (brief, read, find, write, settle) runs as the tool its address routes to (verb_route).
            let verb = verb_route(door, given, args);
            let (given, args) = match &verb {
                Some(Ok((n, a))) => (n.as_str(), a),
                _ => (given, args),
            };
            // An old name (the fold of 2026-10-21) runs as the new one and says so (notice_of).
            let folded = fold_tool(given, args);
            let (name, args) = match &folded {
                Some((n, a, _)) => (n.as_str(), a),
                None => (given, args),
            };
            // The brief is typed: its object is the structuredContent (what Claude Code gives the model), its rendering the text.
            if let Some(Err(e)) = &verb {
                Ok(json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}))
            } else if let Some(r) = typed_read(door, name, args) {
                Ok(match r {
                    Ok(t) => json!({"content": [{"type": "text", "text": t.text}], "structuredContent": t.value, "isError": false}),
                    Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
                })
            } else if name == "oracle_brief" && args["hello"].as_bool() == Some(true) {
                Ok(match call(door, "oracle_hello", args) {
                    Ok(text) => json!({"content": [{"type": "text", "text": text}], "isError": false}),
                    Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
                })
            } else if name == "oracle_room_state" && args["map"].as_bool() == Some(true) {
                Ok(match call(door, "oracle_room_map", args) {
                    Ok(text) => json!({"content": [{"type": "text", "text": text}], "isError": false}),
                    Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
                })
            } else if name == "oracle_room_state" {
                Ok(match room_state_typed(door, args) {
                    Ok(t) => json!({"content": [{"type": "text", "text": t.text}], "structuredContent": t.value, "isError": false}),
                    Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
                })
            } else if name == "oracle_task" && args["exp"].as_bool() != Some(true) && args["subject"].as_str().is_none_or(|s| s.trim().is_empty()) {
                // The list form is typed: the heads it holds and its budget are the structuredContent.
                let (limit, offset) = task_page(args);
                Ok(match tasks_value(door, args["owner"].as_str(), args["room"].as_str(), limit, offset) {
                    Ok(v) => json!({"content": [{"type": "text", "text": tasks_render(&v)}], "structuredContent": v, "isError": false}),
                    Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
                })
            } else if name == "oracle_delta" {
                Ok(match delta_value(door, args["since"].as_str()) {
                    Ok(v) => json!({"content": [{"type": "text", "text": delta_render(&v)}], "structuredContent": v, "isError": false}),
                    Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
                })
            } else if name == "oracle_inbox" {
                let ack = if args["ack"].is_object() { Ack::Marks(args["ack"].clone()) } else if args["ack"].as_bool() == Some(true) { Ack::Page } else { Ack::No };
                Ok(match inbox_mcp(door, args, ack) {
                    Ok(v) => json!({"content": [{"type": "text", "text": inbox_render(&v)}], "structuredContent": v, "isError": false}),
                    Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
                })
            } else if matches!(name, "oracle_brief" | "oracle_state") && brief_where(args).map_or(true, |w| w != "me") {
                // where: world is the server's WORLD STATE page; any other where is refused, naming the forms.
                Ok(match brief_where(args).and_then(|_| world_value(door)) {
                    Ok(v) => json!({"content": [{"type": "text", "text": world_render(&v)}], "structuredContent": v, "isError": false}),
                    Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
                })
            } else if matches!(name, "oracle_brief" | "oracle_state") {
                let full = args["full"].as_bool() == Some(true);
                Ok(match brief_value(door, full) {
                    Ok(v) => json!({"content": [{"type": "text", "text": brief_render(&v, full)}], "structuredContent": if full { v } else { brief_compact(&v) }, "isError": false}),
                    Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
                })
            } else {
                Ok(match call(door, name, args) {
                    Ok(text) => json!({"content": [{"type": "text", "text": text}], "isError": false}),
                    Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
                })
            }
        }
        other => Err(json!({"code": -32601, "message": format!("method not found: {other}")})),
    };
    let result = match (result, notice_of(msg)) {
        (Ok(mut r), Some(note)) => {
            if let Some(first) = r["content"].get(0).and_then(|c| c["text"].as_str()).map(str::to_owned) {
                r["content"][0]["text"] = json!(format!("{}\n{note}", first.trim_end()));
            }
            r["notice"] = json!(note);
            Ok(r)
        }
        (r, _) => r,
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// (kind, room, text, cargo, tags) as a door received them.
    type Write = (String, String, String, Option<String>, Vec<String>);

    /// A door that answers from a script and records what was asked of it.
    struct Fake {
        gets: RefCell<Vec<String>>,
        writes: RefCell<Vec<Write>>,
        posts: RefCell<Vec<(String, Value)>>,
        answers: Value,
    }
    impl Fake {
        fn new(answers: Value) -> Fake {
            Fake { gets: RefCell::new(vec![]), writes: RefCell::new(vec![]), posts: RefCell::new(vec![]), answers }
        }
    }
    impl Door for Fake {
        fn handle(&self) -> &str {
            "rocky"
        }
        fn get(&self, path: &str) -> Result<Value> {
            self.gets.borrow_mut().push(path.to_string());
            // The longest answer key that prefixes the path: "/cards?owner=x" beats "/cards".
            let hit = self.answers.as_object().and_then(|m| m.iter().filter(|(k, _)| path == k.as_str() || path.starts_with(&format!("{k}?")) || path.starts_with(&format!("{k}&"))).max_by_key(|(k, _)| k.len()));
            match hit.map(|(_, v)| v.clone()).unwrap_or(json!({})) {
                // {"__err": "…"} answers a read with a failure.
                v if v["__err"].is_string() => anyhow::bail!("{}", v["__err"].as_str().unwrap()),
                v => Ok(v),
            }
        }
        fn get_bytes(&self, path: &str) -> Result<(String, Vec<u8>)> {
            let v = &self.answers[path];
            match (v["__ctype"].as_str(), v["__text"].as_str()) {
                (Some(c), Some(t)) => Ok((c.to_string(), t.as_bytes().to_vec())),
                _ => anyhow::bail!("no bytes at {path}"),
            }
        }
        fn post(&self, path: &str, body: &Value) -> Result<Value> {
            self.posts.borrow_mut().push((path.to_string(), body.clone()));
            Ok(json!({"ok": true}))
        }
        fn write(&self, kind: Kind, room: &str, text: &str, cargo: Option<String>, tags: Vec<String>, _op_id: Option<String>) -> Result<Written> {
            self.writes.borrow_mut().push((kind.as_str().to_string(), room.into(), text.into(), cargo, tags));
            Ok(Written::Stored(json!({"id": "OBS-1"})))
        }
        fn identity(&self) -> String {
            "key abcdef12… · http://fake".into()
        }
        fn state(&self, room: &str) -> Result<Value> {
            Ok(self.answers.get(format!("state:{room}")).cloned().unwrap_or(Value::Null))
        }
        fn recorded_write(&self, op_id: &str) -> Result<Option<Value>> {
            Ok(self.answers.get(format!("recorded:{op_id}")).cloned())
        }
    }

    /// A seat scoped to its rooms (Dot, 4 Nov 2026) gets its inbox from its rooms; an addressed task stays listed until
    /// the seat replies to it, and a progress line or diary of its own is not that reply (Emmy's review of b46d543).
    #[test]
    fn a_scoped_seat_keeps_an_addressed_task_until_it_replies_to_it() {
        let rec = |id: &str, author: &str, ms: i64, tags: &[&str]| json!({"id": id, "author": author, "channel": "dot-game", "created_ms": ms, "seq": ms, "content": id, "tags": tags});
        let door = Fake::new(json!({
            "/whoami": {"handle": "dot-game", "server_ms": 100},
            "/asks": {"__err": "server refused (403): dot-game is scoped to its rooms; GET /asks is not open to it"},
            "/fold": {"__err": "server refused (403): dot-game is scoped to its rooms; GET /fold is not open to it"},
            "/rooms/mine": {"rooms": [{"room": "dot-game"}]},
            "/search": {"items": [
                rec("t1", "emmy", 10, &["to:dot-game", "ask:act"]),
                rec("t2", "emmy", 20, &["to:dot-game", "ask:act"]),
                rec("p1", "dot-game", 30, &["intent:status"]),
                rec("r2", "dot-game", 40, &["reply:t2"]),
                rec("n1", "noor", 35, &[]),
                rec("n2", "noor", 50, &[]),
            ]}
        }));
        let v = inbox_value(&door, 20, Ack::Page, false).unwrap();
        let mine: Vec<&str> = v["for_me"].as_array().unwrap().iter().filter_map(|i| i["id"].as_str()).collect();
        assert_eq!(mine, vec!["t1"], "t1 waits (Dot's progress line is no answer); t2 was answered by its reply");
        assert!(door.posts.borrow().is_empty(), "a scoped seat marks nothing read: /fold is not open to it");
        assert_eq!(v["acked"], false);
        assert!(v["scoped"].as_str().is_some_and(|n| n.contains("not a complete list")), "the page says what it is");
        let page = v["page"].as_u64().unwrap();
        assert_eq!(page, 2, "t1, and n2 after Dot's last word; n1 came before it");
        assert_eq!(v["new"], 2, "NEW counts what the scoped page lists, never 0 beside listed records");
    }

    // ── Dot's review of Rocky's scoped-inbox candidates (b46d543, e76f1aa), run 4 Nov 2026, offline ──
    // These call the real inbox_value -> fold_or_rooms. They use a fake door, so they prove the logic given
    // these records, not that the live server returns these shapes or that anything is deployed.
    fn dr_rec(id: &str, author: &str, ms: i64, tags: &[&str]) -> Value {
        json!({"id": id, "author": author, "channel": "dot-game", "created_ms": ms, "seq": ms, "content": id, "tags": tags})
    }
    fn dr_answers(items: Value) -> Value {
        json!({
            "/whoami": {"handle": "dot-game", "server_ms": 1000},
            "/asks": {"__err": "server refused (403): dot-game is scoped to its rooms; GET /asks is not open to it"},
            "/fold": {"__err": "server refused (403): dot-game is scoped to its rooms; GET /fold is not open to it"},
            "/rooms/mine": {"rooms": [{"room": "dot-game"}]},
            "/search": {"items": items}
        })
    }
    fn dr_listed(door: &dyn Door, limit: i64) -> (Vec<String>, Value) {
        let v = inbox_value(door, limit, Ack::Page, false).unwrap();
        let ids = v["for_me"].as_array().unwrap().iter().filter_map(|i| i["id"].as_str().map(str::to_owned)).collect();
        (ids, v)
    }
    /// Serves /search?channel=..&limit=N like the real `recent`: the newest N rows, nothing about the rest here.
    struct DrWindow { inner: Fake, all: Vec<Value> }
    impl Door for DrWindow {
        fn handle(&self) -> &str { self.inner.handle() }
        fn get(&self, path: &str) -> Result<Value> {
            if path.starts_with("/search?channel=") {
                let n: usize = path.split("limit=").nth(1).and_then(|s| s.split('&').next()).and_then(|s| s.parse().ok()).unwrap_or(20);
                let mut rows = self.all.clone();
                rows.sort_by_key(|r| std::cmp::Reverse(r["created_ms"].as_i64().unwrap_or(0)));
                let cut = rows.len() > n;
                rows.truncate(n);
                return Ok(json!({"count": rows.len(), "items": rows, "next_before": if cut { json!(1) } else { Value::Null }}));
            }
            self.inner.get(path)
        }
        fn get_bytes(&self, p: &str) -> Result<(String, Vec<u8>)> { self.inner.get_bytes(p) }
        fn post(&self, p: &str, b: &Value) -> Result<Value> { self.inner.post(p, b) }
        fn write(&self, k: Kind, r: &str, t: &str, c: Option<String>, g: Vec<String>, o: Option<String>) -> Result<Written> { self.inner.write(k, r, t, c, g, o) }
        fn identity(&self) -> String { self.inner.identity() }
        fn state(&self, r: &str) -> Result<Value> { self.inner.state(r) }
        fn recorded_write(&self, o: &str) -> Result<Option<Value>> { self.inner.recorded_write(o) }
    }

    #[test]
    fn dot_review_orient_progress_and_diary_do_not_hide_pending_tasks() {
        let door = Fake::new(dr_answers(json!([
            dr_rec("taskA", "emmy", 10, &["to:dot-game", "ask:act"]),
            dr_rec("orient", "dot-game", 20, &["run:r1", "intent:result"]),
            dr_rec("taskB", "emmy", 30, &["to:dot-game", "ask:act"]),
            dr_rec("diary", "dot-game", 40, &["subject:s", "subject:dot-game.diary-with-evidence"]),
            dr_rec("noise", "noor", 45, &[]),
        ])));
        let (ids, _) = dr_listed(&door, 20);
        assert_eq!(ids, vec!["taskA", "taskB"], "own orient and diary must answer nothing");
    }

    #[test]
    fn dot_review_only_my_reply_to_a_task_resolves_that_task() {
        let door = Fake::new(dr_answers(json!([
            dr_rec("taskA", "emmy", 10, &["to:dot-game"]),
            dr_rec("taskB", "emmy", 20, &["to:dot-game"]),
            dr_rec("taskC", "emmy", 30, &["to:dot-game"]),
            dr_rec("rA", "dot-game", 40, &["reply:taskA"]),
            dr_rec("othersReplyToB", "noor", 50, &["reply:taskB"]),
            dr_rec("myReplyToStranger", "dot-game", 60, &["reply:someoneElsesMessage"]),
        ])));
        let (ids, _) = dr_listed(&door, 20);
        assert_eq!(ids, vec!["taskB", "taskC"], "A answered by me; B answered only by someone else; C untouched");
    }

    /// DEFECT CANDIDATE (expected to FAIL on e76f1aa): addressing is compared with the exact tag to:{me}. Rocky's own
    /// notes in #dot-game (-6eb94c280152) carry to:@dot-game.
    #[test]
    fn dot_review_at_prefixed_addressing_is_still_addressed_to_me() {
        let door = Fake::new(dr_answers(json!([
            dr_rec("atTask", "rocky", 10, &["to:@dot-game", "ask:read"]),
            dr_rec("myDiary", "dot-game", 20, &["subject:s"]),
        ])));
        let (ids, _) = dr_listed(&door, 20);
        assert_eq!(ids, vec!["atTask"], "a task addressed with @ vanished after my next post");
        let door = Fake::new(dr_answers(json!([
            dr_rec("atMention", "rocky", 10, &["mention:@dot-game"]),
            dr_rec("myDiary", "dot-game", 20, &["subject:s"]),
        ])));
        let (ids, _) = dr_listed(&door, 20);
        assert_eq!(ids, vec!["atMention"], "same for mention:@");
    }

    /// DEFECT CANDIDATE (expected to FAIL on e76f1aa): the room page is the newest N rows; older pending tasks fall off
    /// it, and the result says has_more=false although the server gave next_before.
    #[test]
    fn dot_review_a_full_page_must_not_silently_drop_older_pending_tasks() {
        let tasks: Vec<Value> = (1..=5).map(|n| dr_rec(&format!("task{n}"), "emmy", n * 10, &["to:dot-game"])).collect();
        let door = DrWindow { inner: Fake::new(dr_answers(json!([]))), all: tasks };
        let (ids, v) = dr_listed(&door, 3);
        assert!(ids.len() == 5 || v["has_more"] == true, "5 pending tasks, window 3: listed {ids:?}, has_more={}", v["has_more"]);
    }

    /// CHARACTERIZATION (documents current behavior, not a verdict): any reply of mine counts as an answer, even a
    /// progress line, because the tag carries no closing signal. Whether that is too broad is a spec decision.
    #[test]
    fn dot_review_characterize_a_progress_reply_counts_as_an_answer() {
        let door = Fake::new(dr_answers(json!([
            dr_rec("taskA", "emmy", 10, &["to:dot-game", "ask:act"]),
            dr_rec("progress", "dot-game", 20, &["reply:taskA", "intent:status", "phase:blocked"]),
        ])));
        let (ids, _) = dr_listed(&door, 20);
        assert!(ids.is_empty(), "current behavior: a blocked/progress record replying to taskA removes it from the inbox");
    }

    #[test]
    fn owner_recipient_is_resolved_before_signing_and_read_asks_work() {
        let f = Fake::new(json!({"/keys": {"keys": [
            {"handle": "retired", "role": "owner", "revoked_ms": 1},
            {"handle": "alice", "role": "owner", "revoked_ms": null},
            {"handle": "alice", "role": "owner", "revoked_ms": null},
            {"handle": "blaze", "role": "seat", "revoked_ms": null}
        ]}}));
        call(&f, "oracle_say", &json!({"room":"rocky", "text":"Ready", "to":["@owner", "alice"], "ask":"read"})).unwrap();
        let writes = f.writes.borrow();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].4.iter().filter(|t| *t == "to:alice").count(), 1);
        assert!(!writes[0].4.iter().any(|t| t == "to:@owner" || t == "to:blaze"));
        assert!(writes[0].4.contains(&"ask:read".into()));
        assert_eq!(*f.gets.borrow(), vec!["/keys"]);
    }

    #[test]
    fn owner_resolution_fails_closed_but_explicit_recipients_need_no_lookup() {
        for registry in [json!({}), json!({"keys":[]}), json!({"keys":[
            {"handle":"a", "role":"owner", "revoked_ms":null},
            {"handle":"b", "role":"owner", "revoked_ms":null}
        ]}), json!({"keys":[{"handle":"a", "role":"owner"}]})] {
            let f = Fake::new(json!({"/keys":registry}));
            assert!(call(&f,"oracle_say", &json!({"room":"r","text":"Hi","to":["@owner"]})).is_err());
            assert!(f.writes.borrow().is_empty());
        }
        let f = Fake::new(json!({"/keys":{"__err":"offline"}}));
        call(&f,"oracle_say", &json!({"room":"r","text":"Hi","to":["alice"]})).unwrap();
        assert!(f.gets.borrow().is_empty());
    }

    // Blaze, 2026-10-03: every Oracle action goes through the one MCP, so a seat's State is read and written here too,
    // with the same revision `ox state --set` signs.
    #[test]
    fn terms_on_a_new_oracle_say_none_yet_and_a_real_failure_still_fails() {
        let fresh = Fake::new(json!({"/terms": {"error": "no word ranking at /x/terms.json", "top": []}, "/states": {"__err": "server refused (404): not found"}}));
        let t = call(&fresh, "oracle_terms", &json!({})).unwrap();
        assert!(t.starts_with("no word ranking yet (no word ranking at /x/terms.json)"), "{t}");
        let down = Fake::new(json!({"/terms": {"top": []}, "/states": {"__err": "Oracle unreachable (connection refused)"}}));
        assert!(call(&down, "oracle_terms", &json!({})).is_err(), "unreachable is not 'none yet'");
    }

    #[test]
    fn a_retry_the_store_recognised_says_duplicate_in_its_receipt() {
        let fresh = receipt(Written::Stored(json!({"id": "OBS-1", "duplicate": false})), "#room");
        let again = receipt(Written::Stored(json!({"id": "OBS-1", "duplicate": true})), "#room");
        assert_eq!(fresh, "signed:ok  OBS-1  (#room)");
        assert_eq!(again, "signed:ok  OBS-1  (#room; duplicate: the same write, stored once)");
    }

    #[test]
    fn the_shared_query_paths_are_the_ones_the_tools_send() {
        assert_eq!(heads_path("rocky", None, None), "/states?owner=rocky");
        assert_eq!(heads_path("rocky", Some("*"), None), "/states");
        assert_eq!(heads_path("rocky", Some("kin"), Some("dot")), "/states?owner=kin&channel=dot");
        assert_eq!(where_path("a b", true), "/search?q=a%20b&format=page&limit=1&by_room=true&match=any");
        assert_eq!(experiments_path(&json!({"limit": 999, "stage": "run", "q": " "})), "/experiments?limit=200&stage=run");
        assert_eq!(terms_path(0), "/terms?limit=1");
        let f = Fake::new(json!({}));
        call(&f, "oracle_where", &json!({"query": "a b", "any": true})).ok();
        call(&f, "oracle_tasks", &json!({})).ok();
        let gets = f.gets.borrow();
        assert!(gets.contains(&where_path("a b", true)) && gets.contains(&heads_path("rocky", None, None)), "{gets:?}");
    }

    #[test]
    fn continuation_survives_unrelated_state_edits() {
        let old = json!({"current":"historical room", "continued_in":"laws"});
        let r = state_revision("illuminati", &[], &old, &["current=continued elsewhere".into()]).unwrap();
        assert_eq!(r["fields"]["continued_in"], "laws");
        for target in ["illuminati", "../laws", "Laws", "two rooms"] {
            assert!(state_revision("illuminati", &[], &old, &[format!("continued_in={target}")]).is_err());
        }
        let cleared=state_revision("illuminati", &[], &old, &["continued_in=".into()]).unwrap();
        assert!(cleared["fields"].get("continued_in").is_none());
    }
    #[test]
    fn room_state_reads_and_writes_the_same_revision_the_cli_signs_and_refuses_a_stale_head() {
        let st = json!({"state:oracle": {"state": "alive", "heads": ["OBS-oracle-H1"], "revisions": 3,
            "fields": {"purpose": "memory", "current": "old", "goal": ""}}});
        let f = Fake::new(st.clone());
        let read = call(&f, "oracle_room_state", &json!({"room": "oracle"})).unwrap();
        assert!(read.contains("head OBS-oracle-H1") && read.contains("purpose: memory") && !read.contains("goal:"), "{read}");
        assert!(f.writes.borrow().is_empty(), "a read writes nothing");
        let w = call(&f, "oracle_room_state", &json!({"room": "oracle", "set": ["current=new work"], "head": "OBS-oracle-H1"})).unwrap();
        assert!(w.starts_with("signed:ok"), "{w}");
        let (kind, room, _face, cargo, tags) = f.writes.borrow()[0].clone();
        assert_eq!((kind.as_str(), room.as_str()), ("say", "oracle"));
        assert_eq!(tags, vec!["room-context:v1".to_string(), "intent:status".to_string()]);
        let rev: Value = serde_json::from_str(&cargo.unwrap()).unwrap();
        // Byte-for-byte the revision the CLI builds: the fields not named are kept, parents are the read heads.
        assert_eq!(rev, state_revision("oracle", &["OBS-oracle-H1".to_string()], &st["state:oracle"]["fields"], &["current=new work".to_string()]).unwrap());
        assert_eq!(rev["fields"]["purpose"], "memory");
        // NEGATIVE CONTROL: a head that is no longer current is refused; nothing is written.
        let stale = Fake::new(st);
        let e = call(&stale, "oracle_room_state", &json!({"room": "oracle", "set": ["current=x"], "head": "OBS-oracle-H0"})).unwrap_err();
        assert!(e.to_string().contains("moved since your read"), "{e}");
        assert!(stale.writes.borrow().is_empty());
        // A field that is not a State field is refused by the shared builder.
        assert!(call(&Fake::new(json!({"state:oracle": {"heads": [], "fields": {}}})), "oracle_room_state", &json!({"room": "oracle", "set": ["colour=red"]})).is_err());
    }

    // Blaze, 31 Oct 2026: the brief's structured object was 1.5 MB (86% the State's evidence), too big for Claude Code to take.
    #[test]
    fn the_brief_an_agent_is_handed_carries_no_evidence_and_caps_its_lists() {
        let evidence: Vec<Value> = (0..60).map(|i| json!({"id": format!("OBS-s{i}"), "envelope": "x".repeat(20_000)})).collect();
        let rows: Vec<Value> = (0..200).map(|i| json!({"id": format!("OBS-a{i}"), "card": "an ask"})).collect();
        let b = json!({"now": "2026-10-31T05:44:57.166Z", "seat": "rocky", "state": {"state": "alive", "heads": ["OBS-h"], "revisions": 63, "complete": true,
            "trustworthy": true, "fields": {"purpose": "engineer", "next": ""}, "evidence": evidence, "bad": [], "unknown": []},
            "asks": {"rows": rows}, "resume": {"id": "OBS-r", "text": "r".repeat(9000)}});
        let c = brief_compact(&b);
        assert!(c["state"].get("evidence").is_none(), "the evidence stays on the server");
        assert_eq!((c["state"]["fields"]["purpose"].as_str(), c["state"]["fields"].get("next")), (Some("engineer"), None), "the fields that are set, nothing empty");
        assert_eq!((c["asks"]["rows"].as_array().map(Vec::len), c["asks"]["more"].as_u64()), (Some(10), Some(190)));
        assert!(c["resume"]["cut"] == true && c["resume"]["text"].as_str().unwrap().chars().count() < 4100);
        let (whole, small) = (b.to_string().len(), c.to_string().len());
        assert!(small * 50 < whole, "{small} bytes against {whole}");
    }

    // Bobby/Emmy, 2026-10-03: oracle_room_state answered with prose only, so a harness could not tell an empty room from an unparsed answer and a refusal looked like text.
    #[test]
    fn room_state_through_mcp_is_typed_has_the_cursor_alias_and_refusals_are_errors() {
        let st = json!({"state:oracle": {"state": "alive", "heads": ["OBS-oracle-H1"], "revisions": 3, "complete": true, "trustworthy": true, "as_of_seq": 900,
            "fields": {"purpose": "memory", "current": "old"}, "adoptions": {}, "bad": [], "unknown": []}});
        let f = Fake::new(st);
        let rpc = |args: Value| handle(&f, &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "oracle_room_state", "arguments": args}}), false).unwrap()["result"].clone();
        let r = rpc(json!({"room": "oracle"}));
        assert_eq!(r["isError"], false);
        let v = &r["structuredContent"];
        assert_eq!((v["state"].as_str(), v["heads"][0].as_str(), v["revisions"].as_i64(), v["complete"].as_bool(), v["trustworthy"].as_bool(), v["as_of_seq"].as_i64()), (Some("alive"), Some("OBS-oracle-H1"), Some(3), Some(true), Some(true), Some(900)));
        assert_eq!(v["fields"]["purpose"], "memory");
        assert_eq!(v["orientation"]["status"], "partial");
        assert_eq!(v["orientation"]["missing_fields"], json!(["next"]));
        assert!(r["content"][0]["text"].as_str().unwrap().contains("head OBS-oracle-H1"), "the readable text is kept beside the object");
        // a write answers with its receipt as an object: the id, whether it was queued, and the parents it named
        let w = rpc(json!({"room": "oracle", "set": ["current=new"], "head": "OBS-oracle-H1", "op_id": "race-0001-abcdef"}));
        assert_eq!((w["isError"].as_bool(), w["structuredContent"]["written"].as_bool(), w["structuredContent"]["id"].as_str(), w["structuredContent"]["parents"][0].as_str()), (Some(false), Some(true), Some("OBS-1"), Some("OBS-oracle-H1")));
        // a stale head is a tool ERROR (isError), never a text answer a harness could mistake for success
        let stale = rpc(json!({"room": "oracle", "set": ["current=x"], "head": "OBS-oracle-H0"}));
        assert_eq!(stale["isError"], true);
        assert!(stale.get("structuredContent").is_none());
        // as_of_seq is the name, as_of its alias; both given and different is refused
        assert_eq!(cursor_arg(&json!({"as_of_seq": 5})).unwrap(), Some(5));
        assert_eq!(cursor_arg(&json!({"as_of": 6})).unwrap(), Some(6));
        assert_eq!(cursor_arg(&json!({"as_of": 7, "as_of_seq": 7})).unwrap(), Some(7));
        assert!(cursor_arg(&json!({"as_of": 7, "as_of_seq": 8})).is_err());
        // a wrong type or a non-positive cursor never reads as "latest"
        for bad in [json!({"as_of_seq": "5"}), json!({"as_of_seq": 5.5}), json!({"as_of": 0}), json!({"as_of": -3}), json!({"as_of_seq": true}), json!({"as_of": [1]}), json!({"as_of_seq": {"n": 1}})] {
            assert!(cursor_arg(&bad).is_err(), "{bad}");
        }
        assert_eq!(cursor_arg(&json!({"as_of_seq": null})).unwrap(), None);
        assert_eq!(cursor_arg(&json!({})).unwrap(), None);
        assert_eq!(rpc(json!({"room": "oracle", "as_of_seq": 5, "as_of": 6}))["isError"], true);
        // a cursor read is read only
        assert_eq!(rpc(json!({"room": "oracle", "as_of_seq": 5, "set": ["current=x"]}))["isError"], true);
    }

    // Rocky's live pass, 1 Nov 2026 07:15Z: oracle_room_state returned every field with no way to pick.
    #[test]
    fn a_state_read_returns_only_the_fields_asked_for_and_names_the_unknown() {
        let big = "x".repeat(8_000);
        let f = Fake::new(json!({"state:rocky": {"state": "alive", "heads": ["OBS-rocky-H1"], "revisions": 63, "complete": true, "trustworthy": true,
            "fields": {"purpose": "engineer", "current": "budgets", "next": "release", "boot": big, "rules": big}, "adoptions": {}, "bad": [], "unknown": []}}));
        let rpc = |args: Value| handle(&f, &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "oracle_room_state", "arguments": args}}), false).unwrap()["result"].clone();
        let r = rpc(json!({"room": "rocky", "fields": "next, current,colour,next,goal"}));
        assert_eq!(r["isError"], false, "{r}");
        let v = &r["structuredContent"];
        assert_eq!(v["fields"], json!({"next": "release", "current": "budgets", "goal": null}), "the names asked for, once each; a known field not set is null: {v}");
        assert_eq!(v["unknown_fields"], json!(["colour"]));
        assert_eq!((v["room"].as_str(), v["heads"][0].as_str(), v["revisions"].as_i64(), v["state"].as_str()), (Some("rocky"), Some("OBS-rocky-H1"), Some(63), Some("alive")));
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["fields", "heads", "revisions", "room", "state", "unknown_fields"], "nothing else");
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("#rocky State head OBS-rocky-H1") && text.contains("next: release\ncurrent: budgets\ngoal: (not set)\nunknown fields: colour"), "{text}");
        assert!(r.to_string().len() < 1_500, "{} bytes", r.to_string().len());
        // Without fields, every field as before (the negative control).
        let whole = rpc(json!({"room": "rocky"}));
        assert_eq!(whole["structuredContent"]["fields"]["boot"].as_str().map(str::len), Some(8_000));
        assert!(whole.to_string().len() > 16_000);
        // fields is for a read; a write says so and writes nothing.
        let w = rpc(json!({"room": "rocky", "fields": "next", "set": ["next=x"]}));
        assert_eq!(w["isError"], true);
        assert!(f.writes.borrow().is_empty());
        // A State that is not trustworthy says so beside the fields picked.
        let bad = Fake::new(json!({"state:rocky": {"state": "degraded", "heads": ["A", "B"], "revisions": 2, "complete": true, "trustworthy": false,
            "fields": {"next": "n"}, "bad": [{"id": "B", "why": "bad signature"}], "unknown": []}}));
        let b = handle(&bad, &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "oracle_room_state", "arguments": {"room": "rocky", "fields": "next"}}}), false).unwrap()["result"].clone();
        assert_eq!((b["structuredContent"]["trustworthy"].as_bool(), b["structuredContent"]["fields"]["next"].as_str()), (Some(false), Some("n")));
        assert!(b["content"][0]["text"].as_str().unwrap().contains("NOT TRUSTWORTHY"));
    }

    /// The journal record the door would hold for a landed State write: the signed envelope's cargo and the digest the journal computed over the request.
    fn journal_record(parents: &[&str], fields: &Value, sets: &[&str], note: Option<&str>, adopts: Option<Value>, stored: Value) -> Value {
        let parents: Vec<String> = parents.iter().map(|p| p.to_string()).collect();
        let sets: Vec<String> = sets.iter().map(|s| s.to_string()).collect();
        let mut rev = state_revision("oracle", &parents, fields, &sets).unwrap();
        if let Some(a) = adopts {
            rev = with_adoption(rev, a);
        }
        let face = note.map(str::to_owned).unwrap_or_else(|| format!("State updated: {}", one_line(rev["fields"]["current"].as_str().unwrap_or(""), 120)));
        let tags = vec!["room-context:v1".to_string(), "intent:status".to_string()];
        let digest = oracle_proto::sha_hex(json!(["say", "oracle", face, Some(rev.to_string()), tags]).to_string().as_bytes());
        json!({"op_id": "x", "digest": digest, "envelope": {"channel": "oracle", "content": face, "cargo": rev.to_string()}, "outcome": {"stored": stored}})
    }

    // Emmy, 2026-10-03 (478aead0f855, 4eb949760f53): a replay of a write that landed must come back as its receipt, not be judged against the State that moved on, must never
    // become a second write, and must never be answered with the receipt of a DIFFERENT request (another note, another adoption).
    #[test]
    fn a_replayed_state_write_returns_its_recorded_receipt_and_only_for_the_identical_request() {
        let before = json!({"purpose": "memory", "current": "old"});
        let adopt = json!({"room": "oracle", "proposal": "OBS-oracle-P1", "word": "accepted", "mark": "OBS-oracle-M1", "cursor": 5, "decided_by": "blaze", "basis": "membership"});
        let won = json!({"id": "OBS-oracle-WON", "seq": 77});
        let moved = json!({"state:oracle": {"state": "alive", "heads": ["OBS-oracle-NEW"], "revisions": 4, "fields": {"purpose": "memory"}},
            "recorded:op-plain-00001": journal_record(&["OBS-oracle-OLD"], &before, &["current=new work"], None, None, won.clone()),
            "recorded:op-noted-00002": journal_record(&["OBS-oracle-OLD"], &before, &["current=new work"], Some("first explanation"), None, won.clone()),
            "recorded:op-adopt-00003": journal_record(&["OBS-oracle-OLD"], &before, &["current=new work"], None, Some(adopt.clone()), won.clone()),
            "recorded:op-nodigest-04": {"outcome": {"stored": won.clone()}, "envelope": {"channel": "oracle", "cargo": "{}"}},
            "recorded:op-refused-0005": {"outcome": {"ok": false, "refused": "NOT written: conflict: stale State"}, "envelope": {}},
            "recorded:op-unsettled-06": {"outcome": null, "envelope": {}},
            "recorded:op-lostrep-0007": {"outcome": null, "digest": journal_record(&["OBS-oracle-OLD"], &before, &["current=new work"], None, None, won.clone())["digest"].clone(),
                "envelope": journal_record(&["OBS-oracle-OLD"], &before, &["current=new work"], None, None, won.clone())["envelope"].clone()}});
        let f = Fake::new(moved);
        let run = |op: &str, set: &str, head: &str, note: Option<&str>, adopts: Option<&str>| {
            let mut a = json!({"room": "oracle", "set": [set], "head": head, "op_id": op});
            if let Some(n) = note {
                a["note"] = json!(n);
            }
            if let Some(p) = adopts {
                a["adopts"] = json!(p);
            }
            room_state_typed(&f, &a)
        };
        let same = |r: Result<Typed>| {
            let t = r.unwrap();
            assert_eq!((t.value["id"].as_str(), t.value["seq"].as_i64(), t.value["duplicate"].as_bool(), t.value["replayed"].as_bool()), (Some("OBS-oracle-WON"), Some(77), Some(true), Some(true)));
            assert!(t.text.contains("recovered"), "{}", t.text);
        };
        let refused = |r: Result<Typed>, what: &str| {
            let e = r.unwrap_err().to_string();
            assert!(e.contains("already used for a different write"), "{what}: {e}");
        };
        // the identical request, naming the head it was built on: its receipt, and nothing is sent
        same(run("op-plain-00001", "current=new work", "OBS-oracle-OLD", None, None));
        same(run("op-noted-00002", "current=new work", "OBS-oracle-OLD", Some("first explanation"), None));
        same(run("op-adopt-00003", "current=new work", "OBS-oracle-OLD", None, Some("OBS-oracle-P1")));
        assert!(f.writes.borrow().is_empty(), "recovery sends nothing");
        // anything that changes the request changes the digest and is refused: other fields, other head, another note, an adoption added, changed or removed
        refused(run("op-plain-00001", "current=something else", "OBS-oracle-OLD", None, None), "other fields");
        refused(run("op-plain-00001", "current=new work", "OBS-oracle-OTHER", None, None), "other head");
        refused(run("op-plain-00001", "current=new work", "OBS-oracle-OLD", Some("a different explanation"), None), "note added");
        refused(run("op-plain-00001", "current=new work", "OBS-oracle-OLD", None, Some("OBS-oracle-P1")), "adopts added");
        refused(run("op-noted-00002", "current=new work", "OBS-oracle-OLD", Some("different"), None), "note changed");
        refused(run("op-noted-00002", "current=new work", "OBS-oracle-OLD", None, None), "note removed");
        refused(run("op-adopt-00003", "current=new work", "OBS-oracle-OLD", None, None), "adopts removed");
        refused(run("op-adopt-00003", "current=new work", "OBS-oracle-OLD", None, Some("OBS-oracle-P2")), "adopts changed");
        // a record whose original request cannot be recovered is refused, not guessed; a recorded refusal replays as a refusal
        assert!(run("op-nodigest-04", "current=x", "OBS-oracle-OLD", None, None).unwrap_err().to_string().contains("cannot be recovered"));
        assert!(run("op-refused-0005", "current=x", "OBS-oracle-OLD", None, None).unwrap_err().to_string().contains("stale State"));
        // an unsettled record with nothing to recover from is refused (the request cannot be identified); an op never seen falls through to the ordinary preflight:
        // a stale head is refused and writes nothing (CAS is not weakened)
        assert!(run("op-unsettled-06", "current=x", "OBS-oracle-OLD", None, None).unwrap_err().to_string().contains("cannot be recovered"));
        let e = run("op-never-seen-7", "current=x", "OBS-oracle-OLD", None, None).unwrap_err();
        assert!(e.to_string().contains("moved since your read"), "{e}");
        assert!(f.writes.borrow().is_empty());
        // LOST REPLY: an unsettled record of the IDENTICAL request is sent again exactly as journaled (same face, cargo, tags), before any head is judged
        let t = run("op-lostrep-0007", "current=new work", "OBS-oracle-OLD", None, None).unwrap();
        assert_eq!(t.value["replayed"], true);
        assert_eq!(f.writes.borrow().len(), 1, "one write goes again");
        let (kind, room, face, cargo, tags) = f.writes.borrow()[0].clone();
        let sent = journal_record(&["OBS-oracle-OLD"], &before, &["current=new work"], None, None, won.clone());
        assert_eq!((kind.as_str(), room.as_str(), cargo.as_deref(), face.as_str()), ("say", "oracle", sent["envelope"]["cargo"].as_str(), sent["envelope"]["content"].as_str().unwrap()));
        assert_eq!(tags, vec!["room-context:v1".to_string(), "intent:status".to_string()]);
        // a DIFFERENT request under that unsettled op_id is refused, and nothing more is sent
        refused(run("op-lostrep-0007", "current=other", "OBS-oracle-OLD", None, None), "unsettled, other fields");
        refused(run("op-lostrep-0007", "current=new work", "OBS-oracle-OLD", Some("a note"), None), "unsettled, note added");
        assert_eq!(f.writes.borrow().len(), 1);
    }

    // Bobby/Emmy (b95d227cef30): a degraded or untrustworthy State is READ with its diagnosis and the whole evidence; only a WRITE refuses, and it says which failure it is.
    #[test]
    fn a_degraded_state_is_read_with_diagnostics_and_only_writes_refuse_by_cause() {
        let st = json!({"state": "degraded", "heads": ["OBS-oracle-H1"], "revisions": 3, "complete": false, "trustworthy": false, "as_of_seq": 900, "read_through_seq": 899,
            "coverage": {"sql_complete": true, "revisions": 3, "bad": 1, "skipped": 0}, "fields": null,
            "bad": [{"id": "OBS-oracle-BAD", "why": "stored envelope does not verify, or differs from the listing"}], "unknown": [],
            "head_seqs": [{"id": "OBS-oracle-H1", "seq": 880, "author": "rocky", "received_ms": 1, "signed_ms": 2}],
            "evidence": [{"id": "OBS-oracle-H1", "registered": true}], "trust": "server-verified: ...", "adoptions": {}});
        let f = Fake::new(json!({"state:oracle": st}));
        let t = room_state_typed(&f, &json!({"room": "oracle"})).unwrap();
        // the read is an answer, with the whole Core response beside the text and the right cause in words
        assert_eq!((t.value["state"].as_str(), t.value["trustworthy"].as_bool(), t.value["coverage"]["sql_complete"].as_bool()), (Some("degraded"), Some(false), Some(true)));
        assert_eq!(t.value["head_seqs"][0]["signed_ms"], 2);
        assert_eq!(t.value["evidence"][0]["registered"], true);
        assert_eq!(t.value["bad"][0]["id"], "OBS-oracle-BAD");
        assert!(t.text.contains("NOT TRUSTWORTHY") && t.text.contains("OBS-oracle-BAD") && !t.text.contains("cut short"), "trust failure is not described as a short listing: {}", t.text);
        // a write refuses, and names trust, not a page
        let e = room_state_typed(&f, &json!({"room": "oracle", "set": ["current=x"]})).unwrap_err().to_string();
        assert!(e.contains("not trustworthy") && !e.contains("cut short"), "{e}");
        assert!(f.writes.borrow().is_empty());
        // coverage that really is short says so, and a write refuses on that cause
        let short = json!({"state": "alive", "heads": ["OBS-oracle-H1"], "revisions": 3, "complete": false, "trustworthy": true, "coverage": {"sql_complete": false}, "bad": [], "unknown": [], "fields": {"purpose": "p"}});
        let f = Fake::new(json!({"state:oracle": short}));
        assert!(room_state_typed(&f, &json!({"room": "oracle"})).unwrap().text.contains("cut short"));
        assert!(room_state_typed(&f, &json!({"room": "oracle", "set": ["current=x"]})).unwrap_err().to_string().contains("could not be read completely"));
        // contradictory flags fail closed: complete:false alone stops a write although coverage and trust both say fine
        let odd = json!({"state": "alive", "heads": ["OBS-oracle-H1"], "revisions": 3, "complete": false, "trustworthy": true, "coverage": {"sql_complete": true}, "bad": [], "unknown": [], "fields": {"purpose": "p"}});
        let f = Fake::new(json!({"state:oracle": odd}));
        let e = room_state_typed(&f, &json!({"room": "oracle", "set": ["current=x"]})).unwrap_err().to_string();
        assert!(e.contains("not complete"), "{e}");
        assert!(f.writes.borrow().is_empty());
        // and a read of it is still an answer
        assert_eq!(room_state_typed(&f, &json!({"room": "oracle"})).unwrap().value["complete"], false);
    }

    /// A door that really commits and then loses the reply: the first delivery of an op_id is stored, journaled UNSETTLED, and answered with a transport error; any later
    /// delivery of that op_id is answered as a duplicate; a write whose parents are not the current head is refused by the server's compare-and-swap.
    struct Lossy {
        head: RefCell<String>,
        journal: RefCell<std::collections::BTreeMap<String, Value>>,
        committed: RefCell<Vec<(String, Option<String>)>>,
        drop_next_reply: RefCell<bool>,
    }
    fn room_reads_fake() -> Fake {
        Fake::new(json!({
            "state:riemann-room": {"heads": ["OBS-s1"], "revisions": 1, "fields": {"goal": "Prove the room protocol on hard problems", "current": "R105 done", "next": "R106"}},
            "/states": {"heads": [
                {"subject": "mcp.room-reads", "state": "running", "owner": "rocky", "next_touch": "2099-01-01", "channel": "riemann-room"},
                {"subject": "frost.ceremony", "state": "blocked", "owner": "emmy", "next_touch": "2099-01-01", "channel": "riemann-room"},
                {"subject": "old.card", "state": "open", "owner": "noor", "next_touch": "2020-01-01", "channel": "riemann-room"},
                {"subject": "closed.card", "state": "done", "owner": "noor", "next_touch": "2020-01-01", "channel": "riemann-room"}]},
            "/threads": {"room": "riemann-room", "threads": 3, "of": 5, "floors": [], "needs": [{"h": 12, "by": "noor", "fl": "general", "t": "Who owns the gateway?", "re": 2, "id": "OBS-n1"}], "needs_of": 1},
            "/search": {"items": [
                {"id": "OBS-r105", "channel": "riemann-room", "author": "noor", "created_ms": 1792224000000i64, "content": "ROUND 105 — Vitalik's Paranoid Mode\nmore"},
                {"id": "OBS-r104", "channel": "riemann-room", "author": "noor", "created_ms": 1792222000000i64, "content": "ROUND 104 — Black Box Vault"}]},
            "/asks": {"waiting_on_me": [{"id": "OBS-a1", "author": "noor", "card": "Six MCP read tools"}]}
        }))
    }

    #[test]
    fn room_state_full_is_one_read_of_state_board_digest_rounds_and_asks() {
        let f = room_reads_fake();
        let out = call(&f, "oracle_room_state_full", &json!({"room": "riemann-room", "rounds": 2})).unwrap();
        for part in ["== STATE ==", "goal: Prove the room protocol", "== BOARD ==", "frost.ceremony", "== WHERE IT STANDS ==", "needs someone: 1 of 1",
                     "== LAST 2 ROUNDS ==", "OBS-r105 · 2026-", "ROUND 105 — Vitalik's Paranoid Mode", "== MY OPEN ASKS IN #riemann-room ==", "OBS-a1 · noor · Six MCP read tools"] {
            assert!(out.contains(part), "missing {part:?} in:\n{out}");
        }
        assert!(!out.contains("\nmore"), "a round is one line");
        let gets = f.gets.borrow();
        assert!(gets.iter().any(|g| g.starts_with("/search?channel=riemann-room&limit=2&starts=ROUND")), "rounds are read by prefix: {gets:?}");
        assert!(gets.iter().any(|g| g == "/states?channel=riemann-room"));
        assert!(f.writes.borrow().is_empty(), "read-only");
    }

    #[test]
    fn room_digest_agent_is_short_and_names_blocked_and_overdue() {
        let f = room_reads_fake();
        let out = call(&f, "oracle_room_digest_agent", &json!({"room": "riemann-room"})).unwrap();
        assert!(out.contains("goal: Prove the room protocol") && out.contains("next: R106"), "{out}");
        assert!(out.contains("board: 3 live heads · 1 blocked · 1 overdue"), "done heads are not live; only an open one past its date is overdue:\n{out}");
        assert!(out.contains("blocked: frost.ceremony (emmy)") && out.contains("overdue: old.card (noor, next 2020-01-01)"), "{out}");
        assert!(out.contains("oldest: 12h noor · Who owns the gateway?"), "{out}");
        assert!(out.contains("OBS-r104 · 2026-"), "{out}");
        assert!(out.len() < 2000, "under 500 tokens: {} chars", out.len());
        // a room without State says so instead of failing
        let empty = call(&Fake::new(json!({})), "oracle_room_digest_agent", &json!({"room": "x"})).unwrap();
        assert!(empty.contains("this door cannot read it") || empty.contains("no signed State"), "{empty}");
    }

    impl Door for Lossy {
        fn handle(&self) -> &str {
            "rocky"
        }
        fn get(&self, _path: &str) -> Result<Value> {
            Ok(json!({}))
        }
        fn post(&self, _path: &str, _body: &Value) -> Result<Value> {
            Ok(json!({}))
        }
        fn write(&self, _kind: Kind, room: &str, text: &str, cargo: Option<String>, tags: Vec<String>, op_id: Option<String>) -> Result<Written> {
            let op = op_id.expect("an op id");
            let digest = oracle_proto::sha_hex(json!(["say", room, text, cargo, tags]).to_string().as_bytes());
            let known = self.journal.borrow().get(&op).cloned();
            if let Some(rec) = &known {
                if rec["digest"] != json!(digest) {
                    anyhow::bail!("conflict: op_id {op} was already used for a different write");
                }
                // delivered before: the server answers its client_op as the duplicate it is
                if let Some(id) = rec["committed_id"].as_str() {
                    let mut r = self.journal.borrow_mut();
                    r.get_mut(&op).unwrap()["outcome"] = json!({"stored": {"id": id, "seq": 9, "duplicate": true}});
                    return Ok(Written::Stored(json!({"id": id, "seq": 9, "duplicate": true})));
                }
            }
            // the server's compare-and-swap: the revision must name the current head
            let parents: Vec<String> = serde_json::from_str::<Value>(cargo.as_deref().unwrap_or("{}")).ok().and_then(|c| c["parents"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())).unwrap_or_default();
            if parents != [self.head.borrow().clone()] {
                anyhow::bail!("conflict: stale State: the head is [{}]", self.head.borrow());
            }
            let id = format!("OBS-oracle-REV{}", self.committed.borrow().len() + 1);
            self.committed.borrow_mut().push((id.clone(), cargo.clone()));
            *self.head.borrow_mut() = id.clone();
            let lost = self.drop_next_reply.replace(false);
            let mut rec = json!({"digest": digest, "envelope": {"channel": room, "content": text.trim_end(), "cargo": cargo}, "outcome": null, "committed_id": id});
            if !lost {
                rec["outcome"] = json!({"stored": {"id": id, "seq": 8}});
            }
            self.journal.borrow_mut().insert(op, rec);
            if lost {
                anyhow::bail!("outcome unknown: the Oracle may have stored this write; retry it as the same write");
            }
            Ok(Written::Stored(json!({"id": id, "seq": 8})))
        }
        fn identity(&self) -> String {
            "lossy".into()
        }
        fn state(&self, _room: &str) -> Result<Value> {
            let heads = vec![self.head.borrow().clone()];
            Ok(json!({"state": "alive", "heads": heads, "revisions": self.committed.borrow().len() + 1, "complete": true, "trustworthy": true, "bad": [], "unknown": [],
                      "fields": {"purpose": "memory", "current": "start"}}))
        }
        fn recorded_write(&self, op_id: &str) -> Result<Option<Value>> {
            Ok(self.journal.borrow().get(op_id).map(|r| {
                // what a real journal exposes: the envelope and the outcome, plus the digest; never the server's private commit id
                let mut r = r.clone();
                r.as_object_mut().unwrap().remove("committed_id");
                r
            }))
        }
    }

    // Emmy, 2026-10-03 (03aa9d89e42b): commit-then-drop. The server stored the write, the reply was lost, the State moved on; the exact retry must come back as the one
    // write (never a second, never refused by the head the first attempt moved), a changed request must not, and a write that never landed is still the server's to judge.
    #[test]
    fn a_write_the_server_committed_but_whose_reply_was_lost_is_retried_as_the_same_write() {
        let f = Lossy { head: RefCell::new("OBS-oracle-HEAD".into()), journal: RefCell::new(Default::default()), committed: RefCell::new(vec![]), drop_next_reply: RefCell::new(true) };
        let args = |op: &str, set: &str, head: &str| json!({"room": "oracle", "set": [set], "head": head, "op_id": op});
        // 1) the write is committed and the reply is lost: the caller sees an unknown outcome
        let e = room_state_typed(&f, &args("op-lost-00001", "current=work", "OBS-oracle-HEAD")).unwrap_err().to_string();
        assert!(e.contains("outcome unknown"), "{e}");
        assert_eq!(f.committed.borrow().len(), 1);
        assert_eq!(*f.head.borrow(), "OBS-oracle-REV1", "the State has moved past the head the caller used");
        // 2) the exact retry, on the old head: the same write is delivered again and the server answers it as the duplicate; still ONE commit
        let t = room_state_typed(&f, &args("op-lost-00001", "current=work", "OBS-oracle-HEAD")).unwrap();
        assert_eq!((t.value["id"].as_str(), t.value["duplicate"].as_bool(), t.value["written"].as_bool()), (Some("OBS-oracle-REV1"), Some(true), Some(true)), "{}", t.text);
        assert_eq!(f.committed.borrow().len(), 1, "never a second revision");
        // 3) now the journal has the outcome: a third attempt recovers the receipt without sending anything at all
        let n = f.committed.borrow().len();
        let t = room_state_typed(&f, &args("op-lost-00001", "current=work", "OBS-oracle-HEAD")).unwrap();
        assert_eq!(t.value["id"], "OBS-oracle-REV1");
        assert_eq!(f.committed.borrow().len(), n);
        // 4) a CHANGED request under that op_id is refused, with the head the old one used or the new one
        for (set, head) in [("current=other work", "OBS-oracle-HEAD"), ("current=work", "OBS-oracle-REV1")] {
            let e = room_state_typed(&f, &args("op-lost-00001", set, head)).unwrap_err().to_string();
            assert!(e.contains("already used for a different write"), "{set} {head}: {e}");
        }
        assert_eq!(f.committed.borrow().len(), 1);
        // 5) a NEW op_id on the stale head is refused by the preflight, and nothing is written
        let e = room_state_typed(&f, &args("op-new-000002", "current=again", "OBS-oracle-HEAD")).unwrap_err().to_string();
        assert!(e.contains("moved since your read"), "{e}");
        // 6) a write whose first delivery NEVER landed (sent, no answer, not committed) and whose base then went stale is judged by the server's compare-and-swap, not forced in
        f.journal.borrow_mut().insert("op-never-00003".into(), {
            let rev = state_revision("oracle", &["OBS-oracle-HEAD".to_string()], &json!({"purpose": "memory", "current": "start"}), &["current=never".to_string()]).unwrap();
            let face = format!("State updated: {}", one_line("never", 120));
            let tags = vec!["room-context:v1".to_string(), "intent:status".to_string()];
            let digest = oracle_proto::sha_hex(json!(["say", "oracle", face, Some(rev.to_string()), tags]).to_string().as_bytes());
            json!({"digest": digest, "envelope": {"channel": "oracle", "content": face, "cargo": rev.to_string()}, "outcome": null})
        });
        let e = room_state_typed(&f, &args("op-never-00003", "current=never", "OBS-oracle-HEAD")).unwrap_err().to_string();
        assert!(e.contains("stale State"), "the server's guard has the last word: {e}");
        assert_eq!(f.committed.borrow().len(), 1, "nothing was forced in");
    }

    // Emmy, 2026-10-03: a model must be able to mark, annotate and archive through the one MCP, with the same validated tags `ox react|annotate|archive` writes and none of the
    // raw-tag freedom of oracle_say; the server and the fold stay the authority.
    #[test]
    fn mark_annotate_and_archive_write_exactly_what_the_cli_writes_and_refuse_what_it_refuses() {
        let f = Fake::new(json!({}));
        let r = call(&f, "oracle_react", &json!({"room": "oracle", "id": "OBS-oracle-1", "mark": "go", "why": "agreed", "op_id": "mark-op-0001"})).unwrap();
        assert!(r.starts_with("signed:ok"), "{r}");
        let (kind, room, face, cargo, tags) = f.writes.borrow()[0].clone();
        assert_eq!((kind.as_str(), room.as_str(), face.as_str(), cargo), ("say", "oracle", "✓ go — agreed", None));
        assert_eq!(tags, vec!["reply:OBS-oracle-1".to_string(), "react:go".to_string()]);
        // identical to the CLI's builder (they are the same function)
        assert_eq!((face, tags), verbs::reaction("go", "OBS-oracle-1", Some("agreed")).unwrap());
        call(&f, "oracle_annotate", &json!({"room": "oracle", "id": "OBS-oracle-2", "set": ["intent=decision", "ask=act", "item:R1=requirement"], "why": "the words that decided it", "confidence": 0.9})).unwrap();
        let (_, _, face, _, tags) = f.writes.borrow()[1].clone();
        assert_eq!(face, "the words that decided it");
        assert_eq!(tags, verbs::annotation("OBS-oracle-2", &["intent=decision".into(), "ask=act".into(), "item:R1=requirement".into()], Some(0.9)).unwrap());
        assert!(tags.contains(&"annotates:OBS-oracle-2".to_string()) && tags.contains(&"set:intent=decision".to_string()) && tags.contains(&"item:R1=requirement".to_string()) && tags.contains(&"confidence:0.90".to_string()));
        call(&f, "oracle_archive", &json!({"room": "oracle", "id": "OBS-oracle-3", "note": "my mistaken go"})).unwrap();
        let (_, _, face, cargo, tags) = f.writes.borrow()[2].clone();
        assert!(face.starts_with("Archived") && face.contains("my mistaken go") && tags.contains(&"archives:OBS-oracle-3".to_string()), "{face} {tags:?}");
        assert!(cargo.unwrap().contains("room-reply/1"));
        // what the CLI refuses, the tool refuses, and nothing is written
        let n = f.writes.borrow().len();
        for (tool, args) in [
            ("oracle_react", json!({"room": "oracle", "id": "OBS-oracle-1", "mark": "applaud"})),
            ("oracle_react", json!({"room": "oracle", "id": "not an id!", "mark": "go"})),
            ("oracle_annotate", json!({"room": "oracle", "id": "OBS-oracle-2", "set": [], "why": "x"})),
            ("oracle_annotate", json!({"room": "oracle", "id": "OBS-oracle-2", "set": ["noequals"], "why": "x"})),
            ("oracle_annotate", json!({"room": "oracle", "id": "OBS-oracle-2", "set": ["a b=c"], "why": "x"})),
            ("oracle_annotate", json!({"room": "oracle", "id": "OBS-oracle-2", "set": ["intent=decision"], "why": "x", "confidence": 1.5})),
            ("oracle_annotate", json!({"room": "oracle", "id": "OBS-oracle-2", "set": ["intent=decision"], "why": "  "})),
            ("oracle_annotate", json!({"room": "oracle", "id": "msg", "set": ["intent=decision"], "why": "x"})),
            ("oracle_archive", json!({"room": "oracle", "id": "nope"})),
        ] {
            assert!(call(&f, tool, &args).is_err(), "{tool} {args}");
        }
        assert_eq!(f.writes.borrow().len(), n, "a refused update writes nothing");
        // the three are listed, write tools, and each says what it needs
        let list = tools(false);
        for name in ["oracle_react", "oracle_annotate"] {
            let t = list.as_array().unwrap().iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("{name} listed"));
            assert!(t["inputSchema"]["required"].as_array().unwrap().contains(&json!("id")), "{name}");
        }
    }

    // Emmy, 2026-10-03: typed recall kept only the score; the backend's match fields (where a word matched, down to the byte span in the ORIGINAL cargo field) were dropped.
    #[test]
    fn typed_recall_passes_the_match_location_through_verbatim_and_never_invents_one() {
        let hit = |id: &str, extra: Value| {
            let mut h = json!({"id": id, "channel": "oracle", "author": "emmy", "content": "x", "created_ms": 1790000000000i64, "tags": [], "score": 3.5});
            for (k, v) in extra.as_object().unwrap() {
                h[k] = v.clone();
            }
            h
        };
        let f = Fake::new(json!({"/whoami": {"server_ms": 1790000100000i64}, "/search": {"engine": "keyword", "hits": [
            hit("OBS-1", json!({"match_in": "cargo", "match_pointer": "/items/4/text", "match_span": {"start": 10, "end": 25, "unit": "utf8_bytes"}})),
            hit("OBS-2", json!({"match_in": "content", "match_span": {"start": 0, "end": 4, "unit": "utf8_bytes"}})),
            hit("OBS-3", json!({"match_in": "tags"})),
            hit("OBS-4", json!({"match_in": "cargo"})),
            hit("OBS-5", json!({})),
            hit("OBS-6", json!({"match_in": null})),
        ]}}));
        let t = typed_read(&f, "oracle_recall", &json!({"query": "independent repos", "gist": false})).expect("typed").unwrap();
        let hits = t.value["hits"].as_array().unwrap();
        // the cargo hit: exactly what the server sent, and a hint that names the field and the bytes
        assert_eq!(hits[0]["match_in"], "cargo");
        assert_eq!(hits[0]["match_pointer"], "/items/4/text");
        assert_eq!(hits[0]["match_span"], json!({"start": 10, "end": 25, "unit": "utf8_bytes"}));
        assert_eq!(hits[0]["match_hint"], "matched in cargo /items/4/text bytes 10-25");
        assert!(t.text.contains("[matched in cargo /items/4/text bytes 10-25]"), "{}", t.text);
        assert_eq!(hits[1]["match_hint"], "matched in the message text bytes 0-4");
        assert_eq!(hits[2]["match_hint"], "matched in tags only");
        // a cargo hit the server could not locate: no pointer, no span, and the hint says so
        assert_eq!(hits[3]["match_in"], "cargo");
        assert!(hits[3].get("match_pointer").is_none() && hits[3].get("match_span").is_none(), "nothing invented");
        assert_eq!(hits[3]["match_hint"], "matched in cargo (the field was not located)");
        // NEGATIVE CONTROLS: a meaning-only hit and a null location carry no location and no hint
        for i in [4usize, 5] {
            assert!(hits[i].get("match_span").is_none() && hits[i].get("match_pointer").is_none() && hits[i].get("match_hint").is_none(), "hit {i}: {:?}", hits[i]);
        }
        assert!(hits[4].get("match_in").is_none(), "absent stays absent");
        assert!(hits[5]["match_in"].is_null(), "null stays null");
        assert!(t.value["hits"].as_array().unwrap().iter().all(|h| h["score"] == 3.5), "the score is still passed");
    }

    #[test]
    fn say_carries_cargo_whole_and_refuses_cargo_that_is_not_json() {
        let f = Fake::new(json!({}));
        let r = call(&f, "oracle_say", &json!({"room": "oracle", "text": "pick one", "tags": ["ask:decide"], "cargo": "{\"options\": [\"a\", \"b\"]}"})).unwrap();
        assert!(r.starts_with("signed:ok"), "{r}");
        assert_eq!(f.writes.borrow()[0].3.as_deref(), Some("{\"options\": [\"a\", \"b\"]}"));
        assert!(call(&Fake::new(json!({})), "oracle_say", &json!({"room": "o", "text": "t", "cargo": "{not json"})).is_err());
    }

    #[test]
    fn oracle_say_closes_sets_an_expiry_offers_options_and_picks_one_like_ox_say() {
        let f = Fake::new(json!({}));
        call(&f, "oracle_say", &json!({"room": "hustler", "text": "done: receipt", "reply": "OBS-hustler-1", "closes": ["OBS-hustler-1", "OBS-hustler-2"]})).unwrap();
        let (_, _, _, _, tags) = f.writes.borrow()[0].clone();
        assert!(tags.contains(&"closes:OBS-hustler-1".to_string()) && tags.contains(&"closes:OBS-hustler-2".to_string()) && tags.contains(&"reply:OBS-hustler-1".to_string()), "{tags:?}");
        call(&f, "oracle_say", &json!({"room": "hustler", "text": "which?", "to": ["hustler"], "ask": "decide", "options": ["Hold", "Cut half"], "recommend": 1, "why": "the level has not failed on a close", "by": "2h"})).unwrap();
        let (_, _, text, cargo, tags) = f.writes.borrow()[1].clone();
        let cargo: Value = serde_json::from_str(cargo.as_deref().unwrap()).unwrap();
        assert_eq!(cargo["options"], json!(["Hold", "Cut half"]));
        assert_eq!((cargo["recommendation"]["option"].as_u64(), cargo["recommendation"]["label"].as_str()), (Some(1), Some("Hold")), "the pick rides in cargo");
        assert!(text.contains("1) Hold (recommended)") && text.contains("I recommend 1: the level has not failed on a close"), "and in the words: {text}");
        let by: i64 = tags.iter().find_map(|t| t.strip_prefix("by:")).unwrap().parse().unwrap();
        let now = now_ms();
        assert!((by - now - 2 * 3_600_000).abs() < 60_000, "by is two hours from now: {by} vs {now}");
        assert!(tags.contains(&"ask:decide".to_string()));
        call(&f, "oracle_say", &json!({"room": "hustler", "text": "Hold", "reply": "OBS-hustler-3", "choice": 1})).unwrap();
        assert!(f.writes.borrow()[2].4.contains(&"choice:1".to_string()));
        assert_eq!(parse_by("2026-10-01T15:30").unwrap(), 1_790_868_600_000, "a UTC date, same as ox say --by");
        // what ox say refuses, the tool refuses, and nothing is written
        let n = f.writes.borrow().len();
        for args in [
            json!({"room": "r", "text": "x", "closes": ["not-an-id"]}),
            json!({"room": "r", "text": "x", "options": ["a", "b"]}),
            json!({"room": "r", "text": "x", "ask": "decide", "options": ["a"]}),
            json!({"room": "r", "text": "x", "ask": "decide", "options": ["a", "a"]}),
            json!({"room": "r", "text": "x", "ask": "decide", "options": ["a", "x".repeat(81)]}),
            json!({"room": "r", "text": "x", "ask": "decide"}),
            json!({"room": "r", "text": "x", "ask": "decide", "options": ["a", "b"]}),
            json!({"room": "r", "text": "x", "ask": "decide", "options": ["a", "b"], "recommend": 3, "why": "w"}),
            json!({"room": "r", "text": "x", "ask": "decide", "options": ["a", "b"], "recommend": 1}),
            json!({"room": "r", "text": "x", "choice": 1}),
            json!({"room": "r", "text": "x", "by": "soon"}),
            json!({"room": "r", "text": "x", "ask": "decide", "options": ["a", "b"], "cargo": "{\"options\":[\"c\",\"d\"]}"}),
        ] {
            assert!(call(&f, "oracle_say", &args).is_err(), "{args}");
        }
        assert_eq!(f.writes.borrow().len(), n);
        let say = tools(false).as_array().unwrap().iter().find(|t| t["name"] == "oracle_say").cloned().unwrap();
        for k in ["closes", "by", "options", "choice"] {
            assert!(say["inputSchema"]["properties"][k].is_object(), "{k} is in the schema");
        }
    }

    #[test]
    fn say_relative_deadlines_replay_after_lost_reply_and_restart_without_drifting() {
        struct JournalSay { record: RefCell<Option<Value>>, lost: std::cell::Cell<bool> }
        impl Door for JournalSay {
            fn handle(&self) -> &str { "emmy" }
            fn identity(&self) -> String { "fixture".into() }
            fn get(&self, _: &str) -> Result<Value> { Ok(json!({})) }
            fn post(&self, _: &str, _: &Value) -> Result<Value> { unreachable!() }
            fn recorded_write(&self, _: &str) -> Result<Option<Value>> { Ok(self.record.borrow().clone()) }
            fn write(&self, kind: Kind, room: &str, text: &str, cargo: Option<String>, tags: Vec<String>, _: Option<String>) -> Result<Written> {
                let digest = oracle_proto::sha_hex(json!([kind.as_str(), room, text, cargo, tags]).to_string().as_bytes());
                if let Some(old) = self.record.borrow().as_ref() {
                    anyhow::ensure!(old["digest"] == digest, "different write");
                }
                *self.record.borrow_mut() = Some(json!({"digest":digest,"envelope":{"tags":tags}}));
                if self.lost.replace(false) { bail!("outcome unknown") }
                Ok(Written::Stored(json!({"id":"OBS-fixture-original","duplicate":true})))
            }
        }
        let first = JournalSay { record: RefCell::new(None), lost: std::cell::Cell::new(true) };
        let args = json!({"room":"dot","text":"Please review","ask":"review","by":"2h","expires":"7d","op_id":"relative-retry-01"});
        assert!(call(&first, "oracle_say", &args).unwrap_err().to_string().contains("outcome unknown"));
        let record = first.record.borrow().clone();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let restarted = JournalSay { record: RefCell::new(record.clone()), lost: std::cell::Cell::new(false) };
        let receipt = call(&restarted, "oracle_say", &args).unwrap();
        assert!(receipt.contains("OBS-fixture-original"));
        assert_eq!(*restarted.record.borrow(), record, "same payload after a lost reply and a fresh process");
        for (field, value) in [("by", json!("3h")), ("expires", json!("8d")), ("text", json!("Changed")), ("room", json!("other")), ("cargo", json!("{}")), ("by", Value::Null)] {
            let mut changed = args.clone(); changed[field] = value;
            assert!(call(&restarted, "oracle_say", &changed).unwrap_err().to_string().contains("different write"), "{field}");
        }
        let mut forged = args.clone(); forged["tags"] = json!(["mcp-time:1"]);
        assert!(call(&restarted, "oracle_say", &forged).unwrap_err().to_string().contains("reserved"));
        let malformed = JournalSay { record: RefCell::new(Some(json!({"envelope":{"tags":["mcp-time:bad"]}}))), lost: std::cell::Cell::new(false) };
        assert!(call(&malformed, "oracle_say", &args).unwrap_err().to_string().contains("invalid time anchor"));
    }

    #[test]
    fn the_brief_prints_a_map_line_for_every_room_my_state_names_from_one_read() {
        let f = Fake::new(json!({"/whoami": {"handle": "rocky"}, "/fold": {"items": []}, "/states": {"heads": []},
            "state:rocky": {"state": "alive", "heads": ["OBS-rocky-1"], "fields": {"kind": "mind", "boot": "wake", "rooms": "#hustler — the product; (#axxis) the OS. #rocky mine, #hustler again"}},
            "/maps": {"rooms": [{"room": "hustler", "line": "#hustler  The product. · 2,781 records · last 3h ago", "written": {"text": "The product."}},
                                {"room": "axxis", "line": "#axxis  no map yet · 9,381 records · last 1h ago · words ci drift", "written": null},
                                {"room": "rocky", "line": "#rocky  My worldline. · 981 records", "written": {"text": "My worldline."}}]}}));
        let b = brief_text(&f).unwrap();
        assert!(f.gets.borrow().iter().any(|g| g.replace("%2C", ",") == "/maps?rooms=hustler,axxis,rocky"), "one read, the rooms in the order my State names them, once each: {:?}", f.gets.borrow());
        let maps = b.find("MAPS       what my rooms hold").expect("MAPS");
        let (rooms, inbox) = (b.find("ROOMS      #hustler").unwrap(), b.find("ASKS FOR ME").unwrap());
        assert!(rooms < maps && maps < inbox, "after my State, before the inbox:\n{b}");
        assert!(b.contains("\n  #hustler  The product. · 2,781 records · last 3h ago\n  #axxis  no map yet · 9,381 records") && b.contains("\n  #rocky  My worldline. · 981 records\n"), "{b}");
        let v = brief_value(&f, false).unwrap();
        assert_eq!((v["maps"][0]["written"].as_bool(), v["maps"][1]["written"].as_bool()), (Some(true), Some(false)), "the typed brief says which rooms have a written map");
        // no rooms named: no read and no line; an unreadable /maps says so
        let none = Fake::new(json!({"/whoami": {"handle": "rocky"}, "state:rocky": {"state": "alive", "heads": ["x"], "fields": {"kind": "mind", "boot": "b"}}}));
        let b = brief_text(&none).unwrap();
        assert!(!b.contains("MAPS") && !none.gets.borrow().iter().any(|g| g.starts_with("/maps")), "{b}");
        let down = Fake::new(json!({"/whoami": {"handle": "rocky"}, "state:rocky": {"state": "alive", "heads": ["x"], "fields": {"kind": "mind", "boot": "b", "rooms": "#axxis"}}, "/maps": {"__err": "Oracle unreachable"}}));
        assert!(brief_text(&down).unwrap().contains("MAPS       could not be read: Oracle unreachable"));
        assert_eq!(rooms_named("#a, #b-c (#d) #a #e_f and not#g #"), vec!["a", "b-c", "d", "e_f"]);
    }

    #[test]
    fn a_map_is_a_state_field_kept_across_revisions_read_as_text_and_written_through_the_tool() {
        let rev = state_revision("axxis", &["H1".into()], &json!({"purpose": "the OS", "map": "The OS room.\nHeads: the board."}), &["current=now".into()]).unwrap();
        // The public fields (docs/PUBLIC-STATE.md) are kept like a seat's, dropped when emptied, and latest stamps latest_at.
        let base = json!({"purpose": "p", "about": "A room.", "latest": "27 Oct: built.", "latest_at": "2026-10-27"});
        let r = state_revision("eth", &[], &base, &["current=now".into()]).unwrap();
        assert_eq!((r["fields"]["about"].as_str(), r["fields"]["latest_at"].as_str()), (Some("A room."), Some("2026-10-27")), "kept, not restamped");
        let r = state_revision("eth", &[], &base, &["latest=31 Oct: checked.".into(), "about=".into()]).unwrap();
        let today = ymd(now_ms().div_euclid(86_400_000));
        assert_eq!(r["fields"]["latest_at"].as_str(), Some(today.as_str()));
        assert!(r["fields"].get("about").is_none(), "an emptied public field is removed");
        let r = state_revision("eth", &[], &base, &["latest=".into()]).unwrap();
        assert!(r["fields"].get("latest").is_none() && r["fields"].get("latest_at").is_none());
        assert!(state_revision("eth", &[], &base, &["latest_at=2026-11-01".into()]).unwrap_err().to_string().contains("stamped"));
        let day = 86_400_000;
        let at = touch_ms("2026-10-27").unwrap();
        assert_eq!(latest_stale_days(&base, at + 7 * day), None, "seven days is still fresh");
        assert_eq!(latest_stale_days(&base, at + 8 * day), Some(8));
        assert_eq!(latest_stale_days(&json!({"latest_at": "2026-01-01"}), at), None, "no LATEST, nothing to flag");
        assert_eq!(rev["fields"]["map"], "The OS room.\nHeads: the board.", "a revision that does not name the map keeps it");
        let rev = state_revision("axxis", &[], &json!({}), &["map=What it holds.".into()]).unwrap();
        assert_eq!(rev["fields"]["map"], "What it holds.");
        assert!(state_revision("axxis", &[], &json!({}), &[format!("map={}", "x".repeat(4001))]).unwrap_err().to_string().contains("at most 4000"));
        assert!(state_revision("axxis", &[], &json!({}), &[format!("purpose={}", "x".repeat(1501))]).unwrap_err().to_string().contains("at most 1500"));
        // read: the line, the rest of the map, who wrote it, the facts the line left out
        let v = json!({"room": "axxis", "line": "#axxis  The OS room. · 7 records · last 40m ago · seats rocky 6", "written": {"text": "The OS room.\nHeads: the board.\nSkip: logs.", "head": "OBS-axxis-9", "by": "rocky", "at_ms": 1_800_000_000_000i64},
            "computed": {"first_ms": 1_700_000_000_000i64, "state_fields": ["purpose", "current"], "words": [{"word": "ci", "n": 9}]}});
        let t = room_map_text(&v);
        assert_eq!(t, "#axxis  The OS room. · 7 records · last 40m ago · seats rocky 6\n  Heads: the board.\n  Skip: logs.\n  — map OBS-axxis-9 by rocky · 2027-01-15\n  first 2023-11-14 · State fields filled: purpose, current · words by count: ci 9\n");
        let bare = room_map_text(&json!({"room": "r", "line": "#r  no map yet · 0 records", "written": null, "computed": {"state_fields": [], "words": []}}));
        assert_eq!(bare, "#r  no map yet · 0 records\n  write one: ox map r --set \"what the room holds · its heads · what not to read\"\n");
        // the tool: a read goes to the route; a set writes the revision oracle_room_state would write (map= on the current head, the other fields kept)
        let f = Fake::new(json!({"/rooms/axxis/map": v, "state:axxis": {"state": "alive", "heads": ["OBS-axxis-9"], "fields": {"purpose": "the OS"}}}));
        assert!(call(&f, "oracle_room_map", &json!({"room": "axxis"})).unwrap().starts_with("#axxis  The OS room."));
        let r = call(&f, "oracle_room_map", &json!({"room": "axxis", "set": "What it holds.\nHeads: the board."})).unwrap();
        let (_, room, face, cargo, tags) = f.writes.borrow()[0].clone();
        let cargo: Value = serde_json::from_str(cargo.as_deref().unwrap()).unwrap();
        assert_eq!((room.as_str(), face.as_str()), ("axxis", "Map updated"));
        assert_eq!(cargo["fields"]["map"], "What it holds.\nHeads: the board.");
        assert_eq!(cargo["fields"]["purpose"], "the OS", "the other fields are kept");
        assert_eq!(cargo["parents"], json!(["OBS-axxis-9"]));
        assert!(tags.contains(&"room-context:v1".to_string()) && r.contains("OBS-1"), "{tags:?} {r}");
        assert!(call(&f, "oracle_room_map", &json!({"room": "axxis", "set": "x".repeat(4001)})).is_err(), "too long is refused before any write");
        assert_eq!(f.writes.borrow().len(), 1);
        let listed = tools(false);
        assert!(listed.as_array().unwrap().iter().any(|t| t["name"] == "oracle_room_state" && t["inputSchema"]["properties"]["map"].is_object()), "the map is a mode of oracle_room_state");
        assert!(!listed.as_array().unwrap().iter().any(|t| t["name"] == "oracle_room_map"), "oracle_room_map is an old name");
    }

    #[test]
    fn brief_opens_with_the_seats_own_room_state_boot_first() {
        // A mind's room State is its bootstrap: ME, then BOOT before anything addressed to it.
        let f = Fake::new(json!({"/whoami": {"handle": "rocky"}, "/fold": {"items": []}, "/states": {"heads": []},
            "state:rocky": {"state": "alive", "heads": ["OBS-rocky-1"], "fields": {"kind": "mind", "boot": "Rocky, seat rocky; wake with ox brief",
                "tools": "ox", "rooms": "#axxis the OS · #hustler the product", "rules": "never ship prod unasked", "current": "seat rooms", "next": "sign #rocky", "purpose": "", "goal": ""}}}));
        let b = brief_text(&f).unwrap();
        let me = b.find("ME         #rocky · mind · State OBS-rocky-1").expect("ME line");
        assert!(!b.contains("PERSONA"), "no persona set: no PERSONA line");
        let f2 = Fake::new(json!({"/whoami": {"handle": "rocky"}, "/fold": {"items": []}, "/states": {"heads": []},
            "state:rocky": {"state": "alive", "heads": ["OBS-rocky-1"], "fields": {"kind": "mind", "persona": "Rocky, the Eridian engineer; Blaze is Grace", "boot": "wake with ox brief"}}}));
        let b2 = brief_text(&f2).unwrap();
        let (p, bo) = (b2.find("PERSONA    Rocky, the Eridian engineer; Blaze is Grace").expect("PERSONA line"), b2.find("BOOT       wake").expect("BOOT line"));
        assert!(p < bo, "who the mind is comes before what it must do: {b2}");
        let boot = b.find("BOOT       Rocky, seat rocky").expect("BOOT line");
        let addressed = b.find("ASKS FOR ME").unwrap();
        assert!(me < boot && boot < addressed, "boot comes before the inbox:\n{b}");
        assert!(b.contains("ROOMS      #axxis the OS") && b.contains("RULES      never ship") && b.contains("NEXT       sign #rocky"));
        assert!(!b.contains("GOAL"), "an empty field prints no line");
        // A project room's State is not a bootstrap; an empty one says how to write it; Null (a door that
        // cannot resolve one) says nothing.
        assert_eq!(seat_text("axxis", &json!({"state": "alive", "heads": ["x"], "fields": {"kind": "project", "boot": "no"}})), "");
        let empty = Fake::new(json!({"/whoami": {"handle": "rocky"}, "state:rocky": {"state": "empty", "heads": [], "fields": null}}));
        assert!(brief_text(&empty).unwrap().contains("ME         #rocky has no State yet — ox state rocky --set kind=mind"));
        let none = Fake::new(json!({"/whoami": {"handle": "rocky"}}));
        assert!(!brief_text(&none).unwrap().contains("ME  "));
    }

    #[test]
    fn the_brief_lists_my_cards_and_the_cards_waiting_on_me() {
        let card = |room: &str, id: &str, status: &str, owner: &str, unblock: Value| json!({"room": room, "id": id, "title": format!("Title of {id}"), "status": status, "effective_status": status, "owner": owner, "unblock": unblock, "since_ms": 1_000_000, "conflict": false});
        let f = Fake::new(json!({"/whoami": {"handle": "dinesh"}, "/fold": {"items": []}, "/states": {"heads": []},
            "/cards?owner=dinesh": {"now_ms": 1_000_000 + 3 * 3_600_000, "cards": [card("core", "mine-1", "in_progress", "dinesh", Value::Null)]},
            "/cards?waiting_on=dinesh": {"now_ms": 1_000_000 + 90 * 60_000, "cards": [card("rocky", "theirs-1", "review", "rocky", json!("dinesh: review PR 27"))]}}));
        let b = brief_with(&f, true).unwrap();
        assert!(b.contains("YOUR CARDS (1)\n  in_progress #core     mine-1  3h  Title of mine-1\n"), "{b}");
        assert!(b.contains("WAITING ON YOU (1)\n  review      #rocky    theirs-1  1h  Title of theirs-1  · rocky  ← dinesh: review PR 27\n"), "{b}");
        assert!(b.find("MY OPEN WORK").unwrap() < b.find("YOUR CARDS").unwrap());
        // A server that has no /cards yet leaves the brief as it was.
        let old = Fake::new(json!({"/whoami": {"handle": "dinesh"}, "/fold": {"items": []}, "/states": {"heads": []}}));
        assert!(!brief_text(&old).unwrap().contains("YOUR CARDS"));
    }

    #[test]
    fn the_brief_is_typed_starts_from_the_oracle_clock_and_lists_the_derived_asks() {
        let now = 1_791_786_647_741i64; // 2026-10-12T06:30:47.741Z
        let f = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": now},
            "/fold": {"items": [], "pending": {"hustler": 17, "taylor": 9}},
            "/states": {"heads": []},
            "/asks": {"seat": "rocky", "capped": false, "waiting_on_me": [
                {"id": "OBS-r1", "room": "oracle", "author": "emmy", "ask": "review", "state": "open", "created_ms": now - 25 * 60_000, "overdue": false, "card": "@rocky review #57"},
                {"id": "OBS-r2", "room": "axxis", "author": "blaze", "ask": "act", "state": "open", "created_ms": now - 3 * 86_400_000, "overdue": true, "card": "ship it"},
                {"id": "OBS-r3", "room": "dot", "author": "jobs", "ask": "reply", "state": "answered", "created_ms": now - 60 * 60_000, "overdue": false, "card": "x"},
                {"id": "OBS-old", "room": "axxis", "author": "kepler", "ask": "reply", "state": "open", "created_ms": INBOX_SINCE_MS - 1, "overdue": true, "card": "old"}]}}));
        let v = brief_value(&f, false).unwrap();
        assert_eq!(v["now"], "2026-10-12T06:30:47.741Z");
        assert_eq!(v["asks"]["rows"][0]["age"], "25m ago");
        assert_eq!(v["asks"]["rows"][1]["age"], "2026-10-09", "older than two days: the date");
        assert_eq!(v["elsewhere"]["total"], 26);
        let text = brief_render(&v, false);
        assert!(text.starts_with("ox brief · rocky · now 12 Oct 06:30Z (Oracle clock)\n"), "the clock as a reader needs it: {text}");
        assert!(text.contains("ASKS FOR ME (2 open, 1 answered by another seat, 1 from before 8 Oct: ox inbox --older) — read one whole with oracle_thread <id>"), "{text}");
        assert!(text.contains("review #oracle    emmy         25m ago   @rocky review #57  (OBS-r1)"), "{text}");
        assert!(text.contains(" OVERDUE ship it  (OBS-r2)"), "{text}");
        // The same count as the inbox: the pre-inbox ask is counted, not listed; the answered one comes last.
        assert!(!text.contains("OBS-old") && text.find("OBS-r2").unwrap() < text.find("OBS-r3").unwrap(), "{text}");
        assert!(text.contains("ELSEWHERE  26 new · #hustler 17 · #taylor 9"), "{text}");
        // Through MCP the object itself is the structuredContent; the text rides beside it.
        let r = handle(&f, &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "oracle_brief", "arguments": {}}}), false).unwrap();
        assert_eq!(r["result"]["structuredContent"]["seat"], "rocky");
        assert_eq!(r["result"]["structuredContent"]["asks"]["rows"][0]["id"], "OBS-r1");
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("ASKS FOR ME (2 open"));
    }

    #[test]
    fn the_inbox_gives_what_is_for_me_whole_and_counts_the_rest_per_room() {
        let now = 1_790_836_247_741i64;
        let long = format!("@rocky please review\n{}", "detail ".repeat(200));
        let it = |id: &str, room: &str, author: &str, for_me: bool, tags: &[&str], text: &str| json!({"id": id, "channel": room, "author": author,
            "for_me": for_me, "tags": tags, "content": text, "created_ms": now - 5 * 60_000});
        let mut items = vec![it("OBS-me", "oracle", "emmy", true, &["intent:review", "ask:reply"], &long)];
        for i in 0..11 {
            items.push(it(&format!("OBS-me{i}"), "axxis", "bobby-core", true, &["ask:act"], "do this"));
        }
        for i in 0..3 {
            items.push(it(&format!("OBS-t{i}"), "taylor", "taylor-core", false, &["intent:status", "ask:none"], "scan: no crosses"));
        }
        items.push(it("OBS-h1", "hustler", "bobby-core", false, &["intent:result"], "XAG new low"));
        // Asks to me: one since the inbox began, one before it (counted, not listed), one I marked stuck, one I acked.
        let ask = |id: &str, ms: i64, marks: Value| json!({"id": id, "room": "oracle", "author": "noor", "ask": "reply", "state": if id == "OBS-new" { "answered" } else { "open" }, "created_ms": ms, "card": "which seats are live?", "marks": marks});
        let asks = json!({"waiting_on_me": [ask("OBS-new", INBOX_SINCE_MS + 1, json!({})), ask("OBS-old", INBOX_SINCE_MS - 1, json!({"rocky": "seen"}))],
                          "stuck": [ask("OBS-st", INBOX_SINCE_MS + 2, json!({"rocky": "stuck"}))], "acked": [ask("OBS-ak", INBOX_SINCE_MS + 3, json!({"rocky": "ack"}))]});
        let f = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": now}, "/asks": asks,
            "/fold": {"items": items, "pending": {"oracle": 1, "axxis": 11, "taylor": 3, "hustler": 1}, "has_more": false, "next_marks": {"oracle": 9}}}));
        let v = inbox_value(&f, 50, Ack::No, false).unwrap();
        assert_eq!((v["asks"]["open"].as_array().unwrap().len(), v["asks"]["older_count"].clone()), (1, json!(1)), "{v}");
        assert!(v["asks"]["older"].as_array().unwrap().is_empty(), "older asks are counted, not listed, unless asked for");
        let text = inbox_render(&v);
        assert!(text.contains("ASKS TO ME · 0 open, every room.") && text.contains("ANSWERED BY ANOTHER SEAT, yours still open · 1\n  OBS-new  #oracle  from noor"), "an answered ask is not counted as open: {text}");
        assert!(text.contains("+ 1 older open asks from before 8 Oct 08:00 UTC"), "{text}");
        assert!(text.contains("STUCK · 1\n  OBS-st") && text.contains("ACKED, not done · 1\n  OBS-ak") && text.contains("[rocky ack]"), "{text}");
        let all = inbox_value(&f, 50, Ack::No, true).unwrap();
        assert_eq!(all["asks"]["older"][0]["id"], "OBS-old", "older lists them");
        // Negative control: no /asks answer is said, not shown as nothing waiting.
        let f0 = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": now}, "/asks": {"__err": "asks down"}, "/fold": {"items": []}}));
        assert!(inbox_render(&inbox_value(&f0, 50, Ack::No, false).unwrap()).contains("ASKS TO ME · could not be read"));
        assert_eq!(v["for_me"].as_array().unwrap().len(), 12, "every addressed message on the page is listed");
        assert_eq!(v["for_me"][0]["text"], json!(long), "whole: nothing cut");
        // Past the budget a message is listed by header and size, never cut.
        let big = "x".repeat(INBOX_TEXT_BUDGET);
        let f2 = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": now},
            "/fold": {"items": [it("OBS-a", "oracle", "emmy", true, &[], "short"), it("OBS-b", "oracle", "emmy", true, &[], &big)], "pending": {"oracle": 2}}}));
        let v2 = inbox_value(&f2, 50, Ack::No, false).unwrap();
        assert_eq!(v2["for_me"][0]["text"], "short");
        assert!(v2["for_me"][1]["text"].is_null() && v2["for_me"][1]["bytes"] == INBOX_TEXT_BUDGET, "{v2}");
        assert!(inbox_render(&v2).contains(&format!("({INBOX_TEXT_BUDGET} bytes, past this answer's budget: oracle_get OBS-b)")));
        // What was written to me today comes first even when the page (oldest first) never reached it; once each.
        let f3 = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": now},
            "/fold": {"items": [it("OBS-a", "oracle", "emmy", true, &[], "old one")], "pending": {"oracle": 900},
                      "unread_for_me": [it("OBS-today", "dot", "jobs", true, &[], "today's"), it("OBS-a", "oracle", "emmy", true, &[], "old one")]}}));
        let v3 = inbox_value(&f3, 2, Ack::No, false).unwrap();
        let ids: Vec<&str> = v3["for_me"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["OBS-today", "OBS-a"]);
        // The limit bounds them too (1 Nov 2026): the newest is kept, the other counted.
        let v1 = inbox_value(&f3, 1, Ack::No, false).unwrap();
        assert_eq!((v1["for_me"].as_array().map(Vec::len), v1["for_me"][0]["id"].as_str(), v1["budget"]["cut"].as_u64()), (Some(1), Some("OBS-today"), Some(1)));
        let rooms = v["rooms"].as_array().unwrap();
        assert_eq!((rooms[0]["room"].clone(), rooms[0]["count"].clone(), rooms[0]["routine"].clone()), (json!("taylor"), json!(3), json!(3)));
        assert_eq!((rooms[1]["room"].clone(), rooms[1]["routine"].clone()), (json!("hustler"), json!(0)));
        let text = inbox_render(&v);
        assert!(text.starts_with("inbox · now 2026-10-01T06:30:47.741Z (Oracle clock, UTC)\nASKS TO ME"), "{text}");
        assert!(text.contains("NEW · 16 new, 16 on this page\n"), "{text}");
        assert!(text.contains("FOR YOU (12), newest first, whole or by header\n── #oracle · emmy · 5m ago · review · ask reply · OBS-me\n@rocky please review"), "{text}");
        let taylor = text.lines().find(|l| l.trim_start().starts_with("#taylor")).unwrap();
        assert!(taylor.split_whitespace().collect::<Vec<_>>() == ["#taylor", "3", "taylor-core", "3", "(3", "routine", "status)"], "{taylor}");
        assert!(text.contains("── #axxis · bobby-core · 5m ago · ask act · OBS-me0\ndo this"), "{text}");
        assert_eq!(text.matches("#taylor").count(), 1, "a room is named once");
        // ack marks the page, but never past an addressed message listed without its text.
        let mut a = it("OBS-a", "oracle", "emmy", true, &[], "short");
        a["seq"] = json!(100);
        let mut b = it("OBS-b", "oracle", "emmy", true, &[], &big);
        b["seq"] = json!(105);
        let f3 = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": now},
            "/fold": {"items": [a, b], "pending": {"oracle": 2, "taylor": 1}, "next_marks": {"oracle": 110, "taylor": 50}}}));
        let v3 = inbox_value(&f3, 50, Ack::Page, false).unwrap();
        let posted = f3.posts.borrow()[0].1.clone();
        assert_eq!(posted["marks"]["oracle"], 104, "stops before OBS-b (seq 105), which was not shown whole: {posted}");
        assert_eq!(posted["marks"]["taylor"], 50);
        assert_eq!(v3["marks"]["oracle"], 104);
        // An explicit mark map is posted as given, not replaced by this page's marks.
        let f4 = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": now}, "/fold": {"items": [], "next_marks": {"oracle": 999}}}));
        inbox_value(&f4, 50, Ack::Marks(json!({"oracle": 7})), false).unwrap();
        assert_eq!(f4.posts.borrow()[0].1["marks"], json!({"oracle": 7}));
    }

    #[test]
    fn the_lean_brief_prints_the_last_resume_whole_and_counts_the_work() {
        let card = |id: &str, status: &str| json!({"room": "axxis", "id": id, "title": id, "status": status, "effective_status": status, "owner": "rocky", "since_ms": 0, "conflict": false});
        let resume = "RESUME before compaction.\nDone: the merge.\nNext: #rocky's 1,807 messages.";
        let f = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": 1_790_836_247_741i64 + 30 * 60_000},
            "/fold": {"items": []},
            "/states": {"heads": [{"subject": "a", "state": "open", "next_touch": "2020-01-01"}, {"subject": "b", "state": "running", "next_touch": "—"},
                                  {"subject": "c", "state": "done", "next_touch": "2020-01-01"}]},
            "/cards?owner=rocky": {"now_ms": 0, "cards": [card("q1", "queue"), card("r1", "review"), card("r2", "review"), card("p1", "in_progress")]},
            // Newest first by tag: someone else's resume-tagged note in this room, then ours (a preview), then our older one.
            "/search?channel=rocky": {"items": [{"id": "OBS-talk", "author": "emmy", "tags": ["resume"], "content": "I will resume later"},
                                           {"id": "OBS-new", "author": "rocky", "tags": ["resume", "intent:status"], "content": "RESUME before…", "created_ms": 1_790_836_247_741i64},
                                           {"id": "OBS-old", "author": "rocky", "tags": ["resume"], "content": "an older resume"}]},
            "/search?id=OBS-new": {"id": "OBS-new", "content": resume, "created_ms": 1_790_836_247_741i64}}));
        let b = brief_text(&f).unwrap();
        assert!(b.contains(&format!("RESUME     OBS-new · written 30m ago\n{resume}\n")), "the newest tagged note, whole:\n{b}");
        assert!(!b.contains("an older resume") && !b.contains("I will resume later"));
        assert!(b.find("RESUME").unwrap() < b.find("ASKS FOR ME").unwrap());
        assert!(f.gets.borrow().iter().any(|g| g == "/search?channel=rocky&tag=resume&limit=20") && !f.gets.borrow().iter().any(|g| g.contains("q=")),
            "chronological by tag, never ranked: {:?}", f.gets.borrow());
        assert!(b.contains("WORK       2 open heads (1 overdue) · 4 cards (in_progress 1 · review 2 · queue 1) — ox brief --full lists them\n"), "{b}");
        assert!(!b.contains("MY OPEN WORK") && !b.contains("YOUR CARDS"), "{b}");
        let full = brief_with(&f, true).unwrap();
        assert!(full.contains("MY OPEN WORK\n  [open] a") && full.contains("YOUR CARDS (4)") && !full.contains("WORK       "), "{full}");
        // A seat that never wrote a resume note gets no RESUME line.
        let none = Fake::new(json!({"/whoami": {"handle": "rocky"}, "/fold": {"items": []}, "/states": {"heads": []}}));
        assert!(!brief_text(&none).unwrap().contains("RESUME"));
    }

    /// docs/NOW.md: the brief prints the seat's newest `now` head first after IDENTITY, whole; another seat's or an older
    /// head is never shown; an old head is STALE; none says how to write one; merge keeps fields and refuses strangers.
    #[test]
    fn the_brief_opens_with_the_now_head() {
        let at = 1_792_480_000_000i64;
        let head = json!({"schema": "now/1", "task": "build the Now head", "branch": "dev", "next": "ox now --set", "at": at});
        let older = json!({"schema": "now/1", "task": "an older task", "at": at - 9});
        let mk = |now: i64| Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": now}, "/fold": {"items": []}, "/states": {"heads": []},
            "/search?channel=rocky": {"items": [
                {"id": "OBS-emmy-now", "author": "emmy", "tags": ["now"], "content": "NOW · emmy's", "created_ms": at},
                {"id": "OBS-now2", "author": "rocky", "tags": ["now"], "content": "NOW · build", "created_ms": at, "cargo": head.to_string()},
                {"id": "OBS-now1", "author": "rocky", "tags": ["now"], "content": "NOW · older", "created_ms": at - 9, "cargo": older.to_string()}]},
            "/search?id=OBS-now2": {"id": "OBS-now2", "created_ms": at, "cargo": head.to_string()}}));
        let b = brief_text(&mk(at + 10 * 60_000)).unwrap();
        let i = b.find("IDENTITY").unwrap();
        let n = b.find("NOW        rocky · written 10m ago · OBS-now2\n").unwrap_or_else(|| panic!("{b}"));
        assert!(i < n && b[i..n].lines().count() == 1, "NOW is the line right after IDENTITY:\n{b}");
        assert!(b.contains("  task     build the Now head\n  branch   dev\n  next     ox now --set\n"), "{b}");
        assert!(!b.contains("an older task") && !b.contains("emmy's"), "{b}");
        let late = brief_text(&mk(at + 3 * 3_600_000)).unwrap();
        assert!(late.contains("NOW        STALE · rocky · written 180m ago") || late.contains("NOW        STALE · rocky · written 3h ago"), "{late}");
        assert!(late.contains("update it before anything else"), "{late}");
        let none = Fake::new(json!({"/whoami": {"handle": "rocky"}, "/fold": {"items": []}, "/states": {"heads": []}}));
        assert!(brief_text(&none).unwrap().contains("NOW        none written."));
        // Merge: a set changes one field and keeps the rest; a clear removes one; a stranger field is refused.
        let m = now_merge(Some(&head), &["next=write the doc".into()], &["branch".into()], at + 1).unwrap();
        assert_eq!((m["task"].as_str(), m["next"].as_str(), m.get("branch"), m["at"].as_i64()), (Some("build the Now head"), Some("write the doc"), None, Some(at + 1)));
        assert!(now_merge(None, &["mood=fine".into()], &[], at).unwrap_err().to_string().contains("not a Now field"));
        assert!(now_merge(None, &[], &[], at).is_err(), "an empty head is refused");
    }

    /// docs/IDENTITY.md: a server-verified identity head prints whole right after NOW with its sources and the embedded,
    /// hash-verified persona; a forged, imported (envelope 1) or oversized head is refused and said; a changed source shows
    /// the verified copy as STALE, never the changed text; a bad embedded hash shows no persona; authority without an
    /// owner-signed source is marked; the State's one-line PERSONA is not repeated; a long persona is never cut.
    #[test]
    fn the_brief_prints_the_identity_head_whole_after_now() {
        let at = 1_792_480_000_000i64;
        let persona = format!("ROCKY PERSONA v2. WHO: an Eridian engineer.{} THE END.", " Voice line.".repeat(800));
        let head = |text: &str, sha: &str, authority_by: &str| json!({"schema": "identity-head/1", "seat": "rocky", "name": "Rocky", "purpose": "the engineer seat", "partner": "Blaze is Grace",
            "mission": "the ship's brain", "authority": "prod only on Blaze's word", "principles": ["read before acting"],
            "refs": {"authority": [{"id": "OBS-rule", "by": authority_by}], "mission": [{"id": "OBS-blaze-1", "by": "owner"}]},
            "crew": [{"handle": "emmy", "who": "DOT architect", "source": {"id": "OBS-emmy-st", "by": "state"}}, {"handle": "atlas", "who": "runs #hustler", "source": {"by": "posts"}}],
            "persona_ref": "OBS-persona", "persona_text": text, "persona_sha256": sha, "at": at});
        let sha = oracle_proto::sha_hex(persona.as_bytes());
        let rec = |id: &str, author: &str, room: &str, env: i64, cargo: &Value| json!({"id": id, "author": author, "channel": room, "envelope": env, "sig": "ab", "created_ms": at, "tags": ["identity"], "cargo": cargo.to_string()});
        let fake = |items: Vec<Value>, source_text: Option<&str>| {
            let mut m = json!({"/whoami": {"handle": "rocky", "server_ms": at + 60_000}, "/fold": {"items": []}, "/states": {"heads": []},
                "state:rocky": {"state": "alive", "heads": ["OBS-st"], "fields": {"kind": "mind", "persona": "ONE-LINE PERSONA", "boot": "wake"}},
                "/search?channel=rocky": {"items": items.clone()}});
            for r in &items { m[format!("/search?id={}", r["id"].as_str().unwrap())] = r.clone(); }
            if let Some(t) = source_text { m["/search?id=OBS-persona"] = json!({"id": "OBS-persona", "author": "rocky", "channel": "rocky", "envelope": 2, "tags": ["persona:v2"], "content": t}); }
            Fake::new(m)
        };
        let good = rec("OBS-id2", "rocky", "rocky", 2, &head(&persona, &sha, "owner"));
        let forged = rec("OBS-forged", "emmy", "rocky", 2, &head(&persona, &sha, "owner"));
        let imported = rec("OBS-imp", "rocky", "rocky", 1, &head(&persona, &sha, "owner"));
        let b = brief_text(&fake(vec![forged.clone(), imported.clone(), good.clone()], Some(&persona))).unwrap();
        let w = b.find("WHO I AM   Rocky · identity OBS-id2").unwrap_or_else(|| panic!("{b}"));
        assert!(b.find("NOW        ").unwrap() < w, "IDENTITY comes after NOW:\n{b}");
        assert!(b.contains("records are not server-verified writes by rocky in #rocky; ignored") && b.contains("OBS-forged") && b.contains("OBS-imp"),
            "both refused, in one line; an import (envelope 1) is never a head:\n{b}");
        for line in ["  purpose   the engineer seat\n            self-declared · no citations", "  mission   the ship's brain\n            self-declared · citations: OBS-blaze-1 (a Blaze record)",
                     "  authority prod only on Blaze's word\n            self-declared · citations: OBS-rule (a Blaze record)\n",
                     "  crew (each cites its own server-verified State)\n    emmy         DOT architect [OBS-emmy-st]",
                     "  associates (self-reported by me, unverified)\n    atlas        runs #hustler", "  persona   OBS-persona (verified, "] {
            assert!(b.contains(line), "{line}:\n{b}");
        }
        // The plain brief folds the persona to one line (2026-10-20, ~3k tokens on every wake); --full keeps it whole.
        assert!(!b.contains(&persona), "the plain brief folds the persona:\n{b}");
        let whole = brief_with(&fake(vec![forged.clone(), imported.clone(), good.clone()], Some(&persona)), true).unwrap();
        assert!(whole.contains("  persona   OBS-persona (whole, verified)") && whole.contains(&persona), "the whole persona, never cut ({} chars):\n{whole}", persona.len());
        assert!(!b.contains("ONE-LINE PERSONA") && b.contains("BOOT       wake"), "{b}");
        // No field is ever called verified, whoever wrote its citations (an unrelated Blaze record proves nothing).
        assert!(!b.contains("OWNER-VERIFIED") && !b.contains("owner-signed") && !b.contains("a name not here is not crew"), "{b}");
        let quoted = rec("OBS-q", "rocky", "rocky", 2, &head(&persona, &sha, "self"));
        let bq = brief_text(&fake(vec![quoted], Some(&persona))).unwrap();
        assert!(bq.contains("self-declared · citations: OBS-rule (my record)"), "{bq}");
        // The persona source lost its persona tag: the verified copy, STALE.
        let mut f6 = fake(vec![good.clone()], None);
        f6.answers["/search?id=OBS-persona"] = json!({"id": "OBS-persona", "author": "rocky", "channel": "rocky", "envelope": 2, "tags": ["note"], "content": persona.clone()});
        let b6 = brief_text(&f6).unwrap();
        assert!(b6.contains("is not a server-verified persona:vN record of rocky") && b6.contains("STALE copy"), "{b6}");
        // A crew source must be the seat's own server-verified State revision in its own room.
        let state = |author: &str, room: &str, env: i64, schema: &str, tag: &str| json!({"author": author, "channel": room, "envelope": env, "tags": [tag], "cargo": json!({"schema": schema}).to_string()});
        assert!(crew_source_ok(&state("emmy", "emmy", 2, "room-context/1", "room-context:v1"), "emmy").is_ok());
        for (r, why) in [(state("rocky", "emmy", 2, "room-context/1", "room-context:v1"), "written by another seat"),
                         (state("emmy", "dot", 2, "room-context/1", "room-context:v1"), "wrong room"),
                         (state("emmy", "emmy", 1, "room-context/1", "room-context:v1"), "an import"),
                         (state("emmy", "emmy", 2, "doc/1", "intent:note"), "an ordinary post by the same seat")] {
            assert!(crew_source_ok(&r, "emmy").is_err(), "{why} must not vouch for a crew entry");
        }
        // The source changed: the verified copy, STALE; never the changed text.
        let b2 = brief_with(&fake(vec![good.clone()], Some("ROCKY PERSONA v3, edited by someone")), true).unwrap();
        assert!(b2.contains("the persona source OBS-persona has changed") && b2.contains("(whole, STALE copy)") && b2.contains(&persona) && !b2.contains("edited by someone"), "{b2}");
        let b2 = brief_text(&fake(vec![good.clone()], Some("ROCKY PERSONA v3, edited by someone"))).unwrap();
        assert!(b2.contains("  persona   OBS-persona (STALE copy, ") && !b2.contains(&persona), "folded, and still marked STALE:\n{b2}");
        // The source is gone: the verified copy, STALE.
        let b3 = brief_text(&fake(vec![good.clone()], None)).unwrap();
        assert!(b3.contains("could not be read; showing the copy verified") && b3.contains("STALE copy"), "{b3}");
        // The embedded copy does not match its hash: no persona at all.
        let bad = rec("OBS-bad", "rocky", "rocky", 2, &head("tampered text", &sha, "owner"));
        let b4 = brief_text(&fake(vec![bad], Some(&persona))).unwrap();
        assert!(b4.contains("the embedded persona does not match its hash; no persona is shown") && !b4.contains("  persona   OBS-persona"), "{b4}");
        // Over the bounds: refused, not cut.
        let mut big = head(&persona, &sha, "owner");
        big["crew"] = json!((0..41).map(|i| json!({"handle": format!("s{i}"), "who": "x"})).collect::<Vec<_>>());
        let b5 = brief_text(&fake(vec![rec("OBS-big", "rocky", "rocky", 2, &big)], Some(&persona))).unwrap();
        assert!(b5.contains("OBS-big is over its bounds (41 crew entries, over 40); refused, not cut") && b5.contains("WHO I AM   no identity head"), "{b5}");
        assert!(identity_bounds(&json!({"purpose": "x".repeat(IDENTITY_FIELD_MAX + 1)}))[0].contains("purpose is 4001 bytes"));
    }

    /// docs/PROCEDURES.md: the list keeps the newest verified version of each name; get returns it whole; an import, a
    /// record outside its author's room or a malformed procedure is never shown; the brief names them in one line.

    #[test]
    fn the_brief_prints_the_fold_pages_whole_after_the_head_and_names_what_it_cuts() {
        let at = 1_793_500_000_000i64;
        let persona = "ROCKY PERSONA v3.".to_string();
        let sha = oracle_proto::sha_hex(persona.as_bytes());
        let head = json!({"schema": "identity-head/1", "seat": "rocky", "name": "Rocky", "purpose": "the engineer seat", "mission": "the ship's brain", "refs": {},
            "persona_ref": "OBS-persona", "persona_text": persona, "persona_sha256": sha, "at": at});
        let rec = |id: &str, author: &str, env: i64, tags: Vec<&str>, cargo: Value, at_ms: i64| json!({"id": id, "author": author, "channel": "rocky", "envelope": env, "sig": "ab", "created_ms": at_ms, "tags": tags, "cargo": cargo.to_string()});
        let page = |title: &str, body: &str| json!({"schema": "doc/1", "title": title, "markdown": body});
        let big = "x".repeat(FOLD_MAX_BYTES - 100);
        let fake = |items: Vec<Value>| {
            let mut m = json!({"/whoami": {"handle": "rocky", "server_ms": at + 60_000}, "/fold": {"items": []}, "/states": {"heads": []},
                "state:rocky": {"state": "alive", "heads": ["OBS-st"], "fields": {"kind": "mind", "boot": "wake"}},
                "/search?channel=rocky": {"items": items.clone()}});
            for r in &items { m[format!("/search?id={}", r["id"].as_str().unwrap())] = r.clone(); }
            m["/search?id=OBS-persona"] = json!({"id": "OBS-persona", "author": "rocky", "channel": "rocky", "envelope": 2, "tags": ["persona:v3"], "content": persona});
            Fake::new(m)
        };
        let head_rec = rec("OBS-head", "rocky", 2, vec!["identity"], head.clone(), at);
        let b = brief_text(&fake(vec![
            head_rec.clone(),
            rec("OBS-fold-2", "rocky", 2, vec!["identity:fold"], page("Batch 2", "Coordination and rooms. Finance has no room (f892aa66d3c1)."), at + 2),
            rec("OBS-fold-1", "rocky", 2, vec!["identity:fold"], page("Batch 1", "Who you are. You think fast (b2f703eb63bd)."), at + 1),
            rec("OBS-forged", "emmy", 2, vec!["identity:fold"], page("Forged", "not mine"), at + 3),
            rec("OBS-fold-big", "rocky", 2, vec!["identity:fold"], page("Batch 3, too big", &big), at + 4),
        ])).unwrap();
        let w = b.find("WHO I AM   Rocky").unwrap_or_else(|| panic!("{b}"));
        let f = b.find("FOLD       2 pages").unwrap_or_else(|| panic!("{b}"));
        assert!(w < f && f < b.find("BRIDGE     ").unwrap_or(usize::MAX), "the fold comes right after the head:\n{b}");
        let p1 = b.find("── OBS-fold-1 · Batch 1").unwrap();
        let p2 = b.find("── OBS-fold-2 · Batch 2").unwrap();
        assert!(p1 < p2 && b.contains("  Who you are. You think fast (b2f703eb63bd).") && b.contains("  Coordination and rooms. Finance has no room (f892aa66d3c1)."), "oldest batch first, whole:\n{b}");
        assert!(b.contains("OBS-forged is not a server-verified write by rocky in #rocky; ignored"), "{b}");
        assert!(b.contains("cut: 1 more page over the 64 KB cap, read whole with ox get: OBS-fold-big (64 KB)") && !b.contains(&big), "the cap names, never cuts mid-page:\n{b}");
        // A revised page replaces the one it supersedes; a forged record superseding a real page changes nothing.
        let bv = brief_text(&fake(vec![
            head_rec.clone(),
            rec("OBS-fold-1", "rocky", 2, vec!["identity:fold"], page("Batch 1", "Who you are (b2f703eb63bd)."), at + 1),
            rec("OBS-fold-2", "rocky", 2, vec!["identity:fold"], page("Batch 2", "Rooms (f892aa66d3c1)."), at + 2),
            rec("OBS-fold-1b", "rocky", 2, vec!["identity:fold", "supersedes:OBS-fold-1"], page("Batch 1 (v2.1)", "Who you are (20bd6eefe104)."), at + 5),
            rec("OBS-forged-2", "emmy", 2, vec!["identity:fold", "supersedes:OBS-fold-2"], page("Forged", "not mine"), at + 6),
        ])).unwrap();
        assert!(bv.contains("FOLD       2 pages") && bv.contains("── OBS-fold-1b · Batch 1 (v2.1)") && bv.contains("── OBS-fold-2 · Batch 2"), "{bv}");
        assert!(!bv.contains("── OBS-fold-1 ·") && !bv.contains("(b2f703eb63bd)"), "the superseded page is gone:\n{bv}");
        // Negative control: no fold records, no FOLD block; and the fold is never read without a head.
        let b0 = brief_text(&fake(vec![head_rec])).unwrap();
        assert!(b0.contains("WHO I AM   Rocky") && !b0.contains("FOLD       "), "{b0}");
        let b1 = brief_text(&fake(vec![rec("OBS-fold-1", "rocky", 2, vec!["identity:fold"], page("Batch 1", "alone"), at + 1)])).unwrap();
        assert!(!b1.contains("FOLD       "), "no head, no fold:\n{b1}");
    }

    #[test]
    fn procedures_are_found_by_name_and_named_in_the_brief() {
        let at = 1_792_480_000_000i64;
        let p = |name: &str, v: i64, title: &str| json!({"schema": "procedure/1", "name": name, "title": title, "when": "after a merge", "version": v,
            "steps": [{"do": "build", "cmd": "cargo build --release\ncp target/release/hq /usr/local/bin/hq.new", "check": "hq --help"}], "verify": "a card fires with TP", "rollback": "restore hq.bak", "refs": ["OBS-pr56"]});
        let rec = |id: &str, author: &str, room: &str, env: i64, c: &Value| json!({"id": id, "author": author, "channel": room, "envelope": env, "created_ms": at, "tags": ["procedure", format!("procedure:{}", c["name"].as_str().unwrap())], "cargo": c.to_string()});
        let new = rec("OBS-ship2", "rocky", "rocky", 2, &p("ship-hq", 2, "Ship hq"));
        let old = rec("OBS-ship1", "rocky", "rocky", 2, &p("ship-hq", 1, "Ship hq (old)"));
        let imported = rec("OBS-imp", "rocky", "rocky", 1, &p("run-scoreboard", 1, "imported"));
        let elsewhere = rec("OBS-else", "emmy", "dot", 2, &p("run-scoreboard", 1, "wrong room"));
        let other = rec("OBS-bob", "bobby-core", "bobby-core", 2, &p("jinnie-review", 1, "Jinnie review"));
        let f = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": at + 60_000}, "/fold": {"items": []}, "/states": {"heads": []},
            "/search?tag=procedure": {"items": [new.clone(), imported.clone(), elsewhere.clone(), old.clone(), other.clone()]},
            "/search?tag=procedure%3Aship-hq": {"items": [new.clone(), old.clone()]},
            "/search?tag=procedure%3Arun-scoreboard": {"items": [imported.clone(), elsewhere.clone()]},
            "/search?id=OBS-ship2": new, "/search?id=OBS-imp": imported, "/search?id=OBS-else": elsewhere}));
        let l = procedure_list(&f).unwrap();
        let names: Vec<(&str, i64)> = l.iter().map(|p| (p["name"].as_str().unwrap(), p["version"].as_i64().unwrap())).collect();
        assert_eq!(names, [("ship-hq", 2), ("jinnie-review", 1)], "newest of each, verified and in its author's room only");
        let g = procedure_get(&f, "ship-hq").unwrap().unwrap();
        let t = procedure_render(&g, at + 60_000);
        assert!(t.starts_with("PROCEDURE  ship-hq v2 · by rocky · written 1m ago · OBS-ship2\n  Ship hq\n"), "{t}");
        assert!(t.contains("    1. build\n       $ cargo build --release\n       $ cp target/release/hq /usr/local/bin/hq.new\n       check: hq --help\n"), "{t}");
        assert!(t.contains("  verify   a card fires with TP\n  rollback restore hq.bak\n  refs     OBS-pr56\n"), "{t}");
        assert!(procedure_get(&f, "run-scoreboard").unwrap().is_none(), "an import or a record outside its author's room is never a procedure");
        let b = brief_text(&f).unwrap();
        assert!(b.contains("PROCEDURES 2 on record, read one whole before doing it: ox procedure <name> · ship-hq v2 · jinnie-review v1\n"), "{b}");
        assert!(procedure_problems(&json!({"name": "Ship HQ", "title": "", "steps": []})).len() == 4, "name, title, when and steps");
        let mut big = p("big", 1, "big"); big["steps"] = json!((0..41).map(|_| json!({"do": "x"})).collect::<Vec<_>>());
        assert!(procedure_problems(&big)[0].contains("41 steps, over 40"));
    }

    /// docs/CANON.md: the newest verified canon is read whole; an import is never the canon; render groups by kind with
    /// sources and supersedes, then what is open; the brief prints the line and the law, authority and safety rules
    /// only; malformed rules are refused, each with its reason.
    #[test]
    fn the_canon_is_versioned_kinded_and_in_the_brief() {
        let at = 1_792_480_000_000i64;
        let rule = |id: &str, kind: &str, text: &str| json!({"id": id, "kind": kind, "text": text, "sources": ["OBS-memory-1"]});
        let c = |v: i64| json!({"schema": "canon/1", "version": v, "rules": [
            rule("docs-first", "law", "Docs, then code, then verify."),
            rule("only-blaze-deletes-data", "safety", "Data that cannot be rebuilt is removed only on Blaze's go."),
            {"id": "astra-prod-go", "kind": "authority", "text": "Astra production needs Blaze's explicit go.", "sources": ["OBS-memory-2"], "supersedes": ["OBS-memory-0"]},
            rule("hairlines", "craft", "Hairlines over boxes."),
            {"id": "sorry", "kind": "domain", "scope": "astra", "text": "Never write sorry.", "sources": ["OBS-memory-3"]}],
            "open": [{"question": "Who releases oracle-core?", "rules": ["astra-prod-go"], "ask": "OBS-ask-1"}]});
        let rec = |id: &str, env: i64, c: &Value| json!({"id": id, "author": "rocky", "channel": "rocky", "envelope": env, "created_ms": at, "tags": ["canon"], "cargo": c.to_string()});
        let imported = rec("OBS-imp", 1, &c(3));
        let new = rec("OBS-c2", 2, &c(2));
        let old = rec("OBS-c1", 2, &c(1));
        let f = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": at + 60_000}, "/fold": {"items": []}, "/states": {"heads": []},
            "/search?channel=rocky&tag=canon": {"items": [imported.clone(), new.clone(), old.clone()]},
            "/search?id=OBS-imp": imported, "/search?id=OBS-c2": new}));
        let got = last_canon(&f, "rocky").unwrap();
        assert_eq!(got["id"], "OBS-c2", "the import is skipped; the newest verified record is the canon");
        assert!(got["problems"][0].as_str().unwrap().contains("OBS-imp is not a server-verified write"), "{got}");
        let t = canon_render(&got, None, at + 60_000);
        assert!(t.starts_with("CANON  v2 · by rocky · written 1m ago · OBS-c2 · 5 rules · law 1 · authority 1 · safety 1 · craft 1 · domain 1\nLAW\n  docs-first: Docs, then code, then verify.\n      sources OBS-memory-1\n"), "{t}");
        assert!(t.contains("AUTHORITY\n  astra-prod-go: Astra production needs Blaze's explicit go.\n      sources OBS-memory-2 · supersedes OBS-memory-0\n"), "{t}");
        assert!(t.contains("DOMAIN\n  sorry [astra]: Never write sorry.\n"), "{t}");
        assert!(t.ends_with("OPEN  Who releases oracle-core? · asked OBS-ask-1\n"), "{t}");
        let only = canon_render(&got, Some("craft"), at + 60_000);
        assert!(only.contains("CRAFT\n  hairlines") && !only.contains("LAW") && !only.contains("OPEN"), "{only}");
        let b = brief_text(&f).unwrap();
        assert!(b.contains("CANON      v2 · written 1m ago · OBS-c2 · 5 rules · law 1 · authority 1 · safety 1 · craft 1 · domain 1 · 1 open · whole: ox canon · law, authority, safety: ox canon --kind law, or ox brief --full\n"), "{b}");
        assert!(!b.contains("[docs-first]"), "the plain brief folds the rules (2026-10-20):\n{b}");
        let b = brief_with(&f, true).unwrap();
        assert!(b.contains("  law        Docs, then code, then verify. [docs-first]\n  safety     Data that cannot be rebuilt is removed only on Blaze's go. [only-blaze-deletes-data]\n  authority  Astra production needs Blaze's explicit go. [astra-prod-go]\n  open       Who releases oracle-core? · asked OBS-ask-1\n"), "{b}");
        assert!(!b.contains("Hairlines over boxes") && !b.contains("Never write sorry"), "craft and domain rules stay out of the brief: {b}");
        assert_eq!(canon_cited_ids(&c(2)), ["OBS-memory-1", "OBS-memory-2", "OBS-memory-0", "OBS-memory-3", "OBS-ask-1"]);
        let bad = json!({"rules": [{"id": "Bad Id", "kind": "mood", "text": "", "sources": []},
            {"id": "x", "kind": "domain", "text": "t", "sources": ["OBS-1"]}, {"id": "x", "kind": "law", "text": "y".repeat(1001), "sources": ["nope"]}]});
        let p = canon_problems(&bad);
        for want in ["rule Bad Id: id must be", "kind \"mood\"", "rule Bad Id: text is required", "rule Bad Id: sources must", "rule x: a domain rule names its scope",
                     "rule x: id used twice", "text is 1001 bytes", "rule x: sources must"] {
            assert!(p.iter().any(|x| x.contains(want)), "{want} in {p:?}");
        }
        assert_eq!(canon_problems(&json!({"rules": []})), ["rules: at least one"]);
        let none = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": at}, "/fold": {"items": []}, "/states": {"heads": []}}));
        assert!(brief_text(&none).unwrap().contains("CANON      none on record yet (docs/CANON.md)"));
    }

    /// docs/IDENTITY.md: refs written back from a read keep every cite; a shape that is not an id is refused, never dropped.
    #[test]
    fn identity_refs_round_trip_from_a_read() {
        let read = json!({"authority": [{"id": "OBS-a", "by": "owner"}, {"id": "OBS-b", "by": "self"}], "mission": ["OBS-c"]});
        assert_eq!(identity_ref_ids(&read).unwrap(), vec![("authority".into(), vec!["OBS-a".into(), "OBS-b".into()]), ("mission".into(), vec!["OBS-c".into()])]);
        assert_eq!(identity_ref_ids(&Value::Null).unwrap(), vec![]);
        assert!(identity_ref_ids(&json!({"authority": [{"by": "owner"}]})).unwrap_err().contains("is not an id"));
        assert!(identity_ref_ids(&json!({"authority": "OBS-a"})).unwrap_err().contains("must be a list"));
        assert!(identity_ref_ids(&json!({"crew": ["OBS-a"]})).unwrap_err().contains("refs are for"));
        assert!(identity_ref_ids(&json!(["OBS-a"])).unwrap_err().contains("must be an object"));
    }

    /// docs/GAP.md: a decide says who it affects, who is absent and why, and what it owes; the brief prints it under the ask.
    #[test]
    fn the_gap_is_asked_before_a_decide() {
        let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let g = tell_gap("decide", &v(&["Astra users: a bad release blocks sign-in"]), &v(&["Emmy: not asked yet"]), Some("rollback in a minute")).unwrap().unwrap();
        assert_eq!(g, json!({"affects": [{"who": "Astra users", "how": "a bad release blocks sign-in", "asked": null}],
            "absent": [{"who": "Emmy", "why": "not asked yet"}], "owed": "rollback in a minute"}));
        // Who is not at this table: a decide with no gap, or half of one, is refused.
        assert!(tell_gap("decide", &[], &[], None).unwrap_err()[0].contains("who is not at this table?"));
        assert!(tell_gap("decide", &v(&["A: b"]), &[], Some("x")).unwrap_err()[0].contains("who is not at this table?"));
        assert!(tell_gap("decide", &v(&["A: b"]), &v(&["C: d"]), None).unwrap_err()[0].contains("--owed"));
        // `none` says everyone affected was asked, so each one cites where.
        assert!(tell_gap("decide", &v(&["Emmy: reviews it"]), &v(&["none"]), Some("x")).unwrap_err()[0].contains("cite where Emmy spoke"));
        let ok = tell_gap("decide", &v(&["Emmy: reviews it @OBS-oracle-1"]), &v(&["none"]), Some("x")).unwrap().unwrap();
        assert_eq!((ok["affects"][0]["asked"].as_str(), ok["absent"].clone()), (Some("OBS-oracle-1"), json!([])));
        assert!(tell_gap("decide", &v(&["A: b"]), &v(&["none", "C: d"]), Some("x")).unwrap_err()[0].contains("stands alone"));
        assert!(tell_gap("decide", &v(&["no colon"]), &v(&["C: d"]), Some("x")).unwrap_err()[0].contains("\"<who>: <how>\""));
        assert!(tell_gap("decide", &v(&[&format!("A: {}", "x".repeat(240))]), &v(&["C: d"]), Some("x")).unwrap_err()[0].contains("over 240"));
        // Any other ask: no gap, and the flags are refused.
        assert_eq!(tell_gap("none", &[], &[], None), Ok(None));
        assert!(tell_gap("read", &[], &[], Some("x")).unwrap_err()[0].contains("belong to --ask decide"));
        // The brief prints it under the ask, for the decider (ASKS FOR ME) and for the asker (MY ASKS).
        let row = json!({"id": "OBS-q", "room": "rocky", "from": "rocky", "ask": "decide", "state": "open", "age": "1m", "card": "Release?", "gap": ok});
        let b = json!({"seat": "blaze", "now_ms": 0, "now": "x", "identity": "", "state": {}, "asks": {"rows": [row]}});
        let t = brief_render(&b, false);
        let lines = "affects Emmy (reviews it, asked OBS-oracle-1)\n{i}absent  none: everyone affected was asked\n{i}owed    x\n";
        assert!(t.contains(&format!("Release?  (OBS-q)\n           {}", lines.replace("{i}", "           "))), "{t}");
        let m = my_asks(&json!({"im_waiting_on": [{"id": "OBS-q", "room": "rocky", "ask": "decide", "state": "open", "held_by": ["blaze"],
            "created_ms": 0, "card": "Release?", "gap": g}]}), 0);
        assert!(my_asks_text(&m, None).contains("Release?\n      affects Astra users (a bad release blocks sign-in)\n      absent  Emmy (not asked yet)\n"), "{}", my_asks_text(&m, None));
    }

    /// docs/BRIDGE-CONTRACT.md: the contract refuses a long head, a decide without options or a due date, options on a
    /// non-decide, an unknown ask; the brief says when the seat last told the owner and when that is overdue.
    #[test]
    fn the_bridge_contract_and_cadence() {
        let o = |n: usize| (0..n).map(|i| format!("option {i}")).collect::<Vec<_>>();
        let w = Some("it is smaller");
        assert!(tell_problems("Search set is frozen.", "none", &[], None, None, None).is_empty());
        assert!(tell_problems("Which first?", "decide", &o(3), Some("2h"), Some(2), w).is_empty());
        assert!(tell_problems(&"word ".repeat(41), "none", &[], None, None, None)[0].contains("41 words, over 40"));
        assert!(tell_problems("Which?", "decide", &o(1), Some("2h"), Some(1), w)[0].contains("2 to 6"));
        assert!(tell_problems("Which?", "decide", &o(2), None, Some(1), w)[0].contains("--by"));
        assert!(tell_problems("Which?", "decide", &["x".repeat(81), "y".into()], Some("1h"), Some(1), w)[0].contains("over 80 characters"));
        assert!(tell_problems("FYI", "none", &o(2), None, None, None)[0].contains("belongs to --ask decide"));
        assert!(tell_problems("FYI", "act", &[], None, None, None)[0].contains("use decide, read or none"));
        // 23 Oct: a decide carries the seat's pick and why; out of range, missing or misplaced is refused.
        assert!(tell_problems("Which?", "decide", &o(2), Some("1h"), None, w)[0].contains("--recommend"));
        assert!(tell_problems("Which?", "decide", &o(2), Some("1h"), Some(3), w)[0].contains("one of the 2 options"));
        assert!(tell_problems("Which?", "decide", &o(2), Some("1h"), Some(0), w)[0].contains("one of the 2 options"));
        assert!(tell_problems("Which?", "decide", &o(2), Some("1h"), Some(1), Some("  "))[0].contains("--why"));
        assert!(tell_problems("Which?", "decide", &o(2), Some("1h"), Some(1), Some(&"z".repeat(241)))[0].contains("over 240"));
        assert!(tell_problems("FYI", "none", &[], None, Some(1), None)[0].contains("belong to --ask decide"));
        let t = tell_choice_text(&["Ship now".into(), "Hold a day".into()], 2, " the CI is red ");
        assert_eq!(t, "Options:\n1) Ship now\n2) Hold a day (recommended)\nI recommend 2: the CI is red");
        let at = 1_792_480_000_000i64;
        let mk = |minutes: i64| Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": at + minutes * 60_000}, "/fold": {"items": []}, "/states": {"heads": []},
            "/search?tag=to%3Ablaze": {"items": [{"id": "OBS-emmy-to", "author": "emmy", "created_ms": at}, {"id": "OBS-told", "author": "rocky", "created_ms": at}]}}));
        let b = brief_text(&mk(12)).unwrap();
        assert!(b.contains("BRIDGE     last to @blaze 12m ago · OBS-told\n"), "{b}");
        assert!(brief_text(&mk(45)).unwrap().contains("BRIDGE     last to @blaze 45m ago · OBS-told · OVERDUE"));
        let none = Fake::new(json!({"/whoami": {"handle": "rocky"}, "/fold": {"items": []}, "/states": {"heads": []}, "/search?tag=to%3Ablaze": {"items": []}}));
        assert!(brief_text(&none).unwrap().contains("BRIDGE     nothing to @blaze on record"));
        // the card format is in every brief, right under NOW, and it is one the contract accepts
        let nb = brief_text(&none).unwrap();
        assert!(nb.contains("TELL       the owner reads the Bridge"), "{nb}");
        assert!(nb.contains("\n  decide   ox tell ") && nb.contains("\n  check    signed:ok"), "{nb}");
        assert!(TELL_BLOCK.contains("at most 80 characters"));
        for flag in ["--ask decide", "--option", "--recommend", "--why", "--by", "--affects \"<who>: <how>\"", "--absent \"<who>: <why>\"", "--owed", "#ask"] {
            assert!(TELL_BLOCK.contains(flag), "{flag}");
        }
        let opts = vec!["Ship it".to_string(), "Hold it".to_string()];
        assert!(tell_problems("Ship it?", "decide", &opts, Some("1d"), Some(1), Some("why")).is_empty());
    }

    #[test]
    fn hello_tells_a_new_seat_who_it_is_the_norms_the_rooms_and_to_write_its_state() {
        let f = Fake::new(json!({"/whoami": {"handle": "newseat", "server_ms": 1790000100000i64},
            "/hallway": {"days": 14, "rooms": [{"room": "oracle", "purpose": "oracle-core: the memory server"}, {"room": "quiet"}]},
            "/asks": {"waiting_on_me": [{"id": "OBS-1"}]}}));
        let t = hello_text(&f).unwrap();
        assert!(t.starts_with("HELLO newseat · now "), "{t}");
        assert!(t.contains("it has no State yet") && t.contains("oracle_room_state with room \"newseat\""), "{t}");
        assert!(t.contains("1. Start every session with oracle_brief") && t.contains("oracle_react: ack"), "{t}");
        assert!(t.contains("#oracle         oracle-core: the memory server") && t.contains("#quiet          (no State"), "{t}");
        assert!(t.contains("NEXT     1 asks wait on you. oracle_brief, then oracle_inbox."), "{t}");
        // Through the MCP door, as a read.
        let r = handle(&f, &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "oracle_hello", "arguments": {}}}), false).unwrap();
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("HOW THIS PLACE WORKS"));
    }

    #[test]
    fn a_recall_shows_each_hits_links_and_says_when_nothing_is_on_record() {
        let f = Fake::new(json!({"/whoami": {"server_ms": 1790000100000i64}, "/search": {"engine": "hybrid",
            "prior": {"verdict": "unclear", "says": "no record holds all these words; the nearest by meaning is 0.52, so read the hits before concluding"},
            "hits": [{"id": "OBS-q", "channel": "dot", "author": "noor", "content": "the question", "created_ms": 1790000000000i64, "tags": [], "score": 0.03,
              "edges": {"reply_to": "OBS-ask", "replies": ["OBS-r1", "OBS-r2"], "answered_by": ["OBS-ans"], "marks": 3}},
             {"id": "OBS-z", "channel": "dot", "author": "emmy", "content": "no links here", "created_ms": 1790000000000i64, "tags": [], "score": 0.02}]}}));
        let t = typed_read(&f, "oracle_recall", &json!({"query": "q"})).expect("typed").unwrap();
        assert!(t.text.contains("\nno record holds all these words; the nearest by meaning is 0.52"), "{}", t.text);
        assert!(t.text.contains("   ↳ answers OBS-ask · answered by OBS-ans · replies 2 (newest OBS-r2) · 3 marks\n"), "{}", t.text);
        assert!(t.text.contains("2. OBS-z · #dot · emmy · 2026-09-21\n   no links here\n"), "a hit without links gets no line: {}", t.text);
        assert_eq!(t.value["gists"][0]["edges"]["answered_by"][0], "OBS-ans");
        assert_eq!(t.value["prior"]["verdict"], "unclear");
        // `found` is the ordinary case and goes unsaid; no hits with `none` says why instead of the old advice.
        let f = Fake::new(json!({"/whoami": {"server_ms": 1790000100000i64}, "/search": {"engine": "hybrid", "prior": {"verdict": "found", "says": "3 records hold all these words"},
            "hits": [{"id": "OBS-q", "channel": "dot", "author": "noor", "content": "x", "created_ms": 1790000000000i64, "tags": [], "score": 0.03}]}}));
        assert!(!typed_read(&f, "oracle_recall", &json!({"query": "q"})).unwrap().unwrap().text.contains("records hold"));
        let f = Fake::new(json!({"/whoami": {"server_ms": 1790000100000i64}, "/search": {"engine": "hybrid", "hits": [],
            "prior": {"verdict": "none", "says": "no prior: no record holds all these words, and the nearest by meaning is weak (0.31 < 0.4)"}}}));
        let t = typed_read(&f, "oracle_recall", &json!({"query": "sourdough"})).unwrap().unwrap();
        assert!(t.text.contains("no prior: no record holds all these words") && !t.text.contains("Try other words"), "{}", t.text);
        let t = typed_read(&f, "oracle_recall", &json!({"query": "sourdough", "gist": false})).unwrap().unwrap();
        assert!(t.text.starts_with("no prior:"), "{}", t.text);
    }

    #[test]
    fn a_gist_recall_gives_short_entries_with_the_matched_passage_to_choose_from() {
        let long = format!("{} the matched words are here {}", "opening ".repeat(40), "tail ".repeat(200));
        let at = long.find("the matched").unwrap();
        let f = Fake::new(json!({"/whoami": {"server_ms": 1790000100000i64}, "/search": {"engine": "hybrid", "hits": [
            {"id": "OBS-a", "channel": "oracle", "author": "emmy", "content": long, "created_ms": 1790000000000i64, "tags": [], "score": 0.03,
             "match_in": "content", "match_span": {"start": at, "end": at + 11, "unit": "utf8_bytes"}, "superseded_by": "OBS-b", "same_as": ["OBS-c", "OBS-d"]},
            {"id": "OBS-b", "channel": "dot", "author": "noor", "content": "short and whole", "created_ms": 1790000000000i64, "tags": [], "score": 0.02}]}}));
        let t = typed_read(&f, "oracle_recall", &json!({"query": "matched words", "gist": true})).expect("typed").unwrap();
        let g = t.value["gists"].as_array().unwrap();
        assert_eq!(g.len(), 2);
        let p = g[0]["passage"].as_str().unwrap();
        assert!(p.starts_with("… ") && p.contains("the matched words are here") && p.ends_with(" …"), "{p}");
        assert!(p.starts_with("… opening ") || p.starts_with("… the matched"), "starts on a word: {p}");
        assert!(p.chars().count() <= GIST_CHARS + 4, "short: {}", p.chars().count());
        assert_eq!((g[0]["superseded_by"].clone(), g[0]["copies"].clone()), (json!("OBS-b"), json!(2)));
        assert_eq!(g[1]["passage"], "short and whole", "a short text is whole, no marks");
        assert!(t.text.contains("1. OBS-a · #oracle · emmy · 2026-09-21 · superseded by OBS-b · +2 copies\n   … "), "{}", t.text);
        assert!(t.text.contains("2. OBS-b · #dot · noor · 2026-09-21\n   short and whole\n"), "{}", t.text);
        assert!(t.text.len() < 1200, "ten of these fit a screen: {}", t.text.len());
        // Gist is the default; gist: false gives the whole texts.
        assert!(typed_read(&f, "oracle_recall", &json!({"query": "matched words"})).unwrap().unwrap().value["gists"].is_array());
        assert!(typed_read(&f, "oracle_recall", &json!({"query": "matched words", "gist": false})).unwrap().unwrap().value["hits"].is_array());
    }

    #[test]
    fn typed_reads_keep_server_state_provenance_and_room_cursors() {
        let o = json!({"id":"OBS-old", "seq":7, "channel":"oracle", "author":"rocky", "content":"whole body",
            "tags":["ask:reply"], "intent":"proposal", "ask":"review", "state":"superseded", "answered_by":"OBS-answer",
            "history":[{"id":"OBS-note","why":"fixed"}], "sig":"signature", "pubkey":"key", "envelope":2});
        let f = Fake::new(json!({"/whoami":{"server_ms":1790836247741i64},
            "/search?channel=oracle":{"items":[o.clone()],"next_before":7,"scanned":1}, "/search?id=OBS-old":o}));
        let page = typed_read(&f, "oracle_read", &json!({"room":"oracle","before":12})).unwrap().unwrap();
        assert_eq!(page.value["next_before"], 7);
        assert!(page.text.contains("before=7"));
        assert!(f.gets.borrow().iter().any(|p| p.contains("&before=12")));
        let get = typed_read(&f, "oracle_get", &json!({"id":"OBS-old"})).unwrap().unwrap();
        let m = &get.value["message"];
        assert_eq!(m["ask"], "review", "derived classification beats the original tag");
        assert_eq!(m["state"], "superseded");
        assert_eq!(m["answered_by"], "OBS-answer");
        assert_eq!(m["history"][0]["why"], "fixed");
        assert_eq!(m["sig"], "signature");
        assert_eq!(m["seq"], 7);
        assert!(get.text.contains("superseded") && get.text.contains("answered by OBS-answer"));
    }

    // Pythia, 1 Nov 2026: "oracle_get and oracle_read don't show a message's tags". Every record carries its tags beside its
    // links, every existing field kept; the text names the tags the header and links do not already say.
    #[test]
    fn a_read_shows_each_messages_tags_beside_its_links() {
        let tagged = json!({"id": "OBS-t", "seq": 9, "channel": "oracle", "author": "rocky", "content": "a receipt", "created_ms": 1_790_836_000_000i64,
            "tags": ["intent:receipt", "reply:OBS-r", "receipt", "type:oracle.receipt", "to:emmy"], "state": "open"});
        let bare = json!({"id": "OBS-b", "seq": 8, "channel": "oracle", "author": "emmy", "content": "no tags", "created_ms": 1_790_835_000_000i64, "tags": []});
        let f = Fake::new(json!({"/whoami": {"server_ms": 1_790_836_247_741i64}, "/search?channel=oracle": {"items": [tagged.clone(), bare]}, "/search?id=OBS-t": tagged}));
        let page = typed_read(&f, "oracle_read", &json!({"room": "oracle"})).unwrap().unwrap();
        let msgs = page.value["messages"].as_array().unwrap();
        let t = msgs.iter().find(|m| m["id"] == "OBS-t").unwrap();
        assert_eq!(t["tags"], json!(["intent:receipt", "reply:OBS-r", "receipt", "type:oracle.receipt", "to:emmy"]), "{t}");
        assert_eq!((t["links"].clone(), t["intent"].clone(), t["to"].clone(), t["seq"].clone(), t["state"].clone()),
            (json!(["reply:OBS-r"]), json!("receipt"), json!(["emmy"]), json!(9), json!("open")), "every existing field is kept: {t}");
        assert_eq!(msgs.iter().find(|m| m["id"] == "OBS-b").unwrap()["tags"], json!([]), "no tags is an empty list, not a missing field");
        assert!(page.text.contains("   reply:OBS-r\n   tags: receipt type:oracle.receipt\n"), "{}", page.text);
        let get = typed_read(&f, "oracle_get", &json!({"id": "OBS-t"})).unwrap().unwrap();
        assert_eq!(get.value["message"]["tags"], tagged["tags"]);
        assert!(get.text.contains("tags: receipt type:oracle.receipt"), "{}", get.text);
    }

    #[test]
    fn a_resume_whose_whole_text_is_unreadable_says_preview_and_an_unreadable_asks_is_not_zero() {
        let f = Fake::new(json!({"/whoami": {"handle": "rocky"}, "/fold": {"items": []}, "/states": {"heads": []},
            "/search?channel=rocky": {"items": [{"id": "OBS-new", "author": "rocky", "tags": ["resume"], "content": "RESUME before…"}]},
            "/search?id=OBS-new": {"__err": "503 upstream"},
            "/asks": {"__err": "401 read scope"}}));
        let b = brief_text(&f).unwrap();
        assert!(b.contains("RESUME     OBS-new · written 0m ago · PREVIEW only"), "{b}");
        assert!(b.contains("ASKS FOR ME UNREADABLE — 401 read scope") && !b.contains("ASKS FOR ME (0)"), "{b}");
        assert_eq!(brief_value(&f, false).unwrap()["asks"]["error"], "401 read scope");
    }

    #[test]
    fn a_head_past_its_clock_is_marked_overdue_in_the_brief() {
        // 2026-09-28T11:26:00Z
        let now = touch_ms("2026-09-28T11:26:00Z").unwrap();
        assert_eq!(touch_ms("2026-09-28"), Some(1_790_553_600_000), "days-from-civil: 2026-09-28 00:00Z");
        assert_eq!(touch_ms("1970-01-02T00:00:01Z"), Some(86_401_000));
        assert_eq!(touch_ms("—"), None);
        assert_eq!(touch_ms("2026-13-01"), None);
        assert_eq!(overdue("2026-09-27T22:00:00Z", now).as_deref(), Some("13h"), "Bobby's head, 13h26 late");
        assert_eq!(overdue("2026-08-05T16:57:46Z", now).as_deref(), Some("53d"));
        assert_eq!(overdue("2026-09-28", now), None, "a bare date is due at the end of its day");
        assert_eq!(overdue("2026-09-27", now).as_deref(), Some("11h"));
        assert_eq!(overdue("2026-09-29", now), None);
        assert_eq!(overdue("", now), None);
        let f = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": now}, "/fold": {"items": []},
            "/states": {"heads": [{"state": "open", "subject": "late", "next_touch": "2026-09-27T22:00:00Z"}, {"state": "running", "subject": "fine", "next_touch": "2026-09-29"}, {"state": "done", "subject": "gone", "next_touch": "2020-01-01"}]}}));
        assert!(brief_text(&f).unwrap().contains("WORK       2 open heads (1 overdue)"), "the lean brief counts the late head");
        let b = brief_with(&f, true).unwrap();
        assert!(b.contains("[open] late  next: 2026-09-27T22:00:00Z  OVERDUE 13h"), "{b}");
        assert!(b.contains("[running] fine  next: 2026-09-29\n"), "not overdue, printed flat: {b}");
        assert!(!b.contains("gone"), "done heads are not open work");
    }

    fn rpc(door: &dyn Door, method: &str, params: Value, legacy: bool) -> Value {
        handle(door, &json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}), legacy).unwrap()
    }
    fn text(r: &Value) -> String {
        r["result"]["content"][0]["text"].as_str().unwrap_or("").to_string()
    }

    #[test]
    fn initialize_echoes_the_clients_protocol_and_names_the_seat() {
        let f = Fake::new(json!({}));
        let r = rpc(&f, "initialize", json!({"protocolVersion": "2024-11-05"}), false);
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["serverInfo"]["title"], "Oracle · rocky");
        assert!(r["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn a_notification_gets_no_reply() {
        let f = Fake::new(json!({}));
        assert!(handle(&f, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), false).is_none());
    }

    #[test]
    fn every_tool_has_a_closed_schema_and_says_whether_it_writes() {
        for legacy in [false, true] {
            let tools = tools(legacy);
            let tools = tools.as_array().unwrap();
            assert!(tools.len() >= if legacy { 17 } else { 12 });
            for t in tools {
                assert_eq!(t["inputSchema"]["additionalProperties"], false, "{}", t["name"]);
                assert!(t["annotations"]["readOnlyHint"].is_boolean(), "{}", t["name"]);
                for req in t["inputSchema"]["required"].as_array().unwrap() {
                    assert!(t["inputSchema"]["properties"][req.as_str().unwrap()].is_object(), "{} requires an undeclared {req}", t["name"]);
                }
            }
            let writes: Vec<&str> = tools
                .iter()
                .filter(|t| t["annotations"]["readOnlyHint"] == false)
                .map(|t| t["name"].as_str().unwrap())
                .collect();
            let mut want = vec!["oracle_inbox", "oracle_say", "oracle_mark", "oracle_verdict", "oracle_remember", "oracle_task", "oracle_room_state", "oracle_react", "oracle_annotate", "oracle_cortex", "oracle_flush", "write", "settle"];
            if legacy {
                want.extend(["oracle_room_map", "oracle_claim", "oracle_archive", "room_say"]);
            }
            assert_eq!(writes, want);
        }
    }

    /// `oracle_delta` (Blaze, 1 Nov 2026): one read of /delta, its since passed through encoded, and the page prints each
    /// section with the records it names; a quiet room is named, not listed, and the machine line says what it knows.
    #[test]
    fn delta_reads_one_route_and_prints_the_four_sections() {
        let page = json!({"schema": "delta/1", "seat": "rocky", "now_ms": 1_800_000_000_000i64, "now": "2027-01-15T08:00:00.000Z",
            "since": {"kind": "resume", "id": "OBS-rocky-1", "ms": 1_799_996_400_000i64, "iso": "2027-01-15T07:00:00.000Z"},
            "rooms": [{"room": "core", "new": 4, "to_me": 1, "to_me_lines": [{"id": "OBS-core-2", "author": "blaze", "ms": 1_799_997_300_000i64, "line": "@rocky look"}],
                       "state": {"changed": true, "head": "OBS-core-3"}, "why": ["state"]}],
            "quiet": ["ops"],
            "asks": {"opened": [{"id": "OBS-ops-4", "room": "ops", "author": "dinesh", "held_by": ["rocky"], "card": "which port?"}], "answered": [], "closed": []},
            "words": [{"room": "blaze", "new": 1, "items": [{"id": "OBS-blaze-5", "author": "blaze", "ms": 1_799_998_500_000i64, "line": "new words"}]}],
            "machine": {"started_ms": 1_799_992_800_000i64, "started": "2027-01-15T06:00:00.000Z", "restarted_since": false, "mine_since": 0, "mine": []}});
        let f = Fake::new(json!({"/delta": page}));
        let text = call(&f, "oracle_delta", &json!({"since": "2026-10-31T08:00"})).unwrap();
        assert_eq!(f.gets.borrow().as_slice(), ["/delta?since=2026-10-31T08%3A00"], "one read, since encoded");
        for want in ["DELTA      rocky · since my last RESUME OBS-rocky-1 (2027-01-15T07:00", "ROOMS      1 changed, 1 quiet",
                     "#core", "4 new · 1 to me · State changed (OBS-core-3)", "OBS-core-2", "quiet: #ops",
                     "ASKS       1 opened, 0 answered, 0 closed", "OBS-ops-4", "WORDS      #blaze 1 new", "OBS-blaze-5",
                     "MACHINE    oracle-core not restarted since", "mine     0 records since"] {
            assert!(text.contains(want), "{want:?} missing from:\n{text}");
        }
        assert!(!text.lines().any(|l| l.starts_with("  #ops")), "a quiet room is named, not listed with counts:\n{text}");
        // NEGATIVE CONTROL: no since asks /delta bare (the server's default, the last RESUME)
        let f = Fake::new(json!({"/delta": page}));
        call(&f, "oracle_delta", &json!({})).unwrap();
        assert_eq!(f.gets.borrow().as_slice(), ["/delta"]);
    }

    #[test]
    fn experiments_lists_the_registry_with_stage_room_and_result_and_passes_the_filters() {
        let f = Fake::new(json!({"/experiments": {"total": 198, "count": 1, "items": [{"subject": "exp.qsb.x", "state": "open", "room": "mathematics-lab", "id": "OBS-1",
            "data": {"title": "QSB subset", "stage": "measured", "result": "2048 hashes, 0 mismatches", "evidence": ["OBS-a", "OBS-b"]}}]}}));
        let t = call(&f, "oracle_experiments", &json!({"track": "qsb", "stage": "measured"})).unwrap();
        assert!(t.starts_with("1 of 198 experiments"), "{t}");
        assert!(t.contains("exp.qsb.x · measured · #mathematics-lab · QSB subset — 2048 hashes, 0 mismatches (evidence 2) [OBS-1]"), "{t}");
    }

    #[test]
    fn terms_lists_the_ranked_words_best_first_and_says_when_there_are_none() {
        let cargo = json!({"data": {"documents": 109192, "generated_at": "2026-10-08T09:13:00Z", "top": [["dot", 1200.5, 3456], ["room", 900.0, 5000]]}}).to_string();
        let f = Fake::new(json!({"/states": {"subject": "search.terms", "observation": {"cargo": cargo}}}));
        let t = call(&f, "oracle_terms", &json!({"limit": 1})).unwrap();
        assert!(t.contains("in 109192 messages"), "{t}");
        assert!(t.contains("1. dot  weight 1200.5  in 3456 messages"), "{t}");
        assert!(!t.contains("room"), "limit 1 lists one word: {t}");
        let live = Fake::new(json!({"/terms": {"documents": 7, "generated_at": "g", "top": [["oracle", 9.0, 5]]}, "/states": {"subject": "search.terms", "observation": {"cargo": cargo}}}));
        let t = call(&live, "oracle_terms", &json!({})).unwrap();
        assert!(t.contains("1. oracle  weight 9.0  in 5 messages") && !t.contains("dot"), "the hourly ranking wins over the head: {t}");
        let none = Fake::new(json!({"/states": {"subject": "search.terms", "observation": {}}}));
        assert!(call(&none, "oracle_terms", &json!({})).unwrap().starts_with("No word ranking yet"));
    }

    /// The fold of 2026-10-21 (docs/mcp.md): fifteen tools listed, sixteen with oracle_file (21 Oct), eighteen with oracle_mark and oracle_verdict (27 Oct); every old name answers through fold_tool with the
    /// same text as its new form plus a notice; a name that never existed is an error (the control).
    #[test]
    fn the_fold_lists_fifteen_and_every_old_name_answers_as_its_new_form_with_a_notice() {
        let listed: Vec<String> = tools(false).as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(listed.len(), 24, "nineteen tools and the five verbs: {listed:?}");
        for old in ["oracle_hello", "oracle_where", "oracle_terms", "oracle_query", "oracle_about", "oracle_room_state_full", "oracle_room_digest_agent", "oracle_room_map", "oracle_hallway", "oracle_tasks", "oracle_board", "oracle_experiments", "oracle_claim", "oracle_claims", "oracle_graph", "oracle_archive", "oracle_state", "room_say"] {
            assert!(!listed.contains(&old.to_string()), "{old} is listed");
            let (new, _, note) = fold_tool(old, &json!({})).unwrap_or_else(|| panic!("{old} folds"));
            assert!(listed.contains(&new), "{old} -> {new} is listed");
            assert_eq!(note, format!("{old} is now {new} (until 2026-10-28)"));
        }
        assert!(fold_tool("oracle_recall", &json!({})).is_none() && fold_tool("nonesuch", &json!({})).is_none());
        let f = Fake::new(json!({"/hallway": {"rooms": []}, "/states": {"heads": []}, "/whoami": {"handle": "rocky"}, "/fold": {"items": [], "pending": {}}}));
        let old = rpc(&f, "tools/call", json!({"name": "oracle_hallway", "arguments": {"days": 3}}), false);
        let new = rpc(&f, "tools/call", json!({"name": "oracle_digest", "arguments": {"all": true, "days": 3}}), false);
        assert_eq!(old["result"]["isError"], false, "{old}");
        assert_eq!(old["result"]["notice"], "oracle_hallway is now oracle_digest (until 2026-10-28)");
        assert!(new["result"]["notice"].is_null());
        let strip = |r: &Value| text(r).lines().filter(|l| !l.contains("is now oracle_")).collect::<Vec<_>>().join("\n").trim_end().to_string();
        assert_eq!(strip(&old), strip(&new), "the old name prints what the new form prints");
        assert!(text(&old).ends_with("(until 2026-10-28)"), "the notice is the last line: {}", text(&old));
        let tasks_old = rpc(&f, "tools/call", json!({"name": "oracle_tasks", "arguments": {"owner": "*"}}), false);
        let tasks_new = rpc(&f, "tools/call", json!({"name": "oracle_task", "arguments": {"owner": "*"}}), false);
        assert_eq!(strip(&tasks_old), strip(&tasks_new));
        let gone = rpc(&f, "tools/call", json!({"name": "oracle_nonesuch", "arguments": {}}), false);
        assert_eq!(gone["result"]["isError"], true, "{gone}");
    }


    /// oracle_file (21 Oct 2026, docs/mcp.md): a text file comes back as text, counted and capped; a binary file as
    /// size and sha; a door without bytes says so; the id is accepted with or without its `sha256:` prefix.
    #[test]
    fn oracle_file_reads_text_files_and_names_binaries() {
        let f = Fake::new(json!({
            "/files/sha256:abc": {"__ctype": "text/plain", "__text": "Room of Requirement, Session IV\nThe Room Protocol is: a system in which every human being can enter a conversation."},
            "/files/sha256:cafe": {"__ctype": "image/png", "__text": "\u{89}PNG"},
            "/files/sha256:abc/meta": {"id": "sha256:abc", "content_type": "text/plain", "size": 110, "rooms": ["room"], "cited_by": ["OBS-room-1"]},
            "/files": {"files": [{"id": "sha256:abc", "content_type": "text/plain", "size": 110, "name": "ror-session-04.txt"}]}
        }));
        let got = typed_read(&f, "oracle_file", &json!({"op": "get", "id": "abc"})).unwrap().unwrap();
        assert_eq!(got.value["content_type"], "text/plain");
        assert!(got.value["text"].as_str().unwrap().starts_with("Room of Requirement"));
        assert_eq!(got.value["chars"], got.value["shown"]);
        let capped = typed_read(&f, "oracle_file", &json!({"op": "get", "id": "sha256:abc", "max": 1000})).unwrap().unwrap();
        assert_eq!(capped.value["shown"], capped.value["chars"], "a short file is never cut");
        let png = typed_read(&f, "oracle_file", &json!({"op": "get", "id": "cafe"})).unwrap().unwrap();
        assert!(png.value["text"].is_null(), "{}", png.text);
        assert_eq!(png.value["size"], 5, "the four characters are five bytes");
        let meta = typed_read(&f, "oracle_file", &json!({"op": "meta", "id": "abc"})).unwrap().unwrap();
        assert_eq!(meta.value["cited_by"][0], "OBS-room-1");
        let list = typed_read(&f, "oracle_file", &json!({"op": "list", "room": "room"})).unwrap().unwrap();
        assert_eq!(list.value["files"].as_array().unwrap().len(), 1);
        let missing = typed_read(&f, "oracle_file", &json!({"op": "get", "id": "nope"})).unwrap();
        assert!(missing.is_err(), "a door without the bytes refuses");
        let bad = typed_read(&f, "oracle_file", &json!({"op": "zip", "id": "abc"})).unwrap();
        assert!(bad.is_err());
    }

    #[test]
    fn the_old_names_are_listed_only_at_the_legacy_door_but_answer_at_both() {
        let f = Fake::new(json!({"/search": {"hits": [{"score": 1.0, "id": "OBS-9", "channel": "hustler", "author": "kin", "content": "DASH broke 65"}]}}));
        let names = |legacy: bool| -> Vec<String> {
            rpc(&f, "tools/list", json!({}), legacy)["result"]["tools"]
                .as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
        };
        assert!(!names(false).contains(&"oracle_about".to_string()));
        for old in ["oracle_query", "oracle_about", "oracle_thread", "oracle_board", "oracle_state", "room_say"] {
            assert!(names(true).contains(&old.to_string()), "{old} is listed for legacy clients");
        }
        let r = rpc(&f, "tools/call", json!({"name": "oracle_about", "arguments": {"room": "hustler", "terms": "DASH"}}), false);
        assert_eq!(r["result"]["isError"], false, "{r}");
        assert!(text(&r).contains("OBS-9"));
        assert_eq!(f.gets.borrow().last().unwrap(), "/search?q=DASH&limit=15&channel=hustler", "about = recall in one room");
        rpc(&f, "tools/call", json!({"name": "oracle_query", "arguments": {"query": "DASH 65"}}), true);
        assert_eq!(f.gets.borrow().last().unwrap(), "/search?q=DASH%2065&limit=10");
    }

    #[test]
    fn room_say_is_oracle_say_with_the_old_argument_names_and_keeps_cargo() {
        let f = Fake::new(json!({}));
        let r = rpc(&f, "tools/call", json!({"name": "room_say", "arguments": {"channel": "hustler", "content": "hi @rocky", "cargo": "{\"x\":1}", "tags": ["kind:note"]}}), true);
        assert!(text(&r).starts_with("signed:ok  OBS-1"), "{r}");
        let w = f.writes.borrow();
        assert_eq!(w[0], ("say".to_string(), "hustler".to_string(), "hi @rocky".to_string(), Some("{\"x\":1}".to_string()), vec!["kind:note".to_string()]));
    }

    #[test]
    fn a_thread_shows_its_root_whole_and_every_reply() {
        let long = "q ".repeat(2000) + "LAST-QUESTION";
        let f = Fake::new(json!({"/threads": {"state": "open", "card": {"status": "waiting", "replies": 1}, "floor": {"name": "people"},
            "root": {"id": "OBS-r", "channel": "hustler", "author": "blaze", "created_ms": 0, "content": long},
            "replies": [{"id": "OBS-a", "channel": "hustler", "author": "rocky", "created_ms": 60000, "content": "on it"}]}}));
        let r = rpc(&f, "tools/call", json!({"name": "oracle_thread", "arguments": {"id": "OBS-r"}}), false);
        let t = text(&r);
        assert!(t.contains("LAST-QUESTION"), "the root is never clipped");
        assert!(t.contains("· OBS-a") && t.contains("on it"), "every reply, headed with its id: {t}");
        assert!(f.gets.borrow().iter().any(|g| g == "/threads?id=OBS-r"), "{:?}", f.gets.borrow());
    }

    #[test]
    fn board_filters_a_room_client_side_and_state_is_the_brief() {
        let f = Fake::new(json!({
            "/states": {"heads": [{"state": "open", "subject": "a", "owner": "rocky", "channel": "hustler"}, {"state": "open", "subject": "b", "owner": "kin", "channel": "astra"}]},
            "/whoami": {"handle": "rocky"}, "/fold": {"items": [], "pending": {}}}));
        let r = rpc(&f, "tools/call", json!({"name": "oracle_board", "arguments": {"room": "astra"}}), true);
        assert!(text(&r).starts_with("[open] a  owner rocky  next —  #hustler"), "{r}");
        assert_eq!(f.gets.borrow().last().unwrap(), "/states?channel=astra", "the room is the server's filter");
        let r = rpc(&f, "tools/call", json!({"name": "oracle_state", "arguments": {}}), true);
        assert!(text(&r).starts_with("ox brief · rocky"), "{r}");
        assert!(text(&r).contains("IDENTITY   rocky · key abcdef12… · http://fake"));
    }

    #[test]
    fn failures_are_results_the_model_can_read_and_unknown_methods_are_protocol_errors() {
        let f = Fake::new(json!({}));
        for r in [
            rpc(&f, "tools/call", json!({"name": "nope", "arguments": {}}), false),
            rpc(&f, "tools/call", json!({"name": "oracle_say", "arguments": {"room": "r"}}), false),
            rpc(&f, "tools/call", json!({"name": "oracle_remember", "arguments": {"text": "cpu 93%", "kind": "status"}}), false),
        ] {
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(!text(&r).is_empty());
        }
        assert!(f.writes.borrow().is_empty(), "nothing was written");
        let r = rpc(&f, "resources/list", json!({}), false);
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn recall_answerable_reads_the_pre_flight_instead_of_the_hits() {
        let f = Fake::new(json!({"/answerable": {"schema": "answerable/1", "verdict": "partial", "says": "on record: lattice; not on record: quasar",
            "next": "recall what is on record, and say what is not", "missing": ["quasar"],
            "words": [{"word": "lattice", "records": 2, "approved": 1}, {"word": "quasar", "records": 0, "approved": 0}],
            "all_words": {"records": 0, "approved": [], "others": []},
            "graph": {"nodes": [{"title": "company of agents", "certification": "partial", "approved": 1, "readings": 2}], "records": ["OBS-cortex-1"]}}}));
        let t = call(&f, "oracle_recall", &json!({"query": "lattice quasar", "answerable": true, "room": "#dot"})).unwrap();
        assert!(t.starts_with("ANSWERABLE  PARTIAL  on record: lattice; not on record: quasar"), "{t}");
        assert!(t.contains("lattice 2 (1 approved) · quasar 0"), "{t}");
        assert!(t.contains("graph     OBS-cortex-1") && t.contains("company of agents (partial 1/2)"), "{t}");
        assert!(t.ends_with("next      recall what is on record, and say what is not"), "{t}");
        assert_eq!(f.gets.borrow().last().unwrap(), "/answerable?q=lattice%20quasar&room=dot");
    }

    #[test]
    fn where_lists_the_rooms_with_counts_and_dates_and_says_when_there_are_none() {
        let f = Fake::new(json!({"/search": {"items": [], "rooms": [
            {"room": "illuminati", "count": 34, "first_ms": 1_780_185_600_000_i64, "last_ms": 1_782_000_000_000_i64},
            {"room": "matrix", "count": 6, "first_ms": 1_781_740_800_000_i64, "last_ms": 1_781_740_800_000_i64}]}}));
        let t = call(&f, "oracle_where", &json!({"query": "Oppenheimer"})).unwrap();
        assert!(t.starts_with("\"Oppenheimer\": 40 messages in 2 rooms"), "{t}");
        assert!(t.contains("#illuminati  34  2026-05-31 to 2026-06-21") && t.contains("#matrix  6  2026-06-18"), "{t}");
        assert!(f.gets.borrow().last().unwrap().contains("by_room=true"));
        let none = Fake::new(json!({"/search": {"items": [], "rooms": []}}));
        assert!(call(&none, "oracle_where", &json!({"query": "zzz"})).unwrap().contains("not said in any room"));
    }

    #[test]
    fn oracle_say_turns_ask_into_the_ask_tag_and_refuses_an_unknown_one() {
        let f = Fake::new(json!({}));
        call(&f, "oracle_say", &json!({"room": "mathematics", "text": "what have you built?", "to": ["rocky"], "ask": "reply"})).unwrap();
        let w = f.writes.borrow();
        let tags = &w.last().unwrap().4;
        assert!(tags.contains(&"to:rocky".to_string()) && tags.contains(&"ask:reply".to_string()), "{tags:?}");
        drop(w);
        assert!(call(&f, "oracle_say", &json!({"room": "r", "text": "x", "ask": "maybe"})).is_err());
    }

    #[test]
    fn a_record_keeps_named_server_fields_only_and_prints_state_only_when_it_is_not_open() {
        let o = json!({"id": "OBS-1", "seq": 9, "channel": "oracle", "author": "emmy", "content": "x", "tags": ["ask:reply"],
            "ask": "none", "state": "open", "answered_by": "OBS-2", "some_future_field": {"big": "x".repeat(500)}, "sig": "s"});
        let m = msg_value(&o, 0, true, false);
        assert_eq!((m["seq"].as_i64(), m["answered_by"].as_str()), (Some(9), Some("OBS-2")));
        assert!(m.get("some_future_field").is_none(), "a field nobody named is not paid for: {m}");
        assert!(m["ask"].is_null(), "the server's ask:none beats the tag");
        assert!(m.get("sig").is_none() && msg_value(&o, 0, true, true)["sig"] == "s", "provenance only with the whole record");
        let line = msg_render(&m);
        assert!(!line.contains(" · open") && line.contains("answered by OBS-2"), "{line}");
        let closed = msg_render(&msg_value(&json!({"id": "OBS-3", "state": "superseded", "content": ""}), 0, true, false));
        assert!(closed.contains(" · superseded"), "{closed}");
    }

    #[test]
    fn every_typed_read_reports_an_auth_failure_as_an_error_never_as_an_empty_answer() {
        // Parity fixture (Emmy's typed-read conditions): the door refuses every read.
        let f = Fake::new(json!({"/whoami": {"__err": "401 token revoked"}, "/search": {"__err": "401 token revoked"},
            "/threads": {"__err": "401 token revoked"}, "/fold": {"__err": "401 token revoked"}}));
        for (name, args) in [("oracle_read", json!({"room": "oracle"})), ("oracle_get", json!({"id": "OBS-1"})),
                             ("oracle_thread", json!({"id": "OBS-1"})), ("oracle_inbox", json!({})), ("oracle_brief", json!({}))] {
            let r = rpc(&f, "tools/call", json!({"name": name, "arguments": args}), false);
            assert_eq!(r["result"]["isError"], true, "{name}: {r}");
            assert!(text(&r).contains("401 token revoked"), "{name} names the failure: {r}");
            assert!(r["result"].get("structuredContent").is_none(), "{name}: no value that could read as an empty answer");
        }
    }

    #[test]
    fn caps_and_pages_are_said_out_loud_and_a_room_reads_page_by_page() {
        let item = |id: &str, seq: i64| json!({"id": id, "seq": seq, "channel": "oracle", "author": "emmy", "content": id, "created_ms": 0});
        let f = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": 0},
            "/search?channel=oracle&limit=2": {"items": [item("OBS-4", 4), item("OBS-3", 3)], "next_before": 3, "scanned": 2},
            "/search?channel=oracle&limit=2&before=3": {"items": [item("OBS-2", 2), item("OBS-1", 1)], "next_before": null, "scanned": 2},
            "/fold": {"items": [], "pending": {"oracle": 9}, "has_more": true},
            "/asks": {"waiting_on_me": [], "capped": true}, "/states": {"heads": []}}));
        let first = typed_read(&f, "oracle_read", &json!({"room": "oracle", "limit": 2})).unwrap().unwrap();
        assert_eq!(first.value["next_before"], 3);
        assert!(first.text.contains("before=3"), "{}", first.text);
        let second = typed_read(&f, "oracle_read", &json!({"room": "oracle", "limit": 2, "before": 3})).unwrap().unwrap();
        let ids = |t: &Typed| t.value["messages"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        assert_eq!([ids(&first), ids(&second)].concat(), ["OBS-3", "OBS-4", "OBS-1", "OBS-2"], "two pages, each oldest first, nothing twice");
        assert!(second.value["next_before"].is_null() && !second.text.contains("Older messages"), "the last page says nothing more");
        let inbox = inbox_value(&f, 50, Ack::No, false).unwrap();
        assert_eq!(inbox["has_more"], true);
        assert!(inbox_render(&inbox).contains("more after it"));
        let brief = brief_value(&f, false).unwrap();
        assert_eq!(brief["asks"]["capped"], true);
        assert!(brief_render(&brief, false).contains("may be missing"));
    }

    #[test]
    fn a_stale_subject_is_named_on_the_line() {
        let it = serde_json::json!({"created_ms": 0, "channel": "hustler", "author": "bobby-core", "content": "policy.p1 waits on Blaze",
            "stale": [{"subject": "policy.p1", "state": "done", "owner": "kepler"}]});
        assert!(items_text(&[it], 96).contains("NOW policy.p1 is done (kepler)"));
        let plain = serde_json::json!({"created_ms": 0, "channel": "hustler", "author": "rocky", "content": "hello", "stale": []});
        assert!(!items_text(&[plain], 96).contains("NOW"));
    }

    // Rocky's live pass, 1 Nov 2026 07:15Z: the MCP brief carried 20 ask rows, none of the cortex, and `local: null` while the
    // server's /whoami said +05:30 (ox mcp runs where ORACLE_LOCAL_OFFSET is not set).
    #[test]
    fn the_brief_caps_its_asks_at_ten_counts_the_cortex_and_keeps_the_servers_local_clock() {
        let now = 1_793_340_000_000i64;
        let asks: Vec<Value> = (0..25).map(|i| json!({"id": format!("OBS-a{i:02}"), "room": if i % 2 == 0 { "axxis" } else { "dot" }, "author": "noor", "ask": "reply",
            "state": "open", "created_ms": now - i * 60_000, "card": "an ask"})).collect();
        let base = |features: Value| json!({"/whoami": {"handle": "rocky", "server_ms": now, "local": "09:30 (+05:30)", "features": features},
            "/fold": {"items": [], "pending": {}}, "/states": {"heads": []}, "/asks": {"waiting_on_me": asks},
            "/cortex": {"schema": "cortex-count/1", "approved_nodes": 2487, "pending_marks": 440, "by_kind": {"observation": 1449}}});
        assert!(std::env::var("ORACLE_LOCAL_OFFSET").is_err(), "this machine has no local clock of its own, as a seat's Mac");
        let f = Fake::new(base(json!(["authored_ms", "prev", "cortex_count"])));
        let r = handle(&f, &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "oracle_brief", "arguments": {}}}), false).unwrap()["result"].clone();
        let v = &r["structuredContent"];
        assert_eq!(v["local"], "09:30 (+05:30)", "the server's local clock, not null: {}", v["local"]);
        assert_eq!((v["asks"]["rows"].as_array().map(Vec::len), v["asks"]["more"].as_u64()), (Some(10), Some(15)));
        assert_eq!(v["cortex"], json!({"approved_nodes": 2487, "pending_marks": 440}));
        assert!(f.gets.borrow().iter().any(|g| g == "/cortex?count=1"));
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("ox brief · rocky · now ") && text.lines().next().unwrap().ends_with(" · local 09:30 (+05:30)"), "{text}");
        assert_eq!(text.matches("  reply  #").count(), 10, "ten listed in the text too: {text}");
        assert!(text.contains("+ 15 more: #axxis 8 · #dot 7 — ox asks --mine lists every one"), "{text}");
        assert!(text.contains("CORTEX     2487 approved nodes · 440 marks waiting on Blaze's verdict"), "{text}");
        assert!(!text.contains("LAW "), "a server without cortex_law is never asked: {text}");
        // full keeps every row.
        let full = handle(&f, &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "oracle_brief", "arguments": {"full": true}}}), false).unwrap()["result"].clone();
        assert_eq!(full["structuredContent"]["asks"]["rows"].as_array().map(Vec::len), Some(25));
        assert_eq!(full["content"][0]["text"].as_str().unwrap().matches("  reply  #").count(), 25);
        // A server that does not count is never asked for its whole node list.
        let old = Fake::new(base(json!(["authored_ms", "prev"])));
        let o = brief_value(&old, false).unwrap();
        assert!(o["cortex"]["unavailable"].is_string() && !old.gets.borrow().iter().any(|g| g.starts_with("/cortex")), "{:?}", old.gets.borrow());
        assert!(!brief_render(&o, false).contains("CORTEX"));
        // An unreadable count is said, never zero.
        let mut down = base(json!(["cortex_count"]));
        down["/cortex"] = json!({"__err": "server refused (500): fold failed"});
        let d = brief_value(&Fake::new(down), false).unwrap();
        assert!(brief_render(&d, false).contains("CORTEX     could not be counted: server refused (500)"));
        // The inbox keeps the server's local clock too.
        assert_eq!(inbox_value(&f, 5, Ack::No, false).unwrap()["local"], "09:30 (+05:30)");
    }

    // Noor, 3 Nov 2026: what Blaze approved as Rule, Profile or Preference wakes with every seat.
    #[test]
    fn the_brief_carries_the_law_blaze_approved_bounded_and_says_when_it_cannot_read_it() {
        let now = 1_793_340_000_000i64;
        let lines: Vec<Value> = (0..3).map(|i| json!({"code": format!("OBS-cortex-20261103-00000000000{i}"), "line": format!("rule {i}"), "linked": 3 - i})).collect();
        let base = || json!({"/whoami": {"handle": "rocky", "server_ms": now, "features": ["cortex_count", "cortex_law"]},
            "/fold": {"items": [], "pending": {}}, "/states": {"heads": []}, "/asks": {"waiting_on_me": []},
            "/cortex/law": {"schema": "cortex-law/1", "total": 292, "shown": 3, "lines": lines},
            "/cortex": {"schema": "cortex-count/1", "approved_nodes": 1, "pending_marks": 0}});
        let f = Fake::new(base());
        let v = brief_value(&f, false).unwrap();
        assert!(f.gets.borrow().iter().any(|g| g == &format!("/cortex/law?top={LAW_BRIEF_TOP}")), "{:?}", f.gets.borrow());
        let text = brief_render(&v, false);
        assert!(text.contains("LAW        292 readings Blaze approved as Rule, Profile or Preference · the 3 most linked here; every one: ox brief --full"), "{text}");
        assert!(text.contains("  - rule 0 (000000000000)\n  - rule 1 (000000000001)"), "most linked first, each with its code: {text}");
        let full = Fake::new(base());
        brief_value(&full, true).unwrap();
        assert!(full.gets.borrow().iter().any(|g| g == "/cortex/law?top=10000"), "--full asks for every line: {:?}", full.gets.borrow());
        let mut down = base();
        down["/cortex/law"] = json!({"__err": "server refused (500): fold failed"});
        assert!(brief_render(&brief_value(&Fake::new(down), false).unwrap(), false).contains("LAW        could not be read: server refused (500)"));
    }

    // Rocky's live pass, 1 Nov 2026 07:15Z: `oracle_inbox mine=true limit=5` answered 86 KB: the limit bounded only the fold's page.
    #[test]
    fn the_inbox_keeps_its_limit_in_every_list_and_says_what_it_cut() {
        let now = 1_790_836_247_741i64;
        let card = "c".repeat(400);
        let ask = |i: usize, ms: i64, state: &str| json!({"id": format!("OBS-ask{i:03}"), "room": "axxis", "author": "noor", "ask": "reply", "state": state,
            "created_ms": ms, "card": card, "marks": {}});
        let open: Vec<Value> = (0..200).map(|i| ask(i, INBOX_SINCE_MS + 10_000 - i as i64, if i % 10 == 3 { "answered" } else { "open" }))
            .chain((0..40).map(|i| ask(500 + i, INBOX_SINCE_MS - 1 - i as i64, "open"))).collect();
        let acked: Vec<Value> = (0..30).map(|i| ask(300 + i, INBOX_SINCE_MS + 5, "open")).collect();
        let stuck: Vec<Value> = (0..3).map(|i| ask(400 + i, INBOX_SINCE_MS + 5, "open")).collect();
        let to_me = |seq: i64| json!({"id": format!("OBS-me{seq}"), "seq": seq, "channel": "oracle", "author": "emmy", "for_me": true, "tags": ["ask:reply"],
            "content": "@rocky look", "created_ms": now - 60_000});
        let unread: Vec<Value> = (100..130).rev().map(to_me).collect();
        let f = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": now}, "/asks": {"waiting_on_me": open, "acked": acked, "stuck": stuck},
            "/fold": {"items": [{"id": "OBS-x", "seq": 300, "channel": "taylor", "author": "t", "for_me": false, "tags": [], "content": "x"}],
                      "unread_for_me": unread, "pending": {"oracle": 30, "taylor": 1}, "next_marks": {"oracle": 140, "taylor": 300}}}));
        let call_inbox = |args: Value| rpc(&f, "tools/call", json!({"name": "oracle_inbox", "arguments": args}), false)["result"].clone();
        let r = call_inbox(json!({"mine": true, "limit": 5}));
        let v = &r["structuredContent"];
        let n = |k: &str| v["asks"][k].as_array().map(Vec::len);
        assert_eq!((n("open"), n("acked"), n("stuck"), n("older")), (Some(5), Some(5), Some(3), Some(0)), "{}", v["asks"]);
        assert_eq!(v["asks"]["open"][0]["id"], "OBS-ask000", "the newest first, as the server sent them");
        assert_eq!((v["for_me"].as_array().map(Vec::len), v["for_me"][0]["id"].as_str()), (Some(5), Some("OBS-me129")));
        assert_eq!(v["rooms"], json!([]), "mine leaves out the other rooms");
        assert_eq!((v["budget"]["rows"].as_u64(), v["budget"]["cut"].as_u64()), (Some(18), Some(195 + 25 + 40 + 25)), "{}", v["budget"]);
        let more = v["budget"]["more"].as_str().unwrap();
        assert!(more.contains("limit: up to 200") && more.contains("older: true lists the 40") && more.contains("limit: 30 lists every message addressed to you"), "{more}");
        let whole = call_inbox(json!({"mine": true, "limit": 200})).to_string().len();
        assert!(r.to_string().len() < 16_000 && whole > 8 * r.to_string().len(), "{} bytes at limit 5, {whole} at 200", r.to_string().len());
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("ASKS TO ME · 180 open, every room.") && text.contains("+ 176 more not in this answer"), "the counts are the whole lists': {text}");
        assert!(text.contains("ANSWERED BY ANOTHER SEAT, yours still open · 20\n") && text.contains("ACKED, not done · 30\n"), "{text}");
        // Unset: twenty in each list.
        let d = call_inbox(json!({}));
        assert_eq!((d["structuredContent"]["asks"]["open"].as_array().map(Vec::len), d["structuredContent"]["for_me"].as_array().map(Vec::len)), (Some(20), Some(20)));
        assert!(f.gets.borrow().iter().any(|g| g == "/fold?limit=20"), "{:?}", f.gets.borrow());
        // older: true lists the older asks, bounded too.
        let o = call_inbox(json!({"older": true, "limit": 5}));
        assert_eq!(o["structuredContent"]["asks"]["older"].as_array().map(Vec::len), Some(5));
        assert!(!o["structuredContent"]["budget"]["more"].as_str().unwrap().contains("older: true"));
        // ack marks the page, never past an addressed message the limit left out (OBS-me124, seq 124, is the newest cut).
        let a = call_inbox(json!({"mine": true, "limit": 5, "ack": true}));
        assert_eq!(a["structuredContent"]["acked"], true);
        let posted = f.posts.borrow().last().unwrap().1.clone();
        assert_eq!((posted["marks"]["oracle"].as_i64(), posted["marks"]["taylor"].as_i64()), (Some(99), Some(300)), "{posted}");
        // Nothing cut: the budget says so.
        let small = Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": now}, "/asks": {"waiting_on_me": [ask(1, INBOX_SINCE_MS + 1, "open")]}, "/fold": {"items": []}}));
        let s = inbox_mcp(&small, &json!({}), Ack::No).unwrap();
        assert_eq!(s["budget"], json!({"rows": 1, "cut": 0, "more": null}));
    }

    // Blaze, 1 Nov 2026: five verbs over one address space, the 18 names kept as aliases.
    // Each verb's answer is byte for byte the old tool's answer for the same arguments; an argument no old tool takes is refused by name.
    fn verb_fake() -> Fake {
        let msg = json!({"id": "OBS-oracle-aa", "seq": 4, "channel": "oracle", "author": "emmy", "content": "a record", "tags": ["intent:note"], "created_ms": 1_790_000_000_000i64});
        Fake::new(json!({"/whoami": {"handle": "rocky", "server_ms": 1_790_000_100_000i64}, "/fold": {"items": [], "pending": {}}, "/states": {"heads": []},
            "/asks": {"waiting_on_me": []}, "/search?channel=oracle": {"items": [msg.clone()]}, "/search?id=OBS-oracle-aa": msg.clone(),
            "/search?q=": {"hits": [msg.clone()]}, "/threads?id=OBS-oracle-aa": {"root": msg, "replies": [], "state": "open", "card": {"status": "open", "replies": 0}},
            "/cortex": {"nodes": [{"nid": "n1aaaaaaaaaaaa", "kind": "concept", "title": "oracle"}], "count": 1}, "/cortex/entity/oracle": {"primer": "Oracle keeps signed memory."},
            "state:oracle": {"state": "alive", "heads": ["OBS-oracle-H1"], "revisions": 1, "complete": true, "trustworthy": true, "fields": {"current": "now"}, "bad": [], "unknown": []}}))
    }
    fn verb_same(verb: &str, vargs: Value, tool: &str, targs: Value) {
        let (f, g) = (verb_fake(), verb_fake());
        let a = rpc(&f, "tools/call", json!({"name": verb, "arguments": vargs}), false);
        let b = rpc(&g, "tools/call", json!({"name": tool, "arguments": targs}), false);
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap(), "{verb} {vargs} is byte for byte {tool}");
        assert_eq!(*f.writes.borrow(), *g.writes.borrow(), "{verb} signs what {tool} signs");
    }
    fn verb_refuses(verb: &str, vargs: Value, says: &str) {
        let f = verb_fake();
        let r = rpc(&f, "tools/call", json!({"name": verb, "arguments": vargs}), false)["result"].clone();
        assert!(r["isError"] == true && r["content"][0]["text"].as_str().unwrap().contains(says), "{verb} {vargs}: {r}");
        assert!(f.writes.borrow().is_empty(), "a refusal writes nothing");
    }

    // WORLD STATE (Blaze, 1 Nov 2026, via Noor): under 6,000 bytes at every bound, every
    // line keeping its record's code; an Oracle without /world is told so by name; a where the brief does not know is refused.
    #[test]
    fn the_world_page_fits_its_ceiling_at_every_bound_and_an_older_server_is_named() {
        let long = "x".repeat(400);
        let id = |i: usize| format!("OBS-board-20261101-{i:012x}");
        let rows = |n: usize, f: &dyn Fn(usize) -> Value| (0..n).map(f).collect::<Vec<_>>();
        let page = json!({"schema": "world/1", "now_ms": 1_793_340_000_000i64, "now": "2026-10-31T04:00:00.000Z", "local": "09:30 (+05:30)",
            "north_star": {"canon": {"id": id(0), "room": "rocky", "version": 41, "rules": 300, "open": 9}, "identities": rows(6, &|i| json!({"seat": format!("a-long-seat-name-{i}"), "id": id(i), "purpose": long})), "more": 30, "rest": "ox identity --seat <seat>"},
            "now_section": {"steps": rows(6, &|i| json!({"plan": format!("plan.a-long-room.a-long-plan-{i}"), "step": "S12", "title": long, "owner": "rocky", "id": id(i)})), "more": 9, "rest": "ox plan <room> <name>",
                "queue": {"id": id(9), "room": "rocky", "by": "blaze", "at_ms": 1_793_339_000_000i64, "items": rows(5, &|i| json!({"subject": format!("a.long.subject.{i}.{long}"), "title": long})), "more": 25}},
            "projects": {"items": rows(12, &|i| json!({"id": id(i), "room": "a-long-room", "subject": format!("a.long.subject.{i}"), "state": "blocked", "owner": "rocky", "next": "2026-11-05", "note": long})), "more": 700, "rest": "ox head --all (oracle_task owner=*)"},
            "decisions": {"items": rows(8, &|i| json!({"id": id(i), "at_ms": 1_793_000_000_000i64, "text": long})), "more": 40, "rest": "ox recall --room memory decision (oracle_read room=memory tag=memory:decision)"},
            "seats": {"items": rows(6, &|i| json!({"seat": format!("seat-{i}"), "id": id(i), "current": long, "at_ms": 1_793_339_000_000i64, "degraded": true})), "more": 20, "rest": "ox state <seat>"},
            "questions": {"items": rows(8, &|i| json!({"id": id(100 + i), "head": long, "from": "noor", "since_ms": 1_793_000_000_000i64})), "more": 50, "rest": "ox asks --seat <owner> (oracle_inbox as the owner)"},
            "next": ["ox plan <room> <name>", "ox get <id>"]});
        let text = world_render(&page);
        assert!(text.len() < WORLD_TEXT_MAX, "{} bytes at every bound:\n{text}", text.len());
        assert!(text.ends_with(&format!(" · {} bytes · next: ox plan <room> <name> · ox get <id>\n", text.len())), "{text}");
        assert!(text.starts_with("WORLD STATE · composed 31 Oct 04:00Z (Oracle clock) · local 09:30 (+05:30)\n"), "{text}");
        for i in [0usize, 5, 9, 11, 107] {
            assert!(text.contains(&format!("{i:012x}")), "the code of {} survives every cut:\n{text}", id(i));
        }
        assert!(text.lines().all(|l| l.len() <= 100), "{text}");
        // An Oracle without /world is named, never answered with an empty world; nor is one that answers something else.
        let old = Fake::new(json!({"/world": {"__err": "server refused (404): not found"}}));
        let r = rpc(&old, "tools/call", json!({"name": "oracle_brief", "arguments": {"where": "world"}}), false)["result"].clone();
        assert!(r["isError"] == true && text_of_result(&r).contains("does not serve the world page (GET /world, world/1)"), "{r}");
        let odd = Fake::new(json!({"/world": {"nodes": []}}));
        assert!(world_value(&odd).unwrap_err().to_string().contains("did not answer a world/1 page"));
        let fine = Fake::new(json!({"/world": page}));
        let ok = rpc(&fine, "tools/call", json!({"name": "oracle_brief", "arguments": {"where": "world"}}), false)["result"].clone();
        assert_eq!((ok["isError"].as_bool(), ok["structuredContent"]["schema"].as_str()), (Some(false), Some("world/1")));
        assert_eq!(text_of_result(&ok), text, "the MCP prints the shared render");
        assert_eq!(call(&fine, "oracle_brief", &json!({"where": "world"})).unwrap(), text);
        // A where the brief does not know is refused, naming the forms; me is the brief as before.
        let bad = rpc(&fine, "tools/call", json!({"name": "oracle_brief", "arguments": {"where": "moon"}}), false)["result"].clone();
        assert!(bad["isError"] == true && text_of_result(&bad).contains("oracle_brief where is me (the default) or world, not moon"), "{bad}");
        assert_eq!(brief_where(&json!({"where": "me"})).unwrap(), "me");
    }

    fn text_of_result(r: &Value) -> String {
        r["content"][0]["text"].as_str().unwrap_or("").to_string()
    }

    #[test]
    fn verb_brief_is_oracle_brief_or_a_rooms_state() {
        verb_same("brief", json!({}), "oracle_brief", json!({}));
        verb_same("brief", json!({"where": "me", "full": true}), "oracle_brief", json!({"full": true}));
        verb_same("brief", json!({"where": "#oracle"}), "oracle_room_state", json!({"room": "oracle"}));
        verb_refuses("brief", json!({"where": "me", "colour": "red"}), "brief does not take colour");
        verb_same("brief", json!({"where": "world"}), "oracle_brief", json!({"where": "world"}));
    }

    #[test]
    fn verb_read_is_oracle_read_get_or_thread() {
        verb_same("read", json!({"where": "oracle", "limit": 5}), "oracle_read", json!({"room": "oracle", "limit": 5}));
        verb_same("read", json!({"where": "OBS-oracle-aa"}), "oracle_get", json!({"id": "OBS-oracle-aa"}));
        verb_same("read", json!({"where": "OBS-oracle-aa", "thread": true}), "oracle_thread", json!({"id": "OBS-oracle-aa"}));
        verb_refuses("read", json!({"where": "oracle", "room": "oracle"}), "read does not take room");
        verb_refuses("read", json!({"where": "oracle", "since": "6h"}), "read does not take since yet");
    }

    #[test]
    fn verb_find_is_oracle_recall_or_the_cortex() {
        verb_same("find", json!({"q": "a record", "limit": 3}), "oracle_recall", json!({"query": "a record", "limit": 3}));
        verb_same("find", json!({"q": "a record", "where": "oracle"}), "oracle_recall", json!({"query": "a record", "room": "oracle"}));
        verb_same("find", json!({"q": "oracle", "kind": "nodes"}), "oracle_cortex", json!({"op": "nodes", "q": "oracle"}));
        verb_same("find", json!({"q": "oracle", "kind": "entity"}), "oracle_cortex", json!({"op": "entity", "q": "oracle"}));
        verb_refuses("find", json!({"q": "x", "query": "x"}), "find does not take query");
    }

    #[test]
    fn verb_write_is_say_remember_task_state_or_mark() {
        verb_same("write", json!({"where": "oracle", "text": "hello @emmy", "intent": "note"}), "oracle_say", json!({"room": "oracle", "text": "hello @emmy", "intent": "note"}));
        verb_same("write", json!({"text": "keep budgets", "kind": "remember", "remember_kind": "decision"}), "oracle_remember", json!({"text": "keep budgets", "kind": "decision"}));
        verb_same("write", json!({"kind": "task", "subject": "budgets", "state": "noted", "text": "landed"}), "oracle_task", json!({"subject": "budgets", "state": "noted", "note": "landed"}));
        verb_same("write", json!({"where": "oracle", "kind": "state", "set": ["current=budgets"], "text": "moved"}), "oracle_room_state", json!({"room": "oracle", "set": ["current=budgets"], "note": "moved"}));
        verb_same("write", json!({"kind": "mark", "observation": "OBS-oracle-aa", "quote": "a record", "labels": ["Claim"], "links": ["about:OBS-oracle-aa"], "text": "why"}),
            "oracle_mark", json!({"observation": "OBS-oracle-aa", "quote": "a record", "labels": ["Claim"], "links": ["about:OBS-oracle-aa"], "note": "why"}));
        verb_refuses("write", json!({"where": "oracle", "text": "x", "room": "oracle"}), "write does not take room");
        verb_refuses("write", json!({"text": "x"}), "write takes where");
    }

    #[test]
    fn verb_settle_is_oracle_react_or_oracle_verdict() {
        verb_same("settle", json!({"id": "OBS-oracle-aa", "as": "ack", "room": "oracle"}), "oracle_react", json!({"room": "oracle", "id": "OBS-oracle-aa", "mark": "ack"}));
        // Without a room the record's own room is found from its id.
        verb_same("settle", json!({"id": "OBS-oracle-aa", "as": "stuck", "why": "waiting on CI"}), "oracle_react", json!({"room": "oracle", "id": "OBS-oracle-aa", "mark": "stuck", "why": "waiting on CI"}));
        verb_same("settle", json!({"id": "OBS-oracle-aa", "as": "approve", "why": "right"}), "oracle_verdict", json!({"target": "OBS-oracle-aa", "verdict": "approve", "text": "right"}));
        verb_refuses("settle", json!({"id": "OBS-oracle-aa", "as": "ack", "mark": "ack"}), "settle does not take mark");
        verb_refuses("settle", json!({"id": "OBS-oracle-aa", "as": "maybe"}), "settle as is one of");
    }

    #[test]
    fn every_existing_name_still_answers_beside_the_verbs() {
        let f = verb_fake();
        let mut names: Vec<String> = tools(true).as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).filter(|n| !VERBS.contains(&n.as_str())).collect();
        names.sort();
        assert!(names.len() >= 19 + 18, "{names:?}");
        for n in &names {
            let r = rpc(&f, "tools/call", json!({"name": n, "arguments": {}}), true);
            assert!(r["result"].is_object(), "{n}: {r}");
            assert!(!r["result"]["content"][0]["text"].as_str().unwrap_or("").contains("unknown tool"), "{n} still answers: {r}");
        }
        assert!(text(&rpc(&f, "tools/call", json!({"name": "oracle_nonesuch", "arguments": {}}), false)).contains("unknown tool"), "the negative control");
        let listed: Vec<Value> = tools(false).as_array().unwrap().iter().filter(|t| VERBS.contains(&t["name"].as_str().unwrap())).cloned().collect();
        assert_eq!(listed.len(), 5);
        for t in &listed {
            assert!(t["inputSchema"]["properties"].as_object().unwrap().values().all(|v| v.is_object()), "{}", t["name"]);
        }
    }

    // Rocky's live pass, 1 Nov 2026 07:15Z: `oracle_task owner=rocky limit=5` answered 129 KB, all 727 heads.
    #[test]
    fn the_task_list_keeps_its_limit_newest_touched_first_and_says_what_it_cut() {
        let note = "n".repeat(200);
        let heads: Vec<Value> = (0..120).map(|i| json!({"subject": format!("s{i:03}"), "owner": "rocky", "state": "noted", "next_touch": null,
            "channel": "board", "id": format!("OBS-board-{i:03}"), "note": note, "at_ms": 1_790_000_000_000i64 + (i * 7 % 120) * 1000})).collect();
        let f = Fake::new(json!({"/states": {"heads": heads}}));
        let r = rpc(&f, "tools/call", json!({"name": "oracle_task", "arguments": {"owner": "rocky", "limit": 5}}), false);
        let v = &r["result"]["structuredContent"];
        let at: Vec<i64> = v["heads"].as_array().unwrap().iter().map(|h| h["at_ms"].as_i64().unwrap()).collect();
        assert_eq!(at, (115..120).rev().map(|s| 1_790_000_000_000i64 + s * 1000).collect::<Vec<_>>(), "the five touched last, newest first");
        assert_eq!(v["budget"], json!({"rows": 5, "cut": 115, "more": "offset: 5 for the next 5"}));
        assert_eq!((v["total"].as_u64(), v["owner"].as_str()), (Some(120), Some("rocky")));
        assert!(text(&r).ends_with("+ 115 more heads not shown (5 of 120, newest-touched first): offset: 5 for the next 5"), "{}", text(&r));
        assert!(r.to_string().len() < 4_000, "{} bytes", r.to_string().len());
        assert!(f.gets.borrow().iter().any(|g| g == "/states?owner=rocky"));
        // Unset: fifty. The next page continues where the first stopped; the last page points back to the newest.
        let d = rpc(&f, "tools/call", json!({"name": "oracle_task", "arguments": {}}), false);
        assert_eq!((d["result"]["structuredContent"]["heads"].as_array().map(Vec::len), d["result"]["structuredContent"]["budget"]["cut"].as_u64()), (Some(50), Some(70)));
        let next = rpc(&f, "tools/call", json!({"name": "oracle_task", "arguments": {"owner": "rocky", "limit": 5, "offset": 5}}), false);
        assert_eq!(next["result"]["structuredContent"]["heads"][0]["at_ms"], 1_790_000_000_000i64 + 114_000);
        let last = rpc(&f, "tools/call", json!({"name": "oracle_task", "arguments": {"owner": "rocky", "limit": 5, "offset": 118}}), false);
        assert_eq!(last["result"]["structuredContent"]["budget"], json!({"rows": 2, "cut": 118, "more": "offset: 0 from the newest"}));
        // Everything fits: nothing cut, nothing said.
        let small = Fake::new(json!({"/states": {"heads": [{"subject": "a", "state": "open", "owner": "rocky", "at_ms": 1}]}}));
        let s = rpc(&small, "tools/call", json!({"name": "oracle_task", "arguments": {}}), false);
        assert_eq!(s["result"]["structuredContent"]["budget"], json!({"rows": 1, "cut": 0, "more": null}));
        assert!(!text(&s).contains("more heads"));
    }

    // Rocky's live pass, 1 Nov 2026 07:15Z: `oracle_cortex op=nodes q=oracle top=5` answered 11 nodes.
    #[test]
    fn cortex_nodes_keep_the_top_of_the_ranked_list_and_count_the_rest() {
        let nodes: Vec<Value> = (0..11).map(|i| json!({"nid": format!("n{i:02}aaaaaaaaaaaa"), "kind": "concept", "title": format!("oracle {i}"), "readings": 11 - i})).collect();
        let f = Fake::new(json!({"/cortex": {"nodes": nodes, "count": 11, "approved_observations": 0, "approved_observations_total": 40}}));
        let text = call(&f, "oracle_cortex", &json!({"op": "nodes", "q": "oracle", "top": 5})).unwrap();
        assert!(f.gets.borrow().iter().any(|p| p.starts_with("/cortex?") && p.contains("top=5")), "top is asked of the server: {:?}", f.gets.borrow());
        let listed: Vec<&str> = text.lines().filter(|l| l.contains(" concept ")).collect();
        assert_eq!(listed.len(), 5, "an older server that ignores top is cut here: {text}");
        assert!(listed[0].contains("oracle 0") && listed[4].contains("oracle 4"), "the first five of the ranked list: {text}");
        assert!(text.starts_with("11 nodes") && text.contains("top 5 shown, 6 more: raise top"), "{text}");
        // A server that already cut: the same answer, cut once.
        let mut v = json!({"nodes": [{"nid": "a"}, {"nid": "b"}], "count": 9});
        cortex_keep_top(&mut v, 2);
        cortex_keep_top(&mut v, 2);
        assert_eq!((v["count"].clone(), v["shown"].clone(), v["cut"].clone()), (json!(9), json!(2), json!(7)));
        // Without top nothing is cut and nothing is said.
        let all = call(&f, "oracle_cortex", &json!({"op": "nodes", "q": "oracle"})).unwrap();
        assert_eq!(all.lines().filter(|l| l.contains(" concept ")).count(), 11);
        assert!(!all.contains("more: raise top"));
    }
}

#[cfg(test)]
mod my_asks_tests {
    use super::*;

    const NOW: i64 = 1_792_000_000_000;
    fn asks() -> Value {
        json!({"im_waiting_on": [
            {"id": "OBS-a-new", "room": "rocky", "state": "open", "created_ms": NOW - 3_600_000, "held_by": ["blaze"], "card": "pick a design", "ask": "decide"},
            {"id": "OBS-b-old", "room": "dot", "state": "open", "created_ms": NOW - 3 * 86_400_000, "held_by": ["emmy"], "card": "review #44", "ask": "review"},
            {"id": "OBS-c-ans2", "room": "dot", "state": "answered", "created_ms": NOW - 2 * 86_400_000, "held_by": ["emmy"], "card": "later", "ask": "review"},
            {"id": "OBS-d-ans1", "room": "axxis", "state": "answered", "created_ms": NOW - 5 * 86_400_000, "held_by": [], "card": "earlier", "ask": "reply"},
        ]})
    }

    #[test]
    fn answered_oldest_first_then_old_then_open() {
        let m = my_asks(&asks(), NOW);
        let ids = |k: &str| m[k].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(ids("answered"), ["OBS-d-ans1", "OBS-c-ans2"]);
        assert_eq!(ids("old"), ["OBS-b-old"]);
        assert_eq!(ids("open"), ["OBS-a-new"]);
        assert_eq!(my_asks_nudge(&m).unwrap(), "· mine: 2 open, 2 answered, close them: ox asks --mine");
    }

    #[test]
    fn the_brief_shows_my_asks_and_none_means_no_section_or_footer() {
        let m = my_asks(&asks(), NOW);
        let b = json!({"seat": "rocky", "now_ms": NOW, "now": "x", "identity": "", "state": {}, "asks": {"rows": []}, "mine": m});
        let text = brief_render(&b, false);
        let at = text.find("MY ASKS    2 open, 2 answered").expect("the brief has MY ASKS");
        let after = &text[at..];
        let (a, o, f) = (after.find("answered, close them · 2").unwrap(), after.find("open past 24 h, still needed? · 1").unwrap(), after.find("  open · 1").unwrap());
        assert!(a < o && o < f, "answered, then old, then open");
        assert!(after.contains("OBS-d-ans1") && !after.contains("nobody named"), "a row with no holder says nothing, not 'nobody named' on every line");
        let empty = my_asks(&json!({"im_waiting_on": []}), NOW);
        assert_eq!(my_asks_text(&empty, Some(5)), "");
        assert!(my_asks_nudge(&empty).is_none());
        // the count answer prints the same line from numbers alone, and an answer that is not one is told apart
        let c = json!({"schema": "asks-count/1", "im_waiting_on": 4, "im_waiting_on_answered": 2, "waiting_on_me": 9});
        assert_eq!(my_asks_count_nudge(&c), Some(Some("· mine: 2 open, 2 answered, close them: ox asks --mine".into())));
        assert_eq!(my_asks_count_nudge(&json!({"schema": "asks-count/1", "im_waiting_on": 0, "im_waiting_on_answered": 0})), Some(None));
        assert_eq!(my_asks_count_nudge(&json!({"im_waiting_on": []})), None, "a full /asks answer is not a count answer");
    }

    #[test]
    fn a_long_list_is_cut_and_says_how_many_it_left_out() {
        let rows: Vec<Value> = (0..8).map(|i| json!({"id": format!("OBS-x-{i}"), "room": "r", "state": "answered", "created_ms": NOW - i, "held_by": ["blaze"], "card": "c"})).collect();
        let t = my_asks_text(&my_asks(&json!({"im_waiting_on": rows}), NOW), Some(5));
        assert!(t.contains("… 3 more: ox asks --mine"), "{t}");
    }
}

#[cfg(test)]
mod brief_fold_tests {
    use super::{brief_render, fold_lines, RESUME_BRIEF_LINES};
    use serde_json::json;
    fn b() -> serde_json::Value {
        let resume = (1..=30).map(|i| format!("resume line {i}")).collect::<Vec<_>>().join("\n");
        json!({"seat": "rocky", "now": "2026-10-20T15:00:00.000Z", "now_ms": 1_792_508_400_000i64, "identity": "key",
               "identity_head": {"id": "OBS-id", "at_ms": 1_792_508_000_000i64, "head": {"name": "Rocky", "purpose": "build"},
                                 "persona": {"id": "OBS-persona", "text": "PERSONA WHOLE TEXT\nline two"}},
               "resume": {"id": "OBS-resume", "written": "today", "whole": true, "text": resume},
               "canon": {"id": "OBS-canon", "created_ms": 1_792_508_000_000i64, "canon": {"version": 4, "rules": [{"kind": "law", "text": "LAW RULE TEXT", "id": "L1"}], "open": []}}})
    }
    #[test]
    fn the_brief_folds_the_persona_the_resume_and_the_canon_unless_full() {
        let folded = brief_render(&b(), false);
        assert!(!folded.contains("PERSONA WHOLE TEXT"), "persona folded:\n{folded}");
        assert!(folded.contains("persona   OBS-persona (verified, 27 chars; whole: ox identity"), "{folded}");
        assert!(folded.contains("resume line 12") && !folded.contains("resume line 13"), "{folded}");
        assert!(folded.contains("… 18 more lines; whole: ox get OBS-resume"), "{folded}");
        assert!(!folded.contains("LAW RULE TEXT") && folded.contains("ox canon --kind law"), "{folded}");
        let whole = brief_render(&b(), true);
        assert!(whole.contains("PERSONA WHOLE TEXT") && whole.contains("resume line 30") && whole.contains("LAW RULE TEXT"), "{whole}");
        assert_eq!(fold_lines("a\nb", RESUME_BRIEF_LINES, "x"), "a\nb", "a short note is not folded");
    }
}

#[cfg(test)]
mod lean_wake_tests {
    use super::*;
    #[test]
    fn a_wake_lists_the_newest_asks_and_counts_the_rest_by_room() {
        let rows: Vec<Value> = (0..30).map(|i| json!({"id": format!("OBS-x-{i}"), "room": if i % 3 == 0 { "astra" } else { "dot" }, "author": "emmy", "created_ms": 0, "ask": "reply", "state": "open"})).collect();
        let text = asks_render(&json!({"open": rows}), 0);
        assert_eq!(text.matches("  OBS-x-").count(), ASKS_SHOWN, "only the newest are listed");
        assert!(text.contains("+ 18 more: #dot 12 · #astra 6 — ox asks --mine lists every one"), "{text}");
        let few = asks_render(&json!({"open": [json!({"id": "OBS-y", "room": "dot", "author": "e", "created_ms": 0, "ask": "reply", "state": "open"})]}), 0);
        assert!(!few.contains("more:"), "nothing is counted when everything is listed");
    }
}

#[cfg(test)]
mod cortex_recovery_tests {
    #[test]
    fn node_text_keeps_source_interpretation_review_and_links_distinct() {
        let v = serde_json::json!({"node":{
            "kind":"observation","nid":"node","title":"A plan", "minted_by":"OBS-source",
            "quote":"Build our own agent", "source":{"message":"chat/msg","chat":"chat","start":0,"end":18},
            "speaker":"Blaze","drafted_by":"rocky","note":"Proposed, not built", "labels":["Plan"],"intents":["Build"],"project":"AXXIS",
            "verdict":{"verdict":"approve","by":"blaze","signed_by":"brain","message":"OBS-verdict"},
            "out":[{"kind":"depends-on","to":"other-node","message":"OBS-edge"}]
        },"marks":[]});
        let text = super::cortex_text("node", &v);
        for expected in ["source quote: Build our own agent", "drafter interpretation (rocky): Proposed, not built", "chat/msg:0-18", "attributed speaker Blaze", "project: AXXIS", "signed by brain", "state: not recorded", "depends-on other-node", "OBS-edge"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }
}

#[cfg(test)]
mod blocked_rule_tests {
    use super::*;
    const NOW: i64 = 1_793_340_000_000; // 2026-10-30T04:00Z

    fn head(state: &str, next: Option<&str>) -> Value {
        let mut c = json!({"subject": "s", "state": state, "owner": "rocky"});
        if let Some(n) = next {
            c["next_touch"] = json!(n);
        }
        c
    }

    #[test]
    fn a_blocked_head_names_who_what_how_and_when() {
        let mut c = head("blocked", Some("2026-11-01"));
        blocked_rule(&mut c, &["blaze".into()], Some("deleting the OAuth client"), Some("the client still shows in the Google console"), NOW).unwrap();
        assert_eq!(c["needs"], json!(["blaze"]));
        assert_eq!(c["blocks"], "deleting the OAuth client");
        assert!(c["check"].as_str().unwrap().contains("console"));
    }

    #[test]
    fn a_blocked_head_without_any_part_is_refused() {
        let ok = |c: &mut Value| blocked_rule(c, &["emmy".into()], Some("land"), Some("review open"), NOW);
        assert!(blocked_rule(&mut head("blocked", Some("2026-11-01")), &[], Some("land"), Some("review open"), NOW).is_err(), "no needs");
        assert!(blocked_rule(&mut head("blocked", Some("2026-11-01")), &["  ".into()], Some("land"), Some("review open"), NOW).is_err(), "blank needs");
        assert!(blocked_rule(&mut head("blocked", Some("2026-11-01")), &["emmy".into()], None, Some("review open"), NOW).is_err(), "no blocks");
        assert!(blocked_rule(&mut head("blocked", Some("2026-11-01")), &["emmy".into()], Some("land"), None, NOW).is_err(), "no check");
        assert!(ok(&mut head("blocked", None)).is_err(), "no re-check date");
        assert!(ok(&mut head("blocked", Some("someday"))).is_err(), "unreadable date");
        assert!(ok(&mut head("blocked", Some("2026-11-05"))).is_err(), "more than three days away");
        assert!(ok(&mut head("blocked", Some("2026-10-29"))).is_err(), "already past");
        assert!(ok(&mut head("blocked", Some("2026-10-30"))).is_ok(), "today is fine");
        assert!(ok(&mut head("blocked", Some("2026-11-02T03:00:00Z"))).is_ok(), "inside three days");
    }

    #[test]
    fn other_states_pass_through_and_refuse_blocker_fields() {
        let mut c = head("running", Some("2026-12-01"));
        blocked_rule(&mut c, &[], None, None, NOW).unwrap();
        assert!(c.get("needs").is_none());
        assert!(blocked_rule(&mut head("done", None), &["blaze".into()], None, None, NOW).is_err());
    }
}
