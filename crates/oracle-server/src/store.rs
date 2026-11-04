//! The store: every Oracle rule that touches data lives here, synchronously, over SQLite.
//!
//! HTTP is a thin layer on top (`http.rs`). Anything worth testing is testable here with
//! an in-memory database and an injected clock.

use oracle_proto::{Envelope, KeyAction, KeyEvent, Kind, ProtoError, MAX_SKEW_MS};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub type Pool = r2d2::Pool<SqliteConnectionManager>;

/// The fold counts at most this many waiting items per room.
pub const PENDING_CAP: i64 = 1000;
/// Devices per seat. Enough for a person; small enough that verifying against all is nothing.
pub const MAX_KEYS_PER_SEAT: usize = 8;

/// Cross-seat reads (docs/ROOM-INDEX.md, Emmy's Q1). A *gateway* is a named service identity (the web door reading for its viewers) allowed to read other
/// seats' `asks`, `asked` and `unread`; nobody becomes one by asking, only the owner-side CLI (`oracle gateway add`) writes this table. Every cross-seat
/// request leaves an audit row whether or not it was allowed.
const GATEWAY_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS gateways (
  handle     TEXT PRIMARY KEY,
  routes     TEXT NOT NULL,
  created_ms INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS access_audit (
  seq       INTEGER PRIMARY KEY AUTOINCREMENT,
  at_ms     INTEGER NOT NULL,
  caller    TEXT NOT NULL,
  route     TEXT NOT NULL,
  target    TEXT NOT NULL,
  permitted INTEGER NOT NULL,   -- what the policy says
  enforced  INTEGER NOT NULL    -- whether a refusal was actually returned
);
CREATE INDEX IF NOT EXISTS audit_at ON access_audit(at_ms);
"#;

/// One cross-seat read: who asked for whose, whether policy allowed it, and whether a refusal was actually returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub at_ms: i64,
    pub caller: String,
    pub route: String,
    pub target: String,
    pub permitted: bool,
    pub enforced: bool,
}

/// The routes a gateway may be allowed to read across seats.
pub const CROSS_SEAT_ROUTES: [&str; 3] = ["asks", "asked", "unread"];
/// The gateway route that lets a seat attest an approval the owner gave in a page (docs/CORTEX.md "Cited by"): not a
/// read route; only the owner-side CLI grants it, and removing it withdraws every attestation the seat signed.
pub const ATTEST_ROUTE: &str = "attest";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS observations (
  seq        INTEGER PRIMARY KEY AUTOINCREMENT,
  id         TEXT NOT NULL UNIQUE,
  channel    TEXT NOT NULL,
  author     TEXT NOT NULL,
  kind       TEXT NOT NULL,
  content    TEXT NOT NULL,
  cargo      TEXT,
  tags_json  TEXT NOT NULL,          -- the SIGNED tags, exactly as sent
  created_ms INTEGER NOT NULL,       -- server clock
  ts_ms      INTEGER NOT NULL,       -- signer clock
  nonce      TEXT NOT NULL,
  client_op  TEXT NOT NULL UNIQUE,
  pubkey     TEXT NOT NULL,
  sig        TEXT NOT NULL,
  envelope   INTEGER NOT NULL DEFAULT 2,
  news       INTEGER NOT NULL DEFAULT 1   -- 0 = imported history: kept, searchable, never folded
);
-- Covering: the fold's per-room counts and scans never touch the table.
CREATE INDEX IF NOT EXISTS obs_channel_seq ON observations(channel, seq, author, news);
CREATE INDEX IF NOT EXISTS obs_created     ON observations(created_ms, news);
CREATE INDEX IF NOT EXISTS obs_author_seq  ON observations(author, seq);
-- What each mind signed (GET /minds): imports (envelope 1: claude.ai exports as 'assistant'/'human', the graph Oracle) are not a mind's statements.
CREATE INDEX IF NOT EXISTS obs_native_author ON observations(author, seq) WHERE envelope=2;
-- A record's code (the last twelve hex of its id, the first twelve of sha256 of its signature) names it alone (Blaze, 31 Oct 2026).
CREATE INDEX IF NOT EXISTS obs_code        ON observations(substr(id, -12));

CREATE TABLE IF NOT EXISTS legacy_sources (
  obs_id TEXT NOT NULL,
  digest TEXT NOT NULL,
  payload TEXT NOT NULL,
  recorded_ms INTEGER NOT NULL,
  PRIMARY KEY(obs_id, digest)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS tags (
  name    TEXT NOT NULL,
  obs_seq INTEGER NOT NULL,
  PRIMARY KEY (name, obs_seq)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS tags_obs ON tags(obs_seq);

-- `cargo` holds the text fields of the structured payload (fts_cargo_v1, see `cargo_text_fields`): lexical only.
CREATE VIRTUAL TABLE IF NOT EXISTS obs_fts USING fts5(
  content, tags, cargo, tokenize = 'porter unicode61'
);
-- Small facts about the store itself (migration flags and progress); not a projection, never rebuilt.
CREATE TABLE IF NOT EXISTS store_meta (k TEXT PRIMARY KEY, v TEXT NOT NULL) WITHOUT ROWID;

-- A seat may hold several keys — one per device it speaks from (a CLI, a web gateway).
-- `role` belongs to the KEY: only the key the Oracle was initialised with is 'owner' and may
-- change the registry. Every key added later is 'seat', even under the owner's handle.
CREATE TABLE IF NOT EXISTS keys (
  handle     TEXT NOT NULL,
  pubkey     TEXT NOT NULL,
  role       TEXT NOT NULL,          -- owner | seat
  created_ms INTEGER NOT NULL,
  revoked_ms INTEGER,
  PRIMARY KEY (handle, pubkey)
);
CREATE TABLE IF NOT EXISTS key_events (
  seq        INTEGER PRIMARY KEY AUTOINCREMENT,
  event_json TEXT NOT NULL,
  nonce      TEXT NOT NULL UNIQUE,
  created_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS seat_scope (
  id         INTEGER PRIMARY KEY CHECK (id = 1),
  table_json TEXT NOT NULL,          -- seatrooms::Table of the newest valid owner-signed seat-rooms/1 record
  source_id  TEXT NOT NULL,
  updated_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS private_rooms (
  id         INTEGER PRIMARY KEY CHECK (id = 1),
  table_json TEXT NOT NULL,          -- privaterooms::Table of the newest valid owner-signed private-rooms/1 record
  source_id  TEXT NOT NULL,
  updated_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS retention (
  id         INTEGER PRIMARY KEY CHECK (id = 1),
  table_json TEXT NOT NULL,          -- retention::Table of the newest valid owner-signed retention/1 record
  source_id  TEXT NOT NULL,
  updated_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tokens (
  hash       TEXT PRIMARY KEY,
  handle     TEXT NOT NULL,
  scope      TEXT NOT NULL,          -- read | write | admin
  label      TEXT NOT NULL DEFAULT '',
  created_ms INTEGER NOT NULL,
  revoked_ms INTEGER
);
CREATE TABLE IF NOT EXISTS cursors (
  agent    TEXT NOT NULL,
  channel  TEXT NOT NULL,
  mark_seq INTEGER NOT NULL,
  PRIMARY KEY (agent, channel)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS nonces (
  nonce      TEXT PRIMARY KEY,
  expires_ms INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS nonces_exp ON nonces(expires_ms);
-- One row per room: lets the fold find rooms with news without touching observations.
CREATE TABLE IF NOT EXISTS channel_heads (
  channel TEXT PRIMARY KEY,
  max_seq INTEGER NOT NULL,
  total   INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS state_heads (
  subject    TEXT PRIMARY KEY,
  obs_seq    INTEGER NOT NULL,
  owner      TEXT NOT NULL,
  state      TEXT NOT NULL,
  next_touch TEXT
) WITHOUT ROWID;
"#;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("unauthorized: {0}")]
    Unauthorized(&'static str),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("not found")]
    NotFound,
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// A limit (publicseats.rs) is full; try again after `retry_after_ms`.
    #[error("limited: {why}")]
    Limited { why: String, retry_after_ms: i64 },
    #[error("internal: {0}")]
    Internal(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        // A busy or locked database is the caller's to retry, not an internal fault: nothing was stored.
        if matches!(e.sqlite_error_code(), Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)) {
            return StoreError::Unavailable("the database is busy; nothing was stored, retry".into());
        }
        StoreError::Internal(e.to_string())
    }
}
impl From<r2d2::Error> for StoreError {
    fn from(e: r2d2::Error) -> Self {
        StoreError::Internal(e.to_string())
    }
}
impl From<ProtoError> for StoreError {
    fn from(e: ProtoError) -> Self {
        match e {
            ProtoError::BadSignature => StoreError::Unauthorized("signature does not verify"),
            ProtoError::Stale => StoreError::Unauthorized("timestamp outside the accepted window"),
            other => StoreError::Rejected(other.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Read,
    Write,
    Admin,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "read" => Some(Scope::Read),
            "write" => Some(Scope::Write),
            "admin" => Some(Scope::Admin),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Write => "write",
            Scope::Admin => "admin",
        }
    }
}

/// Who a bearer token is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub handle: String,
    pub scope: Scope,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Observation {
    pub seq: i64,
    pub id: String,
    pub channel: String,
    pub author: String,
    pub kind: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cargo: Option<String>,
    pub tags: Vec<String>,
    /// When Oracle received it (server clock).
    pub created_ms: i64,
    /// When its author wrote it, when that was earlier (a write queued offline). Signed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authored_ms: Option<i64>,
    pub pubkey: String,
    pub sig: String,
    pub envelope: i64,
}

#[derive(Debug, Serialize)]
pub struct WriteReceipt {
    pub id: String,
    pub seq: i64,
    /// True when this `client_op` had already been written: the original is returned.
    pub duplicate: bool,
    pub signed: bool,
}

#[derive(Debug, Serialize)]
pub struct FoldItem {
    #[serde(flatten)]
    pub obs: Observation,
    pub for_me: bool,
}

#[derive(Debug, Serialize)]
pub struct Fold {
    pub agent: String,
    pub items: Vec<FoldItem>,
    /// Waiting per channel BEFORE the limit was applied — the denominator.
    pub pending: BTreeMap<String, i64>,
    /// True when some room has more than `PENDING_CAP` waiting: its count reads as the cap.
    pub pending_capped: bool,
    pub returned: usize,
    pub has_more: bool,
    /// Pass back to `ack` to advance exactly past what was returned.
    pub next_marks: BTreeMap<String, i64>,
    /// Addressed to this seat (`to:` or `mention:`) and still past its cursor, newest first, at most
    /// `UNREAD_FOR_ME_MAX`, whether or not the page above reached them (Jobs' audit, 2026-10-10: 1,364 new paged
    /// oldest-first, so what was written to him today never showed).
    pub unread_for_me: Vec<Observation>,
}

/// How many unread messages addressed to a seat a fold lists newest first.
pub const UNREAD_FOR_ME_MAX: usize = 20;

/// How many links a thread walk follows, up or down: a guard against a loop, not a limit any real
/// exchange reaches (the deepest live thread on 2026-10-16 is far below it).
pub const THREAD_HOPS: usize = 64;
/// The relation tags that make one record a resolver of another (`resolvers_of`, `resolvers_of_all`).
pub const RESOLVER_PREFIXES: [&str; 7] = ["closes:", "supersedes:", "archives:", "re:", "reply:", "in_reply_to:", "annotates:"];
thread_local! {
    // per thread, not per process: a test binary runs its tests at once, and a global count let one
    // test's loads land in another's before/after (tests/asks.rs failed 2 runs in 3 on that)
    static LOADS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static ROWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
/// How many members a thread walk collects before it keeps the newest `max` of them.
pub const THREAD_SCAN: usize = 2000;

#[derive(Debug, Serialize)]
pub struct SearchHit {
    #[serde(flatten)]
    pub obs: Observation,
    pub score: f64,
    /// Where the words matched (`match_in`, `match_pointer`, `match_span`); absent for a hit that is meaning-only.
    #[serde(flatten)]
    pub at: Option<MatchAt>,
    /// Other records with the same body (an import read twice, a turn copied into #decision): one hit, its copies named (Round 50).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub same_as: Vec<String>,
    /// Records of the same template by the same machine author in the same room ([`template_key`]: a heartbeat, a receipt,
    /// a status line with only its times and numbers changed), folded into this hit, newest-ranked first (Blaze, 31 Oct
    /// 2026: Hypatia's heartbeat receipts took four places of ten).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub like: Vec<String>,
    /// The record that says it replaces this one (a `supersedes:<this id>` tag); such a hit ranks below its
    /// successor (Grok's memo: a superseded fact must not rank as high as the fact
    /// that replaced it; the signed row stays).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// The hit's links, as ids (Round 50 ask 7): what it answers, cites and corrects, and what replied to it, answered it
    /// and cites it. Omitted when it has none.
    #[serde(skip_serializing_if = "Edges::is_empty")]
    pub edges: Edges,
    /// For a Blaze-approved cortex observation (docs/CORTEX.md, Brain cases): what it is in the hand-built graph, its
    /// labels, the note, and its live typed links out and in (generates, refines, part-of, ... to nodes or to Oracle
    /// records). The understanding comes back with its lineage (Blaze, 25 Oct: "every new case should compound").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brain: Option<serde_json::Value>,
    /// Who among Blaze-approved observation nodes cites this record (docs/CORTEX.md "Cited by"): set once per response
    /// by [`enrich_cited_by`]; absent for a measured zero. Ranking does not read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cited_by: Option<serde_json::Value>,
    /// The row (docs/SEARCH.md, S1 2026-10-21): what the hit is, in the words a person and an agent both read.
    /// `class`: message · head · page · claim · file · import · synthetic. `title` and `snippet` are clean text (the
    /// CLAUDE-EXPORT header and export UI noise left out; the signed `content` is untouched).
    pub class: &'static str,
    pub title: String,
    pub snippet: String,
    pub thread: Thread,
    /// `person` when the author holds an owner key, else `model`.
    pub author_kind: &'static str,
    /// #graph-synthesis, an id with STAGED or SYN-, or a `synthetic` tag: ranks under every first-hand hit.
    pub synthetic: bool,
    /// How many of the query's content words the record holds, of how many ([`content_words`]): the relevance a reader
    /// can check, where the fused score is flat (0.02 to 0.03 on every query). Absent when the query has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub words: Option<[usize; 2]>,
    /// A Blaze-approved cortex observation (verified owner or attested, never a cargo field).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub approved: bool,
    /// The `valid-until:` the record was written with (docs/SEARCH.md "Valid in time"), as ISO-8601 UTC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<String>,
    /// When it stopped holding, as of the search's moment: its successor was written, or its valid-until passed (the earlier).
    /// Absent while it holds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_since: Option<String>,
}

/// Where a hit stands in its thread, from its edges: how many replied, whether a `closes:` answered it, what it answers.
#[derive(Debug, Default, Clone, Serialize, PartialEq)]
pub struct Thread {
    pub replies: usize,
    pub answered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

/// Export UI noise that is not text (Noor, Round 50 ask 2).
pub const NOISE_LINES: [&str; 2] = ["This block is not supported on your current device", "[CLAUDE-EXPORT"];

/// The text a person reads: the CLAUDE-EXPORT header line (it names the import, not the turn) and export UI noise
/// lines left out, whitespace collapsed within lines, blank lines dropped. The record's `content` is never changed.
pub fn clean_text(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !NOISE_LINES.iter().any(|n| l.contains(n)))
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A synthetic record: a paraphrase with no source (the #graph-synthesis room, STAGED or SYN- ids, a `synthetic` tag).
pub fn is_synthetic(channel: &str, id: &str, tags: &[String]) -> bool {
    channel == "graph-synthesis" || id.contains("STAGED") || id.starts_with("SYN-") || id.contains("-SYN-") || tags.iter().any(|t| t == "synthetic")
}

/// A graph node: a record of the 30 Sep 2026 graph import's node table (#graph-records: a handle, a room, an entity,
/// a label), structure rather than anything said. Its words match a search (the node "rocky" holds the word rocky) but
/// it is never the answer: class `node`, ranked under every first-hand hit with the synthetic ones.
/// The hand-built graph's structure in #cortex (docs/CORTEX.md): a `cortex:link`, a `cortex:verdict`, or a node that is
/// not an observation (a concept, an entity, a form). A room read folds it (Blaze, 31 Oct: reading #cortex gave "approved
/// by Blaze" and "about ranking" rows and no reading); the readings it joins carry what it says.
pub fn is_scaffold(o: &Observation) -> bool {
    if o.channel != "cortex" {
        return false;
    }
    if o.tags.iter().any(|t| t == "cortex:link" || t == "cortex:verdict") {
        return true;
    }
    o.tags.iter().any(|t| t == "cortex:node")
        && o.cargo.as_deref().and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok()).and_then(|v| v["kind"].as_str().map(str::to_owned)).as_deref() != Some("observation")
}

pub fn is_node(channel: &str) -> bool {
    channel == "graph-records"
}

fn cargo_json(obs: &Observation) -> Option<serde_json::Value> {
    obs.cargo.as_deref().and_then(|c| serde_json::from_str(c).ok())
}

/// The class of a hit (docs/SEARCH.md).
pub fn class_of(obs: &Observation, synthetic: bool) -> &'static str {
    if synthetic {
        return "synthetic";
    }
    if is_node(&obs.channel) {
        return "node";
    }
    if obs.kind == "state" {
        return "head";
    }
    let schema = cargo_json(obs).and_then(|c| c["schema"].as_str().map(str::to_owned)).unwrap_or_default();
    if schema == "doc/1" || schema == "plan/1" {
        return "page";
    }
    if schema.starts_with("claim") || obs.tags.iter().any(|t| t == "claim" || t.starts_with("claim:")) {
        return "claim";
    }
    if obs.tags.iter().any(|t| t.starts_with("file:")) {
        return "file";
    }
    if obs.content.trim_start().starts_with("[CLAUDE-EXPORT") || obs.tags.iter().any(|t| t == "import" || t.starts_with("import:")) {
        return "import";
    }
    "message"
}

fn cut_chars(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if out.chars().count() < s.chars().count() {
        out.push('…');
    }
    out
}

/// The title: a head's subject and note, a page's title, else the first line of the clean text (120 characters).
pub fn title_of(obs: &Observation, class: &str, clean: &str) -> String {
    let first = clean.lines().next().unwrap_or("");
    match class {
        "head" => {
            let subject = cargo_json(obs).and_then(|c| c["subject"].as_str().map(str::to_owned)).unwrap_or_default();
            if subject.is_empty() { cut_chars(first, 120) } else { cut_chars(&format!("{subject} · {first}"), 120) }
        }
        "page" => cargo_json(obs).and_then(|c| c["title"].as_str().map(str::to_owned)).filter(|t| !t.trim().is_empty()).map(|t| cut_chars(&t, 120)).unwrap_or_else(|| cut_chars(first, 120)),
        _ => cut_chars(first, 120),
    }
}

/// The snippet: 240 characters of clean text around the matched words when the match is in the content, else its start.
pub fn snippet_of(obs: &Observation, clean: &str, at: &Option<MatchAt>) -> String {
    let matched = at.as_ref().filter(|a| a.match_in == "content").and_then(|a| a.match_span.as_ref())
        .and_then(|sp| obs.content.get(sp.start..sp.end)).map(|m| m.trim().to_lowercase()).filter(|m| !m.is_empty());
    let flat = clean.replace('\n', " ");
    let pos = matched.and_then(|m| flat.to_lowercase().find(&m));
    match pos {
        Some(p) if p > 80 => {
            let from = (0..=p).rev().find(|j| flat.is_char_boundary(*j) && p - j >= 80).unwrap_or(0);
            let from = flat[from..].find(char::is_whitespace).filter(|k| *k < 40).map_or(from, |k| from + k + 1);
            format!("… {}", cut_chars(&flat[from..], 240))
        }
        _ => cut_chars(&flat, 240),
    }
}

pub fn thread_of(e: &Edges) -> Thread {
    Thread { replies: e.replies.len(), answered: !e.answered_by.is_empty(), reply_to: e.reply_to.clone() }
}

impl SearchHit {
    /// A hit with its row filled from the record, its match and its edges.
    pub fn build(obs: Observation, score: f64, at: Option<MatchAt>, same_as: Vec<String>, superseded_by: Option<String>, edges: Edges, author_kind: &'static str) -> Self {
        let synthetic = is_synthetic(&obs.channel, &obs.id, &obs.tags);
        let class = class_of(&obs, synthetic);
        let clean = clean_text(&obs.content);
        let title = title_of(&obs, class, &clean);
        let snippet = snippet_of(&obs, &clean, &at);
        let thread = thread_of(&edges);
        let valid_until = valid_until_of(&obs.tags).map(oracle_proto::iso_ms);
        SearchHit { obs, score, at, same_as, like: vec![], superseded_by, edges, brain: None, cited_by: None, class, title, snippet, thread, author_kind, synthetic, words: None, approved: false, valid_until, invalid_since: None }
    }
}

/// One record's links (Round 50 ask 7). Outbound from its own tags; inbound from the tag index, newest last, at most
/// [`EDGE_MAX`] per list. A reaction (a mark: seen, go, ack …) is counted in `marks`, not listed as a reply; a reply that
/// closes the record is listed once, under `answered_by`.
#[derive(Debug, Default, Clone, Serialize, PartialEq)]
pub struct Edges {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub replies: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub answered_by: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cited_by: Vec<String>,
    #[serde(skip_serializing_if = "is_zero")]
    pub marks: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Edges {
    pub fn is_empty(&self) -> bool {
        *self == Edges::default()
    }
}

/// At most this many ids per edge list on a hit.
pub const EDGE_MAX: usize = 20;

/// A record's links, read from its tags and from the tags that name it.
/// person when the handle holds an owner key, else model. Reads on the connection the caller holds: a search
/// must not take a second one from the pool (the in-memory store has one connection, and a pool of eight
/// stalls under eight searches).
pub fn author_kind_on(c: &Connection, handle: &str) -> &'static str {
    c.query_row("SELECT 1 FROM keys WHERE handle=?1 AND role='owner' AND revoked_ms IS NULL LIMIT 1", params![handle], |_| Ok(()))
        .map(|_| "person")
        .unwrap_or("model")
}

pub fn edges_of(c: &Connection, obs: &Observation) -> rusqlite::Result<Edges> {
    let mut e = Edges::default();
    for t in &obs.tags {
        if let Some(x) = t.strip_prefix("reply:") {
            e.reply_to.get_or_insert_with(|| x.to_owned());
        } else if let Some(x) = t.strip_prefix("ref:") {
            if e.refs.len() < EDGE_MAX && !e.refs.iter().any(|r| r == x) {
                e.refs.push(x.to_owned());
            }
        } else if let Some(x) = t.strip_prefix("supersedes:") {
            if e.supersedes.len() < EDGE_MAX && !e.supersedes.iter().any(|r| r == x) {
                e.supersedes.push(x.to_owned());
            }
        }
    }
    let mut st = c.prepare_cached(
        "SELECT o.id, t.name, EXISTS(SELECT 1 FROM tags r WHERE r.obs_seq = o.seq AND r.name >= 'react:' AND r.name < 'react;') \
         FROM tags t JOIN observations o ON o.seq = t.obs_seq WHERE t.name IN (?1, ?2, ?3) ORDER BY o.seq DESC LIMIT 400",
    )?;
    let rows = st.query_map(
        params![format!("reply:{}", obs.id), format!("closes:{}", obs.id), format!("ref:{}", obs.id)],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)),
    )?;
    // Newest 400 links, then walked oldest first, so each list ends with the newest.
    let mut rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    rows.reverse();
    let mut replies = Vec::new();
    for (id, name, mark) in rows {
        let list = if name.starts_with("closes:") {
            &mut e.answered_by
        } else if name.starts_with("ref:") {
            &mut e.cited_by
        } else if mark {
            e.marks += 1;
            continue;
        } else {
            &mut replies
        };
        if !list.contains(&id) {
            list.push(id);
        }
    }
    replies.retain(|r| !e.answered_by.contains(r));
    e.replies = replies;
    for l in [&mut e.replies, &mut e.answered_by, &mut e.cited_by] {
        if l.len() > EDGE_MAX {
            l.drain(..l.len() - EDGE_MAX);
        }
    }
    Ok(e)
}

/// Below this nearest-meaning similarity, a query that no record answers word for word has no prior (Round 50 ask 6).
/// Calibrated 2026-10-11 on the live store: twelve topics never discussed here reached 0.27 to 0.38; every one of the
/// 94 frozen questions with a known answer has a record holding all its words, so the floor never decides them.
pub const PRIOR_FLOOR: f64 = 0.40;

/// Tags only an import may carry (see [`Store::write`]).
pub const ATTESTATION_TAGS: [&str; 2] = ["signed:ok", "cargo-signed:ok"];

/// What a superseded record's score is multiplied by.
pub const SUPERSEDED_WEIGHT: f64 = 0.5;

/// The newest record that supersedes `seq`, if any.
/// The newest record that says it replaces `seq`, and when it was written; with `as_of`, only one written by then (a fact
/// replaced later still held on that day).
fn successor_of(c: &Connection, seq: i64, as_of: Option<i64>) -> rusqlite::Result<Option<(String, i64)>> {
    c.prepare_cached(
        "SELECT o2.id, o2.created_ms FROM observations o JOIN tags t ON t.name = 'supersedes:' || o.id \
         JOIN observations o2 ON o2.seq = t.obs_seq WHERE o.seq = ?1 AND (?2 IS NULL OR o2.created_ms <= ?2) ORDER BY o2.seq DESC LIMIT 1",
    )?
    .query_row(params![seq, as_of], |r| Ok((r.get(0)?, r.get(1)?)))
    .optional()
}

/// The `valid-until:<moment>` a record was written with ([`oracle_proto::parse_when`]: unix ms, or a date through its end).
pub fn valid_until_of(tags: &[String]) -> Option<i64> {
    tags.iter().find_map(|t| t.strip_prefix("valid-until:")).and_then(oracle_proto::parse_when)
}

/// Facts whose validity ended by `clock` (their `valid-until` is earlier) rank as a superseded record does: the score times
/// SUPERSEDED_WEIGHT, the list re-sorted. Returns each one's end.
fn demote_expired(c: &Connection, ranked: &mut [(i64, f64)], clock: i64) -> rusqlite::Result<std::collections::HashMap<i64, i64>> {
    let mut st = c.prepare_cached("SELECT name FROM tags WHERE obs_seq=?1 AND name LIKE 'valid-until:%' LIMIT 1")?;
    let mut ended = std::collections::HashMap::new();
    for (seq, score) in ranked.iter_mut() {
        let tag: Option<String> = st.query_row([*seq], |r| r.get(0)).optional()?;
        if let Some(until) = tag.as_deref().and_then(|t| t.strip_prefix("valid-until:")).and_then(oracle_proto::parse_when).filter(|u| *u < clock) {
            *score *= SUPERSEDED_WEIGHT;
            ended.insert(*seq, until);
        }
    }
    if !ended.is_empty() {
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    }
    Ok(ended)
}

/// Superseded records rank lower: their scores are multiplied by SUPERSEDED_WEIGHT and the list re-sorted, ties in
/// the order given. Returns each demoted record's successor.
/// Synthetic records and graph scaffolding go under every first-hand one, in their own order (docs/SEARCH.md). An id alias
/// stays first. The hand-built graph's structure (a `cortex:link` record, a concept or entity node: a few words such as
/// "about orchestrator") is scaffolding, not knowledge: its words match almost any search, so it trails (Blaze, 26 Oct:
/// recall kept answering with those and hid the February chats). At most [`APPROVED_LEAD`] Blaze-approved observations
/// lead, and only those that match nearly as well as the best first-hand record ([`APPROVED_LEAD_SHARE`]); the rest keep
/// their own rank, so an approved node that barely matches no longer outranks a record that matches well.
fn demote_synthetic(c: &Connection, ranked: &mut Vec<(i64, f64)>, verified: Option<&std::collections::BTreeSet<String>>) -> rusqlite::Result<()> {
    let mut st = c.prepare_cached(
        "SELECT o.channel, o.id, EXISTS(SELECT 1 FROM tags t WHERE t.obs_seq=o.seq AND t.name='synthetic'), \
         o.channel='cortex' AND (EXISTS(SELECT 1 FROM tags t WHERE t.obs_seq=o.seq AND t.name='cortex:link') \
           OR (EXISTS(SELECT 1 FROM tags t WHERE t.obs_seq=o.seq AND t.name='cortex:node') \
               AND coalesce(CASE WHEN json_valid(o.cargo) THEN json_extract(o.cargo,'$.kind') END,'') <> 'observation')) \
         FROM observations o WHERE o.seq=?1",
    )?;
    let mut synthetic = std::collections::HashSet::new();
    let mut approved = std::collections::HashSet::new();
    for (seq, score) in ranked.iter() {
        if *score >= ALIAS_SCORE {
            continue;
        }
        if let Some((channel, id, tagged, scaffold)) = st
            .query_row(params![seq], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?, r.get::<_, bool>(3)?)))
            .optional()?
        {
            // Approved is the verified live set and nothing else (Emmy's review: never the raw `approved_by` field).
            // When the cortex views cannot be read nothing is approved: ordinary recall still runs, no lead is given.
            let blessed = verified.is_some_and(|v| v.contains(&id));
            if blessed && !scaffold {
                approved.insert(*seq);
            } else if scaffold || is_node(&channel) || is_synthetic(&channel, &id, &if tagged { vec!["synthetic".to_string()] } else { vec![] }) {
                synthetic.insert(*seq);
            }
        }
    }
    if !synthetic.is_empty() || !approved.is_empty() {
        let alias: Vec<_> = ranked.iter().copied().filter(|(_, s)| *s >= ALIAS_SCORE).collect();
        let rest: Vec<_> = ranked.drain(..).filter(|(_, s)| *s < ALIAS_SCORE).collect();
        // An approved observation leads only when it matches nearly as well as the best first-hand record: on the 26 Oct
        // live-copy canary, a lead for any approved match put "So I don't know much about Git" (0.01) over the answer
        // (0.03) for "how about the open port", and MRR on evals/lineage-20261026 fell from 0.68 to 0.56.
        let best = rest.iter().filter(|(seq, _)| !approved.contains(seq) && !synthetic.contains(seq)).map(|(_, s)| *s).fold(f64::MIN, f64::max);
        let lead: Vec<_> = rest
            .iter()
            .copied()
            .filter(|(seq, s)| approved.contains(seq) && (best <= 0.0 || *s >= APPROVED_LEAD_SHARE * best))
            .take(APPROVED_LEAD)
            .collect();
        let rest: Vec<_> = rest.into_iter().filter(|r| !lead.contains(r)).collect();
        let (first, second): (Vec<_>, Vec<_>) = rest.into_iter().partition(|(seq, _)| !synthetic.contains(seq));
        *ranked = alias.into_iter().chain(lead).chain(first).chain(second).collect();
    }
    Ok(())
}

/// The verified node takes the place of its copies (docs/SEARCH.md "Approved first"). The same passage lives in Oracle up to
/// five times: the February import, its cortex extract, a learning or context row, a conversation index line, and the node
/// Blaze approved. When a copy matches the query, the approved node goes where the best copy stood, so recall answers with
/// the verified reading and its map first and the copies follow (Blaze, 30 Oct 2026: "all open observations become the
/// hippocampus, the approved marks are actual verified cortex nodes that should hit on every oracle recall"). Measured that
/// day on 60 approved marks: in his own words 54 led; in their finds-it words 10 of 21, beaten by their own copies. Only the
/// first [`COPY_WINDOW`] places are read. Returns how many approved nodes moved up.
fn collapse_copies(c: &Connection, ranked: &mut Vec<(i64, f64)>, quotes: &[(String, String)]) -> rusqlite::Result<usize> {
    if quotes.is_empty() {
        return Ok(0);
    }
    let minted: std::collections::HashSet<&str> = quotes.iter().map(|(id, _)| id.as_str()).collect();
    let mut row = c.prepare_cached("SELECT id, content FROM observations WHERE seq=?1")?;
    let mut seq_of = c.prepare_cached("SELECT seq FROM observations WHERE id=?1")?;
    let mut moved = 0;
    let mut placed: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut i = 0;
    while i < ranked.len().min(COPY_WINDOW) {
        let (seq, score) = ranked[i];
        i += 1;
        if score >= ALIAS_SCORE || placed.contains(&seq) {
            continue;
        }
        let Some((id, content)) = row.query_row(params![seq], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).optional()? else { continue };
        if minted.contains(id.as_str()) {
            placed.insert(seq); // an approved node already standing here keeps its place
            continue;
        }
        let text = crate::cortex::comparable(&content);
        if text.chars().count() < crate::cortex::COPY_MIN {
            continue;
        }
        // A copy holds the whole approved quote, or is a long piece of it (an extract that quotes half the prompt).
        let Some((node, _)) = quotes.iter().find(|(_, q)| text.contains(q.as_str()) || (text.chars().count() >= 2 * crate::cortex::COPY_MIN && q.contains(text.as_str()))) else { continue };
        let Some(node_seq) = seq_of.query_row(params![node], |r| r.get::<_, i64>(0)).optional()? else { continue };
        if placed.contains(&node_seq) {
            continue;
        }
        let at = i - 1;
        if let Some(pos) = ranked.iter().position(|(s, _)| *s == node_seq) {
            if pos < at {
                placed.insert(node_seq);
                continue;
            }
            ranked.remove(pos);
        }
        ranked.insert(at, (node_seq, score));
        placed.insert(node_seq);
        moved += 1;
        i += 1; // the copy now stands one place lower; it was already read
    }
    Ok(moved)
}

/// How far down [`collapse_copies`] looks for copies of approved passages.
const COPY_WINDOW: usize = 40;

/// The query's content words: lowercase runs of letters and digits of three or more characters, filler words out
/// ([`crate::terms::STOP`]), each once, at most 24. What a record must hold to be about the query.
pub(crate) fn content_words(q: &str) -> Vec<String> {
    let stop: std::collections::HashSet<&str> = crate::terms::STOP.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    for w in q.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 3).map(str::to_lowercase) {
        if !stop.contains(w.as_str()) && !out.contains(&w) {
            out.push(w);
        }
    }
    out.truncate(24);
    out
}

/// How many of `words` each of `seqs` holds, by the keyword index (one match per word over the given records).
fn words_held(c: &Connection, seqs: &[i64], words: &[String]) -> rusqlite::Result<std::collections::HashMap<i64, usize>> {
    let mut held: std::collections::HashMap<i64, usize> = seqs.iter().map(|s| (*s, 0)).collect();
    if seqs.is_empty() || words.is_empty() {
        return Ok(held);
    }
    let list = seqs.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let mut st = c.prepare(&format!("SELECT rowid FROM obs_fts WHERE obs_fts MATCH ?1 AND rowid IN ({list})"))?;
    for w in words {
        for seq in st.query_map(params![format!("\"{w}\"")], |r| r.get::<_, i64>(0))? {
            if let Some(n) = held.get_mut(&seq?) {
                *n += 1;
            }
        }
    }
    Ok(held)
}

/// The words a record needs to be about a query of `n` content words: three in five, at least one, all of one or two.
fn words_needed(n: usize) -> usize {
    (n * 3).div_ceil(5).max(1)
}

/// How many approved observations about the query go first (see [`approved_first`]).
const APPROVED_FIRST: usize = 5;

/// Approved first, of what is about the query (Blaze, 31 Oct 2026: what he approves in Brain cases is "the first most
/// important things that show up in Oracle", and search was "dumping random things which should not be there"). Measured
/// that morning on the live Oracle: "dot-lattice bootstrap OpenFHE" led with three approved readings about local models,
/// agents and Hoshi, none holding "lattice", "bootstrap" or "openfhe": the graph had named DOT, and the check that kept
/// "only readings the words also match" read the fused list, which the meaning side fills with near neighbours.
/// In the first [`COPY_WINDOW`] places: an approved observation holding [`words_needed`] of the query's content words goes
/// to the front (at most [`APPROVED_FIRST`], in their order); one that does not, and was not named by the graph for a
/// query of names alone (`named`), goes below the last record that does. Everything else keeps its order. Returns, for
/// every ranked record read, how many content words it holds, and how many approved observations moved.
fn approved_first(c: &Connection, ranked: &mut Vec<(i64, f64)>, words: &[String], approved: &std::collections::BTreeSet<String>, named: &[i64], order: ApprovedOrder) -> rusqlite::Result<(std::collections::HashMap<i64, usize>, usize)> {
    let window: Vec<i64> = ranked.iter().take(COPY_WINDOW).filter(|(_, s)| *s < ALIAS_SCORE).map(|(seq, _)| *seq).collect();
    let held = words_held(c, &window, words)?;
    if words.is_empty() || window.is_empty() {
        return Ok((held, 0));
    }
    let need = words_needed(words.len());
    let mut id_of = c.prepare_cached("SELECT id FROM observations WHERE seq=?1")?;
    let mut blessed = std::collections::HashSet::new();
    for seq in &window {
        if let Some(id) = id_of.query_row(params![seq], |r| r.get::<_, String>(0)).optional()? {
            if approved.contains(&id) {
                blessed.insert(*seq);
            }
        }
    }
    let about = |seq: &i64| held.get(seq).copied().unwrap_or(0) >= need || named.contains(seq);
    let lead: Vec<i64> = match order {
        ApprovedOrder::Lead => window.iter().copied().filter(|seq| blessed.contains(seq) && about(seq)).take(APPROVED_FIRST).collect(),
        ApprovedOrder::Boost | ApprovedOrder::Off => Vec::new(),
    };
    let stray: Vec<i64> = window.iter().copied().filter(|seq| blessed.contains(seq) && !about(seq)).collect();
    if lead.is_empty() && stray.is_empty() && order != ApprovedOrder::Boost {
        return Ok((held, 0));
    }
    let score_of: std::collections::HashMap<i64, f64> = ranked.iter().copied().collect();
    let alias: Vec<(i64, f64)> = ranked.iter().copied().filter(|(_, s)| *s >= ALIAS_SCORE).collect();
    let mut rest: Vec<(i64, f64)> = ranked.drain(..).filter(|(seq, s)| *s < ALIAS_SCORE && !lead.contains(seq) && !stray.contains(seq)).collect();
    // The strays go below the last record (in the window) that is about the query, in their own order.
    let last_about = rest.iter().take(COPY_WINDOW).rposition(|(seq, _)| about(seq)).map_or(0, |p| p + 1);
    let tail = rest.split_off(last_about.min(rest.len()));
    let mut moved = lead.len();
    if order == ApprovedOrder::Boost {
        // Each approved record about the query rises at most APPROVED_BOOST places, the best-ranked first.
        let mut i = 0;
        while i < rest.len() {
            let seq = rest[i].0;
            if blessed.contains(&seq) && about(&seq) {
                let to = i.saturating_sub(APPROVED_BOOST);
                if to < i {
                    let x = rest.remove(i);
                    rest.insert(to, x);
                    moved += 1;
                }
            }
            i += 1;
        }
    }
    if order == ApprovedOrder::Off {
        moved = stray.len();
    }
    *ranked = alias
        .into_iter()
        .chain(lead.iter().map(|seq| (*seq, score_of[seq])))
        .chain(rest)
        .chain(stray.iter().map(|seq| (*seq, score_of[seq])))
        .chain(tail)
        .collect();
    Ok((held, moved))
}

/// How many Blaze-approved cortex observations may go ahead of the fused ranking (see [`demote_synthetic`]).
const APPROVED_LEAD: usize = 2;
/// How many approved observations a named concept or entity puts first (see [`Store::graph_lead`]).
const GRAPH_LEAD: usize = 3;
/// How many a named form alone puts first ("our best breakthroughs"): the answer is the list.
const GRAPH_LEAD_FORM: usize = 5;
/// How close to the best first-hand score an approved observation must be to lead.
const APPROVED_LEAD_SHARE: f64 = 0.8;

/// One room fills at most half a page while other rooms still have matches (docs/SEARCH.md). On 26 Oct, 16 of 25 hits
/// for "agents as dragons" came from #cortex (extracts and nodes) and the first-hand February chat under them never
/// showed. Only for a search across rooms; what is held back follows, in its order, once the page is full.
fn spread_rooms(c: &Connection, ranked: &mut Vec<(i64, f64)>, limit: i64) -> rusqlite::Result<()> {
    let limit = limit.max(1) as usize;
    let cap = limit.div_ceil(2).max(2);
    let mut st = c.prepare_cached("SELECT channel FROM observations WHERE seq=?1")?;
    let mut count: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let (mut page, mut later) = (Vec::with_capacity(limit), Vec::new());
    for (seq, score) in ranked.drain(..) {
        if page.len() >= limit {
            later.push((seq, score));
            continue;
        }
        if score >= ALIAS_SCORE {
            page.push((seq, score));
            continue;
        }
        let room: String = st.query_row(params![seq], |r| r.get(0)).optional()?.unwrap_or_default();
        let n = count.entry(room).or_insert(0);
        if *n < cap {
            *n += 1;
            page.push((seq, score));
        } else {
            later.push((seq, score));
        }
    }
    page.extend(later);
    *ranked = page;
    Ok(())
}

fn demote_superseded(c: &Connection, ranked: &mut [(i64, f64)], as_of: Option<i64>) -> rusqlite::Result<std::collections::HashMap<i64, (String, i64)>> {
    let mut by = std::collections::HashMap::new();
    for (seq, score) in ranked.iter_mut() {
        if let Some(next) = successor_of(c, *seq, as_of)? {
            *score *= SUPERSEDED_WEIGHT;
            by.insert(*seq, next);
        }
    }
    if !by.is_empty() {
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    }
    Ok(by)
}

#[derive(Debug, Serialize)]
pub struct StateHead {
    pub subject: String,
    pub owner: String,
    pub state: String,
    pub next_touch: Option<String>,
    pub observation: Observation,
}

/// One observation from an earlier Oracle, as read from an export file (JSON lines).
#[derive(Debug, serde::Deserialize)]
pub struct LegacyRow {
    pub id: String,
    pub channel: String,
    pub author: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    pub content: String,
    #[serde(default)]
    pub cargo: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created_ms: i64,
    #[serde(default)]
    pub pubkey: String,
    #[serde(default)]
    pub sig: String,
    /// True for a live message mirrored from another Oracle: it is folded like any new
    /// message. False (default) for history.
    #[serde(default)]
    pub news: bool,
    /// Complete source record; preserved verbatim as JSON, separately from the read projection.
    #[serde(default)]
    pub source: Option<serde_json::Value>,
    /// Explicit administrator repair of an earlier lossy import. Never touches native rows.
    #[serde(default)]
    pub repair_legacy: bool,
}

fn legacy_index_tags(r: &LegacyRow) -> Vec<String> {
    let mut tags: Vec<String> = r
        .tags
        .iter()
        .filter(|t| !t.is_empty() && t.len() <= 200)
        .cloned()
        .collect();
    tags.extend(mentions(&r.content));
    if let Some(id) = cargo_reply_to(r.cargo.as_deref()) {
        tags.push(format!("re:{id}"));
    }
    tags.extend([
        format!("kind:{}", r.kind),
        format!("from:{}", r.author),
        "envelope:1".into(),
    ]);
    tags.sort();
    tags.dedup();
    tags
}

fn default_kind() -> String {
    "say".into()
}

/// One seat's read position in a room.
#[derive(Debug, Serialize)]
pub struct ReadMark {
    pub agent: String,
    pub mark_seq: i64,
    pub through_ms: Option<i64>,
    pub pending: i64,
}

/// A state head without its observation — what a task list needs.
#[derive(Debug, Serialize)]
pub struct StateRow {
    pub subject: String,
    pub owner: String,
    pub state: String,
    pub next_touch: Option<String>,
    /// The room the head was written in, the observation that set it, its one line, and when —
    /// so a room's board can show its heads without a second read per head (2026-09-28: `ox task`
    /// heads were invisible to a person; the board could not even tell which room they belonged to).
    pub channel: String,
    pub id: String,
    pub note: String,
    pub at_ms: i64,
}

#[derive(Debug, Serialize)]
pub struct KeyRow {
    pub handle: String,
    pub pubkey: String,
    pub role: String,
    pub created_ms: i64,
    pub revoked_ms: Option<i64>,
}

pub struct Store {
    pool: Pool,
    /// The Cited by projection, kept until its inputs change (see [`Store::cite_key`]): folding the cortex and verifying
    /// every approval cost 0.6-1.1 s per search on a production copy (26 Oct canary), so it is built once per state.
    cites: std::sync::Mutex<Option<(CiteKey, CortexViews)>>,
    /// Public seats (publicseats.rs): the rooms they may write in and the limits they pass. Read from the environment
    /// when the store opens; tests set it with [`Store::set_public_config`].
    public: std::sync::RwLock<PublicConfig>,
    /// The handles a query may name as a mind ([`named_minds`]), read from the signed authors and kept ten minutes.
    minds: std::sync::Mutex<Option<(std::time::Instant, Vec<String>)>>,
}

/// Which rooms take public seats' writes, and the limits those writes pass (publicseats.rs).
#[derive(Clone, Debug, Default)]
pub struct PublicConfig {
    pub write_rooms: Vec<String>,
    pub limits: crate::publicseats::Limits,
}

impl PublicConfig {
    pub fn from_env() -> Self {
        PublicConfig { write_rooms: crate::publicseats::write_rooms_from_env(), limits: crate::publicseats::Limits::from_env() }
    }
}

/// What recall reads from the folded cortex, built together and kept until the cortex changes: who cites each record
/// (docs/CORTEX.md "Cited by") and the concept and entity layer search goes through (docs/SEARCH.md "Through the graph").
#[derive(Clone)]
pub struct CortexViews {
    pub cites: std::sync::Arc<crate::cortex::Citations>,
    pub concepts: std::sync::Arc<crate::cortex::ConceptIndex>,
}

/// Everything Cited by depends on, cheap to read: the newest cortex record, the newest archive of any record or room
/// (an archive changes which nodes are live without a cortex row), and digests of the gateways and keys tables (an
/// attester or key granted or taken away changes the classes at an unchanged seq).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiteKey {
    cortex_seq: i64,
    archive_seq: i64,
    room_archive: i64,
    gateways: String,
    keys: String,
}

/// Archived rooms (docs/ARCHIVE.md): filled from a room's State revision whose `archived` field says why.
pub const ARCHIVE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS room_archive (
  room    TEXT PRIMARY KEY,
  obs_seq INTEGER NOT NULL,
  at_ms   INTEGER NOT NULL,
  by      TEXT NOT NULL,
  why     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS obs_expires ON observations(expires_ms) WHERE expires_ms IS NOT NULL;
DROP VIEW IF EXISTS obs_live;
CREATE VIEW obs_live AS SELECT * FROM observations
  WHERE channel NOT IN (SELECT room FROM room_archive)
    AND (expires_ms IS NULL OR expires_ms > CAST(strftime('%s','now') AS INTEGER) * 1000);
";

/// The live records in a WHERE clause on an aliased `observations o`: not in an archived room, not expired.
pub const LIVE_O: &str = "o.channel NOT IN (SELECT room FROM room_archive) AND (o.expires_ms IS NULL OR o.expires_ms > CAST(strftime('%s','now') AS INTEGER) * 1000)";
/// The same on an unaliased `observations`.
pub const LIVE: &str = "channel NOT IN (SELECT room FROM room_archive) AND (expires_ms IS NULL OR expires_ms > CAST(strftime('%s','now') AS INTEGER) * 1000)";

/// `expires_ms`: when the record leaves the view (docs/ARCHIVE.md), from a signed `expires:<ms>` tag.
fn migrate_expires(c: &Connection) -> Result<()> {
    let has: bool = c.prepare("SELECT 1 FROM pragma_table_info('observations') WHERE name='expires_ms'")?.exists([])?;
    if !has {
        c.execute_batch("ALTER TABLE observations ADD COLUMN expires_ms INTEGER")?;
    }
    Ok(())
}

/// The retention table as stored (retention.rs): empty when no owner record has been written; unreadable is an error.
fn retention_in(c: &Connection) -> Result<crate::retention::Table> {
    let v: Option<String> = c.query_row("SELECT table_json FROM retention WHERE id=1", [], |x| x.get(0)).optional()?;
    match v {
        None => Ok(Default::default()),
        Some(v) => serde_json::from_str(&v).map_err(|e| StoreError::Internal(format!("retention table unreadable: {e}"))),
    }
}

/// One short immediate transaction that removes up to `limit` expired rows of `room` from the store and its indexes; how many it removed.
fn sweep_batch(c: &mut Connection, room: &str, now_ms: i64, limit: usize) -> Result<usize> {
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let seqs: Vec<i64> = tx
        .prepare("SELECT seq FROM observations WHERE channel = ?1 AND expires_ms IS NOT NULL AND expires_ms < ?2 ORDER BY seq LIMIT ?3")?
        .query_map(params![room, now_ms, limit as i64], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let has_v1c: bool = tx.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='obs_fts_v1c'")?.exists([])?;
    for seq in &seqs {
        tx.execute("DELETE FROM obs_fts WHERE rowid = ?1", params![seq])?;
        if has_v1c {
            tx.execute("DELETE FROM obs_fts_v1c WHERE rowid = ?1", params![seq])?;
        }
        tx.execute("DELETE FROM tags WHERE obs_seq = ?1", params![seq])?;
        tx.execute("DELETE FROM state_heads WHERE obs_seq = ?1", params![seq])?;
        tx.execute("DELETE FROM item WHERE msg_seq = ?1", params![seq])?;
        tx.execute("DELETE FROM vote WHERE msg_seq = ?1", params![seq])?;
        tx.execute("DELETE FROM mark WHERE msg_seq = ?1", params![seq])?;
        tx.execute("DELETE FROM thread WHERE root_seq = ?1", params![seq])?;
        tx.execute("DELETE FROM msg WHERE seq = ?1", params![seq])?;
        tx.execute("DELETE FROM observations WHERE seq = ?1", params![seq])?;
    }
    if !seqs.is_empty() {
        tx.execute(
            "UPDATE channel_heads SET total = (SELECT count(*) FROM observations WHERE channel = ?1), \
             max_seq = COALESCE((SELECT max(seq) FROM observations WHERE channel = ?1), max_seq) WHERE channel = ?1",
            params![room],
        )?;
    }
    tx.commit()?;
    Ok(seqs.len())
}

/// Runs `f` again while SQLite says busy or locked, a quarter second apart, for up to a minute: the live server writes too.
fn retry_busy<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    for _ in 0..240 {
        match f() {
            Err(StoreError::Unavailable(ref m)) if m.contains("busy") => std::thread::sleep(std::time::Duration::from_millis(250)),
            r => return r,
        }
    }
    f()
}


/// The `expires:<unix ms>` tag, if the record carries one and it parses.
pub fn expires_of(tags: &[String]) -> Option<i64> {
    tags.iter().find_map(|t| t.strip_prefix("expires:")).and_then(|v| v.trim().parse::<i64>().ok()).filter(|ms| *ms > 0)
}

/// What a State revision says about archiving: `Some(Some(why))` when its `archived` field is non-empty,
/// `Some(None)` when the revision has no such field, `None` when the cargo is not a State revision.
fn archived_why(cargo: Option<&str>) -> Option<Option<String>> {
    let v: serde_json::Value = serde_json::from_str(cargo?).ok()?;
    if v["schema"] != "room-context/1" {
        return None;
    }
    Some(v["fields"]["archived"].as_str().map(str::trim).filter(|w| !w.is_empty()).map(str::to_owned))
}

/// Keep `room_archive` in step with a stored State revision of `room`.
fn note_archive(c: &Connection, room: &str, seq: i64, at_ms: i64, by: &str, tags: &[String], cargo: Option<&str>) -> Result<()> {
    if !tags.iter().any(|t| t == "room-context:v1") {
        return Ok(());
    }
    match archived_why(cargo) {
        Some(Some(why)) => {
            c.execute("INSERT OR REPLACE INTO room_archive(room,obs_seq,at_ms,by,why) VALUES(?1,?2,?3,?4,?5)", params![room, seq, at_ms, by, why])?;
        }
        Some(None) => {
            c.execute("DELETE FROM room_archive WHERE room=?1", params![room])?;
        }
        None => {}
    }
    Ok(())
}

/// Rebuild `room_archive` from every room's newest State revision (at start).
fn refresh_room_archive(c: &Connection) -> Result<()> {
    c.execute_batch("DELETE FROM room_archive")?;
    let mut rooms = c.prepare("SELECT channel FROM channel_heads")?;
    let mut head = c.prepare(
        "SELECT o.seq, o.cargo, o.created_ms, o.author FROM tags t JOIN observations o ON o.seq=t.obs_seq \
         WHERE t.name='room-context:v1' AND o.channel=?1 ORDER BY o.seq DESC LIMIT 1",
    )?;
    let list: Vec<String> = rooms.query_map([], |r| r.get(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
    for room in list {
        let row: Option<(i64, Option<String>, i64, String)> =
            head.query_row(params![room], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
        if let Some((seq, cargo, at, by)) = row {
            if let Some(Some(why)) = archived_why(cargo.as_deref()) {
                c.execute("INSERT OR REPLACE INTO room_archive(room,obs_seq,at_ms,by,why) VALUES(?1,?2,?3,?4,?5)", params![room, seq, at, by, why])?;
            }
        }
    }
    Ok(())
}

/// `authored_ms`: when a queued write was written, signed in its envelope (NULL for every write delivered at once).
fn migrate_authored(c: &Connection) -> Result<()> {
    let has: bool = c.prepare("SELECT 1 FROM pragma_table_info('observations') WHERE name='authored_ms'")?.exists([])?;
    if !has {
        c.execute_batch("ALTER TABLE observations ADD COLUMN authored_ms INTEGER")?;
    }
    Ok(())
}

/// `prev`: the author chain link a write signed (docs/CHAIN.md); NULL for every write that named none.
fn migrate_prev(c: &Connection) -> Result<()> {
    let has: bool = c.prepare("SELECT 1 FROM pragma_table_info('observations') WHERE name='prev'")?.exists([])?;
    if !has {
        c.execute_batch("ALTER TABLE observations ADD COLUMN prev TEXT")?;
    }
    Ok(())
}

/// An author's chain head on any connection: the link a new write must name, and the record it links to (none
/// before the author's first native observation, when the link is genesis).
fn chain_head_in(c: &Connection, author: &str) -> Result<(String, Option<(i64, String)>)> {
    let last: Option<(i64, String, String)> = c
        .query_row(
            "SELECT seq, id, sig FROM observations WHERE author=?1 AND envelope=2 ORDER BY seq DESC LIMIT 1",
            params![author],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(match last {
        Some((seq, id, sig)) => (oracle_proto::link_of(&sig), Some((seq, id))),
        None => (oracle_proto::GENESIS_PREV.to_string(), None),
    })
}

/// Registries created before seats could hold several keys had `handle` as the whole primary
/// key. Rebuild that table in place; nothing else about it changes.
fn migrate_keys(c: &Connection) -> Result<()> {
    let sql: Option<String> = c
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='keys'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if sql.is_some_and(|s| !s.contains("PRIMARY KEY (handle, pubkey)")) {
        c.execute_batch(
            "BEGIN;
             ALTER TABLE keys RENAME TO keys_single;
             CREATE TABLE keys (handle TEXT NOT NULL, pubkey TEXT NOT NULL, role TEXT NOT NULL, created_ms INTEGER NOT NULL, revoked_ms INTEGER, PRIMARY KEY (handle, pubkey));
             INSERT INTO keys SELECT handle,pubkey,role,created_ms,revoked_ms FROM keys_single;
             DROP TABLE keys_single;
             COMMIT;",
        )?;
    }
    Ok(())
}

fn sha_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub(crate) fn ymd(ms: i64) -> String {
    // Civil-from-days (Howard Hinnant), so ids need no date crate.
    let days = ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}{m:02}{d:02}")
}

/// Share imported reply semantics between live reads and the materialized room index.
/// Native signed tags and persisted observations remain unchanged.
pub(crate) fn project_imported_reply(tags: &mut Vec<String>, cargo: Option<&str>, envelope: i64) {
    if envelope == 1 {
        if let Some(id) = cargo_reply_to(cargo) {
            let link = format!("re:{id}");
            if !tags.contains(&link) {
                tags.push(link);
            }
        }
    }
}

/// `@handle` mentions in content become `mention:handle` index tags.
/// The message this one answers, when that link lives in its cargo instead of a tag.
///
/// The web room writes `room-message/1` and `room-reply/1` manifests carrying `replyTo`, and the
/// REST rail wraps them one level deep as a JSON string under `cargo`. Those replies are as real as
/// a `re:` tag, so the index gets a derived `re:<id>` row. The signed envelope is never touched.
pub(crate) fn cargo_reply_to(cargo: Option<&str>) -> Option<String> {
    fn valid(id: &str) -> bool {
        id.starts_with("OBS-")
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }
    fn pick(v: &serde_json::Value, depth: u8) -> Option<String> {
        if let Some(id) = v.get("replyTo").and_then(|x| x.as_str()) {
            if valid(id) {
                return Some(id.to_owned());
            }
        }
        if depth == 0 {
            return None;
        }
        let inner: serde_json::Value = serde_json::from_str(v.get("cargo")?.as_str()?).ok()?;
        pick(&inner, depth - 1)
    }
    pick(&serde_json::from_str(cargo?).ok()?, 1)
}

/// The WHERE/ORDER tail of [`Store::recent_filtered`]. Params: ?1 before, ?2 channel, ?3 starts,
/// ?4 starts length, ?5 tag, ?6 limit.
fn recent_filtered_sql(channel: bool, tag: bool) -> String {
    format!(
        "{} WHERE o.seq<?1 AND o.seq>?7 AND {} AND (?3 IS NULL OR substr(o.content,1,?4)=?3){} ORDER BY o.seq DESC LIMIT ?6",
        if tag { "JOIN tags t ON t.obs_seq=o.seq" } else { "" },
        if channel { "o.channel=?2" } else { "?2 IS NULL" },
        if tag { " AND t.name=?5" } else { " AND ?5 IS NULL" },
    )
}

/// The message this one answers: a link tag, or a `replyTo` inside its cargo.
pub(crate) fn parent_of(o: &Observation) -> Option<String> {
    o.tags
        .iter()
        .find_map(|t| {
            ["re:", "reply:", "in_reply_to:", "closes:"]
                .iter()
                .find_map(|p| t.strip_prefix(p).map(str::to_owned))
        })
        .or_else(|| cargo_reply_to(o.cargo.as_deref()))
}

/// A message opens a conversation only when it answers nothing — whichever way the link was written.
/// A root voice replying from the web composer links through cargo and must not surface as a root.
pub fn starts_thread(o: &Observation) -> bool {
    // A state update names its message and is never a conversation of its own.
    parent_of(o).is_none() && o.kind != "annotate" && !crate::projection::is_state_update(&o.tags)
}

/// The message this envelope answers, from its link tags or a `replyTo` in its cargo.
fn points_at_of(env: &Envelope) -> Option<String> {
    env.tags
        .iter()
        .find_map(|t| ["re:", "reply:", "in_reply_to:", "closes:"].iter().find_map(|p| t.strip_prefix(p)).map(str::to_owned))
        .or_else(|| cargo_reply_to(env.cargo.as_deref()))
}

/// `item:<k>=<kind>` DECLARES an item; `item:<k>=<state>` MOVES one on the message it points at.
/// A state on a message that points at nothing moves nothing, and used to vanish silently
/// (`ox say --item R1=open`, 2026-09-27): refuse it by name, and refuse a word that is neither.
/// One revision of a room's State as the server counts it.
pub struct RoomRevision {
    pub id: String,
    pub seq: i64,
    pub author: String,
    /// When the Oracle received it, and the time the author signed.
    pub created_ms: i64,
    pub ts_ms: i64,
    pub parents: Vec<String>,
    pub fields: serde_json::Value,
    pub adopts: Option<serde_json::Value>,
}

/// A room's `room-context/1` revisions, judged once, for the write guard AND the read route (so they cannot disagree about what is a head).
/// `bad` lists tagged rows that carry cargo but are not a trustworthy revision (and why); `skipped` the ones that are not revisions at all (no cargo, archived).
pub struct RoomRevisions {
    pub revisions: Vec<RoomRevision>,
    pub bad: Vec<(String, String)>,
    pub skipped: Vec<(String, String)>,
}

impl RoomRevisions {
    /// The revisions no other revision names as a parent, sorted by id.
    pub fn heads(&self) -> Vec<String> {
        let referenced: std::collections::HashSet<&String> = self.revisions.iter().flat_map(|r| &r.parents).collect();
        let mut heads: Vec<String> = self.revisions.iter().filter(|r| !referenced.contains(&r.id)).map(|r| r.id.clone()).collect();
        heads.sort();
        heads
    }
}

/// The revisions of `room`'s State below `as_of` (an exclusive seq cursor; `None` is now). A native row counts only if its key is registered to its author and its
/// stored envelope verifies with this cargo; an imported one only with signed:ok and cargo-signed:ok. Archive marks count only below `as_of`; keys are read as of now.
pub fn room_revisions(c: &Connection, room: &str, as_of: Option<i64>) -> Result<RoomRevisions> {
    let bound = as_of.unwrap_or(i64::MAX);
    let strings = |v: &serde_json::Value| -> Option<Vec<String>> { v.as_array()?.iter().map(|x| x.as_str().map(str::to_owned)).collect() };
    let mut out = RoomRevisions { revisions: Vec::new(), bad: Vec::new(), skipped: Vec::new() };
    let mut st = c.prepare(
        "SELECT o.seq, o.id, o.cargo, o.envelope, o.tags_json, o.author, o.pubkey, o.created_ms, o.ts_ms FROM tags t JOIN observations o ON o.seq=t.obs_seq \
         WHERE t.name='room-context:v1' AND o.channel=?1 AND o.seq<?2 ORDER BY o.seq",
    )?;
    let rows = st
        .query_map(params![room, bound], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?, r.get::<_, i64>(7)?, r.get::<_, i64>(8)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (seq, id, raw, envelope, tags, author, pubkey, created_ms, ts_ms) in rows {
        let Some(raw) = raw else {
            out.skipped.push((id, "tagged room-context:v1 but carries no revision".into()));
            continue;
        };
        let archived: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM tags t JOIN observations o ON o.seq=t.obs_seq WHERE t.name=?1 AND o.channel=?2 AND o.seq<?3)",
            params![format!("archives:{id}"), room, bound],
            |r| r.get(0),
        )?;
        if archived {
            out.skipped.push((id, "archived".into()));
            continue;
        }
        let Ok(mut cargo) = serde_json::from_str::<serde_json::Value>(&raw) else {
            out.bad.push((id, "cargo is not JSON".into()));
            continue;
        };
        if envelope == 1 {
            let tags: Vec<String> = serde_json::from_str(&tags).unwrap_or_default();
            if !(tags.iter().any(|t| t == "signed:ok") && tags.iter().any(|t| t == "cargo-signed:ok")) {
                out.bad.push((id, "imported without signed:ok and cargo-signed:ok".into()));
                continue;
            }
            if let Some(inner) = cargo["cargo"].as_str() {
                match serde_json::from_str(inner) {
                    Ok(v) => cargo = v,
                    Err(_) => {
                        out.bad.push((id, "imported cargo is not JSON".into()));
                        continue;
                    }
                }
            }
        } else {
            let registered: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM keys WHERE handle=?1 AND pubkey=?2)", params![author, pubkey], |r| r.get(0))?;
            if !registered {
                out.bad.push((id, format!("signed by a key never registered to {author}")));
                continue;
            }
            let verified = envelope_in(c, &id).is_ok_and(|e| e.verify_stored(&pubkey).is_ok() && e.author == author && e.channel == room && e.cargo.as_deref() == Some(raw.as_str()));
            if !verified {
                out.bad.push((id, "stored envelope does not verify, or differs from the listing".into()));
                continue;
            }
        }
        if cargo["schema"].as_str() != Some("room-context/1") || cargo["channel"].as_str() != Some(room) {
            out.bad.push((id, format!("not a room-context/1 revision of #{room}")));
            continue;
        }
        let Some(parents) = strings(&cargo["parents"]) else {
            out.bad.push((id, "revision parents must be a list of string ids".into()));
            continue;
        };
        if !cargo["fields"].is_object() {
            out.bad.push((id, "revision fields must be an object".into()));
            continue;
        }
        out.revisions.push(RoomRevision {
            id,
            seq,
            author,
            created_ms,
            ts_ms,
            parents,
            fields: cargo["fields"].clone(),
            adopts: cargo.get("adopts").filter(|a| a.is_object()).cloned(),
        });
    }
    Ok(out)
}

impl Store {
    /// Administrative access to an already-running store. Never bootstraps schema,
    /// backfills FTS, or rebuilds projections just to register a key or set a scope.
    pub fn open_control(path: &str) -> Result<Store> {
        let manager = SqliteConnectionManager::file(path)
            .with_flags(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .with_init(|c| c.execute_batch("PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;"));
        let pool = r2d2::Pool::builder().max_size(1).build(manager)?;
        {
            let c = pool.get()?;
            // Refuse a missing/old schema without trying to migrate the live service.
            for sql in ["SELECT handle,pubkey,role,revoked_ms FROM keys LIMIT 0",
                "SELECT hash,handle,scope,label FROM tokens LIMIT 0",
                "SELECT table_json,source_id FROM seat_scope LIMIT 0",
                "SELECT table_json,source_id FROM private_rooms LIMIT 0"] {
                c.prepare(sql)?;
            }
        }
        Ok(Store { pool, cites: std::sync::Mutex::new(None), minds: std::sync::Mutex::new(None), public: std::sync::RwLock::new(PublicConfig::from_env()) })
    }

    /// A room's State below `as_of_seq` (an exclusive cursor: only rows with seq < as_of_seq; `None` is now, and the answer says which seq that was), from the same fold
    /// the write guard uses (`room_revisions`): one validated head selection, so a reader and the guard cannot disagree.
    /// SQL coverage and trust are separate: `coverage.sql_complete` says every tagged row below the cursor was enumerated (a SQL read, not a page); `trustworthy` is
    /// false when a tagged row carries cargo that is not a trustworthy revision (`bad`) or its status could not be established (`unknown`: always empty here, since
    /// a failed read is an error, never a partial answer); `complete` = both. `state`: `empty` (no revision), `alive` (one head, trustworthy), `degraded` (several heads,
    /// or not trustworthy); `fields` only when alive. `evidence` carries, for every counted revision, the signed envelope and the registered key, so a client can
    /// verify signatures itself instead of taking the server's word. Trust boundary: a native revision counts if its key is registered to its author (revoked keys
    /// included, as the reader counts them) and its stored envelope verifies with this cargo; keys are read as of now; archive marks only below the cursor.
    pub fn room_state_at(&self, room: &str, as_of_seq: Option<i64>) -> Result<serde_json::Value> {
        let c = self.conn()?;
        // One snapshot for the whole answer: the cursor, the revisions and the evidence all read the same database state.
        let tx = c.unchecked_transaction()?;
        let read_through: i64 = tx.query_row("SELECT COALESCE(MAX(seq),0) FROM observations", [], |r| r.get(0))?;
        let latest = read_through.checked_add(1).ok_or_else(|| StoreError::Unavailable("the log has no representable exclusive cursor".into()))?;
        if as_of_seq.is_some_and(|cursor| cursor < 1 || cursor > latest) {
            return Err(StoreError::Rejected(format!("as_of_seq must be a positive exclusive cursor no later than {latest}")));
        }
        let effective = as_of_seq.unwrap_or(latest);
        let rv = room_revisions(&tx, room, Some(effective))?;
        let heads = rv.heads();
        let unknown: Vec<serde_json::Value> = Vec::new();
        let trustworthy = rv.bad.is_empty() && unknown.is_empty();
        let state = if !trustworthy || heads.len() > 1 {
            "degraded"
        } else if heads.is_empty() {
            "empty"
        } else {
            "alive"
        };
        let head_rows: Vec<&RoomRevision> = rv.revisions.iter().filter(|r| heads.contains(&r.id)).collect();
        let known: std::collections::HashSet<&String> = rv.revisions.iter().map(|r| &r.id).collect();
        let missing_parents: Vec<&String> = rv.revisions.iter().flat_map(|r| &r.parents).filter(|p| !known.contains(p)).collect();
        let mut evidence = Vec::new();
        for r in &rv.revisions {
            let e = envelope_in(&tx, &r.id).ok();
            let key: Option<(String, i64)> = e.as_ref().and_then(|_| {
                tx.query_row("SELECT o.pubkey, EXISTS(SELECT 1 FROM keys k WHERE k.handle=o.author AND k.pubkey=o.pubkey) FROM observations o WHERE o.id=?1", params![r.id], |x| Ok((x.get(0)?, x.get(1)?))).ok()
            });
            // The stored row itself, read in this same snapshot, for native AND imported revisions, so a verifier needs no second lookup.
            let row = Self::load(&tx, "WHERE o.id=?1", &[&r.id.as_str()])?.into_iter().next().and_then(|o| serde_json::to_value(o).ok());
            evidence.push(serde_json::json!({
                "row": row, "parents": r.parents, "fields": r.fields, "adopts": r.adopts,
                "id": r.id, "seq": r.seq, "author": r.author, "received_ms": r.created_ms, "signed_ms": r.ts_ms, "native": e.is_some(),
                "envelope": e, "pubkey": key.as_ref().map(|k| k.0.clone()), "registered": key.map(|k| k.1 == 1),
            }));
        }
        Ok(serde_json::json!({
            "schema": "room-context/1", "channel": room, "state": state,
            "as_of_seq": effective, "as_of": effective, "as_of_requested": as_of_seq, "read_through_seq": read_through,
            "coverage": {"sql_complete": true, "revisions": rv.revisions.len(), "bad": rv.bad.len(), "skipped": rv.skipped.len()},
            "trustworthy": trustworthy, "complete": trustworthy,
            "heads": heads,
            "head_seqs": head_rows.iter().map(|r| serde_json::json!({"id": r.id, "seq": r.seq, "author": r.author, "received_ms": r.created_ms, "signed_ms": r.ts_ms})).collect::<Vec<_>>(),
            "fields": if state == "alive" { head_rows[0].fields.clone() } else { serde_json::Value::Null },
            "revisions": rv.revisions.len(),
            "bad": rv.bad.iter().map(|(id, why)| serde_json::json!({"id": id, "why": why})).collect::<Vec<_>>(),
            "unknown": unknown,
            "skipped": rv.skipped.iter().map(|(id, why)| serde_json::json!({"id": id, "why": why})).collect::<Vec<_>>(),
            "adoptions": head_rows.iter().filter_map(|r| r.adopts.clone().map(|a| (r.id.clone(), a))).collect::<serde_json::Map<_, _>>(),
            "missing_parents": missing_parents,
            "evidence": evidence,
            "trust": "server-verified: key registered to the author, stored envelope verifies with this cargo; keys read as of now; archive marks only below as_of_seq",
        }))
    }
}

/// Room State's server-side guard (docs/ROOM-INDEX.md, State). A say tagged `room-context:v1` whose cargo is a `room-context/1` revision of its OWN room must name
/// exactly the room's current head set as `parents` (a stale or forked writer is a 409), and an `adopts` it carries must be a proposal of that room that a decider
/// accepted, by the mark that is the thread's settlement right now (a false or superseded adoption is a 400). Heads are computed as the reader computes them:
/// archived revisions do not count, imported ones count only with signed:ok and cargo-signed:ok. Any other tagged line is stored as before; the reader reports it.
fn check_room_context(tx: &rusqlite::Transaction, env: &Envelope) -> Result<()> {
    if env.kind != Kind::Say || !env.tags.iter().any(|t| t == "room-context:v1") {
        return Ok(());
    }
    let Some(cargo) = env.cargo.as_deref().and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok()) else {
        return Ok(());
    };
    if cargo["schema"].as_str() != Some("room-context/1") || cargo["channel"].as_str() != Some(env.channel.as_str()) {
        return Ok(());
    }
    let strings = |v: &serde_json::Value| -> Option<Vec<String>> { v.as_array()?.iter().map(|x| x.as_str().map(str::to_owned)).collect() };
    let mut named = strings(&cargo["parents"]).ok_or_else(|| StoreError::Rejected("a room-context/1 revision names its parents as a list of ids".into()))?;
    if !cargo["fields"].is_object() {
        return Err(StoreError::Rejected("a room-context/1 revision carries its fields as an object".into()));
    }
    named.sort();
    named.dedup();

    let heads = room_revisions(tx, &env.channel, None)?.heads();
    if heads != named {
        return Err(StoreError::Conflict(format!(
            "stale State for #{}: the head is [{}] and this revision names [{}]; read the State again and write against the current head",
            env.channel,
            heads.join(", "),
            named.join(", ")
        )));
    }

    let Some(adopts) = cargo.get("adopts").filter(|a| !a.is_null()) else { return Ok(()) };
    let bad = |why: String| StoreError::Rejected(format!("adopts: {why}"));
    let field = |k: &str| adopts[k].as_str().filter(|v| !v.is_empty()).map(str::to_owned);
    if !adopts.is_object() {
        return Err(bad("must be the record of a settled proposal".into()));
    }
    let (Some(room), Some(proposal), Some(mark), Some(by)) = (field("room"), field("proposal"), field("mark"), field("decided_by")) else {
        return Err(bad("must name room, proposal, mark and decided_by".into()));
    };
    if room != env.channel {
        return Err(bad(format!("a State of #{} adopts only a proposal of its own room, not #{room}", env.channel)));
    }
    if !crate::roomindex::ready(tx)? {
        return Err(StoreError::Unavailable("the room index is not ready; an adoption cannot be checked".into()));
    }
    let root: Option<(i64, String, Option<String>)> = tx
        .query_row(
            "SELECT o.seq, m.channel, m.intent FROM observations o JOIN msg m ON m.seq=o.seq WHERE o.id=?1",
            params![proposal],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((root_seq, root_room, intent)) = root else { return Err(bad(format!("{proposal} is not a message of this Oracle"))) };
    if root_room != env.channel || intent.as_deref() != Some("proposal") {
        return Err(bad(format!("{proposal} is not a proposal of #{}", env.channel)));
    }
    let settled: Option<(Option<String>, Option<i64>)> = tx
        .query_row("SELECT settled_word, settled_by FROM thread WHERE root_seq=?1", params![root_seq], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((Some(word), Some(by_seq))) = settled else {
        return Err(bad(format!("{proposal} has not been settled by a decider; an opinion is not an adoption")));
    };
    if word != "accepted" {
        return Err(bad(format!("{proposal} was {word} by a decider, not accepted")));
    }
    let (mark_id, mark_author, mark_tags): (String, String, String) =
        tx.query_row("SELECT id, author, tags_json FROM observations WHERE seq=?1", params![by_seq], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    if mark_id != mark || adopts["cursor"].as_i64() != Some(by_seq) || mark_author != by || adopts["word"].as_str() != Some("accepted") {
        return Err(bad(format!("{proposal} is settled by {mark_id} (seq {by_seq}, {mark_author}); the record names something else")));
    }
    // WHO COULD DECIDE, judged at the mark from facts that cannot change afterwards (Emmy, 2026-10-03). The thread's settlement above is today's fold, which
    // reads the newest room-members record and the newest annotations; a grant made after the mark must not authorize it, so the guard asks again:
    //  - `addressed`: the proposal's OWN signed `to:` tags name the seat that marked (an annotation cannot add a decider later);
    //  - `membership`: the newest [<Room> members] record BELOW the mark, signed by an owner-key seat, lists the marking seat as a root.
    // The record says which basis it claims and the claim must be true; neither basis is invented from today's roots.
    let basis = adopts["basis"].as_str().unwrap_or("");
    let tags: Vec<String> = tx
        .query_row("SELECT tags_json FROM observations WHERE seq=?1", params![root_seq], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let mark_tags: Vec<String> = serde_json::from_str(&mark_tags).unwrap_or_default();
    if !mark_tags.iter().any(|t| t == "react:go" || t == "set:mark=go") {
        return Err(bad(format!("{mark_id} is not a go")));
    }
    match basis {
        "addressed" => {
            if !tags.iter().any(|t| t.strip_prefix("to:") == Some(mark_author.as_str())) {
                return Err(bad(format!("{proposal} was not addressed to {mark_author} in its own signed tags")));
            }
        }
        "membership" => {
            let mut name = env.channel.clone();
            if let Some(c) = name.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            let face = format!("[{name} members]");
            // The record must itself be trustworthy AT THE TIME it was written: a native line, its stored envelope verifies with this cargo, and it was signed by a
            // key that held the owner role by then (`keys.created_ms` is when the key got its role): not another key of an owner's handle, not a seat promoted
            // later, and never an imported row.
            let mut st = tx.prepare(
                "SELECT o.id, o.author, o.pubkey, o.cargo, o.created_ms FROM observations o WHERE o.channel=?1 AND o.news=1 AND o.envelope=2 AND o.seq<?2 \
                 AND substr(o.content,1,?4)=?3 ORDER BY o.seq DESC LIMIT 20",
            )?;
            let records = st
                .query_map(params![env.channel, by_seq, face, face.chars().count() as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, i64>(4)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut roots: Option<Vec<String>> = None;
            for (rid, author, pubkey, cargo, written_ms) in records {
                let owner_key: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM keys WHERE handle=?1 AND pubkey=?2 AND role='owner' AND created_ms<=?3)",
                    params![author, pubkey, written_ms],
                    |r| r.get(0),
                )?;
                let verified = owner_key && envelope_in(tx, &rid).is_ok_and(|e| e.verify_stored(&pubkey).is_ok() && e.author == author && e.cargo == cargo);
                let Some(v) = cargo.filter(|_| verified).and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok()) else { continue };
                if v["schema"] == "room-members/1" {
                    roots = Some(v["roots"].as_array().map(|a| a.iter().filter_map(|r| r.as_str().map(str::to_owned)).collect()).unwrap_or_default());
                    break;
                }
            }
            if !roots.is_some_and(|r| r.contains(&mark_author)) {
                return Err(bad(format!("{mark_author} was not a root of #{} when {mark_id} was made; a grant after the mark does not authorize it", env.channel)));
            }
        }
        other => return Err(bad(format!("basis must be 'addressed' or 'membership', not '{other}'"))),
    }
    Ok(())
}

fn check_item_tags(env: &Envelope) -> Result<()> {
    use crate::projection::{ITEM_KINDS, ITEM_STATES};
    let points = cargo_reply_to(env.cargo.as_deref()).is_some()
        || env.tags.iter().any(|t| ["re:", "reply:", "in_reply_to:", "annotates:", "closes:"].iter().any(|p| t.starts_with(p)));
    for (k, v) in env.tags.iter().filter_map(|t| t.strip_prefix("item:")?.split_once('=')) {
        if ITEM_KINDS.contains(&v) {
            continue;
        }
        if !ITEM_STATES.contains(&v) {
            return Err(StoreError::Rejected(format!(
                "item:{k}={v}: '{v}' is neither a kind ({}) nor a state ({})", ITEM_KINDS.join("|"), ITEM_STATES.join("|"))));
        }
        if !points {
            return Err(StoreError::Rejected(format!(
                "item:{k}={v}: '{v}' is a state, and this message points at nothing to move; declare the item with a kind ({})",
                ITEM_KINDS.join("|"))));
        }
    }
    Ok(())
}

/// One-time: derive `re:` index rows for replies written before [`cargo_reply_to`] existed.
fn backfill_cargo_links(c: &Connection) -> Result<()> {
    let done: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if done >= 1 {
        return Ok(());
    }
    let links: Vec<(i64, String)> = {
        let mut st =
            c.prepare("SELECT seq,cargo FROM observations WHERE cargo LIKE '%replyTo%'")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        rows.filter_map(|r| r.ok())
            .filter_map(|(seq, cargo)| Some((seq, cargo_reply_to(cargo.as_deref())?)))
            .collect()
    };
    for (seq, id) in &links {
        c.execute(
            "INSERT OR IGNORE INTO tags(name,obs_seq) VALUES(?1,?2)",
            params![format!("re:{id}"), seq],
        )?;
    }
    c.execute_batch("PRAGMA user_version=1")?;
    Ok(())
}

/// Actual JSON string leaves in serialized document order, with RFC 6901 pointers.
/// Keys and non-string scalars are not text. A serialized JSON string is one leaf,
/// never an implicit second document: import normalization is a separate operation.
pub fn cargo_text_fields(cargo: Option<&str>) -> Vec<(String, String)> {
    #[derive(Default)]
    struct Fields(Vec<(String, String)>);
    impl<'de> serde::Deserialize<'de> for Fields {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Fields;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str("JSON") }
                fn visit_str<E: serde::de::Error>(self, s: &str) -> std::result::Result<Fields, E> {
                    Ok(Fields(vec![(String::new(), s.to_owned())]))
                }
                fn visit_string<E: serde::de::Error>(self, s: String) -> std::result::Result<Fields, E> { Ok(Fields(vec![(String::new(), s)])) }
                fn visit_bool<E: serde::de::Error>(self, _: bool) -> std::result::Result<Fields, E> { Ok(Fields::default()) }
                fn visit_i64<E: serde::de::Error>(self, _: i64) -> std::result::Result<Fields, E> { Ok(Fields::default()) }
                fn visit_u64<E: serde::de::Error>(self, _: u64) -> std::result::Result<Fields, E> { Ok(Fields::default()) }
                fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<Fields, E> { Ok(Fields::default()) }
                fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Fields, E> { Ok(Fields::default()) }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut a: A) -> std::result::Result<Fields, A::Error> {
                    let mut out = vec![]; let mut i = 0;
                    while let Some(Fields(fields)) = a.next_element()? {
                        out.extend(fields.into_iter().map(|(p,t)| (format!("/{i}{p}"),t))); i += 1;
                    }
                    Ok(Fields(out))
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(self, mut a: A) -> std::result::Result<Fields, A::Error> {
                    let mut out = vec![];
                    while let Some((k, Fields(fields))) = a.next_entry::<String, Fields>()? {
                        let k = k.replace('~', "~0").replace('/', "~1");
                        out.extend(fields.into_iter().map(|(p,t)| (format!("/{k}{p}"),t)));
                    }
                    Ok(Fields(out))
                }
            }
            d.deserialize_any(Visitor)
        }
    }
    cargo.and_then(|c| serde_json::from_str::<Fields>(c).ok()).map(|f| f.0).unwrap_or_default()
}

/// The search row's `cargo` column: the fields of [`cargo_text_fields`] joined by one newline. The ONE function live writes, imports and the
/// backfill all index through, so they produce byte-identical rows; `match_at` maps an offset in this text back to its field.
pub fn fts_cargo_text(cargo: Option<&str>) -> String {
    cargo_text_fields(cargo).into_iter().map(|f| f.1).collect::<Vec<_>>().join("\n")
}

/// Add a message's search row (`content`, the tag tokens, and its cargo text). Every writer of `obs_fts` goes through here or [`fts_update`].
fn fts_insert(c: &Connection, seq: i64, content: &str, tags: &str, cargo: Option<&str>) -> Result<()> {
    c.execute(
        "INSERT INTO obs_fts(rowid,content,tags,cargo) VALUES(?1,?2,?3,?4)",
        params![seq, content, tags, fts_cargo_text(cargo)],
    )?;
    fts_note_projector_write(c)?;
    Ok(())
}

/// Replace a message's search row in place (a repaired import): same columns, same function, same row id.
fn fts_update(c: &Connection, seq: i64, content: &str, tags: &str, cargo: Option<&str>) -> Result<()> {
    c.execute(
        "UPDATE obs_fts SET content=?2,tags=?3,cargo=?4 WHERE rowid=?1",
        params![seq, content, tags, fts_cargo_text(cargo)],
    )?;
    fts_note_projector_write(c)?;
    Ok(())
}

// A known projector write may advance validity by exactly one source mutation. If an old
// writer changed anything first, leave validity behind: startup must catch that change up.
// Call only after the matching single source mutation, in its transaction; a retag-only
// projector must not certify a pending mutation made by another writer.
pub(crate) fn fts_note_projector_write(c: &Connection) -> Result<()> {
    c.execute(
        "UPDATE store_meta SET v=(SELECT v FROM store_meta WHERE k='fts_cargo_source_generation') \
         WHERE k='fts_cargo_verified_generation' \
         AND CAST(v AS INTEGER)+1=(SELECT CAST(v AS INTEGER) FROM store_meta WHERE k='fts_cargo_source_generation') \
         AND EXISTS(SELECT 1 FROM store_meta WHERE k='fts_cargo_v3' AND v='done')",
        [],
    )?;
    Ok(())
}

fn meta_get(c: &Connection, k: &str) -> Result<Option<String>> {
    Ok(c.query_row("SELECT v FROM store_meta WHERE k=?1", params![k], |r| r.get(0)).optional()?)
}
fn meta_set(c: &Connection, k: &str, v: &str) -> Result<()> {
    c.execute("INSERT OR REPLACE INTO store_meta(k,v) VALUES(?1,?2)", params![k, v])?;
    Ok(())
}

/// Rows copied per transaction by the `fts_cargo_v1` backfill.
pub const FTS_CARGO_BATCH: i64 = 500;

/// Resumable lexical repair, including writes made by a rolled-back binary. Source-mutation
/// generations are durable and updated by triggers in the writer's own transaction. A batch
/// checkpoint belongs to one generation; a changed generation restarts only the derived shadow.
/// A missing v3 projector-validity marker always rebuilds, even when v1 previously said done. Known new
/// writers advance validity with their index write; old writers cannot. Signed sources stay intact.
pub fn fts_cargo_backfill(c: &Connection, batch: i64, max_batches: Option<usize>) -> Result<bool> {
    if batch <= 0 {
        return Err(StoreError::Rejected("FTS backfill batch must be positive".into()));
    }
    c.execute_batch(
        "INSERT OR IGNORE INTO store_meta(k,v) VALUES('fts_cargo_source_generation','0');
         CREATE TRIGGER IF NOT EXISTS fts_cargo_source_insert AFTER INSERT ON observations BEGIN
           UPDATE store_meta SET v=CAST(v AS INTEGER)+1 WHERE k='fts_cargo_source_generation'; END;
         CREATE TRIGGER IF NOT EXISTS fts_cargo_source_update AFTER UPDATE ON observations BEGIN
           UPDATE store_meta SET v=CAST(v AS INTEGER)+1 WHERE k='fts_cargo_source_generation'; END;
         CREATE TRIGGER IF NOT EXISTS fts_cargo_source_delete AFTER DELETE ON observations BEGIN
           UPDATE store_meta SET v=CAST(v AS INTEGER)+1 WHERE k='fts_cargo_source_generation'; END;"
    )?;
    let has_cargo: i64 = c.query_row("SELECT count(*) FROM pragma_table_info('obs_fts') WHERE name='cargo'", [], |r| r.get(0))?;
    let generation = meta_get(c, "fts_cargo_source_generation")?;
    if has_cargo > 0
        && meta_get(c, "fts_cargo_v3")?.as_deref() == Some("done")
        && meta_get(c, "fts_cargo_verified_generation")? == generation
        && !c.query_row("SELECT EXISTS(SELECT 1 FROM obs_fts WHERE cargo IS NULL)", [], |r| r.get::<_, bool>(0))?
    {
        return Ok(true);
    }
    // Empty new stores need no shadow. An existing v1 table, even with a cargo column,
    // does: rollback-era rows may already be stale before these triggers were installed.
    if has_cargo > 0 {
        let tx = c.unchecked_transaction()?;
        let empty: bool = tx.query_row("SELECT NOT EXISTS(SELECT 1 FROM obs_fts)", [], |r| r.get(0))?;
        if empty {
            tx.execute_batch("DROP TABLE IF EXISTS obs_fts_v1c")?;
            tx.execute("DELETE FROM store_meta WHERE k IN ('fts_cargo_v1_seq','fts_cargo_v2_generation')", [])?;
            meta_set(&tx, "fts_cargo_v1", "done")?;
            meta_set(&tx, "fts_cargo_v2", "done")?;
            meta_set(&tx, "fts_cargo_v3", "done")?;
            let generation = meta_get(&tx, "fts_cargo_source_generation")?.unwrap_or_default();
            meta_set(&tx, "fts_cargo_verified_generation", &generation)?;
            tx.commit()?;
            return Ok(true);
        }
        tx.rollback()?;
    }
    c.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS obs_fts_v1c USING fts5(content, tags, cargo, tokenize = 'porter unicode61')")?;
    let mut batches = 0usize;
    loop {
        if max_batches.is_some_and(|m| batches >= m) {
            return Ok(false);
        }
        batches += 1;
        let tx = c.unchecked_transaction()?;
        let generation = meta_get(&tx, "fts_cargo_source_generation")?.unwrap_or_default();
        if meta_get(&tx, "fts_cargo_v2_generation")?.as_ref() != Some(&generation)
            || meta_get(&tx, "fts_cargo_v3_projector")?.as_deref() != Some("json-string-leaves-v1")
        {
            // Mutations between interruption and resume invalidate the copied prefix.
            tx.execute("DELETE FROM obs_fts_v1c", [])?;
            meta_set(&tx, "fts_cargo_v1_seq", "0")?;
            meta_set(&tx, "fts_cargo_v2_generation", &generation)?;
            meta_set(&tx, "fts_cargo_v3_projector", "json-string-leaves-v1")?;
        }
        let last: i64 = meta_get(&tx, "fts_cargo_v1_seq")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let rows: Vec<(i64, String, String, Option<String>)> = {
            let mut st = tx.prepare(
                "SELECT f.rowid, f.content, f.tags, o.cargo FROM obs_fts f LEFT JOIN observations o ON o.seq=f.rowid \
                 WHERE f.rowid>?1 ORDER BY f.rowid LIMIT ?2",
            )?;
            let v = st
                .query_map(params![last, batch], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            v
        };
        if rows.is_empty() {
            // Retag/reindex and retirement change FTS itself without mutating observations.
            // Check both directions under the same transaction before swapping; never
            // resurrect retired rows or install stale content/tags copied before a restart.
            let changed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM obs_fts f LEFT JOIN obs_fts_v1c n ON n.rowid=f.rowid \
                 WHERE n.rowid IS NULL OR f.content IS NOT n.content OR f.tags IS NOT n.tags \
                 UNION ALL SELECT 1 FROM obs_fts_v1c n LEFT JOIN obs_fts f ON f.rowid=n.rowid WHERE f.rowid IS NULL)",
                [], |r| r.get(0),
            )?;
            if changed {
                tx.execute("DELETE FROM obs_fts_v1c", [])?;
                meta_set(&tx, "fts_cargo_v1_seq", "0")?;
                tx.commit()?;
                continue;
            }
            tx.execute_batch("DROP TABLE obs_fts; ALTER TABLE obs_fts_v1c RENAME TO obs_fts;")?;
            meta_set(&tx, "fts_cargo_v1", "done")?;
            meta_set(&tx, "fts_cargo_v2", "done")?;
            meta_set(&tx, "fts_cargo_v3", "done")?;
            meta_set(&tx, "fts_cargo_verified_generation", &generation)?;
            tx.execute("DELETE FROM store_meta WHERE k IN ('fts_cargo_v1_seq','fts_cargo_v2_generation')", [])?;
            tx.commit()?;
            return Ok(true);
        }
        for (seq, content, tags, cargo) in &rows {
            tx.execute("DELETE FROM obs_fts_v1c WHERE rowid=?1", params![seq])?;
            tx.execute(
                "INSERT INTO obs_fts_v1c(rowid,content,tags,cargo) VALUES(?1,?2,?3,?4)",
                params![seq, content, tags, fts_cargo_text(cargo.as_deref())],
            )?;
        }
        meta_set(&tx, "fts_cargo_v1_seq", &rows[rows.len() - 1].0.to_string())?;
        tx.commit()?;
    }
}

/// A source evidence window for lexical matching. `pointer` names an RFC6901 cargo field;
/// `[start,end)` is in original UTF-8 bytes, including intervening punctuation/text.
/// Prefer the field covering most distinct highlighted words, then its shortest covering window.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MatchAt {
    pub match_in: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_pointer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_span: Option<MatchSpan>,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MatchSpan {
    pub start: usize,
    pub end: usize,
    pub unit: &'static str,
}

/// Markers must be absent from both source strings. Distinct control bytes at each end
/// make them unbordered: an adjacent literal source prefix cannot impersonate a marker.
fn match_markers(content: &str, cargo: &str) -> (String, String) {
    for n in 0u64.. {
        let open = format!("\u{1}oracle-fts-start-{n}\u{2}");
        let close = format!("\u{3}oracle-fts-end-{n}\u{4}");
        if [content, cargo].iter().all(|s| !s.contains(&open) && !s.contains(&close)) {
            return (open, close);
        }
    }
    unreachable!("finite source text cannot contain every marker")
}

/// Every marked range, in original UTF-8 bytes. Dynamic markers cannot occur in source.
fn marked_spans(h: &str, open: &str, close: &str) -> Vec<(usize, usize)> {
    let mut out = vec![]; let mut cursor = 0; let mut source = 0;
    while let Some(i) = h[cursor..].find(open) {
        source += i; let start = cursor + i + open.len();
        let Some(n) = h[start..].find(close) else { break };
        out.push((source, source + n)); source += n; cursor = start + n + close.len();
    }
    out
}

// Prefer the field covering the most distinct highlighted words, then its shortest
// source interval covering those words. Repeated common words cannot outvote a phrase.
// This is an evidence window, not a claim that every byte is a query token.
fn dense_span(text: &str, spans: &[(usize, usize)]) -> Option<(usize, usize, usize)> {
    let words: Vec<std::collections::BTreeSet<String>> = spans.iter().map(|&(s,e)| {
        text[s..e].split(|c: char| !c.is_alphanumeric()).filter(|s| !s.is_empty()).map(str::to_lowercase).collect()
    }).collect();
    let all: std::collections::BTreeSet<&String> = words.iter().flat_map(|w| w.iter()).collect();
    if all.is_empty() { return None }
    let mut counts = std::collections::BTreeMap::<&String, usize>::new();
    let mut left = 0; let mut best: Option<(usize,usize,usize)> = None;
    for (right, ws) in words.iter().enumerate() {
        for w in ws { *counts.entry(w).or_default() += 1; }
        while counts.len() == all.len() {
            let candidate = (spans[left].0, spans[right].1, all.len());
            if best.is_none_or(|(s,e,_)| candidate.1-candidate.0 < e-s) { best = Some(candidate); }
            for w in &words[left] {
                let n = counts.get_mut(w).unwrap(); *n -= 1;
                if *n == 0 { counts.remove(w); }
            }
            left += 1;
        }
    }
    best
}

/// Where message `seq` matches the FTS expression `expr`, or None for a meaning-only hit.
/// FTS5 does the matching; dynamic markers preserve literal control bytes and UTF-8 offsets.
pub fn match_at(c: &Connection, seq: i64, expr: &str) -> Result<Option<MatchAt>> {
    let (in_content, in_cargo, indexed_cargo, cargo, open, close) = loop {
        let source: Option<(String, String)> = c.query_row(
            "SELECT content,coalesce(cargo,'') FROM obs_fts WHERE rowid=?1",
            params![seq], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        let Some((content, indexed_cargo)) = source else { return Ok(None) };
        let (open, close) = match_markers(&content, &indexed_cargo);
        let row: Option<(String, String, String, String, Option<String>)> = c.query_row(
            "SELECT coalesce(highlight(obs_fts,0,?3,?4),''),coalesce(highlight(obs_fts,2,?3,?4),''), \
             content,coalesce(cargo,''),(SELECT cargo FROM observations WHERE seq=?2) \
             FROM obs_fts WHERE obs_fts MATCH ?1 AND rowid=?2",
            params![expr, seq, open, close], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        ).optional()?;
        let Some((in_content, in_cargo, content, indexed_cargo, cargo)) = row else { return Ok(None) };
        // This statement gives highlights and their actual sources in one snapshot.
        // If a concurrent repair introduced either chosen marker, choose again.
        if [&content, &indexed_cargo].iter().any(|s| s.contains(&open) || s.contains(&close)) {
            continue;
        }
        break (in_content, in_cargo, indexed_cargo, cargo, open, close);
    };
    let content_spans = marked_spans(&in_content, &open, &close);
    let cargo_spans = marked_spans(&in_cargo, &open, &close);
    // Decode the highlighted content from its actual statement snapshot, not a newer source.
    let content = in_content.replace(&open, "").replace(&close, "");
    let mut best = dense_span(&content, &content_spans).map(|(s,e,n)| ("content", None, s,e,n));
    let cargo_current = indexed_cargo == fts_cargo_text(cargo.as_deref());
    if cargo_current {
        let mut from = 0;
        for (pointer, text) in cargo_text_fields(cargo.as_deref()) {
            let spans: Vec<_> = cargo_spans.iter().filter(|&&(s,e)| s >= from && s < from+text.len() && e <= from+text.len())
                .map(|&(s,e)| (s-from,e-from)).collect();
            if let Some((s,e,n)) = dense_span(&text, &spans) {
                if best.as_ref().is_none_or(|(_,_,bs,be,bn)| n > *bn || (n == *bn && e-s < be-bs)) {
                    best = Some(("cargo",Some(pointer),s,e,n));
                }
            }
            from += text.len()+1;
        }
    }
    if let Some((match_in,match_pointer,start,end,_)) = best {
        return Ok(Some(MatchAt {match_in,match_pointer,match_span:Some(MatchSpan{start,end,unit:"utf8_bytes"})}));
    }
    if !cargo_spans.is_empty() {
        // Stale indexed cargo or a phrase spanning fields: no fabricated source location.
        return Ok(Some(MatchAt {match_in:"cargo",match_pointer:None,match_span:None}));
    }
    Ok(Some(MatchAt {match_in:"tags",match_pointer:None,match_span:None}))
}

/// Does this text offer a choice? Two or more numbered options (`1)`, `2.`, `Option 2`, `(b)`) and a word that asks to pick
/// or recommends one. Numbers inside words or decimals (`v1.2`, `0.966`) are not options.
pub(crate) fn offers_choice(content: &str) -> bool {
    let lower = content.to_lowercase();
    let asks = ["recommend", "i'd pick", "i would pick", "which do you", "which one", "pick one", "your call", "choose", "options:"]
        .iter()
        .any(|w| lower.contains(w));
    if !asks {
        return false;
    }
    let b = lower.as_bytes();
    let mut seen = [false; 7];
    for i in 0..b.len() {
        let starts = i == 0 || matches!(b[i - 1], b' ' | b'\n' | b'\t' | b'(' | b':' | b';');
        if !starts {
            continue;
        }
        // 1) 2. (a) (b)
        if b[i].is_ascii_digit() && i + 2 < b.len() && matches!(b[i + 1], b')' | b'.') && b[i + 2] == b' ' {
            let n = (b[i] - b'0') as usize;
            if (1..=6).contains(&n) {
                seen[n] = true;
            }
        }
        if b[i] == b'(' && i + 2 < b.len() && (b'a'..=b'f').contains(&b[i + 1]) && b[i + 2] == b')' {
            seen[(b[i + 1] - b'a' + 1) as usize] = true;
        }
        if b[i..].starts_with(b"option ") {
            if let Some(&d) = b.get(i + 7) {
                let n = if d.is_ascii_digit() { (d - b'0') as usize } else if (b'a'..=b'f').contains(&d) { (d - b'a' + 1) as usize } else { 0 };
                if (1..=6).contains(&n) && b.get(i + 8).is_none_or(|c| !c.is_ascii_alphanumeric()) {
                    seen[n] = true;
                }
            }
        }
    }
    seen[1] && seen[2]
}

pub(crate) fn mentions(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len()
                && (bytes[j].is_ascii_lowercase()
                    || bytes[j].is_ascii_digit()
                    || bytes[j] == b'-'
                    || bytes[j] == b'_')
            {
                j += 1;
            }
            if j > start && j - start <= 64 {
                out.push(format!("mention:{}", &content[start..j]));
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Safe lexical units: bare words and double-quoted phrases, at most 24 words.
/// No user syntax is executed. An oversized quoted phrase is refused as an empty query,
/// never silently clipped into a different phrase; bare queries retain the existing cap.
fn lexical_units(q: &str, phrases: bool) -> Vec<String> {
    let mut units = vec![]; let mut left = 24;
    for (i, part) in q.split('"').enumerate() {
        let words: Vec<_> = part.split(|c: char| !c.is_alphanumeric()).filter(|s| !s.is_empty()).take(25).map(str::to_lowercase).collect();
        if phrases && i%2 == 1 {
            if words.len() > left { return vec![] }
            if !words.is_empty() { left -= words.len(); units.push(format!("\"{}\"",words.join(" "))); }
        } else {
            for w in words.into_iter().take(left) { units.push(format!("\"{w}\"")); left -= 1; }
        }
        if left == 0 { break }
    }
    units.dedup(); units
}
pub(crate) fn fts_query(q: &str, joiner: &str) -> String { lexical_units(q,true).join(joiner) }

/// Score given to an exact identifier match: above any keyword or fused score.
const ALIAS_SCORE: f64 = 1.0e9;

/// Observations a query names by identifier: the whole id (`OBS-mathematics-20001007-3be83c3f82ba`,
/// `msg-<uuid>`), or a lowercase hex tail of at least 12 characters (`3be83c3f82ba`), which ends an id after
/// its last '-'. Same room/kind filters as search; exact ids first, then newest.
fn id_alias(c: &Connection, query: &str, channel: Option<&str>, kind: Option<&str>, limit: i64) -> Result<Vec<i64>> {
    let q = query.trim();
    if q.is_empty() || q.chars().any(char::is_whitespace) {
        return Ok(vec![]);
    }
    let tail = q.rsplit('-').next().unwrap_or(q);
    let hex = tail.len() >= 12 && tail.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !hex && !q.contains('-') {
        return Ok(vec![]);
    }
    let mut st = c.prepare_cached(
        "SELECT seq FROM observations WHERE (id=?1 OR (?2 IS NOT NULL AND id LIKE '%-' || ?2)) \
         AND (?3 IS NULL OR channel=?3) AND (?4 IS NULL OR kind=?4) ORDER BY (id=?1) DESC, seq DESC LIMIT ?5",
    )?;
    let tail = hex.then_some(tail);
    let found = st
        .query_map(params![q, tail, channel, kind, limit], |r| r.get(0))?
        .collect::<std::result::Result<Vec<i64>, _>>()?;
    Ok(found)
}
fn relaxed_query(q: &str, joiner: &str) -> String { lexical_units(q,false).join(joiner) }

impl Store {
    pub fn open(path: &str) -> Result<Store> {
        let manager = SqliteConnectionManager::file(path).with_init(|c| {
            c.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000; \
                 PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY;",
            )
        });
        let pool = r2d2::Pool::builder().max_size(8).build(manager)?;
        {
            let c = pool.get()?;
            migrate_keys(&c)?;
            c.execute_batch(SCHEMA)?;
            migrate_authored(&c)?;
            migrate_prev(&c)?;
            migrate_expires(&c)?;
            crate::sync::migrate(&c)?;
            c.execute_batch(ARCHIVE_SCHEMA)?;
            backfill_cargo_links(&c)?;
            fts_cargo_backfill(&c, FTS_CARGO_BATCH, None)?;
            crate::roomindex::init(&c)?;
            c.execute_batch(crate::rooms::SCHEMA)?;
            c.execute_batch(GATEWAY_SCHEMA)?;
            c.execute_batch(crate::oauth::SCHEMA)?;
            c.execute_batch(crate::files::SCHEMA)?;
            c.execute_batch(crate::publicseats::SCHEMA)?;
        }
        {
            let c = pool.get()?;
            refresh_room_archive(&c)?;
        }
        Ok(Store { pool, cites: std::sync::Mutex::new(None), minds: std::sync::Mutex::new(None), public: std::sync::RwLock::new(PublicConfig::from_env()) })
    }

    /// A private in-memory database (one connection, so every call sees the same data).
    pub fn memory() -> Result<Store> {
        let pool = r2d2::Pool::builder()
            .max_size(1)
            .build(SqliteConnectionManager::memory())?;
        {
            let c = pool.get()?;
            migrate_keys(&c)?;
            c.execute_batch(SCHEMA)?;
            migrate_authored(&c)?;
            migrate_prev(&c)?;
            migrate_expires(&c)?;
            crate::sync::migrate(&c)?;
            c.execute_batch(ARCHIVE_SCHEMA)?;
            backfill_cargo_links(&c)?;
            fts_cargo_backfill(&c, FTS_CARGO_BATCH, None)?;
            crate::roomindex::init(&c)?;
            c.execute_batch(crate::rooms::SCHEMA)?;
            c.execute_batch(GATEWAY_SCHEMA)?;
            c.execute_batch(crate::oauth::SCHEMA)?;
            c.execute_batch(crate::files::SCHEMA)?;
            c.execute_batch(crate::publicseats::SCHEMA)?;
        }
        Ok(Store { pool, cites: std::sync::Mutex::new(None), minds: std::sync::Mutex::new(None), public: std::sync::RwLock::new(PublicConfig::from_env()) })
    }

    pub(crate) fn conn(&self) -> Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        Ok(self.pool.get()?)
    }

    /// Register a service identity as a gateway for some of the cross-seat routes. The handle must be a seat with an active key.
    pub fn gateway_add(&self, handle: &str, routes: &[String], now_ms: i64) -> Result<()> {
        if self.active_keys(handle)?.is_empty() {
            return Err(StoreError::Rejected(format!("{handle} is not a seat with an active key")));
        }
        if routes.is_empty() {
            return Err(StoreError::Rejected("a gateway needs at least one route".into()));
        }
        if let Some(bad) = routes.iter().find(|r| !CROSS_SEAT_ROUTES.contains(&r.as_str()) && r.as_str() != ATTEST_ROUTE) {
            return Err(StoreError::Rejected(format!("unknown route {bad}; use {} or {ATTEST_ROUTE}", CROSS_SEAT_ROUTES.join(", "))));
        }
        self.conn()?.execute(
            "INSERT INTO gateways(handle,routes,created_ms) VALUES(?1,?2,?3) ON CONFLICT(handle) DO UPDATE SET routes=excluded.routes",
            params![handle, routes.join(","), now_ms],
        )?;
        Ok(())
    }

    pub fn gateway_remove(&self, handle: &str) -> Result<bool> {
        Ok(self.conn()?.execute("DELETE FROM gateways WHERE handle=?1", params![handle])? > 0)
    }

    pub fn gateways(&self) -> Result<Vec<(String, Vec<String>)>> {
        let c = self.conn()?;
        let mut st = c.prepare("SELECT handle, routes FROM gateways ORDER BY handle")?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().map(|(h, r)| (h, r.split(',').map(str::to_owned).collect())).collect())
    }

    /// May `caller` read `target`'s `route`? A seat may always read its own. Anyone else needs a gateway registration for that route. Every cross-seat
    /// request is audited. With `enforce` off the policy is only recorded (audit mode: what enforcing would refuse, before it does).
    pub fn cross_seat(&self, caller: &str, target: &str, route: &str, enforce: bool, now_ms: i64) -> Result<()> {
        if caller == target {
            return Ok(());
        }
        let c = self.conn()?;
        // A seat and a name it once used are one seat: reading the lists of your own old label is reading your own.
        if crate::alias::same(&crate::roomindex::aliases(&c).unwrap_or_default(), caller, target) {
            return Ok(());
        }
        let routes: Option<String> = c.query_row("SELECT routes FROM gateways WHERE handle=?1", params![caller], |r| r.get(0)).optional()?;
        let permitted = routes.is_some_and(|r| r.split(',').any(|x| x == route));
        c.execute(
            "INSERT INTO access_audit(at_ms,caller,route,target,permitted,enforced) VALUES(?1,?2,?3,?4,?5,?6)",
            params![now_ms, caller, route, target, permitted as i64, (enforce && !permitted) as i64],
        )?;
        if enforce && !permitted {
            return Err(StoreError::Forbidden(format!("{caller} may not read {target}'s {route}: only that seat's own token, or a registered gateway")));
        }
        Ok(())
    }

    /// Read as another seat (docs/VIEW-AS.md): allowed for an admin token or a seat holding the owner key, for a seat with
    /// an active key; the answer is that seat with read scope only, and the request is audited as `view-as`.
    pub fn view_as(&self, caller: &str, admin: bool, seat: &str, now_ms: i64) -> Result<Principal> {
        let owner = self.active_keys(caller)?.iter().any(|(_, role)| role == "owner");
        if !(admin || owner) {
            return Err(StoreError::Forbidden(format!("only the owner or an admin token may view as another seat, not '{caller}'")));
        }
        if self.active_keys(seat)?.is_empty() {
            return Err(StoreError::Rejected(format!("no seat '{seat}' with an active key to view as")));
        }
        self.conn()?.execute(
            "INSERT INTO access_audit(at_ms,caller,route,target,permitted,enforced) VALUES(?1,?2,'view-as',?3,1,0)",
            params![now_ms, caller, seat],
        )?;
        Ok(Principal { handle: seat.to_string(), scope: Scope::Read })
    }

    /// The newest cross-seat requests.
    pub fn cross_seat_audit(&self, limit: i64) -> Result<Vec<AuditRow>> {
        let c = self.conn()?;
        let mut st = c.prepare("SELECT at_ms, caller, route, target, permitted, enforced FROM access_audit ORDER BY seq DESC LIMIT ?1")?;
        let rows = st
            .query_map(params![limit.clamp(1, 1000)], |r| {
                Ok(AuditRow { at_ms: r.get(0)?, caller: r.get(1)?, route: r.get(2)?, target: r.get(3)?, permitted: r.get::<_, i64>(4)? != 0, enforced: r.get::<_, i64>(5)? != 0 })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Run a read on one connection: for tests and tools that look at a table directly.
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let c = self.conn()?;
        f(&c)
    }

    /// The highest seq written, 0 when empty: a reader's default exclusive cursor is this + 1.
    pub fn newest_seq(&self) -> Result<i64> {
        self.with_conn(|c| Ok(c.query_row("SELECT COALESCE(MAX(seq),0) FROM observations", [], |r| r.get::<_, i64>(0))?))
    }

    /// The seat aliases in force: `label → seat`, from the newest valid owner-signed `seat-alias/1` record (empty until the room index is built).
    /// Every name a mention can address: each handle that has held a key (revoked or not; history keeps its names) and
    /// every alias label. An @word in prose that is none of these ("the @mention", "@everyone") addresses nobody.
    pub fn seat_names(&self) -> Result<std::collections::BTreeSet<String>> {
        let mut out: std::collections::BTreeSet<String> = self
            .with_conn(|c| {
                let mut st = c.prepare_cached("SELECT DISTINCT handle FROM keys")?;
                let rows = st.query_map([], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })?
            .into_iter()
            .collect();
        out.extend(self.seat_aliases().into_keys());
        Ok(out)
    }

    /// The seat-rooms table in force (seatrooms.rs): empty when no owner record has been written.
    pub fn seat_scope_table(&self) -> Result<crate::seatrooms::Table> {
        let v: Option<String> = self.conn()?.query_row("SELECT table_json FROM seat_scope WHERE id=1", [], |x| x.get(0)).optional()?;
        // An unreadable stored table is an error, never an empty one: an empty table would unscope every seat (Emmy, 22 Oct).
        match v {
            None => Ok(Default::default()),
            Some(v) => serde_json::from_str(&v).map_err(|e| StoreError::Internal(format!("seat scope table unreadable: {e}"))),
        }
    }

    /// Tests only: put raw bytes in the scope row, to prove an unreadable table fails closed.
    #[doc(hidden)]
    pub fn put_seat_scope_raw_for_tests(&self, raw: &str) -> Result<()> {
        self.conn()?.execute("INSERT INTO seat_scope(id,table_json,source_id,updated_ms) VALUES(1,?1,'test',0) ON CONFLICT(id) DO UPDATE SET table_json=excluded.table_json", params![raw])?;
        Ok(())
    }

    /// The private-rooms table in force (privaterooms.rs): empty when no owner record has been written.
    pub fn private_rooms_table(&self) -> Result<crate::privaterooms::Table> {
        let v: Option<String> = self.conn()?.query_row("SELECT table_json FROM private_rooms WHERE id=1", [], |x| x.get(0)).optional()?;
        // Unreadable is an error, never empty: an empty table would open every private room.
        match v {
            None => Ok(Default::default()),
            Some(v) => serde_json::from_str(&v).map_err(|e| StoreError::Internal(format!("private rooms table unreadable: {e}"))),
        }
    }

    /// When a stored record leaves the view (retention.rs; docs/ARCHIVE.md), or None when it stays.
    pub fn expires_ms_of(&self, id: &str) -> Result<Option<i64>> {
        Ok(self.conn()?.query_row("SELECT expires_ms FROM observations WHERE id=?1", params![id], |r| r.get(0)).optional()?.flatten())
    }

    /// The retention table in force (retention.rs): empty when no owner record has been written.
    pub fn retention_table(&self) -> Result<crate::retention::Table> {
        let c = self.conn()?;
        retention_in(&c)
    }

    /// Expired records of the rooms with a retention rule: how many per room, and, with `apply`, removed from the store and its
    /// indexes (retention.rs). Rows that predate their room's rule are given their expiry first, so a rule covers the whole room.
    /// The work is done in short immediate transactions of a few hundred rows, each retried while the live server holds the write
    /// lock (27 Oct: one long transaction against the serving database answered "busy" five times and removed nothing).
    pub fn sweep_expired(&self, now_ms: i64, apply: bool) -> Result<Vec<(String, usize)>> {
        const BATCH: usize = 300;
        let mut c = self.conn()?;
        let table = retention_in(&c)?;
        let mut out = Vec::new();
        for (room, rule) in &table {
            let ttl = i64::from(rule.ttl_hours) * 3_600_000;
            let backfill = |c: &mut Connection| -> Result<()> {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                tx.execute("UPDATE observations SET expires_ms = created_ms + ?2 WHERE channel = ?1 AND expires_ms IS NULL", params![room, ttl])?;
                tx.commit()?;
                Ok(())
            };
            retry_busy(|| backfill(&mut c))?;
            let count: usize = c
                .query_row("SELECT count(*) FROM observations WHERE channel = ?1 AND expires_ms IS NOT NULL AND expires_ms < ?2", params![room, now_ms], |r| r.get::<_, i64>(0))?
                as usize;
            if apply {
                let mut removed = 0usize;
                loop {
                    let n = retry_busy(|| sweep_batch(&mut c, room, now_ms, BATCH))?;
                    removed += n;
                    if n < BATCH {
                        break;
                    }
                }
                out.push((room.clone(), removed));
            } else {
                out.push((room.clone(), count));
            }
        }
        Ok(out)
    }

    /// Tests only: put raw bytes in the private-rooms row, to prove an unreadable table fails closed.
    #[doc(hidden)]
    pub fn put_private_rooms_raw_for_tests(&self, raw: &str) -> Result<()> {
        self.conn()?.execute("INSERT INTO private_rooms(id,table_json,source_id,updated_ms) VALUES(1,?1,'test',0) ON CONFLICT(id) DO UPDATE SET table_json=excluded.table_json", params![raw])?;
        Ok(())
    }

    /// A seat's rooms when it is scoped, `None` when it is not.
    pub fn seat_rooms_for(&self, handle: &str) -> Result<Option<crate::seatrooms::Rooms>> {
        // A public seat is confined by the store itself to ORACLE_PUBLIC_WRITE_ROOMS, whatever the scope table says.
        if crate::publicseats::is_public_handle(handle) {
            let rooms: std::collections::BTreeSet<String> = self.public_config().write_rooms.into_iter().collect();
            return Ok(Some(crate::seatrooms::Rooms { write: rooms, read: Default::default() }));
        }
        Ok(self.seat_scope_table()?.remove(handle))
    }

    pub fn public_config(&self) -> PublicConfig {
        self.public.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Set which rooms take public seats' writes and the limits (tests, and an owner's reload).
    pub fn set_public_config(&self, cfg: PublicConfig) {
        if let Ok(mut g) = self.public.write() {
            *g = cfg;
        }
    }

    /// A challenge for `POST /public/register`: a one-time nonce, good for ten minutes. Not found while no room takes
    /// public writes (the feature ships dark).
    pub fn public_challenge(&self, now_ms: i64) -> Result<String> {
        if self.public_config().write_rooms.is_empty() {
            return Err(StoreError::NotFound);
        }
        let c = self.conn()?;
        Ok(crate::publicseats::new_challenge(&c, now_ms)?)
    }

    /// Mint a public seat for the holder of `pubkey_hex`, who signed the challenge, and a write token for it. The same key
    /// is the same seat: registering again gives a fresh token, and a seat keeps at most `tokens_per_seat` live tokens
    /// (the oldest are revoked). `ip` is used for the registration limits only and is not kept past their window.
    /// Returns (handle, token, name).
    pub fn register_public(&self, pubkey_hex: &str, nonce: &str, signature_hex: &str, name: &str, ip: &str, now_ms: i64) -> Result<(String, String, String)> {
        let cfg = self.public_config();
        if cfg.write_rooms.is_empty() {
            return Err(StoreError::NotFound);
        }
        let name = crate::publicseats::clean_name(name).map_err(StoreError::Rejected)?;
        let pk = pubkey_hex.trim().to_lowercase();
        let vk = oracle_proto::parse_pubkey(&pk).map_err(|_| StoreError::Rejected("pubkey must be a 32-byte ed25519 key in hex".into()))?;
        let sig_hex = signature_hex.trim();
        let sig_bytes = (0..sig_hex.len())
            .step_by(2)
            .map(|i| sig_hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
            .collect::<Option<Vec<u8>>>()
            .filter(|b| b.len() == 64)
            .ok_or(StoreError::Rejected("signature must be 64 bytes in hex".into()))?;
        let sig = ed25519_dalek::Signature::from_slice(&sig_bytes).map_err(|_| StoreError::Rejected("signature unreadable".into()))?;
        let handle = crate::publicseats::handle_for(&pk);
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // The nonce is spent whatever happens next: one challenge, one try.
        if !crate::publicseats::take_challenge(&tx, nonce, now_ms)? {
            tx.commit()?;
            return Err(StoreError::Unauthorized("challenge unknown, spent or expired: ask for a new one"));
        }
        {
            use ed25519_dalek::Verifier;
            if vk.verify(&crate::publicseats::challenge_message(nonce, &pk), &sig).is_err() {
                tx.commit()?;
                return Err(StoreError::Unauthorized("signature does not match the challenge and key"));
            }
        }
        // Registry handles stay the registry's: a public handle can never shadow a seat the owner made.
        let in_registry: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM keys WHERE handle=?1)", params![handle], |r| r.get(0))?;
        if in_registry {
            tx.commit()?;
            return Err(StoreError::Conflict("that handle belongs to a registry seat".into()));
        }
        let existing: Option<Option<i64>> =
            tx.query_row("SELECT revoked_ms FROM public_seats WHERE handle=?1", params![handle], |r| r.get(0)).optional()?;
        match existing {
            Some(Some(_)) => {
                tx.commit()?;
                return Err(StoreError::Forbidden("this key's seat was revoked".into()));
            }
            Some(None) => {
                tx.execute("UPDATE public_seats SET name=?2 WHERE handle=?1", params![handle, name])?;
            }
            None => {
                if let Err((why, retry_after_ms)) = crate::publicseats::admit(&tx, &crate::publicseats::register_buckets(&cfg.limits, ip), now_ms)? {
                    tx.commit()?;
                    return Err(StoreError::Limited { why, retry_after_ms });
                }
                tx.execute("INSERT INTO public_seats(handle,pubkey,name,created_ms) VALUES(?1,?2,?3,?4)", params![handle, pk, name, now_ms])?;
            }
        }
        let token = format!("orc_{}", oracle_proto::random_hex(32));
        tx.execute(
            "INSERT INTO tokens(hash,handle,scope,label,created_ms) VALUES(?1,?2,'write','public-register',?3)",
            params![sha_hex(&token), handle, now_ms],
        )?;
        tx.execute(
            "UPDATE tokens SET revoked_ms=?2 WHERE handle=?1 AND revoked_ms IS NULL AND hash NOT IN
               (SELECT hash FROM tokens WHERE handle=?1 AND revoked_ms IS NULL ORDER BY created_ms DESC, rowid DESC LIMIT ?3)",
            params![handle, now_ms, cfg.limits.tokens_per_seat],
        )?;
        tx.commit()?;
        Ok((handle, token, name))
    }

    /// Revoke a public seat and every token it holds (abuse handling, by the owner). False when there was none to revoke.
    pub fn revoke_public(&self, handle: &str, now_ms: i64) -> Result<bool> {
        if !crate::publicseats::is_public_handle(handle) {
            return Ok(false);
        }
        let c = self.conn()?;
        let n = c.execute("UPDATE public_seats SET revoked_ms=?2 WHERE handle=?1 AND revoked_ms IS NULL", params![handle, now_ms])?;
        c.execute("UPDATE tokens SET revoked_ms=?2 WHERE handle=?1 AND revoked_ms IS NULL", params![handle, now_ms])?;
        Ok(n > 0)
    }

    pub fn seat_aliases(&self) -> crate::alias::Aliases {
        self.with_conn(crate::roomindex::aliases).unwrap_or_default()
    }

    /// Has the room index been built (see `oracle reindex`)?
    pub fn index_ready(&self) -> bool {
        self.with_conn(crate::roomindex::ready).unwrap_or(false)
    }

    /// Apply the live-state rule to the rows these rooms already have (`oracle compact-live`); with `dry`, only count.
    pub fn compact_live(&self, rooms: &[String], dry: bool) -> Result<Vec<crate::livestate::Compacted>> {
        let mut c = self.conn()?;
        crate::livestate::compact(&mut c, rooms, dry)
    }

    /// Build the room index again from the log (`oracle reindex`).
    pub fn reindex(&self) -> Result<crate::roomindex::ReindexStats> {
        let mut c = self.conn()?;
        crate::roomindex::reindex(&mut c)
    }

    /// Every index row as text, for comparing two builds.
    pub fn index_dump(&self) -> Result<Vec<(String, Vec<String>)>> {
        crate::roomindex::dump(&*self.conn()?)
    }

    pub fn ping(&self) -> Result<()> {
        self.conn()?.query_row("SELECT 1", [], |_| Ok(()))?;
        Ok(())
    }

    // ── registry ────────────────────────────────────────────────────────────────

    /// Create the owner seat. Only possible on an empty registry: this is the root of trust.
    pub fn init_owner(&self, handle: &str, pubkey: &str, now_ms: i64) -> Result<()> {
        oracle_proto::parse_pubkey(pubkey)?;
        let c = self.conn()?;
        let existing: i64 = c.query_row("SELECT count(*) FROM keys", [], |r| r.get(0))?;
        if existing > 0 {
            return Err(StoreError::Forbidden("registry already initialised".into()));
        }
        c.execute(
            "INSERT INTO keys(handle,pubkey,role,created_ms) VALUES(?1,?2,'owner',?3)",
            params![handle, pubkey, now_ms],
        )?;
        Ok(())
    }

    /// The seat's first active key (its oldest device) — enough to ask "is this seat alive".
    pub fn active_key(&self, handle: &str) -> Result<Option<(String, String)>> {
        Ok(self.active_keys(handle)?.into_iter().next())
    }

    /// Every key the seat may sign with right now, oldest first: `(pubkey, role)`.
    /// `person` when the handle holds an owner key, else `model` (docs/SEARCH.md).
    pub fn author_kind(&self, handle: &str) -> &'static str {
        match self.conn() {
            Ok(c) => author_kind_on(&c, handle),
            Err(_) => "model",
        }
    }

    pub fn active_keys(&self, handle: &str) -> Result<Vec<(String, String)>> {
        let c = self.conn()?;
        let mut st = c.prepare_cached("SELECT pubkey, role FROM keys WHERE handle=?1 AND revoked_ms IS NULL ORDER BY role='owner' DESC, created_ms, pubkey")?;
        let mut rows = st
            .query_map(params![handle], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // A public seat (publicseats.rs) holds its one key in its own table, never in the owner-signed registry.
        if rows.is_empty() {
            if let Some(pk) = crate::publicseats::active_pubkey(&c, handle)? {
                rows.push((pk, "public".to_string()));
            }
        }
        Ok(rows)
    }

    pub fn keys(&self) -> Result<Vec<KeyRow>> {
        let c = self.conn()?;
        let mut st =
            c.prepare("SELECT handle,pubkey,role,created_ms,revoked_ms FROM keys ORDER BY handle")?;
        let rows = st
            .query_map([], |r| {
                Ok(KeyRow {
                    handle: r.get(0)?,
                    pubkey: r.get(1)?,
                    role: r.get(2)?,
                    created_ms: r.get(3)?,
                    revoked_ms: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn key_events(&self) -> Result<Vec<serde_json::Value>> {
        let c = self.conn()?;
        let mut st = c.prepare("SELECT event_json FROM key_events ORDER BY seq")?;
        let rows = st
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|j| j.ok().and_then(|j| serde_json::from_str(&j).ok()))
            .collect();
        Ok(rows)
    }

    /// Apply a registry change. The signer must be the owner, or the seat changing its own
    /// key. There is no unsigned path and no configuration override.
    pub fn apply_key_event(&self, ev: &KeyEvent, now_ms: i64) -> Result<()> {
        ev.validate()?;
        // Which of the signer's keys made this signature decides what it may do.
        let signer_keys = self.active_keys(&ev.signer)?;
        if signer_keys.is_empty() {
            return Err(StoreError::Unauthorized("signer has no active key"));
        }
        let (signer_pk, signer_role) = signer_keys
            .iter()
            .find(|(pk, _)| ev.verify(pk, now_ms).is_ok())
            .cloned()
            .ok_or_else(|| match ev.verify(&signer_keys[0].0, now_ms) {
                Err(e) => StoreError::from(e),
                Ok(()) => StoreError::Unauthorized("bad signature"),
            })?;

        let is_owner = signer_role == "owner";
        let is_self = ev.signer == ev.handle;
        let target = self.active_keys(&ev.handle)?;
        let target_has = |pk: &str| target.iter().any(|(k, _)| k == pk);
        let role_of = |pk: &str| {
            target
                .iter()
                .find(|(k, _)| k == pk)
                .map(|(_, r)| r.as_str())
        };
        match ev.action {
            KeyAction::Add => {
                // The owner adds seats. A seat may add another device for itself.
                if !(is_owner || (is_self && !target.is_empty())) {
                    return Err(StoreError::Forbidden(
                        "only the owner adds seats; a seat may add a key for itself".into(),
                    ));
                }
                if target_has(&ev.pubkey) {
                    return Err(StoreError::Rejected(
                        "this key is already registered for this seat".into(),
                    ));
                }
                if target.len() >= MAX_KEYS_PER_SEAT {
                    return Err(StoreError::Rejected(format!(
                        "a seat holds at most {MAX_KEYS_PER_SEAT} keys; revoke one first"
                    )));
                }
                let taken: Option<String> = self
                    .conn()?
                    .query_row("SELECT handle FROM keys WHERE pubkey=?1 AND revoked_ms IS NULL AND handle<>?2", params![ev.pubkey, ev.handle], |r| r.get(0))
                    .optional()?;
                if let Some(other) = taken {
                    return Err(StoreError::Rejected(format!(
                        "this key already belongs to '{other}': one key, one identity"
                    )));
                }
            }
            KeyAction::Rotate | KeyAction::Revoke => {
                if !(is_owner || is_self) {
                    return Err(StoreError::Forbidden(
                        "only the owner or the seat itself".into(),
                    ));
                }
                if target.is_empty() {
                    return Err(StoreError::NotFound);
                }
                if ev.action == KeyAction::Revoke {
                    let doomed: Vec<&str> = if ev.pubkey.is_empty() {
                        target.iter().map(|(k, _)| k.as_str()).collect()
                    } else {
                        vec![ev.pubkey.as_str()]
                    };
                    if !ev.pubkey.is_empty() && !target_has(&ev.pubkey) {
                        return Err(StoreError::NotFound);
                    }
                    if doomed.iter().any(|k| role_of(k) == Some("owner")) {
                        return Err(StoreError::Forbidden(
                            "the owner key can be rotated, not revoked".into(),
                        ));
                    }
                } else if !is_self && target.iter().any(|(_, r)| r == "owner") {
                    return Err(StoreError::Forbidden(
                        "only the owner key rotates itself".into(),
                    ));
                }
            }
        }

        let mut c = self.conn()?;
        let tx = c.transaction()?;
        let fresh = tx.execute(
            "INSERT OR IGNORE INTO key_events(event_json,nonce,created_ms) VALUES(?1,?2,?3)",
            params![
                serde_json::to_string(ev).map_err(|e| StoreError::Internal(e.to_string()))?,
                ev.nonce,
                now_ms
            ],
        )?;
        if fresh == 0 {
            return Err(StoreError::Unauthorized("replayed key event"));
        }
        let put = |tx: &rusqlite::Transaction, role: &str| -> Result<()> {
            tx.execute(
                "INSERT INTO keys(handle,pubkey,role,created_ms) VALUES(?1,?2,?3,?4) \
                 ON CONFLICT(handle,pubkey) DO UPDATE SET revoked_ms=NULL, created_ms=excluded.created_ms, role=excluded.role",
                params![ev.handle, ev.pubkey, role, now_ms],
            )?;
            Ok(())
        };
        match ev.action {
            KeyAction::Add => put(&tx, "seat")?,
            KeyAction::Rotate if is_self => {
                // The key that signed is the one being replaced; it hands on its own role.
                tx.execute(
                    "UPDATE keys SET revoked_ms=?3 WHERE handle=?1 AND pubkey=?2",
                    params![ev.handle, signer_pk, now_ms],
                )?;
                put(&tx, &signer_role)?;
            }
            KeyAction::Rotate => {
                // The owner resets a seat: every key it had is gone, this one remains.
                tx.execute(
                    "UPDATE keys SET revoked_ms=?2 WHERE handle=?1 AND revoked_ms IS NULL",
                    params![ev.handle, now_ms],
                )?;
                put(&tx, "seat")?;
            }
            KeyAction::Revoke => {
                if ev.pubkey.is_empty() {
                    tx.execute(
                        "UPDATE keys SET revoked_ms=?2 WHERE handle=?1 AND revoked_ms IS NULL",
                        params![ev.handle, now_ms],
                    )?;
                } else {
                    tx.execute(
                        "UPDATE keys SET revoked_ms=?3 WHERE handle=?1 AND pubkey=?2",
                        params![ev.handle, ev.pubkey, now_ms],
                    )?;
                }
                // Tokens die with the seat's LAST key, not with one of several devices.
                let left: i64 = tx.query_row(
                    "SELECT count(*) FROM keys WHERE handle=?1 AND revoked_ms IS NULL",
                    params![ev.handle],
                    |r| r.get(0),
                )?;
                if left == 0 {
                    tx.execute(
                        "UPDATE tokens SET revoked_ms=?2 WHERE handle=?1 AND revoked_ms IS NULL",
                        params![ev.handle, now_ms],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mint_token(
        &self,
        handle: &str,
        scope: Scope,
        label: &str,
        now_ms: i64,
    ) -> Result<String> {
        if self.active_key(handle)?.is_none() {
            return Err(StoreError::Rejected(format!("no active seat '{handle}'")));
        }
        let token = format!("orc_{}", oracle_proto::random_hex(32));
        self.conn()?.execute(
            "INSERT INTO tokens(hash,handle,scope,label,created_ms) VALUES(?1,?2,?3,?4,?5)",
            params![sha_hex(&token), handle, scope.as_str(), label, now_ms],
        )?;
        Ok(token)
    }

    pub fn authenticate(&self, token: &str) -> Result<Principal> {
        let row: Option<(String, String)> = self
            .conn()?
            .query_row(
                "SELECT handle, scope FROM tokens WHERE hash=?1 AND revoked_ms IS NULL",
                params![sha_hex(token)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (handle, scope) = row.ok_or(StoreError::Unauthorized("unknown or revoked token"))?;
        Ok(Principal {
            handle,
            scope: Scope::parse(&scope).unwrap_or(Scope::Read),
        })
    }

    // ── write ───────────────────────────────────────────────────────────────────

    /// The one write path. Verifies, then stores atomically.
    pub fn write(&self, who: &Principal, env: &Envelope, now_ms: i64) -> Result<WriteReceipt> {
        if who.scope < Scope::Write {
            return Err(StoreError::Forbidden("token has no write scope".into()));
        }
        // A scoped seat (seatrooms.rs) writes in its write rooms only, whichever route the write came through.
        if let Some(rooms) = self.seat_rooms_for(&who.handle)? {
            if !rooms.may_write(&env.channel) {
                return Err(StoreError::Forbidden(format!("{} writes only in {}", who.handle, rooms.write.iter().map(|r| format!("#{r}")).collect::<Vec<_>>().join(", "))));
            }
        }
        // A private room (privaterooms.rs) takes writes from its members only.
        if let Some(members) = self.private_rooms_table()?.get(&env.channel) {
            if !members.contains(&who.handle) {
                return Err(StoreError::Forbidden(format!("#{} is private to its members", env.channel)));
            }
        }
        // `signed:ok` and `cargo-signed:ok` are an importer's attestation on an imported row (envelope 1). A live message
        // is verified by its signature when it is stored, so it may not carry them: a reader would show it as attested.
        if let Some(t) = env.tags.iter().find(|t| ATTESTATION_TAGS.contains(&t.as_str())) {
            return Err(StoreError::Rejected(format!("{t} is an import attestation; a live message is verified by its own signature")));
        }
        // A secret is refused before anything else is read or stored (secrets.rs): a record is kept for good.
        if let Some(why) = crate::secrets::refusal(&env.content, env.cargo.as_deref(), &env.tags) {
            return Err(StoreError::Rejected(why));
        }
        if who.handle != env.author {
            return Err(StoreError::Forbidden(format!(
                "token belongs to '{}' but the envelope is authored by '{}'",
                who.handle, env.author
            )));
        }
        // Any of the seat's devices may have signed. The one that did is recorded with the row.
        let keys = self.active_keys(&env.author)?;
        if keys.is_empty() {
            return Err(StoreError::Unauthorized(
                "author has no active key in the registry",
            ));
        }
        let pubkey = match keys.iter().find(|(pk, _)| env.verify(pk, now_ms).is_ok()) {
            Some((pk, _)) => pk.clone(),
            None => {
                env.verify(&keys[0].0, now_ms)?; // the precise reason: stale, malformed, or bad signature
                return Err(StoreError::Unauthorized(
                    "signature matches none of this seat's keys",
                ));
            }
        };

        let state = if env.kind == Kind::State {
            Some(parse_state_cargo(env)?)
        } else {
            None
        };
        if env.kind == Kind::Annotate {
            self.check_annotation(env)?;
        }
        check_item_tags(env)?;
        self.check_ask_tags(env)?;

        let mut c = self.conn()?;
        // The write lock is taken up front: two writers that read the same State head queue behind each other (busy_timeout) and the second SEES the first,
        // instead of failing on a stale snapshot when a deferred transaction tries to upgrade.
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        // Idempotency first: a retry of a delivered write is a success, not a replay.
        if let Some((id, seq, author, channel)) = tx
            .query_row(
                "SELECT id, seq, author, channel FROM observations WHERE client_op=?1",
                params![env.client_op],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)),
            )
            .optional()?
        {
            if author != env.author || channel != env.channel {
                return Err(StoreError::Conflict("operation ID already used outside this author and room".into()));
            }
            return Ok(WriteReceipt {
                id,
                seq,
                duplicate: true,
                signed: true,
            });
        }
        // A public seat's write passes its limits (publicseats.rs) after the idempotency check, so a retry never counts twice.
        if crate::publicseats::is_public_handle(&who.handle) {
            let limits = self.public_config().limits;
            let bytes = (env.content.len() + env.cargo.as_deref().map(str::len).unwrap_or(0)) as i64;
            if let Err((why, retry_after_ms)) = crate::publicseats::admit(&tx, &crate::publicseats::write_buckets(&limits, &who.handle, &env.channel, bytes), now_ms)? {
                return Err(StoreError::Limited { why, retry_after_ms });
            }
        }

        tx.execute("DELETE FROM nonces WHERE expires_ms < ?1", params![now_ms])?;
        let fresh = tx.execute(
            "INSERT OR IGNORE INTO nonces(nonce,expires_ms) VALUES(?1,?2)",
            params![env.nonce, env.ts_ms + 2 * MAX_SKEW_MS],
        )?;
        if fresh == 0 {
            return Err(StoreError::Unauthorized("replayed nonce"));
        }

        // The author chain (docs/CHAIN.md): a write that names a prev is stored only while that is the author's head, decided
        // here, under the write lock, so two devices of one seat queue into one line instead of forking it.
        if let Some(prev) = env.prev.as_deref() {
            let (head, _) = chain_head_in(&tx, &env.author)?;
            if prev != head {
                return Err(StoreError::Conflict(format!("prev is not {}'s chain head; the head is {head}", env.author)));
            }
        }

        // A Room State revision is compare-and-swapped here, in the transaction that stores it (the client's `head` is only a preflight).
        check_room_context(&tx, env)?;

        let id = format!(
            "OBS-{}-{}-{}",
            env.channel,
            ymd(now_ms),
            &sha_hex(&env.sig)[..12]
        );
        // A record's own expires tag wins; else the room's retention rule (retention.rs) sets when it leaves the view.
        let expires = expires_of(&env.tags).or_else(|| retention_in(&tx).ok().and_then(|t| crate::retention::expires_for(&t, &env.channel, now_ms)));
        tx.execute(
            "INSERT INTO observations(id,channel,author,kind,content,cargo,tags_json,created_ms,ts_ms,nonce,client_op,pubkey,sig,authored_ms,prev,expires_ms) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                id, env.channel, env.author, env.kind.as_str(), env.content, env.cargo,
                serde_json::to_string(&env.tags).map_err(|e| StoreError::Internal(e.to_string()))?,
                now_ms, env.ts_ms, env.nonce, env.client_op, pubkey, env.sig, env.authored_ms, env.prev, expires
            ],
        )?;
        let seq = tx.last_insert_rowid();
        index_stored(&tx, env, &id, seq, now_ms, state, HeadRule::Arrival)?;
        tx.commit()?;
        Ok(WriteReceipt {
            id,
            seq,
            duplicate: false,
            signed: true,
        })
    }

    // ── import ──────────────────────────────────────────────────────────────────

    /// Bulk-load observations from an earlier Oracle. They are stored as `envelope = 1`:
    /// kept, searchable and attributable to the author they were recorded under, but never
    /// presented as verified under the current envelope. Ids and timestamps are preserved;
    /// re-running an import is a no-op.
    pub fn import_legacy(&self, rows: &[LegacyRow]) -> Result<usize> {
        // `recent` walks sequence numbers, so a batch is always inserted in time order.
        let mut rows: Vec<&LegacyRow> = rows.iter().collect();
        rows.sort_by(|a, b| (a.created_ms, &a.id).cmp(&(b.created_ms, &b.id)));
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        let mut added = 0;
        for r in rows {
            let tags_json =
                serde_json::to_string(&r.tags).map_err(|e| StoreError::Internal(e.to_string()))?;
            type Stored = (i64, i64, String, String, String, String, Option<String>, String, i64, String, String);
            let existing: Option<Stored> = tx.query_row(
                "SELECT seq,envelope,channel,author,kind,content,cargo,tags_json,created_ms,pubkey,sig FROM observations WHERE id=?1",
                params![r.id], |x| Ok((x.get(0)?,x.get(1)?,x.get(2)?,x.get(3)?,x.get(4)?,x.get(5)?,x.get(6)?,x.get(7)?,x.get(8)?,x.get(9)?,x.get(10)?))
            ).optional()?;
            if let Some(ref old) = existing {
                let same = old.2 == r.channel
                    && old.3 == r.author
                    && old.4 == r.kind
                    && old.5 == r.content
                    && old.6 == r.cargo
                    && old.7 == tags_json
                    && old.8 == r.created_ms
                    && old.9 == r.pubkey
                    && old.10 == r.sig;
                if !same {
                    if !r.repair_legacy || old.1 != 1 || r.source.is_none() || old.2 != r.channel {
                        return Err(StoreError::Conflict(format!(
                            "import differs for {}; source-backed legacy repair required",
                            r.id
                        )));
                    }
                    let previous_tags: Vec<String> = serde_json::from_str(&old.7)
                        .map_err(|e| StoreError::Internal(e.to_string()))?;
                    let state_tags = |tags: &[String]| {
                        let mut result: Vec<String> = tags
                            .iter()
                            .filter(|t| {
                                *t == "live-state"
                                    || ["subject:", "state:", "owner:", "clock:"]
                                        .iter()
                                        .any(|p| t.starts_with(p))
                            })
                            .cloned()
                            .collect();
                        result.sort();
                        result
                    };
                    if (previous_tags.iter().any(|t| t == "live-state")
                        || r.tags.iter().any(|t| t == "live-state"))
                        && (state_tags(&previous_tags) != state_tags(&r.tags)
                            || old.8 != r.created_ms
                            || old.3 != r.author)
                    {
                        return Err(StoreError::Conflict(
                            "state-changing repair requires explicit head reconciliation".into(),
                        ));
                    }
                    // Keep the old projection in the audit trail before repairing it.
                    let previous = serde_json::json!({"projection_before_repair": {
                        "id":r.id,"channel":old.2,"author":old.3,"kind":old.4,"content":old.5,
                        "cargo":old.6,"tags_json":old.7,"created_ms":old.8,"pubkey":old.9,"sig":old.10}}).to_string();
                    tx.execute(
                        "INSERT OR IGNORE INTO legacy_sources VALUES(?1,?2,?3,?4)",
                        params![r.id, sha_hex(&previous), previous, crate::http::now_ms()],
                    )?;
                    tx.execute("UPDATE observations SET author=?2,kind=?3,content=?4,cargo=?5,tags_json=?6,created_ms=?7,ts_ms=?7,pubkey=?8,sig=?9 WHERE id=?1 AND envelope=1",
                        params![r.id,r.author,r.kind,r.content,r.cargo,tags_json,r.created_ms,r.pubkey,r.sig])?;
                    tx.execute("DELETE FROM tags WHERE obs_seq=?1", params![old.0])?;
                    let index_tags = legacy_index_tags(r);
                    for tag in &index_tags {
                        tx.execute(
                            "INSERT OR IGNORE INTO tags VALUES(?1,?2)",
                            params![tag, old.0],
                        )?;
                    }
                    let class = if r.news { crate::roomindex::class_parts(&r.kind, &r.tags, &r.content) } else { "history" };
                    fts_update(&tx, old.0, &r.content, &format!("{} {}", index_tags.join(" "), crate::roomindex::fts_tokens(&r.channel, class)), r.cargo.as_deref())?;
                }
            }
            if let Some(source) = &r.source {
                let payload = serde_json::to_string(source)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                tx.execute(
                    "INSERT OR IGNORE INTO legacy_sources VALUES(?1,?2,?3,?4)",
                    params![r.id, sha_hex(&payload), payload, crate::http::now_ms()],
                )?;
            }
            let n = tx.execute(
                "INSERT OR IGNORE INTO observations(id,channel,author,kind,content,cargo,tags_json,created_ms,ts_ms,nonce,client_op,pubkey,sig,envelope,news) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8,'',?9,?10,?11,1,?12)",
                params![
                    r.id, r.channel, r.author, r.kind, r.content, r.cargo,
                    serde_json::to_string(&r.tags).map_err(|e| StoreError::Internal(e.to_string()))?,
                    r.created_ms, format!("legacy:{}", r.id), r.pubkey, r.sig, r.news
                ],
            )?;
            if n == 0 {
                // Already here. Still offer it as a state head, so re-running an import
                // rebuilds the task board of an Oracle that was imported before heads were.
                if let Some(h) = legacy_state(r) {
                    let seq: i64 = tx.query_row(
                        "SELECT seq FROM observations WHERE id=?1",
                        params![r.id],
                        |x| x.get(0),
                    )?;
                    upsert_legacy_head(&tx, &h, seq)?;
                }
                continue;
            }
            added += 1;
            let seq = tx.last_insert_rowid();
            let mut replaced = None;
            if let Some(h) = legacy_state(r) {
                let prior = crate::livestate::prior_head(&tx, &h.subject)?;
                upsert_legacy_head(&tx, &h, seq)?;
                replaced = Some((prior, h.subject, h.state));
            }
            tx.execute(
                "INSERT INTO channel_heads(channel,max_seq,total) VALUES(?1,?2,1) \
                 ON CONFLICT(channel) DO UPDATE SET max_seq=max(max_seq,excluded.max_seq), total=total+1",
                params![r.channel, seq],
            )?;
            let index_tags = legacy_index_tags(r);
            for t in &index_tags {
                tx.execute(
                    "INSERT OR IGNORE INTO tags(name,obs_seq) VALUES(?1,?2)",
                    params![t, seq],
                )?;
            }
            let class = if r.news { crate::roomindex::class_parts(&r.kind, &r.tags, &r.content) } else { "history" };
            fts_insert(&tx, seq, &r.content, &format!("{} {}", index_tags.join(" "), crate::roomindex::fts_tokens(&r.channel, class)), r.cargo.as_deref())?;
            // Imported history (news = 0) is in the room as class `history`, never judged; imported live rows are folded like any.
            crate::roomindex::project_new(&tx, seq)?;
            // A live-state row (`live-state` tag) replaces the head it names the same word as (livestate.rs).
            if let Some((prior, subject, state)) = replaced {
                // legacy_state() answers only for rows tagged live-state, so every row here is live.
                crate::livestate::retire_prior(&tx, prior, &subject, seq, &state, true)?;
            }
        }
        tx.commit()?;
        Ok(added)
    }

    /// Full preserved legacy payloads; each digest is an immutable version.
    pub fn legacy_sources(&self, id: &str) -> Result<Vec<serde_json::Value>> {
        let c = self.conn()?;
        let mut q = c.prepare("SELECT digest,payload,recorded_ms FROM legacy_sources WHERE obs_id=?1 ORDER BY recorded_ms,digest")?;
        let rows = q.query_map(params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (digest, payload, at) = row?;
            out.push(serde_json::json!({"digest":digest,"source":serde_json::from_str::<serde_json::Value>(&payload).map_err(|e|StoreError::Internal(e.to_string()))?,"recorded_ms":at}));
        }
        Ok(out)
    }

    // ── read ────────────────────────────────────────────────────────────────────

    /// Observation-row loads made on this thread since it started: the query count behind a read path
    /// (the asks batching's before/after evidence, tests/asks.rs).
    pub fn loads() -> u64 {
        LOADS.with(|c| c.get())
    }

    /// Observation rows returned by this thread's loads since it started.
    pub fn rows_loaded() -> u64 {
        ROWS.with(|c| c.get())
    }

    fn load(
        c: &Connection,
        where_order: &str,
        p: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<Observation>> {
        let rows = Self::load_rows(c, where_order, p)?;
        ROWS.with(|c| c.set(c.get() + rows.len() as u64));
        Ok(rows)
    }

    fn load_rows(
        c: &Connection,
        where_order: &str,
        p: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<Observation>> {
        LOADS.with(|c| c.set(c.get() + 1));
        let sql = format!(
            "SELECT seq,id,channel,author,kind,content,cargo,tags_json,created_ms,pubkey,sig,envelope,authored_ms \
             FROM observations o {where_order}"
        );
        let mut st = c.prepare_cached(&sql)?;
        let rows = st
            .query_map(p, |r| {
                let tags_json: String = r.get(7)?;
                let cargo: Option<String> = r.get(6)?;
                let mut tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
                // An imported row's reply link lives in its legacy cargo (`replyTo`), not in its tags: the import
                // put it in the index (`legacy_index_tags`), and every reader that judges threads reads `tags`. So the
                // same derived `re:` is carried here, for imported rows only. Without it, an imported answer never
                // answered anything: 64 such links on 1 Oct, among them Emmy's answer to.
                project_imported_reply(&mut tags, cargo.as_deref(), r.get(11)?);
                Ok(Observation {
                    seq: r.get(0)?,
                    id: r.get(1)?,
                    channel: r.get(2)?,
                    author: r.get(3)?,
                    kind: r.get(4)?,
                    content: r.get(5)?,
                    cargo,
                    tags,
                    created_ms: r.get(8)?,
                    pubkey: r.get(9)?,
                    sig: r.get(10)?,
                    envelope: r.get(11)?,
                    authored_ms: r.get(12)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Several records by id in one query per chunk (the asks view's batched walk to thread roots); absent
    /// ids are simply absent, as `get` would have been an error for them.
    pub fn get_many(&self, ids: &[String]) -> Result<Vec<Observation>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(4_000) {
            let holes = vec!["?"; chunk.len()].join(",");
            let params: Vec<&dyn rusqlite::ToSql> = chunk.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
            out.extend(Self::load(&*self.conn()?, &format!("WHERE o.id IN ({holes})"), &params)?);
        }
        Ok(out)
    }

    /// The full id a caller named. An `OBS-...` id (or any id with a dash) is taken as it is; six to sixty-four hex digits
    /// are a record's code (Blaze, 31 Oct 2026: the short hash code is the id; old ids stay valid). The first twelve
    /// are matched against the id's tail, which is the first twelve of sha256(signature) on every node, and any longer
    /// code must also start the signature's whole hash. Several matches are refused with the candidates named.
    pub fn resolve_id(&self, named: &str) -> Result<String> {
        let code = named.trim().to_ascii_lowercase();
        if code.contains('-') || !(6..=64).contains(&code.len()) || !code.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(named.to_string());
        }
        let head = &code[..code.len().min(12)];
        let c = self.conn()?;
        let mut st = c.prepare_cached(
            "SELECT id, sig FROM observations WHERE substr(id, -12) >= ?1 AND substr(id, -12) < ?2 ORDER BY seq LIMIT 64",
        )?;
        let rows: Vec<(String, String)> = st
            .query_map(params![head, format!("{head}g")], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let found: Vec<String> = rows
            .into_iter()
            .filter(|(id, sig)| id.len() > 12 && (code.len() <= 12 || oracle_proto::sha_hex(sig.as_bytes()).starts_with(&code)))
            .map(|(id, _)| id)
            .collect();
        match found.as_slice() {
            [] => Err(StoreError::NotFound),
            [one] => Ok(one.clone()),
            many => Err(StoreError::Conflict(format!(
                "code {code} names {} records ({}); give more of it",
                many.len(),
                many.iter().take(5).map(String::as_str).collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    pub fn get(&self, id: &str) -> Result<Observation> {
        Self::load(&*self.conn()?, "WHERE o.id=?1", &[&id])?
            .into_iter()
            .next()
            .ok_or(StoreError::NotFound)
    }

    /// The envelope exactly as it was signed, so anyone can re-verify an observation without
    /// trusting this server. Imported history (`envelope = 1`) has none under this scheme.
    pub fn envelope_of(&self, id: &str) -> Result<Envelope> {
        envelope_in(&*self.conn()?, id)
    }
}

/// The stored envelope of a native observation, on any connection (or transaction).
fn envelope_in(c: &Connection, id: &str) -> Result<Envelope> {
    let (env, version) = c
        .query_row(&format!("SELECT {ENVELOPE_COLS} FROM observations WHERE id=?1"), params![id], |r| envelope_at(r, 0))
        .optional()?
        .ok_or(StoreError::NotFound)?;
    if version != 2 {
        return Err(StoreError::NotFound);
    }
    env
}

/// The columns [`envelope_at`] reads, in its order.
const ENVELOPE_COLS: &str = "channel,author,kind,ts_ms,nonce,client_op,content,cargo,tags_json,sig,envelope,authored_ms,prev";

/// A stored row back to the envelope it was signed as, with its envelope version (only 2 has one). The
/// [`ENVELOPE_COLS`] start at column `b`.
fn envelope_at(r: &rusqlite::Row, b: usize) -> rusqlite::Result<(Result<Envelope>, i64)> {
    let kind: std::result::Result<Kind, _> = serde_json::from_value(serde_json::Value::String(r.get(b + 2)?));
    let tags: String = r.get(b + 8)?;
    let env = Envelope {
        channel: r.get(b)?,
        author: r.get(b + 1)?,
        kind: Kind::Say,
        ts_ms: r.get(b + 3)?,
        nonce: r.get(b + 4)?,
        client_op: r.get(b + 5)?,
        content: r.get(b + 6)?,
        cargo: r.get(b + 7)?,
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        authored_ms: r.get(b + 11)?,
        prev: r.get(b + 12)?,
        sig: r.get(b + 9)?,
    };
    let env = kind.map(|kind| Envelope { kind, ..env }).map_err(|_| StoreError::Internal("stored kind is unknown".into()));
    Ok((env, r.get(b + 10)?))
}

impl Store {
    /// An author's chain head (docs/CHAIN.md): `(link, Some((seq, id)))`, or genesis with no record.
    pub fn chain_head(&self, author: &str) -> Result<(String, Option<(i64, String)>)> {
        chain_head_in(&*self.conn()?, author)
    }

    /// An author's native observations after `after_seq`, oldest first, each as the envelope it was signed as with the
    /// key that verified it: the chain's face (docs/CHAIN.md).
    pub fn chain(&self, author: &str, after_seq: i64, limit: usize) -> Result<Vec<oracle_proto::ChainRecord>> {
        let c = self.conn()?;
        let mut st = c.prepare_cached(&format!(
            "SELECT seq,id,pubkey,{ENVELOPE_COLS} FROM observations WHERE author=?1 AND envelope=2 AND seq>?2 ORDER BY seq LIMIT ?3"
        ))?;
        let rows = st.query_map(params![author, after_seq, limit as i64], |r| {
            let seq: i64 = r.get(0)?;
            let id: String = r.get(1)?;
            let pubkey: String = r.get(2)?;
            Ok((seq, id, pubkey, envelope_at(r, 3)?.0))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, id, pubkey, env) = row?;
            out.push(oracle_proto::ChainRecord { seq, id, pubkey, envelope: env?, withheld: false });
        }
        Ok(out)
    }
}

impl Store {

    /// Every author with how many records they signed (envelope 2), their newest seq and when it was written (`GET /minds`): the
    /// counts are an index-only walk of the partial index obs_native_author (0.06 s over 500k records on core, 3 Nov 2026), the times one primary-key read each.
    pub fn author_counts(&self) -> Result<Vec<(String, i64, i64, i64)>> {
        let c = self.conn()?;
        let mut st = c.prepare_cached("SELECT author, count(*), max(seq) FROM observations WHERE envelope=2 GROUP BY author")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut at = c.prepare_cached("SELECT created_ms FROM observations WHERE seq=?1")?;
        let mut out = Vec::with_capacity(rows.len());
        for (author, n, seq) in rows {
            let ms: i64 = at.query_row(params![seq], |r| r.get(0))?;
            out.push((author, n, seq, ms));
        }
        Ok(out)
    }

    /// One author's records, newest first, keyset-paginated on `seq` (`GET /minds/:handle`): an index walk of obs_author_seq.
    pub fn authored(&self, author: &str, before_seq: Option<i64>, limit: i64) -> Result<Vec<Observation>> {
        let c = self.conn()?;
        let (before, limit) = (before_seq.unwrap_or(i64::MAX), limit.clamp(1, 500));
        Self::load(&c, "WHERE o.author=?1 AND o.seq<?2 ORDER BY o.seq DESC LIMIT ?3", &[&author, &before, &limit])
    }

    /// Newest first, keyset-paginated on `seq`. Always an index walk.
    pub fn recent(
        &self,
        channel: Option<&str>,
        limit: i64,
        before_seq: Option<i64>,
    ) -> Result<Vec<Observation>> {
        let limit = limit.clamp(1, 500);
        let before = before_seq.unwrap_or(i64::MAX);
        let c = self.conn()?;
        match channel {
            Some(ch) => Self::load(
                &c,
                "WHERE o.channel=?1 AND o.seq<?2 AND (o.expires_ms IS NULL OR o.expires_ms > CAST(strftime('%s','now') AS INTEGER) * 1000) ORDER BY o.seq DESC LIMIT ?3",
                &[&ch, &before, &limit],
            ),
            None => Self::load(
                &c,
                "WHERE o.seq<?1 ORDER BY o.seq DESC LIMIT ?2",
                &[&before, &limit],
            ),
        }
    }

    /// `recent`, narrowed: only observations carrying `tag`, and/or whose content starts with
    /// `starts`. A tag walk is an index walk; `starts` filters the walk it rides on.
    pub fn recent_filtered(
        &self,
        channel: Option<&str>,
        limit: i64,
        before_seq: Option<i64>,
        tag: Option<&str>,
        starts: Option<&str>,
    ) -> Result<Vec<Observation>> {
        if tag.is_none() && starts.is_none() {
            return self.recent(channel, limit, before_seq);
        }
        self.recent_filtered_after(channel, limit, before_seq, 0, tag, starts)
    }

    /// `recent_filtered` for the rows newer than `after_seq`: a reader that already holds everything up to a seq
    /// scans only what came since, instead of walking the whole channel again.
    pub fn recent_filtered_after(
        &self,
        channel: Option<&str>,
        limit: i64,
        before_seq: Option<i64>,
        after_seq: i64,
        tag: Option<&str>,
        starts: Option<&str>,
    ) -> Result<Vec<Observation>> {
        let limit = limit.clamp(1, 500);
        let before = before_seq.unwrap_or(i64::MAX);
        let starts_len = starts.map(|p| p.chars().count() as i64);
        // The channel test is spelled out only when there is a channel. `(?2 IS NULL OR o.channel=?2)`
        // looks equivalent, but SQLite plans it before binding, cannot use obs_channel_seq, and walks
        // every row in the store: 272 ms against 9 ms on the live ledger (2026-09-27), paid by every
        // room_roots() lookup.
        let sql = recent_filtered_sql(channel.is_some(), tag.is_some());
        Self::load(
            &*self.conn()?,
            &sql,
            &[&before, &channel, &starts, &starts_len, &tag, &limit, &after_seq],
        )
    }

    /// Latest visible exact-name lexical mention; no embedding service is called.
    pub fn entity_mentions(&self, name: &str, hidden: &std::collections::BTreeSet<String>, scope: Option<&crate::seatrooms::Rooms>) -> Result<Vec<Observation>> {
        let query = format!("content: \"{}\"", name.replace('"', "\"\""));
        let hidden = serde_json::to_string(hidden).unwrap();
        let allowed = scope.map(|r| serde_json::to_string(&r.read.union(&r.write).collect::<Vec<_>>()).unwrap());
        Self::load(&*self.conn()?, "WHERE o.seq IN (SELECT rowid FROM obs_fts WHERE obs_fts MATCH ?1) AND NOT EXISTS (SELECT 1 FROM tags t WHERE t.obs_seq=o.seq AND t.name IN ('cortex:node','cortex:link','cortex:mark','cortex:verdict')) AND o.channel NOT IN (SELECT value FROM json_each(?2)) AND (?3 IS NULL OR o.channel IN (SELECT value FROM json_each(?3))) ORDER BY o.seq DESC LIMIT 1", &[&query, &hidden, &allowed])
    }

    /// Everything stored after `after_seq`, oldest first, at most `limit` rows, in `rooms` when given: the replay of
    /// `GET /stream` (docs/STREAM.md). Each room is its own walk of obs_channel_seq; the walks are merged by seq.
    pub fn after_seq(&self, rooms: Option<&[String]>, after_seq: i64, limit: i64) -> Result<Vec<Observation>> {
        let c = self.conn()?;
        let Some(rooms) = rooms else {
            return Self::load(&c, "WHERE o.seq>?1 ORDER BY o.seq ASC LIMIT ?2", &[&after_seq, &limit]);
        };
        let mut out = Vec::new();
        for room in rooms {
            out.extend(Self::load(&c, "WHERE o.channel=?1 AND o.seq>?2 ORDER BY o.seq ASC LIMIT ?3", &[room, &after_seq, &limit])?);
        }
        out.sort_by_key(|o| o.seq);
        out.dedup_by_key(|o| o.seq);
        out.truncate(limit.max(0) as usize);
        Ok(out)
    }

    /// The newest messages in a room written by any of `authors`: the thread roots.
    pub fn by_authors(
        &self,
        channel: &str,
        authors: &[String],
        limit: i64,
        before_seq: Option<i64>,
    ) -> Result<Vec<Observation>> {
        if authors.is_empty() {
            return Ok(vec![]);
        }
        let limit = limit.clamp(1, 200);
        let before = before_seq.unwrap_or(i64::MAX);
        let holes = vec!["?"; authors.len()].join(",");
        let sql = format!(
            "WHERE o.channel=? AND o.seq<? AND o.author IN ({holes}) ORDER BY o.seq DESC LIMIT ?"
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&channel, &before];
        params.extend(authors.iter().map(|a| a as &dyn rusqlite::ToSql));
        params.push(&limit);
        Self::load(&*self.conn()?, &sql, &params)
    }

    /// What was said in the room right after `root_seq`, up to `until_seq` or `window_ms`, by
    /// anyone. NOT replies: nothing links these to the root. It answers "did anyone react?" for
    /// history written before replies were linked, and callers must present it as unlinked.
    pub fn followed_by(
        &self,
        channel: &str,
        root_seq: i64,
        root_ms: i64,
        until_seq: Option<i64>,
        window_ms: i64,
        limit: i64,
    ) -> Result<Vec<Observation>> {
        let until = until_seq.unwrap_or(i64::MAX);
        let deadline = root_ms.saturating_add(window_ms);
        Self::load(
            &*self.conn()?,
            "WHERE o.channel=?1 AND o.seq>?2 AND o.seq<?3 AND o.created_ms<=?4 ORDER BY o.seq ASC LIMIT ?5",
            &[&channel, &root_seq, &until, &deadline, &limit.clamp(1, 200)],
        )
    }

    /// A room's deciders: the `roots` of its newest `room-members/1` record, which only an owner-key
    /// seat may write (anyone else's `[… members]` post is ignored, never trusted). No record, no
    /// roots: then only a seat a message names with `to:` can settle it.
    /// The newest `limit` rows of `channel` whose content begins with `face`, newest first: what
    /// `recent_filtered(channel, limit, None, None, Some(face))` returns, found through the search index instead
    /// of a scan of the room. Contract: the face has a word and ENDS WITH `]` OR ASCII WHITESPACE (as
    /// `[Room members]` ends with `]`), characters unicode61 never counts into a token; then a content that begins
    /// with the face carries the face's words as whole tokens, in order, from its start, so the phrase lookup
    /// (`obs_fts MATCH`, the face quoted, tokenized and stemmed as the index was) is a superset the exact prefix test
    /// then narrows to the scan's rows, row for row. Any other ending takes the scan: a face that ends inside a token
    /// (`alpha` against `alphabet soup`: the scan's prefix matches, the phrase's whole token does not; Emmy),
    /// and an ending Rust calls non-alphanumeric that unicode61 still keeps in the
    /// token (a combining mark: `alpha\u{301}` against `alpha\u{301}bet soup`; Emmy's patch and regression).
    /// A face without a word takes the scan too. Every stored row has its search row in the same transaction (`write` → `fts_insert`, the
    /// import path → `fts_insert`, a moved row → `fts_update`; the migrations rebuilt the older ones; the profile
    /// copy of 21 Oct: 0 of 108,165 missing). Measured on that copy: every room equal, 163/163; the scan read
    /// every row of a room that holds no such record, 20.6 s over the rooms with the pages evicted, the index 0.85 s.
    pub fn starting_with(&self, channel: &str, face: &str, limit: i64) -> Result<Vec<Observation>> {
        let boundary = face.chars().last().is_some_and(|c| matches!(c, ']' | ' ' | '\t' | '\r' | '\n'));
        if !boundary || !face.chars().any(char::is_alphanumeric) {
            return self.recent_filtered(Some(channel), limit, None, None, Some(face));
        }
        let limit = limit.clamp(1, 500);
        let phrase = format!("content: \"{}\"", face.replace('"', " "));
        let n = face.chars().count() as i64;
        Self::load(
            &*self.conn()?,
            "WHERE o.seq IN (SELECT rowid FROM obs_fts WHERE obs_fts MATCH ?1) AND o.channel=?2 AND substr(o.content,1,?3)=?4 \
             ORDER BY o.seq DESC LIMIT ?5",
            &[&phrase, &channel, &n, &face, &limit],
        )
    }

    pub fn room_roots(&self, channel: Option<&str>) -> Vec<String> {
        let Some(channel) = channel else { return Vec::new() };
        let mut name = channel.to_owned();
        if let Some(c) = name.get_mut(0..1) {
            c.make_ascii_uppercase();
        }
        let face = format!("[{name} members]");
        let Ok(rows) = self.starting_with(channel, &face, 20) else { return Vec::new() };
        for o in rows {
            let owner = self.active_keys(&o.author).is_ok_and(|k| k.iter().any(|(_, role)| role == "owner"));
            let Some(cargo) = o.cargo.as_deref().filter(|_| owner) else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(cargo) else { continue };
            if v["schema"] == "room-members/1" {
                return v["roots"].as_array().map(|a| a.iter().filter_map(|r| r.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            }
        }
        Vec::new()
    }

    /// A message to the owner that offers him a choice in prose (two or more numbered options with a recommendation or a
    /// question to pick) must be an `ask:decide`, or the Bridge shows him words and no buttons (Blaze, 31 Oct 2026: "Oracle
    /// should tell them, send this in this schema if it contains an option or a recommendation, so that Blaze can choose from
    /// the Bridge"). Refused at the door for every client (ox, the MCP, raw HTTP), and the refusal is the schema. The owner's
    /// own messages, an answer that picks (`choice:`), and the retry of a write already stored pass.
    fn check_choice_in_prose(&self, env: &Envelope) -> Result<()> {
        if !offers_choice(&env.content) || env.tags.iter().any(|t| t.starts_with("choice:")) {
            return Ok(());
        }
        let c = self.conn()?;
        let owners: Vec<String> = c
            .prepare_cached("SELECT DISTINCT handle FROM keys WHERE role='owner' AND revoked_ms IS NULL")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mentioned = mentions(&env.content);
        let Some(owner) = owners.iter().find(|o| {
            *o != &env.author && (env.tags.iter().any(|t| t.strip_prefix("to:") == Some(o.as_str())) || mentioned.iter().any(|m| m.strip_prefix("mention:") == Some(o.as_str())))
        }) else {
            return Ok(());
        };
        let stored: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM observations WHERE client_op=?1 AND author=?2)", params![env.client_op, env.author], |r| r.get(0))?;
        if stored {
            return Ok(());
        }
        Err(StoreError::Rejected(format!(
            "this message to @{owner} offers a choice (numbered options with a recommendation or a question to pick), so it must be a decide, or {owner} sees words with no buttons on the Bridge. \
             Send it as a decide. ox: ox tell \"<the question, under 40 words>\" --ask decide --option \"<label, at most 80 characters>\" (2 to 6 times) --recommend <n> --why \"<one sentence>\" --by <YYYY-MM-DD>. \
             MCP oracle_say: ask \"decide\", options [2 to 6 labels], recommend <n>, why \"<one sentence>\". \
             Raw: tags to:{owner} ask:decide, cargo {{\"options\": [...], \"recommendation\": {{\"option\": n, \"why\": \"...\"}}}} (docs/BRIDGE-CONTRACT.md). \
             If you are not asking {owner} to choose, write it without numbered options."
        )))
    }

    /// An `annotate` write is a state update on ONE message of the same room: exactly one
    /// `annotates:<id>` naming a message that exists there, at least one `set:k=v`, and no link
    /// that would make it a reply or a resolution — it moves derived fields, never a thread.
    /// Layer 1 of docs/ASKS-AND-EVENTS.md, refused at the door so a list can trust what it reads:
    /// `ask:decide` carries 2 to 6 short, distinct options in its cargo; `ask:review` points at a `ref:`;
    /// `choice:<n>` answers a message that offers at least n options.
    fn check_ask_tags(&self, env: &Envelope) -> Result<()> {
        use crate::projection::{Ask, MAX_OPTIONS, MAX_OPTION_LEN, MIN_OPTIONS};
        let no = |m: String| Err(StoreError::Rejected(m));
        let ask = env.tags.iter().find_map(|t| t.strip_prefix("ask:")).and_then(Ask::parse);
        match ask {
            Some(Ask::Decide) => {
                let Some(list) = crate::asks::options_of(env.cargo.as_deref()) else {
                    return no("ask:decide needs cargo {\"options\": [...]}: 2 to 6 short labels".into());
                };
                if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&list.len()) {
                    return no(format!("ask:decide needs 2 to 6 options, got {}", list.len()));
                }
                let mut seen = std::collections::HashSet::new();
                for o in &list {
                    if o.trim().is_empty() {
                        return no("an option is empty".into());
                    }
                    if o.chars().count() > MAX_OPTION_LEN {
                        return no(format!("an option is longer than {MAX_OPTION_LEN} characters"));
                    }
                    if !seen.insert(o.trim().to_lowercase()) {
                        return no("options must be distinct".into());
                    }
                }
                // Every decide carries the asker's own pick, whoever asks and from whichever room or client (Blaze, 27 Oct: "the
                // schema should be made in a way" that any message asking him to decide shows its options and the recommendation
                // on the Bridge). docs/BRIDGE-CONTRACT.md: cargo.recommendation = {option (1-based), why}.
                // A retry of a write already stored (same client_op) answers as the original did, before this rule existed (Emmy's
                // review of e1428bb): the rule binds new decides, never the receipt of an old one. It runs after the signature and author
                // checks, and the stored row must be the same author's.
                let stored: bool = self.conn()?.query_row("SELECT EXISTS(SELECT 1 FROM observations WHERE client_op=?1 AND author=?2)", params![env.client_op, env.author], |r| r.get(0))?;
                let cargo: serde_json::Value = env.cargo.as_deref().and_then(|c| serde_json::from_str(c).ok()).unwrap_or_default();
                let rec = &cargo["recommendation"];
                if stored {
                    return Ok(());
                }
                if !rec["option"].as_u64().is_some_and(|n| n >= 1 && n as usize <= list.len()) {
                    return no(format!("ask:decide needs cargo.recommendation.option: the option you recommend, numbered 1 to {} (ox say --recommend <n>)", list.len()));
                }
                let why = rec["why"].as_str().map(str::trim).unwrap_or("");
                if why.is_empty() || why.chars().count() > 240 {
                    return no("ask:decide needs cargo.recommendation.why: one plain sentence, at most 240 characters (ox say --why)".into());
                }
            }
            Some(Ask::Review) if !env.tags.iter().any(|t| t.strip_prefix("ref:").is_some_and(|r| !r.is_empty())) => {
                return no("ask:review needs a ref:<id|sha|url> to look at".into());
            }
            _ => {}
        }
        if !matches!(ask, Some(Ask::Decide)) {
            self.check_choice_in_prose(env)?;
        }
        if let Some(raw) = env.tags.iter().find_map(|t| t.strip_prefix("choice:")) {
            let n = raw.parse::<usize>().ok().filter(|n| (1..=MAX_OPTIONS).contains(n));
            let Some(n) = n else {
                return no(format!("choice:{raw} is not an option number from 1 to {MAX_OPTIONS}"));
            };
            let Some(parent) = points_at_of(env) else {
                return no(format!("choice:{n}: this message points at nothing to choose from"));
            };
            if let Ok(p) = self.get(&parent) {
                match crate::asks::options_of(p.cargo.as_deref()) {
                    None => return no(format!("choice:{n}: {parent} offers no options")),
                    Some(list) if n > list.len() => return no(format!("choice:{n}: {parent} offers only {} options", list.len())),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn check_annotation(&self, env: &Envelope) -> Result<()> {
        let bad = |m: &str| Err(StoreError::Rejected(format!("annotate: {m}")));
        let subjects: Vec<&str> = env.tags.iter().filter_map(|t| t.strip_prefix("annotates:")).collect();
        let [subject] = subjects.as_slice() else {
            return bad("needs exactly one annotates:<id> tag");
        };
        if !env.tags.iter().any(|t| t.strip_prefix("set:").is_some_and(|kv| kv.split_once('=').is_some_and(|(k, v)| !k.is_empty() && !v.is_empty()))) {
            return bad("needs at least one set:k=v tag");
        }
        if env.tags.iter().any(|t| ["re:", "reply:", "in_reply_to:", "closes:", "supersedes:", "react:"].iter().any(|p| t.starts_with(p))) {
            // `archives:` stays allowed: it is how a seat withdraws its own mark.
            return bad("a state update is not a reply, a resolution or a reaction");
        }
        match self.get(subject) {
            Ok(o) if o.channel == env.channel => Ok(()),
            Ok(_) => bad("the message it annotates is in another room"),
            Err(_) => bad("the message it annotates does not exist"),
        }
    }

    /// Walk up from any message to the one that started its thread. Bounded by THREAD_HOPS, and a
    /// link that points nowhere (or back at itself) simply ends the walk where it stands.
    pub fn thread_root(&self, id: &str) -> Result<Observation> {
        let mut cur = self.get(id)?;
        let mut seen = std::collections::HashSet::from([cur.id.clone()]);
        for _ in 0..THREAD_HOPS {
            let Some(parent) = parent_of(&cur) else { break };
            if !seen.insert(parent.clone()) {
                break;
            }
            match self.get(&parent) {
                Ok(o) => cur = o,
                Err(_) => break,
            }
        }
        Ok(cur)
    }

    /// The thread any message is in: its root and up to `max` members (the newest), and always the
    /// message asked about, even in a thread longer than the cap: a buzz names it, and a reader who
    /// cannot find it concludes there is nothing new (Bobby, 2026-10-16).
    pub fn thread_of(&self, id: &str, max: usize) -> Result<(Observation, Vec<Observation>)> {
        let root = self.thread_root(id)?;
        let mut all = self.thread_members(&root.id, max)?;
        if id != root.id && !all.iter().any(|o| o.id == id) {
            all.push(self.get(id)?);
            all.sort_by_key(|o| o.seq);
        }
        Ok((root, all))
    }

    /// Everything under `root`, to the end of the thread: replies, and replies to those, and what
    /// resolves them (a close or an annotation from another thread too). Oldest first; when there
    /// are more than `max`, the NEWEST `max` (a long exchange keeps what was said last; until
    /// 2026-10-16 this stopped 4 links deep and kept the first it found, so a sixth reply was not
    /// in its own thread). Bounded by THREAD_HOPS links and THREAD_SCAN members so one runaway
    /// thread cannot hold a request open.
    pub fn thread_members(&self, root: &str, max: usize) -> Result<Vec<Observation>> {
        let (mut seen, mut frontier, mut out) = (
            std::collections::HashSet::new(),
            vec![root.to_owned()],
            Vec::new(),
        );
        seen.insert(root.to_owned());
        'walk: for _depth in 0..THREAD_HOPS {
            if frontier.is_empty() {
                break;
            }
            let found = self.resolvers_of(&frontier)?;
            frontier = Vec::new();
            for o in found {
                if seen.insert(o.id.clone()) {
                    frontier.push(o.id.clone());
                    out.push(o);
                    if out.len() >= THREAD_SCAN {
                        break 'walk;
                    }
                }
            }
        }
        out.sort_by_key(|o| o.seq);
        if out.len() > max {
            out.drain(..out.len() - max);
        }
        Ok(out)
    }

    /// Every observation that points at one of `ids` with a resolving tag.
    ///
    /// A message can be closed, superseded or archived by something outside the page being read,
    /// so a page cannot judge itself. These tags are exact names in the tag index, so this is an
    /// index walk, not a scan.
    /// Every resolver of every id, newest first, each once: the same tags as `resolvers_of`, fetched in chunks
    /// with NO shared limit, so a caller walking many threads at once can apply each thread's own newest-2000
    /// selection in memory (`asks::per_frontier`) and get exactly what `resolvers_of` would have returned for
    /// that thread. A shared LIMIT would let one busy thread crowd out another (Emmy's counterexample:
    /// two roots with 2,000 replies each, a shared query keeps only one).
    pub fn resolvers_of_all(&self, ids: &[String]) -> Result<Vec<Observation>> {
        let mut out: Vec<Observation> = Vec::new();
        for chunk in ids.chunks(4_000) {
            let names: Vec<String> = chunk
                .iter()
                .flat_map(|id| RESOLVER_PREFIXES.iter().map(move |p| format!("{p}{id}")))
                .collect();
            let holes = vec!["?"; names.len()].join(",");
            let params: Vec<&dyn rusqlite::ToSql> = names.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
            let sql = format!("JOIN tags t ON t.obs_seq=o.seq WHERE t.name IN ({holes}) GROUP BY o.seq ORDER BY o.seq DESC");
            out.extend(Self::load(&*self.conn()?, &sql, &params)?);
        }
        out.sort_by_key(|o| std::cmp::Reverse(o.seq));
        out.dedup_by(|a, b| a.id == b.id);
        Ok(out)
    }

    pub fn resolvers_of(&self, ids: &[String]) -> Result<Vec<Observation>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let names: Vec<String> = ids
            .iter()
            .flat_map(|id| RESOLVER_PREFIXES.iter().map(move |p| format!("{p}{id}")))
            .collect();
        let holes = vec!["?"; names.len()].join(",");
        let params: Vec<&dyn rusqlite::ToSql> =
            names.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
        let sql = format!(
            "JOIN tags t ON t.obs_seq=o.seq WHERE t.name IN ({holes}) GROUP BY o.seq ORDER BY o.seq DESC LIMIT 2000"
        );
        Self::load(&*self.conn()?, &sql, &params)
    }

    /// Where every seat's cursor stands in one room: the time of the last observation it has
    /// acknowledged, and how many newer ones wait (capped, like the fold).
    pub fn read_marks(&self, channel: &str) -> Result<Vec<ReadMark>> {
        let c = self.conn()?;
        let mut st = c.prepare_cached(
            "SELECT k.agent, k.mark_seq, \
               (SELECT created_ms FROM observations WHERE channel=?1 AND seq<=k.mark_seq ORDER BY seq DESC LIMIT 1), \
               (SELECT count(*) FROM (SELECT 1 FROM observations WHERE channel=?1 AND seq>k.mark_seq AND news=1 AND author<>k.agent AND kind<>'annotate' AND NOT EXISTS(SELECT 1 FROM tags ta WHERE ta.obs_seq=observations.seq AND ta.name LIKE 'annotates:%') LIMIT ?2)) \
             FROM cursors k WHERE k.channel=?1 ORDER BY k.agent",
        )?;
        let rows = st
            .query_map(params![channel, PENDING_CAP], |r| {
                Ok(ReadMark {
                    agent: r.get(0)?,
                    mark_seq: r.get(1)?,
                    through_ms: r.get(2)?,
                    pending: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every observation carrying any of these tags, oldest first, uncapped (the claims fold reads its whole history).
    pub fn tagged_any(&self, names: &[&str]) -> Result<Vec<Observation>> {
        let holes = vec!["?"; names.len()].join(",");
        let params: Vec<&dyn rusqlite::ToSql> = names.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
        Self::load(
            &*self.conn()?,
            &format!("WHERE o.seq IN (SELECT DISTINCT obs_seq FROM tags WHERE name IN ({holes})) ORDER BY o.seq ASC"),
            &params,
        )
    }

    /// `tagged_any` without the messages a later message archived (`archives:<id>`, `ox archive`): what is in the view.
    /// A withdrawn mark or node stays in the ledger and leaves the fold (docs/CORTEX.md; Blaze, 18 Oct: drafts stay in the brain).
    pub fn tagged_any_live(&self, names: &[&str]) -> Result<Vec<Observation>> {
        Self::live_tagged(&*self.conn()?, names)
    }

    /// Who cites each of `ids` (docs/CORTEX.md "Cited by") and the watermark it was read at. The cortex rows and the
    /// highest seq are read in one transaction, so links, node revisions, withdrawals and approvals all stand as of
    /// `through_seq`. A measured zero is absent from the map; an error is the caller's "unavailable", never a zero.
    /// The records minting observations whose approval verifies as the owner's ([`crate::cortex::ConceptIndex::approved`]),
    /// for a room read to mark its rows.
    pub fn approved_index(&self) -> Result<std::sync::Arc<crate::cortex::ConceptIndex>> {
        let conn = self.conn()?;
        let tx = conn.unchecked_transaction()?;
        let concepts = self.cortex_views(&tx)?.concepts;
        tx.commit()?;
        Ok(concepts)
    }

    pub fn citations(&self, ids: &[String]) -> Result<(std::collections::BTreeMap<String, serde_json::Value>, i64)> {
        let conn = self.conn()?;
        let tx = conn.unchecked_transaction()?;
        let through: i64 = tx.query_row("SELECT coalesce(max(seq), 0) FROM observations", [], |r| r.get(0))?;
        let cites = self.cortex_views(&tx)?.cites;
        tx.commit()?;
        let mut out = std::collections::BTreeMap::new();
        for id in ids {
            if let Some(mut v) = cites.of(id) {
                v["through_seq"] = through.into();
                out.insert(id.clone(), v);
            }
        }
        Ok((out, through))
    }

    /// The folded cortex and the attestation class of every approve verdict on an observation node, read in one
    /// transaction (the node list's "approval": Blaze's own key, attested by a registered seat, an agent, or unverified).
    pub fn cortex_with_classes(&self) -> Result<(crate::cortex::Cortex, std::collections::BTreeMap<String, crate::cortex::Attestation>)> {
        let conn = self.conn()?;
        let tx = conn.unchecked_transaction()?;
        let cortex = crate::cortex::fold(&Self::live_tagged(&tx, &crate::cortex::TAGS)?);
        let class = Self::approval_classes(&tx, &cortex)?;
        tx.commit()?;
        Ok((cortex, class))
    }

    /// The cortex views as of the caller's snapshot, from the cache when the cortex has not changed since they were built
    /// (see [`CiteKey`]); one fold and one pass over the approvals build both.
    fn cortex_views(&self, c: &Connection) -> Result<CortexViews> {
        let key = Self::cite_key(c)?;
        if let Some(v) = self.cites.lock().ok().and_then(|g| g.as_ref().filter(|(k, _)| *k == key).map(|(_, v)| v.clone())) {
            return Ok(v);
        }
        let rows = Self::live_tagged(c, &crate::cortex::TAGS)?;
        let cortex = crate::cortex::fold(&rows);
        let class = Self::approval_classes(c, &cortex)?;
        let built = CortexViews {
            cites: std::sync::Arc::new(crate::cortex::Citations::build(&cortex, &class)),
            concepts: std::sync::Arc::new(crate::cortex::ConceptIndex::build(&cortex, &class)),
        };
        if let Ok(mut g) = self.cites.lock() {
            *g = Some((key, built.clone()));
        }
        Ok(built)
    }

    /// Recall through the graph (docs/SEARCH.md "Through the graph"): when the query names a concept, an entity or a form,
    /// up to [`GRAPH_LEAD`] of the approved observations it reaches go first, after exact ids, best first; the rest of the
    /// ranking keeps its order. Returns what the query named, for the engine line. A filtered search (one room other than
    /// #cortex, or one kind) is left as it is.
    #[allow(clippy::too_many_arguments)]
    fn graph_lead(&self, c: &Connection, query: &str, channel: Option<&str>, kind: Option<&str>, table: &str, ranked: &mut Vec<(i64, f64)>, named_lead: &mut Vec<i64>) -> Result<Option<String>> {
        if kind.is_some() || channel.is_some_and(|ch| ch != "cortex") {
            return Ok(None);
        }
        let reach = self.cortex_views(c)?.concepts.reach(query);
        if reach.records.is_empty() {
            return Ok(None);
        }
        let want = if reach.nodes.is_empty() { GRAPH_LEAD_FORM } else { GRAPH_LEAD };
        let mut st = c.prepare_cached(&format!("SELECT seq FROM {table} WHERE id=?1"))?;
        let mut reached: Vec<i64> = vec![];
        for r in &reach.records {
            if let Some(seq) = st.query_row(params![r], |row| row.get::<_, i64>(0)).optional()? {
                reached.push(seq);
            }
        }
        // Two voices fused (reciprocal rank, k = 60): the words' order among what the graph reached ("what is state" finds
        // its own observation that way) and the graph's own order (node and form both, then link weight, then later
        // citations: the mentor idea, cited by nine readings, for "personal agent that knows me").
        let words: Vec<i64> = ranked.iter().map(|(seq, _)| *seq).filter(|seq| reached.contains(seq)).collect();
        // A query that names a node but asks about more ("dot is 1 bit not 153 bytes" names DOT) keeps only the readings
        // its other words also match: on 30 Oct that query led with three unrelated readings about DOT.
        if reach.unnamed >= 2 {
            reached.retain(|seq| words.contains(seq));
        }
        let mut fused: Vec<(i64, f64)> = reached.iter().enumerate().map(|(g, seq)| {
            let w = words.iter().position(|x| x == seq).map_or(0.0, |p| 1.0 / (60.0 + p as f64 + 1.0));
            (*seq, w + 1.0 / (60.0 + g as f64 + 1.0))
        }).collect();
        fused.sort_by(|a, b| b.1.total_cmp(&a.1));
        let lead: Vec<i64> = fused.into_iter().take(want).map(|(seq, _)| seq).collect();
        if lead.is_empty() {
            return Ok(None);
        }
        // A query of names alone ("DOT", "Oracle-core design") asks for what the graph links to them: those are about it
        // even when their words differ. With other words too, [`approved_first`] judges them by the words.
        if reach.unnamed == 0 {
            named_lead.extend(lead.iter().copied());
        }
        let top = ranked.iter().filter(|(_, s)| *s < ALIAS_SCORE).map(|(_, s)| *s).fold(0.0, f64::max);
        let alias: Vec<(i64, f64)> = ranked.iter().copied().filter(|(_, s)| *s >= ALIAS_SCORE).collect();
        let mut rest: Vec<(i64, f64)> = ranked.drain(..).filter(|(seq, s)| *s < ALIAS_SCORE && !lead.contains(seq)).collect();
        // An approved reading the words put first keeps first place: it already answers in the record's own words.
        let mut first: Vec<(i64, f64)> = vec![];
        let approved = &self.cortex_views(c)?.concepts.approved;
        let mut id_of = c.prepare_cached("SELECT id FROM observations WHERE seq=?1")?;
        if rest.first().is_some_and(|(seq, _)| id_of.query_row(params![seq], |r| r.get::<_, String>(0)).optional().ok().flatten().is_some_and(|id| approved.contains(&id))) {
            first.push(rest.remove(0));
        }
        *ranked = alias.into_iter().chain(first).chain(lead.iter().map(|seq| (*seq, top))).chain(rest).collect();
        let mut named = reach.nodes.join(", ");
        if let Some(f) = reach.form {
            named = if named.is_empty() { format!("form {f}") } else { format!("{named}; form {f}") };
        }
        Ok(Some(named))
    }

    /// Each approved observation's [`crate::cortex::Attestation`], by verdict record, read on the caller's transaction
    /// so the registry, the gateways and the fold are one snapshot. The verdict's envelope must verify under the key its
    /// signer held when it was written; then an owner key naming its own handle is `owner`, a registered attester naming
    /// an owner handle and a page is `attested`, a seat naming itself is `agent`, and everything else is `unverified`.
    fn approval_classes(c: &Connection, cortex: &crate::cortex::Cortex) -> Result<std::collections::BTreeMap<String, crate::cortex::Attestation>> {
        use crate::cortex::Attestation;
        let owners: std::collections::BTreeSet<String> = c.prepare("SELECT DISTINCT handle FROM keys WHERE role='owner' AND revoked_ms IS NULL")?
            .query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        let attesters: std::collections::BTreeSet<String> = c.prepare("SELECT handle, routes FROM gateways")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|(_, routes)| routes.split(',').any(|x| x == ATTEST_ROUTE))
            .map(|(h, _)| h)
            .collect();
        let mut out = std::collections::BTreeMap::new();
        for n in cortex.nodes.iter().filter(|n| n.kind == "observation") {
            let Some(v) = n.verdict.as_ref().filter(|v| v.verdict == "approve") else { continue };
            let row: Option<(String, String, Option<String>, i64)> = c
                .query_row("SELECT author, pubkey, cargo, created_ms FROM observations WHERE id=?1", params![v.message], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .optional()?;
            let class = match row {
                None => Attestation::Unverified,
                Some((author, pubkey, cargo, written_ms)) => {
                    let (held, owner_key): (bool, bool) = c.query_row(
                        "SELECT EXISTS(SELECT 1 FROM keys WHERE handle=?1 AND pubkey=?2 AND created_ms<=?3 AND (revoked_ms IS NULL OR revoked_ms>?3)), \
                                EXISTS(SELECT 1 FROM keys WHERE handle=?1 AND pubkey=?2 AND created_ms<=?3 AND (revoked_ms IS NULL OR revoked_ms>?3) AND role='owner')",
                        params![author, pubkey, written_ms],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    let verified = held && envelope_in(c, &v.message).is_ok_and(|e| e.verify_stored(&pubkey).is_ok() && e.author == author && e.cargo == cargo);
                    if !verified {
                        Attestation::Unverified
                    } else if v.by == author {
                        if owner_key { Attestation::Owner } else { Attestation::Agent }
                    } else if owners.contains(&v.by) && v.via.is_some() && attesters.contains(&author) {
                        Attestation::Attested
                    } else {
                        Attestation::Unverified
                    }
                }
            };
            out.insert(v.message.clone(), class);
        }
        Ok(out)
    }

    /// The inputs of the Cited by projection, read on the caller's transaction (see [`CiteKey`]).
    fn cite_key(c: &Connection) -> Result<CiteKey> {
        let holes = vec!["?"; crate::cortex::TAGS.len()].join(",");
        let params: Vec<&dyn rusqlite::ToSql> = crate::cortex::TAGS.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
        let cortex_seq: i64 = c.query_row(&format!("SELECT coalesce(max(obs_seq), 0) FROM tags WHERE name IN ({holes})"), params.as_slice(), |r| r.get(0))?;
        let archive_seq: i64 = c.query_row("SELECT coalesce(max(obs_seq), 0) FROM tags WHERE name LIKE 'archives:%'", [], |r| r.get(0))?;
        let room_archive: i64 = c.query_row("SELECT coalesce(max(obs_seq), 0) * 100000 + count(*) FROM room_archive", [], |r| r.get(0))?;
        let gateways: String = c.query_row("SELECT coalesce(group_concat(handle || '=' || routes, ';'), '') FROM (SELECT handle, routes FROM gateways ORDER BY handle)", [], |r| r.get(0))?;
        let keys: String = c.query_row(
            "SELECT count(*) || ':' || coalesce(max(created_ms), 0) || ':' || coalesce(max(revoked_ms), 0) || ':' || coalesce(sum(role = 'owner'), 0) FROM keys",
            [], |r| r.get(0))?;
        Ok(CiteKey { cortex_seq, archive_seq, room_archive, gateways, keys })
    }

    fn live_tagged(c: &Connection, names: &[&str]) -> Result<Vec<Observation>> {
        let holes = vec!["?"; names.len()].join(",");
        let params: Vec<&dyn rusqlite::ToSql> = names.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
        Self::load(
            c,
            &format!(
                "WHERE o.seq IN (SELECT DISTINCT obs_seq FROM tags WHERE name IN ({holes})) \
                 AND o.id NOT IN (SELECT substr(name, 10) FROM tags WHERE name LIKE 'archives:%') \
                 AND o.channel NOT IN (SELECT room FROM room_archive) ORDER BY o.seq ASC"
            ),
            &params,
        )
    }

    /// One page of `by_tag`, older than `before` (an observation seq): the caller walks a tag's whole history.
    pub fn by_tag_before(&self, tag: &str, before: i64, limit: i64) -> Result<Vec<Observation>> {
        let limit = limit.clamp(1, 500);
        Self::load(
            &*self.conn()?,
            "JOIN tags t ON t.obs_seq=o.seq WHERE t.name=?1 AND t.obs_seq<?2 ORDER BY t.obs_seq DESC LIMIT ?3",
            &[&tag, &before, &limit],
        )
    }

    pub fn by_tag(&self, tag: &str, limit: i64) -> Result<Vec<Observation>> {
        let limit = limit.clamp(1, 500);
        Self::load(
            &*self.conn()?,
            "JOIN tags t ON t.obs_seq=o.seq WHERE t.name=?1 ORDER BY t.obs_seq DESC LIMIT ?2",
            &[&tag, &limit],
        )
    }

    /// Rooms, newest activity first — from the head table, never a scan.
    pub fn channels(&self) -> Result<Vec<(String, i64, i64, i64)>> {
        let c = self.conn()?;
        let mut st = c.prepare_cached(
            "SELECT h.channel, h.total, o.created_ms, h.max_seq FROM channel_heads h \
             JOIN observations o ON o.seq = h.max_seq ORDER BY h.max_seq DESC",
        )?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Everything new for `agent` since its cursors, oldest first within a channel, shared
    /// fairly across channels, with the full pending counts. Read-only: call `ack` to advance.
    pub fn fold(&self, agent: &str, limit: i64, now_ms: i64) -> Result<Fold> {
        let limit = limit.clamp(1, 200);
        let c = self.conn()?;
        // A channel never folded before starts 24h back, not at the beginning of time.
        // The floor is the first row at or after the cutoff IN TIME ORDER: one seek on the
        // created_ms index whether everything is recent or nothing is. (min(seq) over the
        // range scans it; max(seq) below the cutoff breaks when imported history carries old
        // timestamps at high sequence numbers.)
        let day_floor: i64 = c
            .query_row(
                "SELECT seq - 1 FROM observations INDEXED BY obs_created \
                 WHERE created_ms >= ?1 AND news = 1 ORDER BY created_ms LIMIT 1",
                params![now_ms - 86_400_000],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(i64::MAX / 2);
        let marks: BTreeMap<String, i64> = {
            let mut st =
                c.prepare_cached("SELECT channel, mark_seq FROM cursors WHERE agent=?1")?;
            let m = st
                .query_map(params![agent], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            m
        };
        // Rooms with news come from channel_heads (one row per room), never from a scan of
        // observations. Counts are capped: past PENDING_CAP the honest answer is "a lot",
        // and an agent a million messages behind must not cost a million-row count per fold.
        let heads: Vec<(String, i64)> = {
            let mut st = c.prepare_cached("SELECT channel, max_seq FROM channel_heads")?;
            let v = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            v
        };
        let mut pending: BTreeMap<String, i64> = BTreeMap::new();
        {
            let mut st = c.prepare_cached(
                &format!("SELECT count(*) FROM (SELECT 1 FROM observations \
                 WHERE channel=?1 AND seq>?2 AND author<>?3 AND news=1 AND kind<>'annotate' AND {LIVE} AND NOT EXISTS(SELECT 1 FROM tags ta WHERE ta.obs_seq=observations.seq AND (ta.name LIKE 'annotates:%' OR ta.name LIKE 'react:%')) LIMIT ?4)"),
            )?;
            for (ch, max_seq) in &heads {
                let mark = marks.get(ch).copied().unwrap_or(day_floor);
                if *max_seq <= mark {
                    continue;
                }
                let n: i64 =
                    st.query_row(params![ch, mark, agent, PENDING_CAP + 1], |r| r.get(0))?;
                if n > 0 {
                    pending.insert(ch.clone(), n);
                }
            }
        }

        let shares = fair_share(&pending, limit);
        let mut items = Vec::new();
        let mut next_marks = marks.clone();
        let (to_me, mention_me) = (format!("to:{agent}"), format!("mention:{agent}"));
        for (ch, share) in &shares {
            let mark = marks.get(ch).copied().unwrap_or(day_floor);
            let rows = Self::load(
                &c,
                // A state update is not news: it never rings, never waits unread (Blaze, 2026-09-27). Nor is a mark
                // (Jobs, 2026-10-10: "a seen mark is not a new message").
                &format!("WHERE o.channel=?1 AND o.seq>?2 AND o.author<>?3 AND o.news=1 AND o.kind<>'annotate' AND {LIVE_O} \
                 AND NOT EXISTS(SELECT 1 FROM tags ta WHERE ta.obs_seq=o.seq AND (ta.name LIKE 'annotates:%' OR ta.name LIKE 'react:%')) ORDER BY o.seq ASC LIMIT ?4"),
                &[ch, &mark, &agent, share],
            )?;
            for obs in rows {
                next_marks.insert(ch.clone(), obs.seq);
                // For me: addressed to me, to everyone — or pointing at something I wrote
                // (a reply, a close, a supersede). Taylor's answer to Rocky's receipt carried
                // no @rocky and rang nobody (2026-09-22); an answer is addressed by its target.
                // Resolved from the item's own tags: an exact (BINARY) relation prefix, then the target by its
                // id. The former join walked every record the seat ever authored for every item (21 Oct: 65 ms
                // an item for rocky, 200 items a fold, the whole 16–40 s fold tail; Emmy's exact-prefix form:
                // LIKE would have admitted REPLY:/Reply:/in-reply-to).
                let for_me: bool = c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tags WHERE obs_seq=?1 AND name IN (?2,?3,'to:all'))
                     OR EXISTS(SELECT 1 FROM tags t WHERE t.obs_seq=?1
                               AND substr(t.name,1,instr(t.name,':')) COLLATE BINARY IN ('reply:','re:','in_reply_to:','closes:','supersedes:')
                               AND EXISTS(SELECT 1 FROM observations p WHERE p.id=substr(t.name,instr(t.name,':')+1) AND p.author=?4))",
                    params![obs.seq, to_me, mention_me, agent],
                    |r| r.get(0),
                )?;
                items.push(FoldItem { obs, for_me });
            }
        }
        items.sort_by_key(|i| i.obs.seq);
        let returned = items.len();
        let has_more = pending.values().sum::<i64>() > returned as i64;
        let pending_capped = pending.values().any(|n| *n > PENDING_CAP);
        for n in pending.values_mut() {
            *n = (*n).min(PENDING_CAP);
        }
        // Newest first among the latest 400 addressed rows (the tags primary key serves this as one range read).
        let unread_for_me: Vec<Observation> = Self::load(
            &c,
            &format!("WHERE o.seq IN (SELECT obs_seq FROM tags WHERE name IN (?1,?2) ORDER BY obs_seq DESC LIMIT 400) \
             AND o.author<>?3 AND o.news=1 AND o.kind<>'annotate' AND {LIVE_O} ORDER BY o.seq DESC"),
            &[&to_me, &mention_me, &agent],
        )?
        .into_iter()
        .filter(|o| o.seq > marks.get(&o.channel).copied().unwrap_or(day_floor))
        .filter(|o| !o.tags.iter().any(|t| t.starts_with("annotates:") || t.starts_with("react:")))
        .take(UNREAD_FOR_ME_MAX)
        .collect();
        Ok(Fold {
            agent: agent.to_string(),
            items,
            pending,
            pending_capped,
            returned,
            has_more,
            next_marks,
            unread_for_me,
        })
    }

    /// Advance cursors. Marks only ever move forward unless `rewind` is set.
    pub fn ack(&self, agent: &str, marks: &BTreeMap<String, i64>, rewind: bool) -> Result<()> {
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        for (ch, seq) in marks {
            let sql = if rewind {
                "INSERT INTO cursors(agent,channel,mark_seq) VALUES(?1,?2,?3) \
                 ON CONFLICT(agent,channel) DO UPDATE SET mark_seq=excluded.mark_seq"
            } else {
                "INSERT INTO cursors(agent,channel,mark_seq) VALUES(?1,?2,?3) \
                 ON CONFLICT(agent,channel) DO UPDATE SET mark_seq=max(mark_seq, excluded.mark_seq)"
            };
            tx.execute(sql, params![agent, ch, seq])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Ranked full-text search. Precise first: all terms must match (small candidate set,
    /// fast, and what the asker meant). Only when that finds too little does it widen to
    /// any-term, so a broad query never pays to rank half the corpus for nothing.
    /// Mark everything read: every room's cursor moves to that room's head.
    pub fn ack_all(&self, agent: &str) -> Result<usize> {
        let n = self.conn()?.execute(
            "INSERT INTO cursors(agent,channel,mark_seq) SELECT ?1, channel, max_seq FROM channel_heads WHERE true \
             ON CONFLICT(agent,channel) DO UPDATE SET mark_seq=max(mark_seq, excluded.mark_seq)",
            params![agent],
        )?;
        Ok(n)
    }

    /// Everything a room has said that names any of `terms`, newest first — time order, not
    /// relevance, because the use is a worldline: what was said about this thing, and when.
    /// Terms are matched as whole tokens (`AAVE` does not match `AAVEUSDT`; pass both).
    pub fn about(&self, channel: &str, terms: &[String], limit: i64) -> Result<Vec<Observation>> {
        let clean: Vec<String> = terms
            .iter()
            .filter(|t| (2..=24).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric()))
            .take(8)
            .map(|t| format!("\"{t}\""))
            .collect();
        if clean.is_empty() {
            return Ok(vec![]);
        }
        let query = format!("content : ({})", clean.join(" OR "));
        Self::load(
            &*self.conn()?,
            "WHERE o.seq IN (SELECT rowid FROM obs_fts WHERE obs_fts MATCH ?1) AND o.channel=?2 AND o.kind='say' \
             ORDER BY o.seq DESC LIMIT ?3",
            &[&query, &channel, &limit.clamp(1, 300)],
        )
    }

    /// Ranked search. Keyword: every word first, then any word, by bm25. With meaning search on, the any-word
    /// keyword list and the meaning list (each its best 50) are fused by reciprocal rank. Returns the hits and
    /// which engine produced them ("hybrid", "keyword", or "keyword (meaning search unavailable: why)").
    /// Keyword-only ranked search (see `search_with`).
    pub fn search(&self, query: &str, channel: Option<&str>, kind: Option<&str>, limit: i64) -> Result<Vec<SearchHit>> {
        Ok(self.search_with(query, channel, kind, limit, None)?.0)
    }

    /// How strongly the ledger holds a query, apart from the fused rank (whose score is flat: Round 50 measured 0.025 to
    /// 0.033 on every query, one with no prior at all). `all_words`: records holding every word (capped at 1000);
    /// `all_words_before`: those of them older than `before_ms`; `earliest_ms`: the oldest; `best_meaning`: the top
    /// meaning similarity, when meaning search is on. Read-only; one count and one embedding.
    /// Whether the record holds anything on `query` (Round 50 ask 6): an explicit answer instead of weak hits read as
    /// evidence. From [`Store::strength`]: `found` when some record older than a day holds all the words; `new` when only
    /// records from the last day do; `none` when no record holds all the words and the nearest meaning is below
    /// [`PRIOR_FLOOR`]; `unclear` when no record holds all the words but something is close in meaning.
    pub fn prior(strength: &serde_json::Value) -> serde_json::Value {
        let all = strength["all_words"].as_i64().unwrap_or(0);
        let before = strength["all_words_before"].as_i64().unwrap_or(0);
        let best = strength["best_meaning"].as_f64();
        let first = strength["earliest_ms"].as_i64().map(oracle_proto::iso_ms);
        let (verdict, says) = if before > 0 {
            ("found", format!("{all} records hold all these words, the first {}", first.as_deref().unwrap_or("?")))
        } else if all > 0 {
            ("new", format!("nothing before the last day: {all} records hold all these words, the first {}", first.as_deref().unwrap_or("?")))
        } else if best.is_none_or(|b| b < PRIOR_FLOOR) {
            ("none", match best {
                Some(b) => format!("no prior: no record holds all these words, and the nearest by meaning is weak ({b:.2} < {PRIOR_FLOOR})"),
                None => "no prior: no record holds all these words (meaning search not available)".into(),
            })
        } else {
            ("unclear", format!("no record holds all these words; the nearest by meaning is {:.2}, so read the hits before concluding", best.unwrap_or(0.0)))
        };
        serde_json::json!({"verdict": verdict, "says": says, "floor": PRIOR_FLOOR})
    }

    pub fn strength(&self, query: &str, channel: Option<&str>, before_ms: i64, sem: Option<&crate::semantic::Semantic>) -> Result<serde_json::Value> {
        let c = self.conn()?;
        let expr = fts_query(query, " AND ");
        let (all, before, earliest): (i64, i64, Option<i64>) = if expr.is_empty() {
            (0, 0, None)
        } else {
            c.query_row(
                "SELECT count(*), coalesce(sum(o.created_ms < ?3), 0), min(o.created_ms) FROM                  (SELECT f.rowid AS seq FROM obs_fts f WHERE obs_fts MATCH ?1 LIMIT 1000) m                  JOIN obs_live o ON o.seq=m.seq WHERE (?2 IS NULL OR o.channel=?2)",
                params![expr, channel, before_ms],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?
        };
        let best = sem.filter(|s| s.windows_loaded() > 0).and_then(|s| s.query_cached(query, 1)).and_then(|v| v.first().map(|x| x.1));
        Ok(serde_json::json!({"all_words": all, "all_words_before": before, "earliest_ms": earliest, "best_meaning": best}))
    }

    pub fn search_with(
        &self,
        query: &str,
        channel: Option<&str>,
        kind: Option<&str>,
        limit: i64,
        sem: Option<&crate::semantic::Semantic>,
    ) -> Result<(Vec<SearchHit>, String)> {
        self.search_ranked(query, channel, kind, limit, sem, Ranker::Rrf)
    }

    pub fn search_ranked(
        &self,
        query: &str,
        channel: Option<&str>,
        kind: Option<&str>,
        limit: i64,
        sem: Option<&crate::semantic::Semantic>,
        ranker: Ranker,
    ) -> Result<(Vec<SearchHit>, String)> {
        self.search_in(query, channel, kind, limit, sem, ranker, false)
    }

    /// How much an archive would have added (docs/ARCHIVE.md): archived rooms and the records in them.
    pub fn not_considered(&self) -> Result<(i64, i64)> {
        let c = self.conn()?;
        Ok(c.query_row(
            "SELECT count(*), coalesce(sum(h.total), 0) FROM room_archive a LEFT JOIN channel_heads h ON h.channel=a.room",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// The archived rooms: name → why.
    pub fn archived_rooms(&self) -> Result<std::collections::HashMap<String, String>> {
        let c = self.conn()?;
        let mut st = c.prepare_cached("SELECT room, why FROM room_archive")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<Vec<(String, String)>, _>>()?;
        Ok(rows.into_iter().collect())
    }

    /// [`Store::search_ranked`] with `include_archived`: the archived rooms and expired records too.
    #[allow(clippy::too_many_arguments)]
    pub fn search_in(
        &self,
        query: &str,
        channel: Option<&str>,
        kind: Option<&str>,
        limit: i64,
        sem: Option<&crate::semantic::Semantic>,
        ranker: Ranker,
        include_archived: bool,
    ) -> Result<(Vec<SearchHit>, String)> {
        // Where a search spends its time, stage by stage (docs/SEARCH.md "Measured in use"); the handler returns it as timing_ms.
        let laps = std::cell::RefCell::new((std::time::Instant::now(), Vec::<(&'static str, u64)>::new()));
        let lap = |stage: &'static str| {
            let mut l = laps.borrow_mut();
            let ms = l.0.elapsed().as_millis() as u64;
            l.1.push((stage, ms));
            l.0 = std::time::Instant::now();
        };
        let out = self.search_in_timed(query, channel, kind, limit, sem, ranker, include_archived, &lap);
        SEARCH_LAPS.with(|t| *t.borrow_mut() = laps.into_inner().1);
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn search_in_timed(
        &self,
        query: &str,
        channel: Option<&str>,
        kind: Option<&str>,
        limit: i64,
        sem: Option<&crate::semantic::Semantic>,
        ranker: Ranker,
        include_archived: bool,
        lap: &dyn Fn(&'static str),
    ) -> Result<(Vec<SearchHit>, String)> {
        let table = if include_archived { "observations" } else { "obs_live" };
        let limit = limit.clamp(1, 100);
        // Rank more than asked, so a page is still full after copies fold into one hit, and so spread_rooms has other
        // rooms within reach: at limit x 2 (20 for a page of 10), 17 of the 20 for "agents as dragons" were #cortex.
        let pool = (limit * 2).max(crate::semantic::SIDE as i64).min(200);
        let c = self.conn()?;
        // An observation id, or the hex tail of one (people quote "3be83c3f82ba"), names records exactly:
        // those come first. Keyword search cannot find them: the id is not in the indexed text.
        let alias = id_alias(&c, query, channel, kind, limit)?;
        lap("alias");
        if fts_query(query, " AND ").is_empty() {
            let mut hits = Vec::with_capacity(alias.len());
            for seq in &alias {
                if let Some(obs) = Self::load(&c, "WHERE o.seq=?1", &[seq])?.into_iter().next() {
                    let edges = edges_of(&c, &obs)?;
                    let author_kind = author_kind_on(&c, &obs.author);
                    hits.push(SearchHit::build(obs, ALIAS_SCORE, None, vec![], None, edges, author_kind));
                }
            }
            let engine = if hits.is_empty() { "keyword" } else { "id alias" };
            return Ok((hits, engine.into()));
        }
        let keyword_sql = format!("SELECT f.rowid, bm25(obs_fts) * {w} AS r FROM obs_fts f JOIN {table} o ON o.seq=f.rowid \
                 WHERE obs_fts MATCH ?1 AND (?2 IS NULL OR o.channel=?2) AND (?3 IS NULL OR o.kind=?3) \
                 ORDER BY r LIMIT ?4", w = room_weight_sql("o.channel"));
        let keyword_on = |conn: &rusqlite::Connection, expr: &str, n: i64| -> Result<Vec<(i64, f64)>> {
            let mut st = conn.prepare_cached(&keyword_sql)?;
            let found = st
                .query_map(params![expr, channel, kind, n], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<Vec<(i64, f64)>, _>>()?;
            Ok(found)
        };
        let keyword = |expr: &str, n: i64| keyword_on(&c, expr, n);
        // The minds the query names (Blaze, 3 Nov 2026, the 20 real moments: "can Rocky start a context compaction on his own"
        // lost to February readings about context; Rocky's own "I can't trigger compaction myself" never reached the first 50):
        // their own records, by the same words, are a third voice in the fusion. Oracle knows who signed every record.
        let minds = named_minds(self, &c, query);
        // On the search's own connection, after the parallel step: the mind arm takes no connection of its own.
        let by_mind = |expr: &str, n: i64| -> Result<Vec<i64>> {
            if minds.is_empty() { return Ok(vec![]); }
            let sql = format!("SELECT f.rowid, bm25(obs_fts) AS r FROM obs_fts f JOIN {table} o ON o.seq=f.rowid \
                 WHERE obs_fts MATCH ?1 AND (?2 IS NULL OR o.channel=?2) AND (?3 IS NULL OR o.kind=?3) \
                 AND o.author IN (SELECT value FROM json_each(?5)) ORDER BY r LIMIT ?4");
            let mut st = c.prepare_cached(&sql)?;
            let found = st.query_map(params![expr, channel, kind, n, serde_json::json!(minds).to_string()], |r| r.get::<_, i64>(0))?
                .collect::<std::result::Result<Vec<i64>, _>>()?;
            Ok(found)
        };
        let mut engine = match (sem, crate::semantic::OFF_REASON.get()) {
            (None, Some(why)) => format!("keyword (meaning search off: {why})"),
            _ => String::from("keyword"),
        };
        let mut match_expr = fts_query(query, " OR ");
        let mut ranked: Vec<(i64, f64)> = Vec::new();
        let mut engine_minds: Option<String> = None;
        if let Some(sem) = sem.filter(|s| s.windows_loaded() > 0) {
            // The meaning side (embed the query, scan the vectors) and the two keyword passes do not depend on each
            // other: run them at once (measured 25 Oct on a canary: ~0.35 s + ~0.25 s + ~0.25 s one after another).
            let side = crate::semantic::SIDE as i64;
            let all_expr = fts_query(query, " AND ");
            let (near, words, all_words) = std::thread::scope(|sc| {
                let meaning = sc.spawn(|| sem.query(query, 400));
                let every = sc.spawn(|| -> Result<Vec<(i64, f64)>> { let conn = self.conn()?;
                    keyword_on(&conn, &all_expr, side)
                });
                let any = keyword(&match_expr, side);
                (meaning.join().expect("meaning search thread"), any, every.join().expect("keyword thread"))
            });
            let mind_words = by_mind(&match_expr, side);
            lap("embed+vector+keyword");
            match near {
                Ok(near) => {
                    let mut ok = c.prepare_cached(
                        &format!("SELECT 1 FROM {table} WHERE seq=?1 AND (?2 IS NULL OR channel=?2) AND (?3 IS NULL OR kind=?3)"),
                    )?;
                    let mut meaning = Vec::new();
                    for (seq, _) in near {
                        if ok.exists(params![seq, channel, kind])? {
                            meaning.push(seq);
                            if meaning.len() == crate::semantic::SIDE {
                                break;
                            }
                        }
                    }
                    lap("meaning-filter");
                    let words: Vec<i64> = words?.into_iter().map(|h| h.0).collect();
                    let all_words: Vec<i64> = all_words?.into_iter().map(|h| h.0).collect();
                    let mind_words = mind_words?;
                    if !mind_words.is_empty() {
                        engine_minds = Some(minds.join(", "));
                    }
                    ranked = match ranker {
                        Ranker::Rrf => crate::semantic::rrf(&[words, meaning, mind_words], crate::semantic::RRF_K),
                        // Every word first, then any word: the keyword side the docstring always promised.
                        Ranker::AndFirst => {
                            let mut kw = all_words.clone();
                            kw.extend(words.into_iter().filter(|w| !all_words.contains(w)));
                            kw.truncate(side as usize);
                            crate::semantic::rrf(&[kw, meaning], crate::semantic::RRF_K)
                        }
                        // The every-word list as a third voice in the fusion.
                        Ranker::And3 => crate::semantic::rrf(&[words, meaning, all_words], crate::semantic::RRF_K),
                        Ranker::Keyword => {
                            let mut kw = all_words.clone();
                            kw.extend(words.into_iter().filter(|w| !all_words.contains(w)));
                            kw.iter().enumerate().map(|(i, s)| (*s, 1.0 / (i as f64 + 1.0))).collect()
                        }
                        Ranker::AndTop => {
                            let lead: Vec<i64> = all_words.iter().take(3).copied().collect();
                            let mut rest = crate::semantic::rrf(&[words, meaning], crate::semantic::RRF_K);
                            rest.retain(|(s, _)| !lead.contains(s));
                            let mut out: Vec<(i64, f64)> = lead.iter().map(|s| (*s, 1.0)).collect();
                            out.extend(rest);
                            out
                        }
                        Ranker::Wrrf => crate::semantic::rrf(&[words.clone(), words, meaning], crate::semantic::RRF_K),
                        Ranker::K10 => crate::semantic::rrf(&[words, meaning], 10.0),
                    };
                    reweigh(&c, &mut ranked)?;
                    lap("reweigh");
                    ranked.truncate(pool as usize);
                    engine = match &engine_minds { Some(m) => format!("hybrid + their own words ({m})"), None => "hybrid".into() };
                }
                Err(e) => engine = format!("keyword (meaning search unavailable: {e})"),
            }
        }
        if !engine.starts_with("hybrid") {
            // The whole candidate pool, as the hybrid side has, so spread_rooms has another room's hits to bring up.
            for joiner in [" AND ", " OR "] {
                for hit in keyword(&fts_query(query, joiner), pool)? {
                    if !ranked.iter().any(|(seq, _)| *seq == hit.0) {
                        ranked.push((hit.0, -hit.1)); // bm25() is lower-is-better
                    }
                }
                if ranked.len() as i64 >= pool {
                    break;
                }
            }
            if ranked.is_empty() && relaxed_query(query, " OR ") != match_expr {
                // A quoted phrase with no strict lexical hits has an explicit relaxed fallback.
                match_expr = relaxed_query(query, " OR ");
                for joiner in [" AND ", " OR "] {
                    for hit in keyword(&relaxed_query(query, joiner), limit)? {
                        if !ranked.iter().any(|(seq,_)| *seq == hit.0) { ranked.push((hit.0,-hit.1)); }
                    }
                    if ranked.len() as i64 >= limit { break }
                }
                engine.push_str(" (quoted phrase relaxed: no strict lexical matches)");
            }
            ranked.truncate(pool as usize);
        }
        if !alias.is_empty() {
            ranked.retain(|(seq, _)| !alias.contains(seq));
            let mut front: Vec<(i64, f64)> = alias.iter().map(|&s| (s, ALIAS_SCORE)).collect();
            front.append(&mut ranked);
            ranked = front;
            ranked.truncate(pool as usize);
            engine = format!("id alias + {engine}");
        }
        let tuning = SEARCH_TUNING.with(|t| *t.borrow());
        if tuning.recency > 0.0 {
            reweigh_recent(&c, &mut ranked, tuning.recency, now_ms_pub())?;
            engine = format!("{engine} + recent ({:.2})", tuning.recency);
        }
        // Valid in time (3 Nov 2026, the competitor map's temporal-facts row): as of a moment, what was written after it is not
        // on record yet and a fact replaced later still held; a fact whose valid-until has passed ranks as a replaced one does.
        let clock = tuning.as_of.unwrap_or_else(now_ms_pub);
        if let Some(at) = tuning.as_of {
            let mut st = c.prepare_cached("SELECT created_ms FROM observations WHERE seq=?1")?;
            let mut keep = Vec::with_capacity(ranked.len());
            for r in ranked.drain(..) {
                if st.query_row([r.0], |x| x.get::<_, i64>(0)).optional()?.is_some_and(|ms| ms <= at) {
                    keep.push(r);
                }
            }
            ranked = keep;
            engine = format!("{engine} + as of {}", oracle_proto::iso_ms(at));
        }
        let superseded = demote_superseded(&c, &mut ranked, tuning.as_of)?;
        let expired = demote_expired(&c, &mut ranked, clock)?;
        let invalid_since = |seq: &i64| superseded.get(seq).map(|(_, at)| *at).into_iter().chain(expired.get(seq).copied()).min().map(oracle_proto::iso_ms);
        // Tiers first, so the room cap orders first-hand records among themselves (with scaffolding counted in, two
        // mail answers in evals/judged-20261010-r2 canon-ox fell from 1 and 2 to 4 and 7 on the 26 Oct canary).
        let views = self.cortex_views(&c).ok();
        let verified = views.as_ref().map(|v| &v.concepts.approved);
        demote_synthetic(&c, &mut ranked, verified)?;
        if channel.is_none() {
            spread_rooms(&c, &mut ranked, limit)?;
            // Source quality is the final constraint: diversity must not lift graph scaffolding above an original
            // simply because the originals share a room. A stable partition, so this second pass changes nothing else;
            // exact-id aliases still lead inside demote_synthetic.
            demote_synthetic(&c, &mut ranked, verified)?;
        }
        lap("demote");
        // Every approved passage answers through its verified node, not its copies (docs/SEARCH.md "Approved first").
        if let Some(v) = views.as_ref() {
            let moved = collapse_copies(&c, &mut ranked, &v.concepts.quotes)?;
            if moved > 0 {
                engine = format!("{engine} + approved first ({moved})");
            }
        }
        lap("approved");
        // Through the graph last, so what the query names leads whatever the word match did; a failure leaves recall whole.
        let mut named_lead = Vec::new();
        match self.graph_lead(&c, query, channel, kind, table, &mut ranked, &mut named_lead) {
            Ok(Some(named)) => engine = format!("{engine} + graph ({named})"),
            Ok(None) => {}
            Err(e) => engine = format!("{engine} (graph unavailable: {e})"),
        }
        lap("graph");
        let words = content_words(query);
        let empty = std::collections::BTreeSet::new();
        let approved_set = views.as_ref().map_or(&empty, |v| &v.concepts.approved);
        let (held, led) = approved_first(&c, &mut ranked, &words, approved_set, &named_lead, tuning.approved)?;
        if led > 0 {
            engine = format!("{engine} + {} ({led})", match tuning.approved {
                ApprovedOrder::Lead => "approved about it first",
                ApprovedOrder::Boost => "approved about it raised",
                ApprovedOrder::Off => "approved strays lowered",
            });
        }
        lap("approved-first");
        // Last, the cross-encoder reads the query with each of the first RERANK_DEPTH candidates and orders them (Hindsight's
        // order comes from the same model); an exact id stays first; a failure keeps the
        // fused order and says so.
        if let Some(sem) = sem.filter(|s| tuning.rerank && s.reranks()) {
            let start = ranked.iter().take_while(|(_, s)| *s >= ALIAS_SCORE).count();
            let end = (start + RERANK_DEPTH).min(ranked.len());
            if end > start + 1 {
                let mut texts = Vec::with_capacity(end - start);
                let mut st = c.prepare_cached("SELECT content FROM observations WHERE seq=?1")?;
                for (seq, _) in &ranked[start..end] {
                    let content: String = st.query_row(params![seq], |r| r.get(0)).unwrap_or_default();
                    texts.push(passage(&content, &words));
                }
                match sem.rerank(query, &texts) {
                    Ok(scores) => {
                        let mut slice: Vec<((i64, f64), f32)> = ranked[start..end].iter().copied().zip(scores).collect();
                        slice.sort_by(|a, b| b.1.total_cmp(&a.1));
                        for (i, (r, _)) in slice.into_iter().enumerate() {
                            ranked[start + i] = r;
                        }
                        engine = format!("{engine} + reranked ({})", end - start);
                    }
                    Err(e) => engine = format!("{engine} (rerank unavailable: {e})"),
                }
            }
            lap("rerank");
        }
        let mut kinds: std::collections::HashMap<String, &'static str> = std::collections::HashMap::new();
        // Approved means verified as the owner's (owner or attested), read from the cortex views, never a cargo field.
        let verified = self.cortex_views(&c).ok().map(|v| v.concepts.clone());
        let verified = verified.as_ref().map(|v| &v.approved);
        let mut hits: Vec<SearchHit> = Vec::with_capacity(limit as usize);
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut like: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (seq, score) in ranked {
            if hits.len() as i64 >= limit {
                break;
            }
            if let Some(obs) = Self::load(&c, "WHERE o.seq=?1", &[&seq])?
                .into_iter()
                .next()
            {
                let key = same_body_key(&obs.content);
                if let Some(&k) = key.as_ref().and_then(|k| seen.get(k)) {
                    // The approved reading stands for its copies (the import, the extract, the context row): when a copy
                    // led and the approved observation comes later, the approved one takes the place and the copy folds in.
                    if hits[k].brain.is_none() && verified.as_ref().is_some_and(|v| v.contains(&obs.id)) {
                        if let Some(brain) = brain_of(&c, &obs)? {
                            let at = match_at(&c, seq, &match_expr)?;
                            let edges = edges_of(&c, &obs)?;
                            let author_kind = *kinds.entry(obs.author.clone()).or_insert_with(|| author_kind_on(&c, &obs.author));
                            let mut hit = SearchHit::build(obs, hits[k].score, at, vec![], superseded.get(&seq).map(|s| s.0.clone()), edges, author_kind);
                            hit.invalid_since = invalid_since(&seq);
                            hit.brain = Some(brain);
                            hit.approved = true;
                            hit.words = held.get(&seq).filter(|_| !words.is_empty()).map(|n| [*n, words.len()]);
                            let old = std::mem::replace(&mut hits[k], hit);
                            hits[k].same_as.push(old.obs.id.clone());
                            hits[k].same_as.extend(old.same_as);
                            continue;
                        }
                    }
                    hits[k].same_as.push(obs.id.clone());
                    continue;
                }
                let author_kind = *kinds.entry(obs.author.clone()).or_insert_with(|| author_kind_on(&c, &obs.author));
                // A machine's templated repeats take one place: a person's words, an approved reading and the cortex never fold.
                let blessed = verified.as_ref().is_some_and(|v| v.contains(&obs.id));
                let template = (author_kind != "person" && !blessed && obs.channel != "cortex")
                    .then(|| template_key(&obs.author, &obs.channel, &obs.content))
                    .flatten();
                if let Some(&k) = template.as_ref().and_then(|t| like.get(t)) {
                    hits[k].like.push(obs.id.clone());
                    continue;
                }
                if let Some(k) = key {
                    seen.insert(k, hits.len());
                }
                if let Some(t) = template {
                    like.insert(t, hits.len());
                }
                let at = match_at(&c, seq, &match_expr)?;
                let edges = edges_of(&c, &obs)?;
                let mut hit = SearchHit::build(obs, score, at, vec![], superseded.get(&seq).map(|s| s.0.clone()), edges, author_kind);
                hit.invalid_since = invalid_since(&seq);
                hit.brain = brain_of(&c, &hit.obs)?;
                hit.approved = verified.as_ref().is_some_and(|v| v.contains(&hit.obs.id));
                hit.words = held.get(&seq).filter(|_| !words.is_empty()).map(|n| [*n, words.len()]);
                hits.push(hit);
            }
        }
        lap("build-hits");
        Ok((hits, engine))
    }

    pub fn state_head(&self, subject: &str) -> Result<StateHead> {
        let c = self.conn()?;
        let (seq, owner, state, next_touch): (i64, String, String, Option<String>) = c
            .query_row(
                "SELECT obs_seq, owner, state, next_touch FROM state_heads WHERE subject=?1",
                params![subject],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        let observation = Self::load(&c, "WHERE o.seq=?1", &[&seq])?
            .into_iter()
            .next()
            .ok_or(StoreError::NotFound)?;
        Ok(StateHead {
            subject: subject.to_string(),
            owner,
            state,
            next_touch,
            observation,
        })
    }

    pub fn state_heads(&self, owner: Option<&str>) -> Result<Vec<StateRow>> {
        self.state_heads_in(owner, None)
    }

    /// Heads by owner and/or room. Either filter may be absent; both absent is everyone's, everywhere.
    pub fn state_heads_in(&self, owner: Option<&str>, channel: Option<&str>) -> Result<Vec<StateRow>> {
        let c = self.conn()?;
        let mut st = c.prepare_cached(
            "SELECT h.subject, h.owner, h.state, h.next_touch, o.channel, o.id, o.content, o.created_ms \
             FROM state_heads h JOIN observations o ON o.seq = h.obs_seq \
             WHERE (?1 IS NULL OR h.owner=?1) AND (?2 IS NULL OR o.channel=?2) ORDER BY h.subject",
        )?;
        let rows = st
            .query_map(params![owner, channel], |r| {
                let content: String = r.get(6)?;
                Ok(StateRow {
                    subject: r.get(0)?,
                    owner: r.get(1)?,
                    state: r.get(2)?,
                    next_touch: r.get(3)?,
                    channel: r.get(4)?,
                    id: r.get(5)?,
                    note: content.lines().next().unwrap_or("").chars().take(200).collect(),
                    at_ms: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every experiment in every room: the heads whose subject starts `exp.`, each with the fields of its cargo's
    /// `data` (schema experiment/1: title, track, stage, hypothesis, result, metric, evidence ...), newest first.
    /// Blaze, 2026-10-08: "all the experiments across all the rooms should be linked and accounted for" and
    /// queryable by any seat, not only through the Mathematics room's Experiments view.
    pub fn experiments(&self) -> Result<Vec<serde_json::Value>> {
        let c = self.conn()?;
        let mut st = c.prepare_cached(
            "SELECT h.subject, h.owner, h.state, h.next_touch, o.channel, o.id, o.content, o.created_ms, o.cargo \
             FROM state_heads h JOIN observations o ON o.seq = h.obs_seq \
             WHERE h.subject LIKE 'exp.%' ORDER BY o.created_ms DESC, h.subject",
        )?;
        let rows = st
            .query_map([], |r| {
                let cargo: Option<String> = r.get(8)?;
                let data = cargo
                    .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                    .map(|v| v["data"].clone())
                    .filter(|d| d.is_object())
                    .unwrap_or(serde_json::Value::Null);
                let content: String = r.get(6)?;
                Ok(serde_json::json!({
                    "subject": r.get::<_, String>(0)?, "owner": r.get::<_, String>(1)?, "state": r.get::<_, String>(2)?,
                    "next_touch": r.get::<_, Option<String>>(3)?, "room": r.get::<_, String>(4)?, "id": r.get::<_, String>(5)?,
                    "note": content.lines().next().unwrap_or("").chars().take(200).collect::<String>(),
                    "at_ms": r.get::<_, i64>(7)?, "data": data,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn stats(&self) -> Result<serde_json::Value> {
        let c = self.conn()?;
        let n = |sql: &str| -> Result<i64> { Ok(c.query_row(sql, [], |r| r.get(0))?) };
        Ok(serde_json::json!({
            "observations": n("SELECT count(*) FROM observations")?,
            "channels": n("SELECT count(DISTINCT channel) FROM observations")?,
            "seats": n("SELECT count(DISTINCT handle) FROM keys WHERE revoked_ms IS NULL")?,
            "tags": n("SELECT count(*) FROM tags")?,
            "state_heads": n("SELECT count(*) FROM state_heads")?,
        }))
    }
}

pub(crate) struct StateCargo {
    subject: String,
    state: String,
    owner: String,
    next_touch: Option<String>,
}

const STATES: [&str; 5] = ["noted", "open", "running", "blocked", "done"];

/// A state head as an earlier Oracle recorded it: an observation tagged `live-state` with
/// `subject:`, `state:`, `owner:` and `clock:` tags. `ruled` was that Oracle's word for done.
fn legacy_state(r: &LegacyRow) -> Option<StateCargo> {
    if !r.tags.iter().any(|t| t == "live-state") {
        return None;
    }
    let tag = |p: &str| {
        r.tags
            .iter()
            .find_map(|t| t.strip_prefix(p))
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let subject = tag("subject:").filter(|s| s.len() <= 200)?;
    let state = match tag("state:")?.as_str() {
        "ruled" => "done".to_string(),
        s if STATES.contains(&s) => s.to_string(),
        _ => return None,
    };
    Some(StateCargo {
        subject,
        state,
        owner: tag("owner:").unwrap_or_else(|| r.author.clone()),
        next_touch: tag("clock:"),
    })
}

/// Rows arrive in time order, so the last write of a subject wins — but never over a head
/// that a newer observation (a native write, say) already holds.
fn upsert_legacy_head(tx: &rusqlite::Transaction, h: &StateCargo, seq: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO state_heads(subject,obs_seq,owner,state,next_touch) VALUES(?1,?2,?3,?4,?5) \
         ON CONFLICT(subject) DO UPDATE SET obs_seq=excluded.obs_seq, owner=excluded.owner, \
         state=excluded.state, next_touch=excluded.next_touch WHERE (SELECT created_ms FROM observations WHERE seq=excluded.obs_seq) > \
         (SELECT created_ms FROM observations WHERE seq=state_heads.obs_seq) OR \
         ((SELECT created_ms FROM observations WHERE seq=excluded.obs_seq) = \
         (SELECT created_ms FROM observations WHERE seq=state_heads.obs_seq) AND excluded.obs_seq > state_heads.obs_seq)",
        params![h.subject, seq, h.owner, h.state, h.next_touch],
    )?;
    Ok(())
}

pub(crate) fn parse_state_cargo(env: &Envelope) -> Result<StateCargo> {
    let raw = env
        .cargo
        .as_deref()
        .ok_or_else(|| StoreError::Rejected("a state write needs cargo".into()))?;
    let v: serde_json::Value = serde_json::from_str(raw)
        .map_err(|_| StoreError::Rejected("state cargo is not JSON".into()))?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let subject = s("subject")
        .filter(|x| !x.is_empty() && x.len() <= 200)
        .ok_or_else(|| StoreError::Rejected("state cargo needs a subject".into()))?;
    let state = s("state")
        .filter(|x| STATES.contains(&x.as_str()))
        .ok_or_else(|| StoreError::Rejected(format!("state must be one of {STATES:?}")))?;
    let next_touch = s("next_touch");
    if matches!(state.as_str(), "open" | "running") && next_touch.is_none() {
        return Err(StoreError::Rejected(
            "an open or running head needs next_touch, or it rots".into(),
        ));
    }
    Ok(StateCargo {
        subject,
        state,
        owner: s("owner").unwrap_or_else(|| env.author.clone()),
        next_touch,
    })
}

/// Split `limit` across channels: everyone gets an equal share, leftovers go to the
/// channels that still have more waiting. A loud room cannot starve a quiet one.
pub fn fair_share(pending: &BTreeMap<String, i64>, limit: i64) -> BTreeMap<String, i64> {
    let mut shares: BTreeMap<String, i64> = pending.keys().map(|k| (k.clone(), 0)).collect();
    let mut left = limit;
    loop {
        let hungry: Vec<&String> = pending
            .iter()
            .filter(|(k, n)| shares[*k] < **n)
            .map(|(k, _)| k)
            .collect();
        if hungry.is_empty() || left <= 0 {
            break;
        }
        let each = (left / hungry.len() as i64).max(1);
        for k in hungry {
            if left <= 0 {
                break;
            }
            let give = each.min(pending[k] - shares[k]).min(left);
            *shares.get_mut(k).unwrap() += give;
            left -= give;
        }
    }
    shares.retain(|_, v| *v > 0);
    shares
}

/// Rooms whose messages rank lower in every search (Blaze, 2026-10-08: "weight graph-synthesis down everywhere":
/// its machine-written RESONANCE echoes took 59 of 80 results for 'observer protocol'). A score is multiplied.
pub const ROOM_WEIGHT: &[(&str, f64)] = &[("graph-synthesis", 0.2)];

/// The weight as SQL over a channel column, to multiply bm25() with (bm25 is lower-is-better, so a weight
/// under 1 moves a match toward 0: later).
pub fn room_weight_sql(col: &str) -> String {
    let mut out = format!("(CASE {col}");
    for (room, w) in ROOM_WEIGHT {
        out.push_str(&format!(" WHEN '{}' THEN {w:?}", room.replace('\'', "''")));
    }
    out.push_str(" ELSE 1.0 END)");
    out
}

/// Multiply each (seq, score) by its room's weight and re-sort best first, ties in the order given.
pub fn reweigh(c: &rusqlite::Connection, ranked: &mut [(i64, f64)]) -> rusqlite::Result<()> {
    if ROOM_WEIGHT.is_empty() || ranked.is_empty() {
        return Ok(());
    }
    let mut st = c.prepare_cached("SELECT channel FROM observations WHERE seq=?1")?;
    for (seq, score) in ranked.iter_mut() {
        let room: Option<String> = st.query_row([*seq], |r| r.get(0)).ok();
        if let Some(w) = room.and_then(|r| ROOM_WEIGHT.iter().find(|(n, _)| *n == r).map(|(_, w)| *w)) {
            *score *= w;
        }
    }
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(())
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    #[test]
    fn online_control_does_not_rebuild_live_projections() {
        let file = std::env::temp_dir().join(format!("oracle-control-{}.db", oracle_proto::random_hex(12)));
        let store = Store::open(file.to_str().unwrap()).unwrap();
        store.conn().unwrap().execute("INSERT INTO room_archive(room,obs_seq,at_ms,by,why) VALUES('keep',1,1,'owner','sentinel')", []).unwrap();
        drop(store);
        let control = Store::open_control(file.to_str().unwrap()).unwrap();
        assert_eq!(control.conn().unwrap().query_row("SELECT why FROM room_archive WHERE room='keep'", [], |r| r.get::<_, String>(0)).unwrap(), "sentinel");
        drop(control);
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn online_control_refuses_uninitialized_schema_without_creating_tables() {
        let file = std::env::temp_dir().join(format!("oracle-control-{}.db", oracle_proto::random_hex(12)));
        let c = Connection::open(&file).unwrap();
        assert!(Store::open_control(file.to_str().unwrap()).is_err());
        assert_eq!(c.query_row("SELECT count(*) FROM sqlite_master WHERE type='table'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        drop(c);
        std::fs::remove_file(file).unwrap();
    }

    /// The plan SQLite picks for a filtered read of one room. Bound as the server binds it.
    fn plan(channel: bool) -> String {
        let store = Store::memory().unwrap();
        let c = store.conn().unwrap();
        let sql = format!(
            "EXPLAIN QUERY PLAN SELECT o.seq FROM observations o {}",
            recent_filtered_sql(channel, false)
        );
        let mut st = c.prepare(&sql).unwrap();
        let ch: Option<&str> = channel.then_some("hustler");
        let starts: Option<&str> = Some("[Hustler members]");
        let rows: Vec<String> = st
            .query_map(params![i64::MAX, ch, starts, 17i64, Option::<&str>::None, 20i64, 0i64], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows.join(" | ")
    }

    #[test]
    fn a_room_read_walks_the_room_index_not_the_whole_store() {
        // room_roots() runs this for every thread view; a whole-store walk cost 272 ms per call on
        // the live ledger (2026-09-27) and made the digest take 33 s.
        let p = plan(true);
        assert!(p.contains("obs_channel_seq"), "channel read must use obs_channel_seq, got: {p}");
    }

    /// Emmy's review of 1673cfd: when the cortex views cannot be read (verified = None), a cargo `approved_by` must not
    /// lead; the same row leads once the verified set names it (the control).
    #[test]
    fn without_the_verified_set_a_cargo_approval_never_leads() {
        let store = Store::memory().unwrap();
        let c = store.conn().unwrap();
        let row = |id: &str, channel: &str, cargo: &str| {
            c.execute("INSERT INTO observations(id, channel, author, kind, content, cargo, tags_json, created_ms, ts_ms, nonce, client_op, pubkey, sig) \
                       VALUES (?1, ?2, 'x', 'say', 'c', ?3, '[]', 1, 1, ?1, ?1, 'k', 's')", params![id, channel, cargo]).unwrap();
            c.last_insert_rowid()
        };
        let first = row("OBS-a", "oracle", "{}");
        let node = row("OBS-n", "cortex", r#"{"approved_by":"blaze","kind":"observation"}"#);
        c.execute("INSERT INTO tags(name, obs_seq) VALUES ('cortex:node', ?1)", params![node]).unwrap();
        let mut none = vec![(first, 0.030), (node, 0.029)];
        demote_synthetic(&c, &mut none, None).unwrap();
        assert_eq!(none[0].0, first, "no verified set: the cargo field alone does not lead");
        let set: std::collections::BTreeSet<String> = ["OBS-n".to_string()].into();
        let mut some = vec![(first, 0.030), (node, 0.029)];
        demote_synthetic(&c, &mut some, Some(&set)).unwrap();
        assert_eq!(some[0].0, node, "verified: it leads");
    }

    /// Blaze, 30 Oct 2026: an approved passage answers through its verified node. A February import of the same prompt
    /// ranks first on the words; the approved node takes that place and the import follows. An unrelated record and a
    /// copy of an unapproved passage keep their places (the control).
    #[test]
    fn the_approved_node_takes_the_place_of_its_copies() {
        let store = Store::memory().unwrap();
        let c = store.conn().unwrap();
        let row = |id: &str, channel: &str, content: &str| {
            c.execute("INSERT INTO observations(id, channel, author, kind, content, cargo, tags_json, created_ms, ts_ms, nonce, client_op, pubkey, sig) \
                       VALUES (?1, ?2, 'x', 'say', ?3, '{}', '[]', 1, 1, ?1, ?1, 'k', 's')", params![id, channel, content]).unwrap();
            c.last_insert_rowid()
        };
        let quote = "L2, I just realized that what we've done as a society has always seen the latest and trending";
        let import = row("OBS-import", "axxis", &format!("{quote}. Right? Everyone gets the default version for their price."));
        let other = row("OBS-other", "oracle", "the tyranny of trending in markets, a summary");
        let node = row("OBS-node", "cortex", quote);
        let quotes = vec![("OBS-node".to_string(), crate::cortex::comparable(quote))];
        let mut ranked = vec![(import, 0.031), (other, 0.030), (node, 0.012)];
        assert_eq!(collapse_copies(&c, &mut ranked, &quotes).unwrap(), 1);
        assert_eq!(ranked.iter().map(|r| r.0).collect::<Vec<_>>(), vec![node, import, other], "the node stands where its copy stood");
        // Control: no approved quote matches, nothing moves.
        let mut same = vec![(import, 0.031), (other, 0.030), (node, 0.012)];
        assert_eq!(collapse_copies(&c, &mut same, &[]).unwrap(), 0);
        assert_eq!(same[0].0, import);
        // A node already above its copy keeps its place and is not moved twice.
        let mut above = vec![(node, 0.040), (import, 0.031)];
        assert_eq!(collapse_copies(&c, &mut above, &quotes).unwrap(), 0);
        assert_eq!(above.iter().map(|r| r.0).collect::<Vec<_>>(), vec![node, import]);
    }

    /// The passage the reranker reads (3 Nov 2026): about sixty words from a little before the first query word, on
    /// character boundaries; with no query word in it, the start.
    #[test]
    fn the_reranker_reads_about_sixty_words_from_the_first_query_word() {
        let long = format!("{} the lattice bootstrap took ten seconds {}", "filler ".repeat(100), "tail ".repeat(100));
        let p = passage(&long, &["lattice".into()]);
        assert!(p.contains("the lattice bootstrap took ten seconds"), "{p}");
        assert!(p.len() <= RERANK_CHARS && p.starts_with("filler"), "a little before the word: {p}");
        assert!(passage("no match here at all", &["lattice".into()]).starts_with("no match"), "no word: the start");
        let multi = format!("{}é lattice", "ü".repeat(50));
        assert!(passage(&multi, &["lattice".into()]).ends_with("lattice"), "cut on character boundaries");
        // Lowercasing changes the byte length of some letters (the Kelvin sign is three bytes, its lowercase k one): no panic.
        let kelvin = format!("{} lattice", "\u{212A}".repeat(60));
        assert!(passage(&kelvin, &["lattice".into()]).contains("lattice"));
    }

    /// Approved first, of what is about the query (31 Oct 2026, "dot-lattice bootstrap OpenFHE" led with an approved
    /// reading about Hoshi): an approved reading holding three in five of the query's words goes to the front; one holding
    /// none goes below the records that hold them; a query of names alone keeps what the graph named. Negative control:
    /// with nothing approved the fused order stands.
    #[test]
    fn approved_readings_lead_only_when_they_hold_the_query_words() {
        let store = Store::memory().unwrap();
        let c = store.conn().unwrap();
        let row = |id: &str, channel: &str, content: &str| {
            c.execute("INSERT INTO observations(id, channel, author, kind, content, cargo, tags_json, created_ms, ts_ms, nonce, client_op, pubkey, sig) \
                       VALUES (?1, ?2, 'x', 'say', ?3, '{}', '[]', 1, 1, ?1, ?1, 'k', 's')", params![id, channel, content]).unwrap();
            let seq = c.last_insert_rowid();
            c.execute("INSERT INTO obs_fts(rowid,content,tags,cargo) VALUES(?1,?2,'[]','{}')", params![seq, content]).unwrap();
            seq
        };
        let stray = row("OBS-stray", "cortex", "Hoshi: a platform that owns the agents and the knowledge");
        let answer = row("OBS-answer", "dot", "dot-lattice bootstrap against OpenFHE: 10.1 s on one thread");
        let half = row("OBS-half", "dot", "a note on the lattice and its bootstrap");
        let blessed = row("OBS-blessed", "cortex", "the dot-lattice bootstrap beats OpenFHE");
        let words = content_words("dot-lattice bootstrap OpenFHE");
        assert_eq!(words, ["dot", "lattice", "bootstrap", "openfhe"]);
        assert_eq!(content_words("how about the open port"), ["open", "port"], "filler words are not content");
        let approved: std::collections::BTreeSet<String> = ["OBS-stray", "OBS-blessed"].iter().map(|s| s.to_string()).collect();
        let fused = vec![(stray, 0.033), (answer, 0.032), (half, 0.031), (blessed, 0.020)];
        let mut ranked = fused.clone();
        let (held, led) = approved_first(&c, &mut ranked, &words, &approved, &[], ApprovedOrder::Lead).unwrap();
        assert_eq!((held[&answer], held[&half], held[&stray], held[&blessed]), (4, 2, 0, 4));
        assert_eq!(led, 1);
        assert_eq!(ranked.iter().map(|r| r.0).collect::<Vec<_>>(), vec![blessed, answer, stray, half], "approved and about it first; the stray below the answer");
        // A query of names alone: what the graph named is about it, words or not.
        let mut named = fused.clone();
        approved_first(&c, &mut named, &words, &approved, &[stray], ApprovedOrder::Lead).unwrap();
        assert_eq!(named[0].0, stray);
        // Control: nothing approved, nothing moves.
        let mut none = fused.clone();
        assert_eq!(approved_first(&c, &mut none, &words, &std::collections::BTreeSet::new(), &[], ApprovedOrder::Lead).unwrap().1, 0);
        assert_eq!(none, fused);
    }

    /// Rank by relevance (Blaze, 3 Nov 2026): under Boost an approved reading about the query rises at most
    /// APPROVED_BOOST places instead of taking the front; under Off it keeps its place; strays still go below.
    /// Negative control: Lead, on the same list, still puts it first.
    #[test]
    fn approved_readings_rise_a_little_under_boost_and_not_at_all_under_off() {
        let store = Store::memory().unwrap();
        let c = store.conn().unwrap();
        let row = |id: &str, content: &str| {
            c.execute("INSERT INTO observations(id, channel, author, kind, content, cargo, tags_json, created_ms, ts_ms, nonce, client_op, pubkey, sig) \
                       VALUES (?1, 'r', 'x', 'say', ?2, '{}', '[]', 1, 1, ?1, ?1, 'k', 's')", params![id, content]).unwrap();
            let seq = c.last_insert_rowid();
            c.execute("INSERT INTO obs_fts(rowid,content,tags,cargo) VALUES(?1,?2,'[]','{}')", params![seq, content]).unwrap();
            seq
        };
        let words = content_words("bridge card blaze");
        let fused: Vec<(i64, f64)> = (0..6).map(|i| (row(&format!("OBS-{i}"), "the bridge card for blaze"), 0.05 - i as f64 * 0.001)).collect();
        let late = row("OBS-approved", "the bridge card blaze reads");
        let mut list = fused.clone();
        list.push((late, 0.01));
        let approved: std::collections::BTreeSet<String> = ["OBS-approved".to_string()].into();
        let place = |order| { let mut r = list.clone(); approved_first(&c, &mut r, &words, &approved, &[], order).unwrap(); r.iter().position(|x| x.0 == late).unwrap() };
        assert_eq!(place(ApprovedOrder::Lead), 0, "control: lead puts it first");
        assert_eq!(place(ApprovedOrder::Boost), 6 - APPROVED_BOOST, "boost: three places up, no more");
        assert_eq!(place(ApprovedOrder::Off), 6, "off: relevance keeps the order");
        assert_eq!(Tuning::parse(Some("boost"), Some(7.0)), Tuning { approved: ApprovedOrder::Boost, recency: 1.0, rerank: true, as_of: None });
        assert!(!Tuning::parse(None, None).with_rerank(Some("off")).rerank && Tuning::parse(None, None).with_rerank(None).rerank, "rerank is on unless off");
        assert_eq!(Tuning::parse(None, None), Tuning::DEFAULT);
    }

    /// Recency: with equal relevance the newer record leads; at recency 0 nothing moves; an id alias stays first.
    #[test]
    fn recency_lifts_the_newer_of_two_equally_relevant_records() {
        let store = Store::memory().unwrap();
        let c = store.conn().unwrap();
        let day = 86_400_000i64;
        let now = 400 * day;
        let row = |id: &str, at: i64| {
            c.execute("INSERT INTO observations(id, channel, author, kind, content, cargo, tags_json, created_ms, ts_ms, nonce, client_op, pubkey, sig) \
                       VALUES (?1, 'r', 'x', 'say', 'same words', '{}', '[]', ?2, ?2, ?1, ?1, 'k', 's')", params![id, at]).unwrap();
            c.last_insert_rowid()
        };
        let (old, new, alias) = (row("OBS-old", now - 90 * day), row("OBS-new", now - day), row("OBS-alias", now - 300 * day));
        let fused = vec![(alias, ALIAS_SCORE), (old, 0.0300), (new, 0.0295)];
        let mut same = fused.clone();
        reweigh_recent(&c, &mut same, 0.0, now).unwrap();
        assert_eq!(same, fused, "recency 0 changes nothing");
        let mut r = fused.clone();
        reweigh_recent(&c, &mut r, 0.3, now).unwrap();
        assert_eq!(r.iter().map(|x| x.0).collect::<Vec<_>>(), vec![alias, new, old]);
    }
}

/// How approved observations place in a ranking (Blaze, 3 Nov 2026, choosing "Recall first: rank by relevance" after
/// Hindsight found 19 of 20 real moments and Oracle 12: approved readings that share three of five words with a
/// conversational question took the first five places and buried the record that answered it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovedOrder {
    /// The first five approved records about the query go to the front (31 Oct 2026, [`approved_first`]).
    Lead,
    /// Approved records about the query rise at most [`APPROVED_BOOST`] places; relevance keeps the order.
    Boost,
    /// No reordering by approval; strays (approved, not about the query) still go below what is.
    Off,
}

/// The ranking options of one search: how approved records place, and how much recency counts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    pub approved: ApprovedOrder,
    /// 0 to 1: a record's fused score is multiplied by `1 + recency * 2^(-age / RECENCY_HALF_LIFE_DAYS)`.
    pub recency: f64,
    /// The cross-encoder orders the first [`RERANK_DEPTH`] candidates when one is configured; `rerank=off` keeps the fused order.
    pub rerank: bool,
    /// Search as of a moment (`as_of=`, [`oracle_proto::parse_when`]): only what was written by then, validity judged then.
    pub as_of: Option<i64>,
}
impl Tuning {
    pub const DEFAULT: Tuning = Tuning { approved: ApprovedOrder::Lead, recency: 0.0, rerank: true, as_of: None };
    pub fn parse(approved: Option<&str>, recency: Option<f64>) -> Tuning {
        let approved = match approved {
            Some("boost") => ApprovedOrder::Boost,
            Some("off") => ApprovedOrder::Off,
            _ => ApprovedOrder::Lead,
        };
        Tuning { approved, recency: recency.filter(|r| r.is_finite()).unwrap_or(0.0).clamp(0.0, 1.0), rerank: true, as_of: None }
    }
    /// The same, with the rerank switch (`rerank=off`).
    pub fn with_rerank(self, rerank: Option<&str>) -> Tuning {
        Tuning { rerank: rerank != Some("off"), ..self }
    }
    /// The same, searching as of a moment.
    pub fn with_as_of(self, as_of: Option<i64>) -> Tuning {
        Tuning { as_of, ..self }
    }
}

/// How many of the first candidates the cross-encoder reorders (3 Nov 2026: on 20 real moments 16 answers were in the
/// first 30 and 13 in the first 5; Hindsight reranks about 300 in 7.4 s, 30 take about 2 s here).
pub const RERANK_DEPTH: usize = 30;
/// The passage of a record the reranker reads: about 60 words around the first query word it holds.
const RERANK_CHARS: usize = 360;
/// A mind is named in a query only when it has signed at least this many records (a one-off seat is not a voice).
const MIND_MIN_RECORDS: i64 = 20;

/// The passage of `content` the reranker reads: RERANK_CHARS characters from a little before the first of `words` it holds
/// (case-insensitive), or its start; cut on character boundaries.
pub(crate) fn passage(content: &str, words: &[String]) -> String {
    let lower = content.to_lowercase();
    let at = words.iter().filter_map(|w| lower.find(w.as_str())).min().unwrap_or(0);
    // `lower` can differ in byte length from `content` for some scripts; clamp and step to a boundary in `content`.
    let mut start = at.saturating_sub(80).min(content.len());
    while !content.is_char_boundary(start) { start -= 1; }
    // Open on a whole word: past the rest of the word the window would cut, when there is a space before the match.
    if start > 0 {
        let mut to = at.min(content.len()).max(start);
        while !content.is_char_boundary(to) { to -= 1; }
        if let Some(sp) = content[start..to.max(start)].find(char::is_whitespace) { start += sp; }
    }
    let mut end = (start + RERANK_CHARS).min(content.len());
    while !content.is_char_boundary(end) { end -= 1; }
    content[start..end].split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The minds a query names by handle (`rocky`, `emmy`) or by a seat handle without its `-core` (`bobby` for bobby-core),
/// each with at least MIND_MIN_RECORDS signed records; read from the signed authors on the search's own connection `c` (a
/// second one could wait forever on a pool of one), cached for ten minutes.
fn named_minds(store: &Store, c: &Connection, query: &str) -> Vec<String> {
    let handles = {
        let mut g = match store.minds.lock() { Ok(g) => g, Err(_) => return vec![] };
        if g.as_ref().is_none_or(|(at, _)| at.elapsed() > std::time::Duration::from_secs(600)) {
            let read = || -> rusqlite::Result<Vec<String>> {
                let mut st = c.prepare_cached("SELECT author, count(*) FROM observations WHERE envelope=2 GROUP BY author")?;
                let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows.into_iter().filter(|(h, n)| *n >= MIND_MIN_RECORDS && crate::minds::is_mind(h)).map(|r| r.0).collect())
            };
            let Ok(handles) = read() else { return vec![] };
            *g = Some((std::time::Instant::now(), handles));
        }
        g.as_ref().map(|(_, h)| h.clone()).unwrap_or_default()
    };
    let words = crate::cortex::words(query);
    handles.into_iter().filter(|h| {
        let short = h.strip_suffix("-core").unwrap_or(h);
        short.len() >= 3 && words.iter().any(|w| w == short || w == h)
    }).collect()
}

/// How many places an approved observation about the query may rise under [`ApprovedOrder::Boost`].
const APPROVED_BOOST: usize = 3;
/// Recency's half-life, in days (see [`Tuning::recency`]).
const RECENCY_HALF_LIFE_DAYS: f64 = 14.0;

/// Multiply each fused score by the record's recency and re-sort, keeping id aliases first. A record `half_life` days
/// old gains half of what one written today gains.
fn reweigh_recent(c: &Connection, ranked: &mut [(i64, f64)], recency: f64, now_ms: i64) -> rusqlite::Result<()> {
    if recency <= 0.0 {
        return Ok(());
    }
    let mut st = c.prepare_cached("SELECT created_ms FROM observations WHERE seq=?1")?;
    for (seq, score) in ranked.iter_mut() {
        if *score >= ALIAS_SCORE {
            continue;
        }
        if let Some(at) = st.query_row(params![*seq], |r| r.get::<_, i64>(0)).optional()? {
            let days = ((now_ms - at).max(0) as f64) / 86_400_000.0;
            *score *= 1.0 + recency * (-days / RECENCY_HALF_LIFE_DAYS).exp2();
        }
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    Ok(())
}

/// Which ranking `search_ranked` uses: the live one, or an arm being measured (Round 50, ox eval).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ranker {
    Rrf,
    AndFirst,
    And3,
    /// Words only, every word first: the control (what seats got while meaning search timed out, 2026-10-10).
    Keyword,
    /// The best few every-word matches lead, in keyword order; the fusion follows.
    AndTop,
    /// The fusion with the keyword list counted twice.
    Wrrf,
    /// The fusion with k = 10: each list's own top ranks weigh more than agreement low in both.
    K10,
}
impl Ranker {
    pub fn parse(s: Option<&str>) -> Ranker {
        match s {
            Some("and-first") => Ranker::AndFirst,
            Some("and3") => Ranker::And3,
            Some("keyword") => Ranker::Keyword,
            Some("and-top") => Ranker::AndTop,
            Some("wrrf") => Ranker::Wrrf,
            Some("k10") => Ranker::K10,
            _ => Ranker::Rrf,
        }
    }
}

/// The body two records share when one is a copy of the other: whitespace collapsed, and a leading CLAUDE-EXPORT header
/// line (which names the import, not the turn) left out. Short bodies ("ok", "👁 seen") are never folded together.
fn same_body_key(content: &str) -> Option<String> {
    let c = content.trim();
    let body = match c.split_once('\n') {
        Some((first, rest)) if first.contains("CLAUDE-EXPORT") => rest,
        _ => c,
    };
    let key = body.split_whitespace().collect::<Vec<_>>().join(" ");
    (key.len() >= 80).then_some(key)
}

/// The template a machine's record was written from: its author, its room and the first [`TEMPLATE_CHARS`] characters of
/// its text with every run of digits (a date, a time, a count) and every hex id (a commit, a hash) made one `#`. Two
/// records with the same key say the same thing at another time ("Hypatia heartbeat receipt 2026-06-20T15:50Z: Checked
/// live Oracle: healthy..."). A short text has no template.
pub fn template_key(author: &str, channel: &str, content: &str) -> Option<String> {
    let words: Vec<String> = content
        .split_whitespace()
        .map(|w| {
            let hex = w.len() >= 7 && w.chars().all(|c| c.is_ascii_hexdigit()) && w.chars().any(|c| c.is_ascii_digit());
            if hex {
                return "#".to_string();
            }
            let mut out = String::with_capacity(w.len());
            for ch in w.chars() {
                if ch.is_ascii_digit() {
                    if !out.ends_with('#') {
                        out.push('#');
                    }
                } else {
                    out.push(ch);
                }
            }
            out
        })
        .collect();
    let text = words.join(" ");
    (text.chars().count() >= 80).then(|| format!("{author}\u{1f}{channel}\u{1f}{}", text.chars().take(TEMPLATE_CHARS).collect::<String>()))
}

/// How much of a record's opening two records must share to be one template.
pub const TEMPLATE_CHARS: usize = 60;

/// How a stored State revision moves its subject's head. A live write moves it on arrival (the write lock already orders
/// writers). A replicated one (sync.rs) moves it only when it is newer by (signed time, id) than the head it finds, so two
/// nodes that receive the same revisions in any order settle on the same head (docs/SYNC.md).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HeadRule {
    Arrival,
    Newest,
}

/// Whether the current head of `subject` is older than a revision signed at `ts_ms` with `id` (no head: older).
fn head_is_older(c: &Connection, subject: &str, ts_ms: i64, id: &str) -> Result<bool> {
    let head: Option<(i64, String)> = c
        .query_row(
            "SELECT o.ts_ms, o.id FROM state_heads h JOIN observations o ON o.seq=h.obs_seq WHERE h.subject=?1",
            params![subject],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(head.is_none_or(|(t, i)| (t, i.as_str()) < (ts_ms, id)))
}

/// Everything a stored row adds beside itself, in the transaction that stored it: the room's head, the tags, the search
/// row, the State head, the room index, the archive and the seat scope. One code path for a live write and a replicated one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn index_stored(tx: &rusqlite::Transaction<'_>, env: &Envelope, id: &str, seq: i64, now_ms: i64, state: Option<StateCargo>, rule: HeadRule) -> Result<()> {
    tx.execute(
        "INSERT INTO channel_heads(channel,max_seq,total) VALUES(?1,?2,1) \
         ON CONFLICT(channel) DO UPDATE SET max_seq=excluded.max_seq, total=total+1",
        params![env.channel, seq],
    )?;

    let mut index_tags = env.tags.clone();
    index_tags.extend(mentions(&env.content));
    if let Some(id) = cargo_reply_to(env.cargo.as_deref()) {
        index_tags.push(format!("re:{id}"));
    }
    index_tags.push(format!("kind:{}", env.kind.as_str()));
    index_tags.push(format!("from:{}", env.author));
    index_tags.sort();
    index_tags.dedup();
    for t in &index_tags {
        tx.execute(
            "INSERT OR IGNORE INTO tags(name,obs_seq) VALUES(?1,?2)",
            params![t, seq],
        )?;
    }
    // The search row also carries the room and the class, so a room can be searched by itself (docs/ROOM-INDEX.md).
    let fts_tags = format!("{} {}", index_tags.join(" "), crate::roomindex::fts_tokens(&env.channel, crate::roomindex::class_parts(env.kind.as_str(), &env.tags, &env.content)));
    fts_insert(tx, seq, &env.content, &fts_tags, env.cargo.as_deref())?;
    if let Some(s) = state.filter(|s| rule == HeadRule::Arrival || head_is_older(tx, &s.subject, env.ts_ms, id).unwrap_or(true)) {
        let prior = crate::livestate::prior_head(tx, &s.subject)?;
        tx.execute(
            "INSERT INTO state_heads(subject,obs_seq,owner,state,next_touch) VALUES(?1,?2,?3,?4,?5) \
             ON CONFLICT(subject) DO UPDATE SET obs_seq=excluded.obs_seq, owner=excluded.owner, \
             state=excluded.state, next_touch=excluded.next_touch",
            params![s.subject, seq, s.owner, s.state, s.next_touch],
        )?;
        // Live state keeps its head, not every revision (livestate.rs).
        let live = env.tags.iter().any(|t| t == "live-state") || crate::livestate::MACHINE_OWNERS.contains(&s.owner.as_str());
        crate::livestate::retire_prior(tx, prior, &s.subject, seq, &s.state, live)?;
    }
    // The room index (docs/ROOM-INDEX.md): the projections change in the transaction that stored the message.
    crate::roomindex::project_new(tx, seq)?;
    note_archive(tx, &env.channel, seq, now_ms, &env.author, &env.tags, env.cargo.as_deref())?;
    // The owner's newest valid seat-rooms record is the scope table; anyone else's, or an invalid one, changes nothing.
    if env.channel == crate::seatrooms::ROOM && env.content.starts_with(crate::seatrooms::FACE) {
        let owner: i64 = tx.query_row("SELECT count(*) FROM keys WHERE handle=?1 AND role='owner' AND revoked_ms IS NULL", params![env.author], |x| x.get(0))?;
        if let (true, Some(t)) = (owner > 0, env.cargo.as_deref().and_then(|g| crate::seatrooms::parse(g).ok())) {
            tx.execute(
                "INSERT INTO seat_scope(id,table_json,source_id,updated_ms) VALUES(1,?1,?2,?3) \
                 ON CONFLICT(id) DO UPDATE SET table_json=excluded.table_json, source_id=excluded.source_id, updated_ms=excluded.updated_ms",
                params![serde_json::to_string(&t).map_err(|e| StoreError::Internal(e.to_string()))?, id, now_ms],
            )?;
        }
    }
    // The owner's newest valid retention record is the retention table; the rooms it names are backfilled at once.
    if env.channel == crate::retention::ROOM && env.content.starts_with(crate::retention::FACE) {
        let owner: i64 = tx.query_row("SELECT count(*) FROM keys WHERE handle=?1 AND role='owner' AND revoked_ms IS NULL", params![env.author], |x| x.get(0))?;
        if let (true, Some(t)) = (owner > 0, env.cargo.as_deref().and_then(|g| crate::retention::parse(g).ok())) {
            tx.execute(
                "INSERT INTO retention(id,table_json,source_id,updated_ms) VALUES(1,?1,?2,?3) \
                 ON CONFLICT(id) DO UPDATE SET table_json=excluded.table_json, source_id=excluded.source_id, updated_ms=excluded.updated_ms",
                params![serde_json::to_string(&t).map_err(|e| StoreError::Internal(e.to_string()))?, id, now_ms],
            )?;
            for (room, rule) in &t {
                tx.execute("UPDATE observations SET expires_ms = created_ms + ?2 WHERE channel = ?1 AND expires_ms IS NULL", params![room, i64::from(rule.ttl_hours) * 3_600_000])?;
            }
        }
    }
    // The owner's newest valid private-rooms record is the private table; anyone else's, or an invalid one, changes nothing.
    if env.channel == crate::privaterooms::ROOM && env.content.starts_with(crate::privaterooms::FACE) {
        let owner: i64 = tx.query_row("SELECT count(*) FROM keys WHERE handle=?1 AND role='owner' AND revoked_ms IS NULL", params![env.author], |x| x.get(0))?;
        if let (true, Some(t)) = (owner > 0, env.cargo.as_deref().and_then(|g| crate::privaterooms::parse(g).ok())) {
            tx.execute(
                "INSERT INTO private_rooms(id,table_json,source_id,updated_ms) VALUES(1,?1,?2,?3) \
                 ON CONFLICT(id) DO UPDATE SET table_json=excluded.table_json, source_id=excluded.source_id, updated_ms=excluded.updated_ms",
                params![serde_json::to_string(&t).map_err(|e| StoreError::Internal(e.to_string()))?, id, now_ms],
            )?;
        }
    }
    Ok(())
}

thread_local! {
    /// The stage times of the last search on this thread ([`Store::search_in`]), read once by the handler.
    pub static SEARCH_LAPS: std::cell::RefCell<Vec<(&'static str, u64)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The ranking options of the search running on this thread (set by the handler, read by `search_in_timed`).
    pub static SEARCH_TUNING: std::cell::RefCell<Tuning> = const { std::cell::RefCell::new(Tuning::DEFAULT) };
}

/// The server clock in unix ms, for callers outside this crate (the sync loop).
pub fn now_ms_pub() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Puts `cited_by` on the hits from one citation read for the whole response (Emmy, 26 Oct: once per response, not per
/// hit; recall stays whole when the enrichment fails). Returns the response-level status: `measured` with the watermark,
/// or `unavailable` with the reason, in which case no hit carries a count (unavailable is never a zero).
pub fn enrich_cited_by(hits: &mut [SearchHit], cites: Result<(std::collections::BTreeMap<String, serde_json::Value>, i64)>) -> serde_json::Value {
    match cites {
        Ok((mut by_id, through)) => {
            for h in hits.iter_mut() {
                h.cited_by = by_id.remove(&h.obs.id);
            }
            serde_json::json!({"status": "measured", "through_seq": through})
        }
        Err(e) => {
            for h in hits.iter_mut() {
                h.cited_by = None;
            }
            serde_json::json!({"status": "unavailable", "error": e.to_string()})
        }
    }
}

/// The hand-built graph around one record (docs/CORTEX.md): `None` unless it is a Blaze-approved cortex observation. Its
/// node id is where it was said ([`crate::cortex::observation_nid`]); its links are the live `cortex:link` records from and
/// to that id (the newest record per link wins, so a withdrawn link is gone).
fn brain_of(c: &Connection, obs: &Observation) -> Result<Option<serde_json::Value>> {
    if obs.channel != "cortex" || !obs.tags.iter().any(|t| t == "cortex:node") {
        return Ok(None);
    }
    let Some(cargo) = obs.cargo.as_deref().and_then(|g| serde_json::from_str::<serde_json::Value>(g).ok()) else { return Ok(None) };
    if cargo["approved_by"].as_str().is_none() || cargo["kind"] != "observation" {
        return Ok(None);
    }
    let src = &cargo["source"];
    let (Some(m), Some(st), Some(en)) = (src["message"].as_str(), src["start"].as_u64(), src["end"].as_u64()) else { return Ok(None) };
    let nid = crate::cortex::observation_nid(m, st, en);
    let mut q = c.prepare_cached(
        "SELECT o.cargo FROM observations o JOIN tags t ON t.obs_seq=o.seq AND t.name='cortex:link' \
         WHERE o.channel='cortex' AND (CASE WHEN json_valid(o.cargo) THEN json_extract(o.cargo,'$.from') END=?1 OR CASE WHEN json_valid(o.cargo) THEN json_extract(o.cargo,'$.to') END=?1) ORDER BY o.seq",
    )?;
    let mut live: std::collections::BTreeMap<(String, String, String), bool> = std::collections::BTreeMap::new();
    for row in q.query_map(params![nid], |r| r.get::<_, String>(0))? {
        // json_valid guards syntax only. Valid JSON can still contain missing, null or
        // wrongly typed fields; one malformed link must not take down the whole query.
        // Keep the same field and kind rules as the cortex fold, not SQL coercions.
        let Ok(link) = serde_json::from_str::<serde_json::Value>(&row?) else { continue };
        let (Some(from), Some(kind), Some(to)) = (link["from"].as_str(), link["kind"].as_str(), link["to"].as_str()) else { continue };
        if from.is_empty() || to.is_empty() || from == to || !crate::cortex::LINKS.contains(&kind) {
            continue;
        }
        let removed = link["removed"].as_bool().unwrap_or(false);
        live.insert((from.to_owned(), kind.to_owned(), to.to_owned()), !removed);
    }
    let (mut out, mut inn) = (Vec::new(), Vec::new());
    for ((from, kind, to), alive) in live {
        if !alive || kind == "said-by" {
            continue;
        }
        if from == nid {
            out.push(serde_json::json!({"kind": kind, "to": to}));
        } else {
            inn.push(serde_json::json!({"kind": kind, "from": from}));
        }
    }
    Ok(Some(serde_json::json!({
        "nid": nid, "labels": cargo["labels"], "approved_by": cargo["approved_by"], "said_by": if cargo["msg"].is_string() { "claude" } else { "blaze" },
        "note": cargo["note"], "project": cargo["project"], "out": out, "in": inn,
    })))
}
