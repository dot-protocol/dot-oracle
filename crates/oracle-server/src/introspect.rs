//! Accept bearer tokens that another system issued, without ever holding their secret.
//!
//! When a token is not one this Oracle minted, and an introspection endpoint is configured,
//! the server asks it (RFC 7662 in spirit): `POST {"token": "…"}` →
//! `{"active": true, "handle": "rocky", "scope": "write", "exp": 1790000000}`.
//!
//! The answer can only ever name a seat that is active HERE, and can only grant `read` or
//! `write` — never `admin`. A signature is still required for every write, so an introspected
//! token is a door pass, not an identity: the seat's key remains the identity.
//! Answers are cached briefly (so revocation upstream takes effect within minutes), by the
//! token's hash, never the token.

use crate::store::{Principal, Scope, Store};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Mutex, time::Duration};

const POSITIVE_TTL_MS: i64 = 5 * 60_000;
const NEGATIVE_TTL_MS: i64 = 30_000;
const CACHE_MAX: usize = 4096;

pub struct Introspector {
    url: String,
    bearer: Option<String>,
    agent: ureq::Agent,
    cache: Mutex<HashMap<String, (Option<Principal>, i64)>>,
}

#[derive(serde::Deserialize)]
struct Answer {
    #[serde(default)]
    active: bool,
    handle: Option<String>,
    scope: Option<String>,
    exp: Option<i64>,
}

impl Introspector {
    /// `ORACLE_INTROSPECT_URL`, and optionally `ORACLE_INTROSPECT_TOKEN_FILE` (a bearer this
    /// server presents to the endpoint). Unset means the feature does not exist.
    pub fn from_env() -> Option<Introspector> {
        let url = std::env::var("ORACLE_INTROSPECT_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())?;
        let bearer = std::env::var("ORACLE_INTROSPECT_TOKEN_FILE")
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        Some(Introspector::new(url, bearer))
    }

    pub fn new(url: String, bearer: Option<String>) -> Introspector {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(5))
            .build();
        Introspector {
            url,
            bearer,
            agent,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Who this foreign token is, if the endpoint vouches for it and the seat is active here.
    pub fn resolve(&self, token: &str, store: &Store, now_ms: i64) -> Option<Principal> {
        let key = hex::encode(Sha256::digest(token.as_bytes()));
        if let Some((who, until)) = self.cache.lock().ok()?.get(&key).cloned() {
            if until > now_ms {
                return who;
            }
        }
        let (who, ttl) = match self.ask(token) {
            Some(a) => {
                let who = decide(&a, store, now_ms);
                let ttl = match (&who, a.exp) {
                    (Some(_), Some(exp)) => POSITIVE_TTL_MS.min(exp * 1000 - now_ms),
                    (Some(_), None) => POSITIVE_TTL_MS,
                    (None, _) => NEGATIVE_TTL_MS,
                };
                (who, ttl)
            }
            // Endpoint down: refuse, and do not remember the refusal for long.
            None => (None, 5_000),
        };
        if let Ok(mut c) = self.cache.lock() {
            if c.len() >= CACHE_MAX {
                c.retain(|_, (_, until)| *until > now_ms);
                if c.len() >= CACHE_MAX {
                    c.clear();
                }
            }
            c.insert(key, (who.clone(), now_ms + ttl));
        }
        who
    }

    fn ask(&self, token: &str) -> Option<Answer> {
        let mut req = self.agent.post(&self.url);
        if let Some(b) = &self.bearer {
            req = req.set("Authorization", &format!("Bearer {b}"));
        }
        req.send_json(serde_json::json!({ "token": token }))
            .ok()?
            .into_json()
            .ok()
    }
}

fn decide(a: &Answer, store: &Store, now_ms: i64) -> Option<Principal> {
    if !a.active || a.exp.is_some_and(|exp| exp * 1000 <= now_ms) {
        return None;
    }
    let handle = a.handle.as_deref()?.trim().to_lowercase();
    // Never more than write, whatever the endpoint says.
    let scope = match a.scope.as_deref() {
        Some("write") => Scope::Write,
        Some("read") => Scope::Read,
        _ => return None,
    };
    store.active_key(&handle).ok()??;
    Some(Principal { handle, scope })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_rocky() -> Store {
        let s = Store::memory().unwrap();
        let owner = oracle_proto::generate_key();
        s.init_owner("blaze", &oracle_proto::pubkey_hex(&owner), 1)
            .unwrap();
        let ev = oracle_proto::KeyEvent::sign(
            &owner,
            "blaze",
            oracle_proto::KeyAction::Add,
            "rocky",
            &oracle_proto::pubkey_hex(&oracle_proto::generate_key()),
            1,
        );
        s.apply_key_event(&ev, 1).unwrap();
        s
    }
    fn answer(active: bool, handle: &str, scope: &str, exp: Option<i64>) -> Answer {
        Answer {
            active,
            handle: Some(handle.into()),
            scope: Some(scope.into()),
            exp,
        }
    }

    #[test]
    fn an_answer_can_only_name_an_active_seat_and_never_grants_admin() {
        let s = store_with_rocky();
        let now = 1_000_000;
        assert_eq!(
            decide(&answer(true, "Rocky", "write", None), &s, now),
            Some(Principal {
                handle: "rocky".into(),
                scope: Scope::Write
            })
        );
        assert_eq!(
            decide(&answer(true, "rocky", "read", Some(2_000)), &s, now)
                .unwrap()
                .scope,
            Scope::Read
        );
        assert_eq!(
            decide(&answer(true, "rocky", "admin", None), &s, now),
            None,
            "admin is never delegated"
        );
        assert_eq!(
            decide(&answer(true, "stranger", "write", None), &s, now),
            None,
            "unknown seat"
        );
        assert_eq!(
            decide(&answer(false, "rocky", "write", None), &s, now),
            None,
            "inactive"
        );
        assert_eq!(
            decide(&answer(true, "rocky", "write", Some(999)), &s, now),
            None,
            "expired"
        );
        assert_eq!(
            decide(
                &Answer {
                    active: true,
                    handle: None,
                    scope: Some("write".into()),
                    exp: None
                },
                &s,
                now
            ),
            None,
            "a token bound to nobody is nobody"
        );
    }
}
