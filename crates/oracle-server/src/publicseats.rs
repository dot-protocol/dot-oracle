//! Public seats and limits (plan.oracle.public-oracle S1 + S2; Blaze, 29 Oct 2026: anyone can use Oracle to do in public
//! rooms what we do in our private ones, at scale).
//!
//! A public seat is an identity anyone mints for themselves with a key they hold: they ask for a challenge, sign it, and
//! get a seat and a token. It is a lower tier than a registry seat: it lives in its own table, the owner-signed registry
//! (the root of trust) is never touched, and the store confines it, whatever route it writes through, to the rooms in
//! ORACLE_PUBLIC_WRITE_ROOMS. That list is empty by default, and then registration answers 404: this ships dark, and
//! which rooms take strangers' writes is the owner's decision, never a default. A public seat never reaches a private
//! room or a private scope (Emmy, 29 Oct).
//!
//! Limits are counted in the store (table rate_events), so they hold across restarts and across processes: writes per
//! public seat (spacing, per day, bytes per day), public-seat writes per room per hour, registrations per address per
//! hour and in all per day. Registry seats are not limited here.
use rusqlite::{params, Connection, OptionalExtension};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS public_seats (
  handle     TEXT PRIMARY KEY,
  pubkey     TEXT NOT NULL UNIQUE,
  name       TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  revoked_ms INTEGER
);
CREATE TABLE IF NOT EXISTS register_challenges (
  nonce      TEXT PRIMARY KEY,
  created_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS rate_events (
  bucket TEXT NOT NULL,
  at_ms  INTEGER NOT NULL,
  weight INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS rate_events_bucket ON rate_events(bucket, at_ms);
";

pub const PREFIX: &str = "p-";
pub const CHALLENGE_TTL_MS: i64 = 10 * 60 * 1000;
const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// The limits in force; each can be lowered or raised by its environment variable (a positive integer).
#[derive(Clone, Debug, PartialEq)]
pub struct Limits {
    pub seat_spacing_ms: i64,
    pub seat_per_day: i64,
    pub seat_bytes_per_day: i64,
    pub room_per_hour: i64,
    pub register_per_ip_hour: i64,
    pub register_per_day: i64,
    pub tokens_per_seat: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            seat_spacing_ms: 20_000,
            seat_per_day: 30,
            seat_bytes_per_day: 200_000,
            room_per_hour: 500,
            register_per_ip_hour: 5,
            register_per_day: 200,
            tokens_per_seat: 3,
        }
    }
}

impl Limits {
    pub fn from_env() -> Self {
        let d = Limits::default();
        let n = |k: &str, v: i64| std::env::var(k).ok().and_then(|s| s.parse::<i64>().ok()).filter(|x| *x > 0).unwrap_or(v);
        Limits {
            seat_spacing_ms: n("ORACLE_PUBLIC_SPACING_MS", d.seat_spacing_ms),
            seat_per_day: n("ORACLE_PUBLIC_WRITES_PER_DAY", d.seat_per_day),
            seat_bytes_per_day: n("ORACLE_PUBLIC_BYTES_PER_DAY", d.seat_bytes_per_day),
            room_per_hour: n("ORACLE_PUBLIC_ROOM_PER_HOUR", d.room_per_hour),
            register_per_ip_hour: n("ORACLE_PUBLIC_REGISTER_PER_IP_HOUR", d.register_per_ip_hour),
            register_per_day: n("ORACLE_PUBLIC_REGISTER_PER_DAY", d.register_per_day),
            tokens_per_seat: n("ORACLE_PUBLIC_TOKENS_PER_SEAT", d.tokens_per_seat),
        }
    }
}

/// ORACLE_PUBLIC_WRITE_ROOMS="eth,riemann": the rooms a public seat may write in. Empty means none, and registration is off.
pub fn write_rooms_from_env() -> Vec<String> {
    parse_rooms(&std::env::var("ORACLE_PUBLIC_WRITE_ROOMS").unwrap_or_default())
}

pub fn parse_rooms(spec: &str) -> Vec<String> {
    let mut v: Vec<String> = spec
        .split(',')
        .map(|s| s.trim().trim_start_matches('#').to_lowercase())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
        .collect();
    v.sort();
    v.dedup();
    v
}

/// A public seat's handle is derived from its key, so registering the same key twice is the same seat.
pub fn handle_for(pubkey_hex: &str) -> String {
    format!("{PREFIX}{}", &oracle_proto::sha_hex(pubkey_hex.to_lowercase().as_bytes())[..16])
}

pub fn is_public_handle(handle: &str) -> bool {
    handle.len() == PREFIX.len() + 16 && handle.starts_with(PREFIX) && handle[PREFIX.len()..].chars().all(|c| c.is_ascii_hexdigit())
}

/// The bytes a registrant signs: the challenge, bound to its key and to this protocol.
pub fn challenge_message(nonce: &str, pubkey_hex: &str) -> Vec<u8> {
    oracle_proto::register_challenge_message(nonce, pubkey_hex)
}

/// A display name: 1 to 60 printable characters, spaces collapsed; no control characters, no '@' (it is not an address).
pub fn clean_name(name: &str) -> Result<String, String> {
    let n = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if n.is_empty() || n.chars().count() > 60 {
        return Err("name must be 1 to 60 characters".into());
    }
    if n.chars().any(|c| c.is_control() || c == '@' || c == '<' || c == '>') {
        return Err("name has a character that is not allowed".into());
    }
    Ok(n)
}

pub fn new_challenge(c: &Connection, now_ms: i64) -> rusqlite::Result<String> {
    c.execute("DELETE FROM register_challenges WHERE created_ms < ?1", params![now_ms - CHALLENGE_TTL_MS])?;
    let nonce = oracle_proto::random_hex(24);
    c.execute("INSERT INTO register_challenges(nonce, created_ms) VALUES(?1, ?2)", params![nonce, now_ms])?;
    Ok(nonce)
}

/// Spend a challenge: true once, when it exists and has not expired. A spent or stale nonce never works again.
pub fn take_challenge(c: &Connection, nonce: &str, now_ms: i64) -> rusqlite::Result<bool> {
    let created: Option<i64> = c
        .query_row("SELECT created_ms FROM register_challenges WHERE nonce=?1", params![nonce], |r| r.get(0))
        .optional()?;
    c.execute("DELETE FROM register_challenges WHERE nonce=?1", params![nonce])?;
    Ok(matches!(created, Some(t) if now_ms - t <= CHALLENGE_TTL_MS && t <= now_ms))
}

pub fn active_pubkey(c: &Connection, handle: &str) -> rusqlite::Result<Option<String>> {
    if !is_public_handle(handle) {
        return Ok(None);
    }
    match c.query_row("SELECT pubkey FROM public_seats WHERE handle=?1 AND revoked_ms IS NULL", params![handle], |r| r.get(0)).optional() {
        // The online-admin opener does not migrate: a store without the table has no public seats.
        Err(rusqlite::Error::SqliteFailure(_, Some(m))) if m.contains("no such table") => Ok(None),
        other => other,
    }
}

/// One bucket of a limit: at most `max` (summed weights) in the window before now.
pub struct Bucket {
    pub key: String,
    pub window_ms: i64,
    pub max: i64,
    pub weight: i64,
    pub what: &'static str,
}

/// Admit an event against every bucket or none: Err(why, retry_after_ms) names the first full bucket and when it frees.
/// On success the event is recorded in every bucket. Run inside the caller's transaction so the check and the record
/// are one step.
pub fn admit(c: &Connection, buckets: &[Bucket], now_ms: i64) -> rusqlite::Result<Result<(), (String, i64)>> {
    for b in buckets {
        let (used, oldest): (i64, Option<i64>) = c.query_row(
            "SELECT COALESCE(SUM(weight),0), MIN(at_ms) FROM rate_events WHERE bucket=?1 AND at_ms > ?2",
            params![b.key, now_ms - b.window_ms],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if used + b.weight > b.max {
            let retry = oldest.map(|t| t + b.window_ms - now_ms).unwrap_or(b.window_ms).max(1);
            return Ok(Err((format!("{} limit reached ({} per {})", b.what, b.max, window_words(b.window_ms)), retry)));
        }
    }
    for b in buckets {
        c.execute("INSERT INTO rate_events(bucket, at_ms, weight) VALUES(?1, ?2, ?3)", params![b.key, now_ms, b.weight])?;
    }
    // Old events are forgotten as we go: nothing here is kept longer than the longest window.
    c.execute("DELETE FROM rate_events WHERE at_ms < ?1", params![now_ms - 2 * DAY])?;
    Ok(Ok(()))
}

fn window_words(ms: i64) -> String {
    match ms {
        m if m >= DAY && m % DAY == 0 => format!("{} day{}", m / DAY, if m / DAY == 1 { "" } else { "s" }),
        m if m >= HOUR && m % HOUR == 0 => format!("{} hour{}", m / HOUR, if m / HOUR == 1 { "" } else { "s" }),
        m => format!("{} seconds", (m + 999) / 1000),
    }
}

/// The buckets a public seat's write passes: spacing, per day, bytes per day, and the room's hourly share.
pub fn write_buckets(l: &Limits, handle: &str, room: &str, bytes: i64) -> Vec<Bucket> {
    vec![
        Bucket { key: format!("seat-spacing:{handle}"), window_ms: l.seat_spacing_ms, max: 1, weight: 1, what: "one write at a time" },
        Bucket { key: format!("seat-day:{handle}"), window_ms: DAY, max: l.seat_per_day, weight: 1, what: "writes per seat" },
        Bucket { key: format!("seat-bytes:{handle}"), window_ms: DAY, max: l.seat_bytes_per_day, weight: bytes.max(1), what: "bytes per seat" },
        Bucket { key: format!("room-hour:{room}"), window_ms: HOUR, max: l.room_per_hour, weight: 1, what: "public writes in this room" },
    ]
}

pub fn register_buckets(l: &Limits, ip: &str) -> Vec<Bucket> {
    vec![
        Bucket { key: format!("register-ip:{ip}"), window_ms: HOUR, max: l.register_per_ip_hour, weight: 1, what: "registrations from this address" },
        Bucket { key: "register-day".into(), window_ms: DAY, max: l.register_per_day, weight: 1, what: "registrations in all" },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c
    }

    #[test]
    fn handles_come_from_keys_and_are_recognised() {
        let h = handle_for("AB".repeat(32).as_str());
        assert!(is_public_handle(&h));
        assert_eq!(h, handle_for(&"ab".repeat(32)), "case of the hex does not make a second seat");
        assert!(!is_public_handle("p-short"));
        assert!(!is_public_handle("rocky"));
        assert!(!is_public_handle("p-zzzzzzzzzzzzzzzz"));
    }

    #[test]
    fn a_challenge_is_spent_once_and_expires() {
        let c = db();
        let n = new_challenge(&c, 1_000).unwrap();
        assert!(take_challenge(&c, &n, 2_000).unwrap());
        assert!(!take_challenge(&c, &n, 2_001).unwrap(), "a spent nonce never works again");
        let m = new_challenge(&c, 1_000).unwrap();
        assert!(!take_challenge(&c, &m, 1_000 + CHALLENGE_TTL_MS + 1).unwrap(), "a stale nonce is refused");
        assert!(!take_challenge(&c, "made-up", 1_000).unwrap());
    }

    #[test]
    fn limits_admit_up_to_the_cap_then_say_when() {
        let c = db();
        let l = Limits { seat_per_day: 2, seat_spacing_ms: 1_000, ..Limits::default() };
        let h = "p-0123456789abcdef";
        assert!(admit(&c, &write_buckets(&l, h, "eth", 10), 0).unwrap().is_ok());
        let too_soon = admit(&c, &write_buckets(&l, h, "eth", 10), 500).unwrap();
        assert!(matches!(&too_soon, Err((why, retry)) if why.contains("one write at a time") && *retry == 500));
        assert!(admit(&c, &write_buckets(&l, h, "eth", 10), 1_500).unwrap().is_ok());
        let day = admit(&c, &write_buckets(&l, h, "eth", 10), 3_000).unwrap();
        assert!(matches!(&day, Err((why, _)) if why.contains("writes per seat")), "{day:?}");
        // A refused write records nothing: another seat in the same room is unaffected.
        assert!(admit(&c, &write_buckets(&l, "p-fedcba9876543210", "eth", 10), 3_000).unwrap().is_ok());
    }

    #[test]
    fn bytes_are_weighed_and_rooms_have_their_own_cap() {
        let c = db();
        let l = Limits { seat_bytes_per_day: 100, seat_spacing_ms: 1, room_per_hour: 2, ..Limits::default() };
        assert!(admit(&c, &write_buckets(&l, "p-0000000000000001", "eth", 90), 0).unwrap().is_ok());
        assert!(matches!(admit(&c, &write_buckets(&l, "p-0000000000000001", "eth", 20), 10).unwrap(), Err((w, _)) if w.contains("bytes")));
        assert!(admit(&c, &write_buckets(&l, "p-0000000000000002", "eth", 5), 20).unwrap().is_ok());
        assert!(matches!(admit(&c, &write_buckets(&l, "p-0000000000000003", "eth", 5), 30).unwrap(), Err((w, _)) if w.contains("room")));
        assert!(admit(&c, &write_buckets(&l, "p-0000000000000003", "riemann", 5), 30).unwrap().is_ok());
    }

    #[test]
    fn rooms_and_names_are_cleaned() {
        assert_eq!(parse_rooms(" #ETH, riemann ,,bad room,eth"), vec!["eth".to_string(), "riemann".to_string()]);
        assert!(parse_rooms("").is_empty());
        assert_eq!(clean_name("  Ada   Lovelace ").unwrap(), "Ada Lovelace");
        assert!(clean_name("").is_err());
        assert!(clean_name("a@b.c").is_err());
        assert!(clean_name(&"x".repeat(61)).is_err());
    }
}
