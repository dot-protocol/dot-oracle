//! Offline verification (docs/VERIFY.md; Blaze, 4 Nov 2026, task oracle.offline-verify, before v0.1.0): a record checks
//! with no server at all. The reader trusts one thing, the owner's public key, published where the reader can fetch it
//! (did:web style: a fixed path on the owner's own site). From it, the key log (`GET /keys?history=true`, every event
//! signed) is replayed with the server's rules, which says which key each seat held at each moment; then each record's
//! signature is checked, its id against its signature, and its key against what its author held when it was signed.

use crate::{link_of, verify_sig, ChainBreak, ChainRecord, KeyAction, KeyEvent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The file a reader needs besides the records: `oracle-keys/1`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeysFile {
    pub schema: String,
    /// The owner's handle and the key the whole log hangs from.
    pub owner: String,
    pub owner_pubkey: String,
    /// Where the Oracle answers, as the exporter saw it (informational; never fetched by the check).
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub exported_ms: i64,
    pub events: Vec<KeyEvent>,
}

pub const KEYS_SCHEMA: &str = "oracle-keys/1";

/// One key's life in a seat: from the event that added it until the one that ended it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeySpan {
    pub pubkey: String,
    pub role: String,
    pub from_ms: i64,
    pub until_ms: Option<i64>,
}

/// Every seat's keys over time, as the log says.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyBook {
    pub seats: BTreeMap<String, Vec<KeySpan>>,
}

impl KeyBook {
    fn active(&self, handle: &str, at: i64) -> Vec<&KeySpan> {
        self.seats.get(handle).map(|v| v.iter().filter(|s| s.from_ms <= at && s.until_ms.is_none_or(|u| at < u)).collect()).unwrap_or_default()
    }

    /// Whether `pubkey` was one of `handle`'s keys at `at_ms`.
    pub fn held(&self, handle: &str, pubkey: &str, at_ms: i64) -> bool {
        self.active(handle, at_ms).iter().any(|s| s.pubkey == pubkey)
    }

    fn known(&self, handle: &str, pubkey: &str) -> Option<&KeySpan> {
        self.seats.get(handle)?.iter().find(|s| s.pubkey == pubkey)
    }

    fn end(&mut self, handle: &str, at: i64, which: impl Fn(&KeySpan) -> bool) {
        for s in self.seats.entry(handle.to_string()).or_default().iter_mut() {
            if s.until_ms.is_none() && which(s) {
                s.until_ms = Some(at);
            }
        }
    }

    fn put(&mut self, handle: &str, pubkey: &str, role: &str, at: i64) {
        self.seats.entry(handle.to_string()).or_default().push(KeySpan { pubkey: pubkey.into(), role: role.into(), from_ms: at, until_ms: None });
    }
}

/// Replay the key log from the owner key, with the server's rules (store.rs apply_key_event): the owner adds seats and
/// rotates or revokes any seat's keys; a seat may add a key for itself, rotate the key that signed, or revoke its own;
/// the owner key is rotated, never revoked. One event no valid key signed, or that the rules refuse, fails the whole log:
/// a log that is not whole proves nothing.
pub fn replay(owner: &str, owner_pubkey: &str, events: &[KeyEvent]) -> Result<KeyBook, String> {
    crate::parse_pubkey(owner_pubkey).map_err(|_| "the owner key is not a key".to_string())?;
    let mut book = KeyBook::default();
    book.put(owner, owner_pubkey, "owner", i64::MIN);
    for (n, ev) in events.iter().enumerate() {
        let at = ev.ts_ms;
        let fail = |why: &str| Err(format!("key event {} ({} {} by {}): {why}", n + 1, ev.action.as_str(), ev.handle, ev.signer));
        if ev.validate().is_err() {
            return fail("malformed");
        }
        let Some(signer) = book.active(&ev.signer, at).into_iter().find(|s| verify_sig(&s.pubkey, &ev.signing_bytes(), &ev.sig).is_ok()).cloned() else {
            return fail("no key the signer held at that moment signed it");
        };
        let is_owner = signer.role == "owner";
        let is_self = ev.signer == ev.handle;
        let target: Vec<KeySpan> = book.active(&ev.handle, at).into_iter().cloned().collect();
        match ev.action {
            KeyAction::Add => {
                if !(is_owner || (is_self && !target.is_empty())) {
                    return fail("only the owner adds seats");
                }
                if target.iter().any(|s| s.pubkey == ev.pubkey) {
                    return fail("the key is already registered for this seat");
                }
                book.put(&ev.handle, &ev.pubkey, "seat", at);
            }
            KeyAction::Rotate | KeyAction::Revoke => {
                if !(is_owner || is_self) {
                    return fail("only the owner or the seat itself");
                }
                if target.is_empty() {
                    return fail("the seat holds no key");
                }
                if ev.action == KeyAction::Revoke {
                    let owner_hit = target.iter().any(|s| s.role == "owner" && (ev.pubkey.is_empty() || s.pubkey == ev.pubkey));
                    if owner_hit {
                        return fail("the owner key can be rotated, not revoked");
                    }
                    if ev.pubkey.is_empty() {
                        book.end(&ev.handle, at, |_| true);
                    } else if target.iter().any(|s| s.pubkey == ev.pubkey) {
                        let pk = ev.pubkey.clone();
                        book.end(&ev.handle, at, |s| s.pubkey == pk);
                    } else {
                        return fail("the key is not one of the seat's");
                    }
                } else if is_self {
                    // The key that signed hands on its own role.
                    let old = signer.pubkey.clone();
                    book.end(&ev.handle, at, |s| s.pubkey == old);
                    book.put(&ev.handle, &ev.pubkey, &signer.role, at);
                } else {
                    if target.iter().any(|s| s.role == "owner") {
                        return fail("only the owner key rotates itself");
                    }
                    book.end(&ev.handle, at, |_| true);
                    book.put(&ev.handle, &ev.pubkey, "seat", at);
                }
            }
        }
    }
    Ok(book)
}

/// What [`verify_records`] found. `failures` is empty when every record checks.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OfflineReport {
    pub records: usize,
    pub verified: usize,
    pub failures: Vec<ChainBreak>,
}

/// Check each record with nothing but the records and the replayed key log.
pub fn verify_records(book: &KeyBook, records: &[ChainRecord]) -> OfflineReport {
    let mut r = OfflineReport { records: records.len(), ..Default::default() };
    for rec in records {
        let e = &rec.envelope;
        let why = if e.verify_stored(&rec.pubkey).is_err() {
            Some("bad signature".to_string())
        } else if rec.id.rsplit('-').next() != Some(&link_of(&e.sig)[..12]) {
            Some("id does not match its signature".to_string())
        } else if !book.held(&e.author, &rec.pubkey, e.ts_ms) {
            Some(match book.known(&e.author, &rec.pubkey) {
                Some(s) if e.ts_ms < s.from_ms => "signed before its key was registered".to_string(),
                Some(_) => "signed after its key was revoked".to_string(),
                None => format!("the key is not one of {}'s", e.author),
            })
        } else {
            None
        };
        match why {
            Some(why) => r.failures.push(ChainBreak { id: rec.id.clone(), why }),
            None => r.verified += 1,
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{pubkey_hex, Envelope, Kind};
    use ed25519_dalek::SigningKey;

    fn key(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    #[test]
    fn a_seat_key_holds_from_its_add_until_its_revoke() {
        let (owner, seat) = (key(1), key(2));
        let add = KeyEvent::sign(&owner, "ada", KeyAction::Add, "bob", &pubkey_hex(&seat), 1_000);
        let revoke = KeyEvent::sign(&owner, "ada", KeyAction::Revoke, "bob", "", 5_000);
        let book = replay("ada", &pubkey_hex(&owner), &[add, revoke]).unwrap();
        assert!(!book.held("bob", &pubkey_hex(&seat), 999), "before the add");
        assert!(book.held("bob", &pubkey_hex(&seat), 1_000));
        assert!(book.held("bob", &pubkey_hex(&seat), 4_999));
        assert!(!book.held("bob", &pubkey_hex(&seat), 5_000), "from the revoke on");
        assert!(book.held("ada", &pubkey_hex(&owner), 0), "the owner key holds from the start");
    }

    #[test]
    fn a_forged_event_fails_the_whole_log() {
        let (owner, mallory) = (key(1), key(9));
        let forged = KeyEvent::sign(&mallory, "ada", KeyAction::Add, "eve", &pubkey_hex(&key(3)), 1_000);
        assert!(replay("ada", &pubkey_hex(&owner), &[forged]).is_err(), "signed by a key the owner never held");
        let seat_adds = KeyEvent::sign(&key(2), "bob", KeyAction::Add, "eve", &pubkey_hex(&key(3)), 2_000);
        let add_bob = KeyEvent::sign(&owner, "ada", KeyAction::Add, "bob", &pubkey_hex(&key(2)), 1_000);
        assert!(replay("ada", &pubkey_hex(&owner), &[add_bob.clone(), seat_adds]).is_err(), "a seat adds only for itself");
        let ok = replay("ada", &pubkey_hex(&owner), &[add_bob]);
        assert!(ok.is_ok(), "negative control: the owner's own add replays");
    }

    #[test]
    fn rotation_hands_the_role_on_and_ends_the_old_key() {
        let (owner, new_owner, seat, seat2) = (key(1), key(4), key(2), key(5));
        let evs = [
            KeyEvent::sign(&owner, "ada", KeyAction::Add, "bob", &pubkey_hex(&seat), 1_000),
            KeyEvent::sign(&owner, "ada", KeyAction::Rotate, "ada", &pubkey_hex(&new_owner), 2_000),
            KeyEvent::sign(&new_owner, "ada", KeyAction::Rotate, "bob", &pubkey_hex(&seat2), 3_000),
        ];
        let book = replay("ada", &pubkey_hex(&owner), &evs).unwrap();
        assert!(!book.held("ada", &pubkey_hex(&owner), 2_000) && book.held("ada", &pubkey_hex(&new_owner), 2_000));
        assert!(!book.held("bob", &pubkey_hex(&seat), 3_000) && book.held("bob", &pubkey_hex(&seat2), 3_000));
        let stale = KeyEvent::sign(&owner, "ada", KeyAction::Add, "eve", &pubkey_hex(&key(6)), 4_000);
        assert!(replay("ada", &pubkey_hex(&owner), &[evs[0].clone(), evs[1].clone(), stale]).is_err(), "the old owner key no longer signs");
    }

    fn record(sk: &SigningKey, author: &str, ts: i64) -> ChainRecord {
        let env = Envelope::sign(sk, "room", author, Kind::Say, "hello", None, vec![], ts);
        let id = format!("OBS-room-20000101-{}", &link_of(&env.sig)[..12]);
        ChainRecord { seq: 1, id, pubkey: pubkey_hex(sk), envelope: env }
    }

    #[test]
    fn a_record_checks_only_with_the_key_its_author_held_then() {
        let (owner, seat) = (key(1), key(2));
        let evs = [
            KeyEvent::sign(&owner, "ada", KeyAction::Add, "bob", &pubkey_hex(&seat), 1_000),
            KeyEvent::sign(&owner, "ada", KeyAction::Revoke, "bob", "", 9_000),
        ];
        let book = replay("ada", &pubkey_hex(&owner), &evs).unwrap();
        let good = record(&seat, "bob", 5_000);
        let early = record(&seat, "bob", 500);
        let late = record(&seat, "bob", 9_500);
        let imposter = record(&key(7), "bob", 5_000);
        let mut tampered = record(&seat, "bob", 5_000);
        tampered.envelope.content = "changed".into();
        let r = verify_records(&book, &[good, early, late, imposter, tampered]);
        assert_eq!(r.verified, 1, "{r:?}");
        let whys: Vec<&str> = r.failures.iter().map(|f| f.why.as_str()).collect();
        assert_eq!(whys, ["signed before its key was registered", "signed after its key was revoked", "the key is not one of bob's", "bad signature"]);
    }
}
