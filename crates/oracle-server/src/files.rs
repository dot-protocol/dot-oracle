//! Files: bytes whose sha256 is their id (Noor, Round 68, from Blaze: "the file system, everything should happen
//! through Oracle"). The first layer of dot-fs living inside the Oracle.
//!
//! A seat puts bytes into a room; the id is `sha256:<64 hex>` of exactly those bytes, so the same bytes put twice,
//! into one room or several, are stored once. A file is read by id under the same grant as a room read. A message
//! cites a file with the tag `file:<64 hex>`; the file's metadata lists the messages that cite it, which is where
//! the signature lives (the put itself is authenticated, not signed). Text and binary alike; at most [`MAX_BYTES`].
//! Never deleted by this module: only a human deletes.
use crate::store::{Result, Store, StoreError};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// 16 MiB per file: a PNG face, a page, a dataset slice; not a video.
pub const MAX_BYTES: usize = 16 * 1024 * 1024;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS files (
  sha256       TEXT PRIMARY KEY,
  size         INTEGER NOT NULL,
  content_type TEXT NOT NULL,
  bytes        BLOB NOT NULL,
  created_ms   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS file_rooms (
  sha256     TEXT NOT NULL,
  room       TEXT NOT NULL,
  author     TEXT NOT NULL,
  name       TEXT,
  created_ms INTEGER NOT NULL,
  PRIMARY KEY (sha256, room)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS file_rooms_room ON file_rooms(room, created_ms);
"#;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Placement {
    pub room: String,
    pub author: String,
    pub name: Option<String>,
    pub created_ms: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileMeta {
    /// `sha256:<hex>`, the id a message cites.
    pub id: String,
    pub sha256: String,
    pub size: i64,
    pub content_type: String,
    pub created_ms: i64,
    /// Every room it was put into, by whom, under what name.
    pub rooms: Vec<Placement>,
    /// Messages tagged `file:<hex>`: where the file is signed for.
    pub cited_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Put {
    pub ok: bool,
    pub id: String,
    pub sha256: String,
    pub size: usize,
    pub content_type: String,
    pub room: String,
    /// The bytes were already stored (by anyone, in any room).
    pub duplicate: bool,
}

/// `sha256:<hex>` or bare hex → the 64 lowercase hex digits, or a rejection.
pub fn parse_id(id: &str) -> Result<String> {
    let h = id.strip_prefix("sha256:").unwrap_or(id).to_ascii_lowercase();
    if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(h)
    } else {
        Err(StoreError::Rejected("a file id is sha256:<64 hex>".into()))
    }
}

fn content_type_ok(t: &str) -> bool {
    !t.is_empty() && t.len() <= 127 && t.bytes().all(|b| b.is_ascii_graphic() || b == b' ') && t.contains('/')
}

fn name_ok(n: &str) -> bool {
    !n.is_empty() && n.len() <= 255 && !n.contains('/') && !n.contains('\\') && !n.chars().any(char::is_control) && n != "." && n != ".."
}

/// Put `bytes` into `room` as `author`. Idempotent: the same bytes into the same room answer the same id.
pub fn put(s: &Store, author: &str, room: &str, name: Option<&str>, content_type: &str, bytes: &[u8], now_ms: i64) -> Result<Put> {
    if !crate::cards::valid_channel(room) {
        return Err(StoreError::Rejected("room must be a room name (a-z, 0-9, -)".into()));
    }
    if bytes.is_empty() {
        return Err(StoreError::Rejected("a file has at least one byte".into()));
    }
    if bytes.len() > MAX_BYTES {
        return Err(StoreError::Rejected(format!("a file is at most {MAX_BYTES} bytes; this one is {}", bytes.len())));
    }
    let ctype = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let ctype = if ctype.is_empty() { "application/octet-stream".to_string() } else { ctype };
    if !content_type_ok(&ctype) {
        return Err(StoreError::Rejected("content type must be type/subtype".into()));
    }
    if let Some(n) = name {
        if !name_ok(n) {
            return Err(StoreError::Rejected("a file name is one path segment, at most 255 bytes".into()));
        }
    }
    let sha = hex::encode(Sha256::digest(bytes));
    let out_room = room.to_owned();
    let (room, author, name, ctype2, sha2) = (room.to_owned(), author.to_owned(), name.map(str::to_owned), ctype.clone(), sha.clone());
    let bytes = bytes.to_vec();
    let size = bytes.len();
    let duplicate = s.with_conn(move |c| {
        let tx = c.unchecked_transaction()?;
        let existed: bool = tx.query_row("SELECT 1 FROM files WHERE sha256 = ?1", [&sha2], |_| Ok(())).optional()?.is_some();
        if !existed {
            tx.execute(
                "INSERT INTO files (sha256, size, content_type, bytes, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![sha2, size as i64, ctype2, bytes, now_ms],
            )?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO file_rooms (sha256, room, author, name, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![sha2, room, author, name, now_ms],
        )?;
        tx.commit()?;
        Ok(existed)
    })?;
    Ok(Put { ok: true, id: format!("sha256:{sha}"), sha256: sha, size, content_type: ctype, room: out_room, duplicate })
}

/// The bytes and their content type, or NotFound. Callers gate this like a room read.
pub fn bytes(s: &Store, id: &str) -> Result<(Vec<u8>, String)> {
    let h = parse_id(id)?;
    s.with_conn(move |c| {
        c.query_row("SELECT bytes, content_type FROM files WHERE sha256 = ?1", [&h], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?)))
            .optional()?
            .ok_or(StoreError::NotFound)
    })
}

/// What is known about one file: size, type, the rooms it is in, the messages that cite it.
pub fn meta(s: &Store, id: &str) -> Result<FileMeta> {
    let h = parse_id(id)?;
    s.with_conn(move |c| {
        let (size, content_type, created_ms) = c
            .query_row("SELECT size, content_type, created_ms FROM files WHERE sha256 = ?1", [&h], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .optional()?
            .ok_or(StoreError::NotFound)?;
        let mut st = c.prepare("SELECT room, author, name, created_ms FROM file_rooms WHERE sha256 = ?1 ORDER BY created_ms, room")?;
        let rooms = st
            .query_map([&h], |r| Ok(Placement { room: r.get(0)?, author: r.get(1)?, name: r.get(2)?, created_ms: r.get(3)? }))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut st = c.prepare(
            "SELECT o.id FROM tags t JOIN observations o ON o.seq = t.obs_seq WHERE t.name = ?1 ORDER BY o.seq DESC LIMIT 200",
        )?;
        let cited_by = st.query_map([format!("file:{h}")], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(FileMeta { id: format!("sha256:{h}"), sha256: h, size, content_type, created_ms, rooms, cited_by })
    })
}

/// A room's files, newest first.
pub fn list(s: &Store, room: &str, limit: i64) -> Result<Vec<FileMeta>> {
    if !crate::cards::valid_channel(room) {
        return Err(StoreError::Rejected("room must be a room name (a-z, 0-9, -)".into()));
    }
    let (room, limit) = (room.to_owned(), limit.clamp(1, 500));
    let ids: Vec<String> = s.with_conn(move |c| {
        let mut st = c.prepare("SELECT sha256 FROM file_rooms WHERE room = ?1 ORDER BY created_ms DESC, sha256 LIMIT ?2")?;
        let ids = st.query_map(params![room, limit], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    })?;
    ids.iter().map(|h| meta(s, h)).collect()
}
