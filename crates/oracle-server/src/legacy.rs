//! Accept the tokens the Oracle before this one issued, verified here, with its secret.
//!
//! That server (`tree_serve.py`) minted stateless bearer tokens for its OAuth clients — the
//! claude.ai connectors, Codex, `claude mcp add … --header` — of the form
//! `ocg_<base64url(json payload)>.<base64url(hmac-sha256(secret, that base64url))>` with
//! `{iss, aud: "oracle-mcp", sub, scope, iat, exp, jti, handle?}`, valid 90 days. They are in
//! every connector's keychain today. When `/mcp` starts pointing here they must keep working,
//! or every one of those clients re-authenticates on the same morning (Blaze, 2026-09-28: "the
//! cutover should work with existing agents using old oracle").
//!
//! So, given the same signing secret (a file only this server reads), a legacy token is
//! checked exactly as the old server checked it — signature, audience, expiry, revocation —
//! and becomes a [`Principal`]:
//! - `handle` claim → that seat, which must be active here (a stranger is nobody);
//! - no handle, but the old server's bindings file names the token's fingerprint → that seat;
//! - no handle at all → `guest`, read-only, and only if the operator allowed guests.
//!
//! Scope is the old grant, capped: `oracle:write` or the legacy `mcp` → write, `oracle:read` →
//! read, never admin. A signature is still required for every write, so a legacy token is a
//! door pass, not an identity, exactly like an introspected one.
//!
//! This is the last dependency on the old server's `/auth/introspect`; once the secret is
//! here, `introspect.rs` is only reached for tokens that are not `ocg_`.

use crate::store::{Principal, Scope, Store};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const PREFIX: &str = "ocg_";
const AUDIENCE: &str = "oracle-mcp";
pub const GUEST: &str = "guest";

pub struct Legacy {
    secret: Vec<u8>,
    /// The old server's `token-bindings.json`: `sha256(token)[..16]` → seat, for tokens minted
    /// before handles rode inside them.
    bindings: Option<PathBuf>,
    /// Revoked `jti`s, one per line. The old server kept these in its graph; export them here.
    revoked: Option<PathBuf>,
    /// sha256 (hex) of whole revoked tokens, one per line: the old server's `:RevokedToken` store, which refused a bearer
    /// by the hash of the token itself, not by its `jti` (token_revocation.py). One of them is an unexpired `ocg_` token
    /// revoked on 2026-07-27, so a jti list alone would let it back in.
    revoked_sha256: Option<PathBuf>,
    /// Whether a valid token bound to nobody may read as `guest`.
    guests: bool,
}

#[derive(serde::Deserialize)]
struct Payload {
    aud: Option<String>,
    scope: Option<String>,
    exp: Option<i64>,
    jti: Option<String>,
    handle: Option<String>,
}

impl Legacy {
    /// `ORACLE_LEGACY_OAUTH_SECRET_FILE` (the old server's `ORACLE_OAUTH_SIGNING_SECRET`), and
    /// optionally `ORACLE_LEGACY_BINDINGS_FILE`, `ORACLE_LEGACY_REVOKED_FILE`,
    /// `ORACLE_LEGACY_GUESTS=1`. No secret file means the feature does not exist.
    pub fn from_env() -> Option<Legacy> {
        let path = std::env::var("ORACLE_LEGACY_OAUTH_SECRET_FILE")
            .ok()
            .filter(|p| !p.trim().is_empty())?;
        let secret = std::fs::read_to_string(&path)
            .map_err(|e| eprintln!("legacy tokens: cannot read {path}: {e}"))
            .ok()?
            .trim()
            .to_string();
        if secret.is_empty() {
            eprintln!("legacy tokens: {path} is empty; old tokens will not be accepted");
            return None;
        }
        let file = |var: &str| std::env::var(var).ok().filter(|p| !p.trim().is_empty()).map(PathBuf::from);
        Some(Legacy {
            secret: secret.into_bytes(),
            bindings: file("ORACLE_LEGACY_BINDINGS_FILE"),
            revoked: file("ORACLE_LEGACY_REVOKED_FILE"),
            revoked_sha256: file("ORACLE_LEGACY_REVOKED_SHA256_FILE"),
            guests: std::env::var("ORACLE_LEGACY_GUESTS").is_ok_and(|v| v == "1"),
        })
    }

    pub fn new(secret: &str, bindings: Option<&Path>, revoked: Option<&Path>, guests: bool) -> Legacy {
        Legacy {
            secret: secret.as_bytes().to_vec(),
            bindings: bindings.map(Path::to_path_buf),
            revoked: revoked.map(Path::to_path_buf),
            revoked_sha256: None,
            guests,
        }
    }

    /// The same, also refusing any token whose sha256 is listed in `path`.
    pub fn with_revoked_sha256(mut self, path: &Path) -> Legacy {
        self.revoked_sha256 = Some(path.to_path_buf());
        self
    }

    /// The old server's fingerprint of a token: the first 16 hex of its sha256. Never the token.
    pub fn fingerprint(token: &str) -> String {
        hex::encode(Sha256::digest(token.as_bytes()))[..16].to_string()
    }

    /// Signature, audience, expiry, revocation — the payload if all hold, else nothing (and
    /// never why: an attacker learns only "401").
    fn payload(&self, token: &str, now_ms: i64) -> Option<Payload> {
        let rest = token.strip_prefix(PREFIX)?;
        if let Some(path) = &self.revoked_sha256 {
            let digest = hex::encode(Sha256::digest(token.as_bytes()));
            if std::fs::read_to_string(path).unwrap_or_default().lines().any(|l| l.trim().eq_ignore_ascii_case(&digest)) {
                return None;
            }
        }
        let (body, sig) = rest.split_once('.')?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret).ok()?;
        mac.update(body.as_bytes());
        mac.verify_slice(&b64url_decode(sig)?).ok()?;
        let p: Payload = serde_json::from_slice(&b64url_decode(body)?).ok()?;
        if p.aud.as_deref() != Some(AUDIENCE) {
            return None;
        }
        if p.exp.unwrap_or(0) * 1000 <= now_ms {
            return None;
        }
        if let (Some(jti), Some(path)) = (p.jti.as_deref(), &self.revoked) {
            let list = std::fs::read_to_string(path).unwrap_or_default();
            if list.lines().any(|l| l.trim() == jti) {
                return None;
            }
        }
        Some(p)
    }

    fn bound_handle(&self, token: &str) -> Option<String> {
        let path = self.bindings.as_ref()?;
        let text = std::fs::read_to_string(path).ok()?;
        let map: HashMap<String, String> = serde_json::from_str(&text).ok()?;
        map.get(&Self::fingerprint(token))
            .map(|h| h.trim().to_lowercase())
            .filter(|h| !h.is_empty())
    }

    /// Who this old token is here, if anyone.
    pub fn resolve(&self, token: &str, store: &Store, now_ms: i64) -> Option<Principal> {
        let p = self.payload(token, now_ms)?;
        let scope = scope_of(p.scope.as_deref().unwrap_or(""))?;
        let handle = p
            .handle
            .as_deref()
            .map(|h| h.trim().to_lowercase())
            .filter(|h| !h.is_empty())
            .or_else(|| self.bound_handle(token));
        match handle {
            Some(h) => {
                store.active_key(&h).ok()??;
                Some(Principal { handle: h, scope })
            }
            None if self.guests => Some(Principal { handle: GUEST.into(), scope: Scope::Read }),
            None => None,
        }
    }
}

/// The old grant, capped at write. `mcp`, `*` and `operator` were the old full grant.
fn scope_of(granted: &str) -> Option<Scope> {
    let items: Vec<&str> = granted.split_whitespace().collect();
    if items.iter().any(|s| matches!(*s, "mcp" | "*" | "operator" | "oracle:write" | "oracle:admin")) {
        return Some(Scope::Write);
    }
    if items.contains(&"oracle:read") {
        return Some(Scope::Read);
    }
    None
}

pub(crate) fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        })
    };
    let bytes = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc: u32 = 0;
        for (i, c) in chunk.iter().enumerate() {
            acc |= val(*c)? << (18 - 6 * i);
        }
        let n = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return None,
        };
        out.extend_from_slice(&acc.to_be_bytes()[1..1 + n]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minted by the old server's own code (tree_serve.py `_issue_oauth_access_token`, Python
    // hmac/base64) with the secret below, exp 2100-01-01. Byte-for-byte what a connector holds.
    const SECRET: &str = "test-secret-7731";
    const BOUND: &str = "ocg_eyJhdWQiOiJvcmFjbGUtbWNwIiwiZXhwIjo0MTAyNDQ0ODAwLCJoYW5kbGUiOiJyb2NreSIsImlhdCI6MTc5MDAwMDAwMCwiaXNzIjoib3JhY2xlLmF4eGlzLndvcmxkIiwianRpIjoianRpLWJvdW5kLTEiLCJzY29wZSI6Im9yYWNsZTpyZWFkIG9yYWNsZTp3cml0ZSIsInN1YiI6ImNsaWVudF9YVUFWTEltIn0.xJbVG4xvRK_Am5hX-guu3yjqHurmclnxPCaPt9Ct3CU";
    const READ: &str = "ocg_eyJhdWQiOiJvcmFjbGUtbWNwIiwiZXhwIjo0MTAyNDQ0ODAwLCJoYW5kbGUiOiJyb2NreSIsImlhdCI6MTc5MDAwMDAwMCwiaXNzIjoib3JhY2xlLmF4eGlzLndvcmxkIiwianRpIjoianRpLXJlYWQtMSIsInNjb3BlIjoib3JhY2xlOnJlYWQiLCJzdWIiOiJjIn0.KENM3-MdYCWEnVQjIcDmMNCGMbaJjER8rNZ443FLEiw";
    const MCP: &str = "ocg_eyJhdWQiOiJvcmFjbGUtbWNwIiwiZXhwIjo0MTAyNDQ0ODAwLCJoYW5kbGUiOiJyb2NreSIsImlhdCI6MTc5MDAwMDAwMCwiaXNzIjoib3JhY2xlLmF4eGlzLndvcmxkIiwianRpIjoianRpLW1jcC0xIiwic2NvcGUiOiJtY3AiLCJzdWIiOiJjIn0.MZi1tomBgYWuENy9V4vduZRNPnPTCLH4VDsS9zbF5z8";
    const UNBOUND: &str = "ocg_eyJhdWQiOiJvcmFjbGUtbWNwIiwiZXhwIjo0MTAyNDQ0ODAwLCJpYXQiOjE3OTAwMDAwMDAsImlzcyI6Im9yYWNsZS5heHhpcy53b3JsZCIsImp0aSI6Imp0aS11bmJvdW5kLTEiLCJzY29wZSI6Im9yYWNsZTpyZWFkIG9yYWNsZTp3cml0ZSIsInN1YiI6ImMifQ.oK-hzu8Jis01cTusDIoro_Mk2E4NR7sXSmTUDM6whJw";
    const EXPIRED: &str = "ocg_eyJhdWQiOiJvcmFjbGUtbWNwIiwiZXhwIjoxNzAwMDg2NDAwLCJoYW5kbGUiOiJyb2NreSIsImlhdCI6MTcwMDAwMDAwMCwiaXNzIjoib3JhY2xlLmF4eGlzLndvcmxkIiwianRpIjoianRpLW9sZC0xIiwic2NvcGUiOiJvcmFjbGU6cmVhZCBvcmFjbGU6d3JpdGUiLCJzdWIiOiJjIn0.e7FhsiaRkovZpLTkfh95-azL4HdxcWJEx7UPSAV0uIo";
    const BAD_AUD: &str = "ocg_eyJhdWQiOiJvdGhlciIsImV4cCI6NDEwMjQ0NDgwMCwiaGFuZGxlIjoicm9ja3kiLCJpYXQiOjE3OTAwMDAwMDAsImlzcyI6Im9yYWNsZS5heHhpcy53b3JsZCIsImp0aSI6Imp0aS1hdWQtMSIsInNjb3BlIjoib3JhY2xlOnJlYWQgb3JhY2xlOndyaXRlIiwic3ViIjoiYyJ9._rjtS2oUeT6oI7zAyWGK9fYVhjEiVeYypSzQOOl2g0c";
    const NOW: i64 = 1_790_600_000_000;

    fn store_with_rocky() -> Store {
        let s = Store::memory().unwrap();
        let owner = oracle_proto::generate_key();
        s.init_owner("blaze", &oracle_proto::pubkey_hex(&owner), 1).unwrap();
        let ev = oracle_proto::KeyEvent::sign(
            &owner, "blaze", oracle_proto::KeyAction::Add, "rocky",
            &oracle_proto::pubkey_hex(&oracle_proto::generate_key()), 1,
        );
        s.apply_key_event(&ev, 1).unwrap();
        s
    }
    fn tmp(name: &str, body: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("legacy-{}-{name}", oracle_proto::random_hex(4)));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn a_token_the_old_server_minted_is_the_seat_it_names_with_its_grant_capped_at_write() {
        let s = store_with_rocky();
        let l = Legacy::new(SECRET, None, None, false);
        assert_eq!(l.resolve(BOUND, &s, NOW), Some(Principal { handle: "rocky".into(), scope: Scope::Write }));
        assert_eq!(l.resolve(READ, &s, NOW).unwrap().scope, Scope::Read);
        assert_eq!(l.resolve(MCP, &s, NOW).unwrap().scope, Scope::Write, "the legacy 'mcp' grant is write, never admin");
    }

    #[test]
    fn wrong_secret_expiry_audience_or_tampering_are_all_just_nobody() {
        let s = store_with_rocky();
        assert_eq!(Legacy::new("another-secret", None, None, true).resolve(BOUND, &s, NOW), None);
        let l = Legacy::new(SECRET, None, None, true);
        assert_eq!(l.resolve(EXPIRED, &s, NOW), None);
        assert_eq!(l.resolve(BAD_AUD, &s, NOW), None);
        assert_eq!(l.resolve(&BOUND[..BOUND.len() - 2], &s, NOW), None, "a clipped signature");
        let (body, sig) = BOUND[4..].split_once('.').unwrap();
        let mut b = body.to_string();
        b.replace_range(..1, if body.starts_with('e') { "f" } else { "e" });
        assert_eq!(l.resolve(&format!("ocg_{b}.{sig}"), &s, NOW), None, "a tampered payload");
        assert_eq!(l.resolve("Bearer nonsense", &s, NOW), None);
        assert_eq!(l.resolve("ocg_", &s, NOW), None);
        assert_eq!(l.resolve("ocg_x.y", &s, NOW), None);
    }

    #[test]
    fn a_seat_unknown_here_is_nobody_even_with_a_perfect_token() {
        let s = Store::memory().unwrap();
        let owner = oracle_proto::generate_key();
        s.init_owner("blaze", &oracle_proto::pubkey_hex(&owner), 1).unwrap();
        assert_eq!(Legacy::new(SECRET, None, None, true).resolve(BOUND, &s, NOW), None, "rocky is not a seat in this store");
    }

    #[test]
    fn a_token_without_a_handle_is_the_bindings_files_seat_or_a_guest_or_nobody() {
        let s = store_with_rocky();
        assert_eq!(Legacy::new(SECRET, None, None, false).resolve(UNBOUND, &s, NOW), None, "guests off");
        assert_eq!(
            Legacy::new(SECRET, None, None, true).resolve(UNBOUND, &s, NOW),
            Some(Principal { handle: GUEST.into(), scope: Scope::Read }),
            "guests on: read only, whatever the grant said"
        );
        assert_eq!(Legacy::fingerprint(UNBOUND), "d510c05262aa78e5", "the old server's fingerprint");
        let bindings = tmp("bindings.json", r#"{"d510c05262aa78e5": "Rocky", "0000000000000000": "kin"}"#);
        assert_eq!(
            Legacy::new(SECRET, Some(&bindings), None, false).resolve(UNBOUND, &s, NOW),
            Some(Principal { handle: "rocky".into(), scope: Scope::Write }),
            "bound by fingerprint, handle lowercased"
        );
        let other = tmp("bindings2.json", r#"{"0000000000000000": "kin"}"#);
        assert_eq!(Legacy::new(SECRET, Some(&other), None, false).resolve(UNBOUND, &s, NOW), None);
    }

    #[test]
    fn a_revoked_jti_is_dead() {
        let s = store_with_rocky();
        let revoked = tmp("revoked", "jti-other\njti-bound-1\n");
        assert_eq!(Legacy::new(SECRET, None, Some(&revoked), false).resolve(BOUND, &s, NOW), None);
        assert!(Legacy::new(SECRET, None, Some(&revoked), false).resolve(READ, &s, NOW).is_some());
    }

    #[test]
    fn a_token_revoked_by_its_sha256_is_dead() {
        let s = store_with_rocky();
        let digest = hex::encode(Sha256::digest(BOUND.as_bytes()));
        let list = tmp("revoked-sha256", &format!("0000\n{}\n", digest.to_uppercase()));
        let l = Legacy::new(SECRET, None, None, false).with_revoked_sha256(&list);
        assert_eq!(l.resolve(BOUND, &s, NOW), None, "the old server's :RevokedToken hash");
        assert!(l.resolve(READ, &s, NOW).is_some(), "only that token");
        assert!(Legacy::new(SECRET, None, None, false).resolve(BOUND, &s, NOW).is_some(), "control: without the list it is accepted");
    }

    #[test]
    fn base64url_decodes_what_python_encodes() {
        assert_eq!(b64url_decode("aGk").unwrap(), b"hi");
        assert_eq!(b64url_decode("aGk=").unwrap(), b"hi");
        assert_eq!(b64url_decode("-_8").unwrap(), vec![0xfb, 0xff]);
        assert!(b64url_decode("a").is_none());
        assert!(b64url_decode("a+b/").is_none(), "standard alphabet is not url-safe");
    }
}
