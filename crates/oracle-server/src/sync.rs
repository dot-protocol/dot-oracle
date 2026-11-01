//! oracle-sync/1: two Oracles (the Mac and the VPS, then any device) keep one signed log (docs/SYNC.md).
//!
//! The log is the truth; every index is rebuilt from it on each node. Sync sends a node the entries it lacks. Each entry
//! keeps its origin id (fixed by its signature) and is verified again on arrival against the author's registered keys, so a
//! copy is as unforgeable as the original. A node never overwrites: an exact duplicate is skipped, divergent bytes under one
//! identity are quarantined. Concurrent State revisions are all kept; the head settles on the newest by (signed time, id), so
//! every node agrees whatever order the entries arrived in. The key registry travels as its own signed events, replayed.
use crate::store::{index_stored, parse_state_cargo, HeadRule, LegacyRow, Result, Store, StoreError};
use oracle_proto::{Envelope, KeyEvent, Kind};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "oracle-sync/1";
/// At most this many entries in one page of the log.
pub const PAGE_MAX: i64 = 1000;

/// One entry of the log as it travels between nodes: every signed field, plus where and when it was first stored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncRow {
    /// The sender's local sequence number: a cursor for paging, never an identity.
    pub seq: i64,
    pub id: String,
    pub channel: String,
    pub author: String,
    pub kind: String,
    pub content: String,
    #[serde(default)]
    pub cargo: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub ts_ms: i64,
    #[serde(default)]
    pub nonce: String,
    #[serde(default)]
    pub client_op: String,
    #[serde(default)]
    pub pubkey: String,
    #[serde(default)]
    pub sig: String,
    #[serde(default)]
    pub authored_ms: Option<i64>,
    #[serde(default)]
    pub prev: Option<String>,
    /// When the origin node stored it (the id's date comes from this).
    pub created_ms: i64,
    /// 2: native and signed (verified again here); 1: imported history (copied as history, never upgraded).
    pub envelope: i64,
    pub news: i64,
    /// The node that first stored it, if not the sender itself.
    #[serde(default)]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Page {
    pub schema: String,
    pub rows: Vec<SyncRow>,
    /// Ask again with `after=` this to continue.
    pub next_after: i64,
    pub more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Report {
    pub added: usize,
    pub duplicate: usize,
    /// (id, why): divergent bytes under an identity that is already stored. Kept aside, never applied.
    pub quarantined: Vec<(String, String)>,
    /// (id, why): could not be verified yet (an unknown key, a malformed row). Kept aside and retried.
    pub refused: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct KeyPage {
    pub schema: String,
    /// (sender's key_events seq, the signed event).
    pub events: Vec<(i64, serde_json::Value)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Cursor {
    pub peer: String,
    /// The peer's log seq we have applied up to.
    pub pulled_after: i64,
    /// Our log seq the peer has acknowledged up to.
    pub pushed_after: i64,
    /// The peer's key_events seq we have replayed up to.
    pub keys_after: i64,
    pub updated_ms: i64,
}

pub(crate) const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS sync_quarantine (
  id         TEXT NOT NULL,
  origin     TEXT NOT NULL,
  why        TEXT NOT NULL,
  row_json   TEXT NOT NULL,
  at_ms      INTEGER NOT NULL,
  PRIMARY KEY (id, origin, why)
);
CREATE TABLE IF NOT EXISTS sync_peers (
  peer         TEXT PRIMARY KEY,
  pulled_after INTEGER NOT NULL DEFAULT 0,
  pushed_after INTEGER NOT NULL DEFAULT 0,
  keys_after   INTEGER NOT NULL DEFAULT 0,
  updated_ms   INTEGER NOT NULL DEFAULT 0
);
";

/// The `origin` column: which node first stored a row (NULL: this node).
pub(crate) fn migrate(c: &rusqlite::Connection) -> Result<()> {
    let has: bool = c.prepare("SELECT 1 FROM pragma_table_info('observations') WHERE name='origin'")?.exists([])?;
    if !has {
        c.execute_batch("ALTER TABLE observations ADD COLUMN origin TEXT")?;
    }
    c.execute_batch(SCHEMA_SQL)?;
    Ok(())
}

impl Store {
    /// A page of the log after `after` (this node's seq), oldest first. `skip_origin` leaves out what came from that node,
    /// so an entry never travels back to where it came from.
    pub fn sync_log(&self, after: i64, limit: i64, skip_origin: Option<&str>) -> Result<Page> {
        let limit = limit.clamp(1, PAGE_MAX);
        let c = self.conn()?;
        let mut st = c.prepare_cached(
            "SELECT seq,id,channel,author,kind,content,cargo,tags_json,ts_ms,nonce,client_op,pubkey,sig,authored_ms,prev,created_ms,envelope,news,origin \
             FROM observations WHERE seq>?1 ORDER BY seq LIMIT ?2",
        )?;
        let mut rows = Vec::new();
        let mut last = after;
        let mut seen = 0;
        let mut q = st.query(params![after, limit])?;
        while let Some(r) = q.next()? {
            seen += 1;
            last = r.get(0)?;
            let origin: Option<String> = r.get(18)?;
            if skip_origin.is_some_and(|s| origin.as_deref() == Some(s)) {
                continue;
            }
            let tags: String = r.get(7)?;
            rows.push(SyncRow {
                seq: last,
                id: r.get(1)?,
                channel: r.get(2)?,
                author: r.get(3)?,
                kind: r.get(4)?,
                content: r.get(5)?,
                cargo: r.get(6)?,
                tags: serde_json::from_str(&tags).unwrap_or_default(),
                ts_ms: r.get(8)?,
                nonce: r.get(9)?,
                client_op: r.get(10)?,
                pubkey: r.get(11)?,
                sig: r.get(12)?,
                authored_ms: r.get(13)?,
                prev: r.get(14)?,
                created_ms: r.get(15)?,
                envelope: r.get(16)?,
                news: r.get(17)?,
                origin,
            });
        }
        Ok(Page { schema: SCHEMA.into(), rows, next_after: last, more: seen as i64 == limit })
    }

    /// The signed key registry events after `after`, oldest first.
    pub fn sync_keys(&self, after: i64) -> Result<KeyPage> {
        let c = self.conn()?;
        let mut st = c.prepare_cached("SELECT seq, event_json FROM key_events WHERE seq>?1 ORDER BY seq")?;
        let events = st
            .query_map(params![after], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|x| x.ok())
            .filter_map(|(s, j)| serde_json::from_str(&j).ok().map(|v| (s, v)))
            .collect();
        Ok(KeyPage { schema: SCHEMA.into(), events })
    }

    /// Replay registry events from a peer, each at its own signed time, in the peer's order. One already applied (its
    /// nonce is here) is skipped. Returns how many were applied.
    pub fn replay_keys(&self, page: &KeyPage) -> Result<usize> {
        let mut applied = 0;
        for (_, v) in &page.events {
            let ev: KeyEvent = serde_json::from_value(v.clone()).map_err(|e| StoreError::Rejected(format!("key event: {e}")))?;
            let known: bool = self.conn()?.prepare_cached("SELECT 1 FROM key_events WHERE nonce=?1")?.exists(params![ev.nonce])?;
            if known {
                continue;
            }
            self.apply_key_event(&ev, ev.ts_ms)?;
            applied += 1;
        }
        Ok(applied)
    }

    /// Store entries that arrived from `origin`. Native entries are verified against the author's keys (any key the seat
    /// held when it signed), stored under their origin id and date, and indexed like a live write. Imported history is
    /// copied as history. Nothing already stored is ever changed.
    pub fn replicate(&self, rows: &[SyncRow], origin: &str, now_ms: i64) -> Result<Report> {
        let mut rep = Report::default();
        for row in rows {
            match self.replicate_one(row, origin, now_ms) {
                Ok(Applied::Added) => rep.added += 1,
                Ok(Applied::Duplicate) => rep.duplicate += 1,
                Ok(Applied::Quarantined(why)) => {
                    self.set_aside(row, origin, &why, now_ms)?;
                    rep.quarantined.push((row.id.clone(), why));
                }
                Err(StoreError::Internal(e)) => return Err(StoreError::Internal(e)),
                Err(e) => {
                    let why = e.to_string();
                    self.set_aside(row, origin, &format!("refused: {why}"), now_ms)?;
                    rep.refused.push((row.id.clone(), why));
                }
            }
        }
        Ok(rep)
    }

    fn replicate_one(&self, row: &SyncRow, origin: &str, now_ms: i64) -> Result<Applied> {
        if row.envelope != 2 {
            let legacy = LegacyRow {
                id: row.id.clone(),
                channel: row.channel.clone(),
                author: row.author.clone(),
                kind: row.kind.clone(),
                content: row.content.clone(),
                cargo: row.cargo.clone(),
                tags: row.tags.clone(),
                created_ms: row.created_ms,
                pubkey: row.pubkey.clone(),
                sig: row.sig.clone(),
                news: row.news != 0,
                source: None,
                repair_legacy: false,
            };
            let before = self.get(&row.id).is_ok();
            return match self.import_legacy(std::slice::from_ref(&legacy)) {
                Ok(_) if before => Ok(Applied::Duplicate),
                Ok(_) => Ok(Applied::Added),
                Err(StoreError::Conflict(why)) => Ok(Applied::Quarantined(format!("history differs: {why}"))),
                Err(e) => Err(e),
            };
        }
        let kind: Kind = serde_json::from_value(serde_json::Value::String(row.kind.clone()))
            .map_err(|_| StoreError::Rejected(format!("unknown kind {}", row.kind)))?;
        let env = Envelope {
            channel: row.channel.clone(),
            author: row.author.clone(),
            kind,
            ts_ms: row.ts_ms,
            nonce: row.nonce.clone(),
            client_op: row.client_op.clone(),
            content: row.content.clone(),
            cargo: row.cargo.clone(),
            tags: row.tags.clone(),
            authored_ms: row.authored_ms,
            prev: row.prev.clone(),
            sig: row.sig.clone(),
        };
        // The id is fixed by the signature: a row whose id does not match its own signature is not that entry.
        let tail = &oracle_proto::sha_hex(env.sig.as_bytes())[..12];
        if !row.id.ends_with(tail) || !row.id.starts_with(&format!("OBS-{}-", env.channel)) {
            return Ok(Applied::Quarantined("id does not match its signature and room".into()));
        }
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(sig) = tx.query_row("SELECT sig FROM observations WHERE id=?1", params![row.id], |r| r.get::<_, String>(0)).optional()? {
            return Ok(if sig == row.sig { Applied::Duplicate } else { Applied::Quarantined("id already stored with other bytes".into()) });
        }
        if let Some(other) = tx.query_row("SELECT id FROM observations WHERE client_op=?1", params![row.client_op], |r| r.get::<_, String>(0)).optional()? {
            return Ok(Applied::Quarantined(format!("client_op already stored as {other}: one operation, two entries")));
        }
        // Any key the seat held when it signed: added at or before the signing time (with the protocol's skew) and not
        // revoked before it.
        let keys: Vec<String> = tx
            .prepare_cached("SELECT pubkey FROM keys WHERE handle=?1 AND created_ms<=?2 AND (revoked_ms IS NULL OR revoked_ms>?3)")?
            .query_map(params![env.author, env.ts_ms + oracle_proto::MAX_SKEW_MS, env.ts_ms], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let pubkey = keys
            .iter()
            .find(|pk| env.verify_stored(pk).is_ok())
            .cloned()
            .ok_or(StoreError::Unauthorized("no key the author held when it signed verifies this entry"))?;
        let state = if env.kind == Kind::State { Some(parse_state_cargo(&env)?) } else { None };
        tx.execute(
            "INSERT INTO observations(id,channel,author,kind,content,cargo,tags_json,created_ms,ts_ms,nonce,client_op,pubkey,sig,authored_ms,prev,expires_ms,news,origin) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                row.id, env.channel, env.author, env.kind.as_str(), env.content, env.cargo,
                serde_json::to_string(&env.tags).map_err(|e| StoreError::Internal(e.to_string()))?,
                row.created_ms, env.ts_ms, env.nonce, env.client_op, pubkey, env.sig, env.authored_ms, env.prev,
                crate::store::expires_of(&env.tags), row.news, row.origin.as_deref().unwrap_or(origin)
            ],
        )?;
        let seq = tx.last_insert_rowid();
        index_stored(&tx, &env, &row.id, seq, now_ms, state, HeadRule::Newest)?;
        tx.commit()?;
        Ok(Applied::Added)
    }

    fn set_aside(&self, row: &SyncRow, origin: &str, why: &str, now_ms: i64) -> Result<()> {
        self.conn()?.execute(
            "INSERT OR IGNORE INTO sync_quarantine(id,origin,why,row_json,at_ms) VALUES(?1,?2,?3,?4,?5)",
            params![row.id, origin, why, serde_json::to_string(row).map_err(|e| StoreError::Internal(e.to_string()))?, now_ms],
        )?;
        Ok(())
    }

    /// What was set aside, newest first.
    pub fn sync_quarantine(&self, limit: i64) -> Result<Vec<(String, String, String, i64)>> {
        let c = self.conn()?;
        let mut st = c.prepare_cached("SELECT id, origin, why, at_ms FROM sync_quarantine ORDER BY at_ms DESC LIMIT ?1")?;
        let rows = st.query_map(params![limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    pub fn sync_cursor(&self, peer: &str) -> Result<Cursor> {
        let c = self.conn()?;
        Ok(c
            .query_row(
                "SELECT peer,pulled_after,pushed_after,keys_after,updated_ms FROM sync_peers WHERE peer=?1",
                params![peer],
                |r| Ok(Cursor { peer: r.get(0)?, pulled_after: r.get(1)?, pushed_after: r.get(2)?, keys_after: r.get(3)?, updated_ms: r.get(4)? }),
            )
            .optional()?
            .unwrap_or(Cursor { peer: peer.into(), ..Default::default() }))
    }

    pub fn set_sync_cursor(&self, cur: &Cursor) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO sync_peers(peer,pulled_after,pushed_after,keys_after,updated_ms) VALUES(?1,?2,?3,?4,?5) \
             ON CONFLICT(peer) DO UPDATE SET pulled_after=excluded.pulled_after, pushed_after=excluded.pushed_after, \
             keys_after=excluded.keys_after, updated_ms=excluded.updated_ms",
            params![cur.peer, cur.pulled_after, cur.pushed_after, cur.keys_after, cur.updated_ms],
        )?;
        Ok(())
    }
}

enum Applied {
    Added,
    Duplicate,
    Quarantined(String),
}

/// The other node, over HTTP: what a sync round needs from it.
pub trait Peer {
    fn keys(&self, after: i64) -> Result<KeyPage>;
    fn log(&self, after: i64, limit: i64, skip_origin: &str) -> Result<Page>;
    fn push(&self, origin: &str, rows: &[SyncRow]) -> Result<Report>;
}

/// One round between this node (`me`) and a peer (`them`): replay their key events, pull what they have that we lack,
/// push what we have that they lack. Each step moves its cursor only after it is applied, so an interrupted round resumes.
pub fn round(store: &Store, me: &str, them: &str, peer: &dyn Peer, now_ms: i64) -> Result<RoundReport> {
    let mut cur = store.sync_cursor(them)?;
    let mut out = RoundReport::default();
    let keys = peer.keys(cur.keys_after)?;
    out.keys = store.replay_keys(&keys)?;
    if let Some((last, _)) = keys.events.last() {
        cur.keys_after = *last;
    }
    loop {
        let page = peer.log(cur.pulled_after, PAGE_MAX, me)?;
        let r = store.replicate(&page.rows, them, now_ms)?;
        out.pulled.added += r.added;
        out.pulled.duplicate += r.duplicate;
        out.pulled.quarantined.extend(r.quarantined);
        out.pulled.refused.extend(r.refused);
        cur.pulled_after = page.next_after;
        cur.updated_ms = now_ms;
        store.set_sync_cursor(&cur)?;
        if !page.more {
            break;
        }
    }
    loop {
        let page = store.sync_log(cur.pushed_after, PAGE_MAX, Some(them))?;
        if !page.rows.is_empty() {
            let r = peer.push(me, &page.rows)?;
            out.pushed.added += r.added;
            out.pushed.duplicate += r.duplicate;
            out.pushed.quarantined.extend(r.quarantined);
            out.pushed.refused.extend(r.refused);
        }
        cur.pushed_after = page.next_after;
        cur.updated_ms = now_ms;
        store.set_sync_cursor(&cur)?;
        if !page.more {
            break;
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RoundReport {
    pub keys: usize,
    pub pulled: Report,
    pub pushed: Report,
}

/// A peer reached over HTTP with a sync token (an admin-scope token of the peer).
pub struct HttpPeer {
    pub base: String,
    pub token: String,
    pub agent: ureq::Agent,
}

impl HttpPeer {
    pub fn new(base: &str, token: &str) -> Self {
        HttpPeer {
            base: base.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
            agent: ureq::AgentBuilder::new().timeout_connect(std::time::Duration::from_secs(10)).timeout(std::time::Duration::from_secs(120)).build(),
        }
    }
    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.agent
            .get(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
            .call()
            .map_err(|e| StoreError::Internal(format!("peer GET {path}: {e}")))?
            .into_json()
            .map_err(|e| StoreError::Internal(format!("peer GET {path}: {e}")))
    }
}

impl Peer for HttpPeer {
    fn keys(&self, after: i64) -> Result<KeyPage> {
        self.get(&format!("/sync/keys?after={after}"))
    }
    fn log(&self, after: i64, limit: i64, skip_origin: &str) -> Result<Page> {
        self.get(&format!("/sync/log?after={after}&limit={limit}&skip_origin={}", urlencoding(skip_origin)))
    }
    fn push(&self, origin: &str, rows: &[SyncRow]) -> Result<Report> {
        self.agent
            .post(&format!("{}/sync/push", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
            .send_json(serde_json::json!({"origin": origin, "rows": rows}))
            .map_err(|e| StoreError::Internal(format!("peer POST /sync/push: {e}")))?
            .into_json()
            .map_err(|e| StoreError::Internal(format!("peer POST /sync/push: {e}")))
    }
}

fn urlencoding(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}
