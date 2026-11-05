#![forbid(unsafe_code)]

//! The Oracle wire contract.
//!
//! This crate is the only place signing and verification are implemented. The server and
//! the `ox` CLI both depend on it, so they cannot disagree about which bytes were signed.
//! See `docs/DESIGN.md` § Trust.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod keyfile;
pub mod microdot;
pub mod offline;
/// Plan heads (docs/PLANS.md): shared by ox and oracle-mcp, so an MCP seat writes a plan exactly as `ox plan` does.
pub mod plan;

/// Accepted clock skew between a client and the server, and the replay window for nonces.
pub const MAX_SKEW_MS: i64 = 5 * 60 * 1000;
/// How far before its delivery a queued write may say it was authored: an outbox older than this is not replayed as news.
pub const MAX_AUTHORED_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const SAY_DOMAIN: &str = "oracle-say/2";
pub const KEY_DOMAIN: &str = "oracle-key/1";
pub const MAX_CONTENT_BYTES: usize = 64 * 1024;
pub const MAX_CARGO_BYTES: usize = 1024 * 1024;
/// The `prev` of an author's first chained write when it has no native observation before it (docs/CHAIN.md).
pub const GENESIS_PREV: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtoError {
    #[error("{0} is not valid hex of the expected length")]
    BadHex(&'static str),
    #[error("signature does not verify")]
    BadSignature,
    #[error("timestamp is outside the accepted window")]
    Stale,
    #[error("invalid field: {0}")]
    Invalid(&'static str),
}

/// What kind of write this is. One envelope, three intents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Speech in a room.
    Say,
    /// An owned state head (subject + state + next-touch clock).
    State,
    /// Durable knowledge: a decision, lesson, preference or commitment.
    Memory,
    /// A state update ON a message, not a message (Blaze, 2026-09-27): names its subject with one
    /// `annotates:<id>` tag and says what it sets with `set:k=v` tags; the content is the why.
    /// Read back as the subject's derived fields and history, never listed as speech.
    Annotate,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Say => "say",
            Kind::State => "state",
            Kind::Memory => "memory",
            Kind::Annotate => "annotate",
        }
    }
}

/// A signed write. Everything that enters Oracle arrives as one of these.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Envelope {
    pub channel: String,
    pub author: String,
    pub kind: Kind,
    pub ts_ms: i64,
    /// Hex, at least 16 random bytes. Usable once inside the skew window.
    pub nonce: String,
    /// Unique forever. A retry with the same `client_op` returns the original observation.
    pub client_op: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// When the author wrote it, if that was earlier than `ts_ms`: a write queued offline and re-signed on
    /// delivery keeps its original time here (signed), while `ts_ms` stays the fresh, replay-checked clock.
    /// Absent on a write delivered at once; old envelopes sign exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authored_ms: Option<i64>,
    /// The author chain (docs/CHAIN.md): [`link_of`] the author's previous native write's signature, or
    /// [`GENESIS_PREV`]. Signed when present; absent, the envelope signs exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    /// Hex Ed25519 signature over [`Envelope::signing_bytes`].
    pub sig: String,
}

/// Lowercase hex SHA-256: the one digest the protocol uses.
/// A type address, `domain.form.topic` (Blaze, 31 Oct 2026: the record's name, judged by the writing seat, routed by
/// prefix): one to six dot-separated parts of lowercase letters, digits and dashes, each starting with a letter or digit,
/// at most 120 characters in all. One part (`decision`) is the older single-word type and stays valid.
pub fn type_addr_ok(addr: &str) -> bool {
    addr.len() <= 120
        && (1..=6).contains(&addr.split('.').count())
        && addr.split('.').all(|p| {
            !p.is_empty() && p.len() <= 48 && p.as_bytes()[0].is_ascii_alphanumeric()
                && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

pub fn sha_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The link to a signed write: SHA-256 of its hex signature, the same digest its observation id is cut from.
pub fn link_of(sig_hex: &str) -> String {
    sha_hex(sig_hex.as_bytes())
}

fn link_ok(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn handle_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

impl Envelope {
    /// The exact bytes that are signed. Content, cargo and tags are covered by hash, so
    /// newlines, CRLF, unicode and size never affect canonicalisation.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut tags = self.tags.clone();
        tags.sort();
        let mut s = format!(
            "{SAY_DOMAIN}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.channel,
            self.author,
            self.kind.as_str(),
            self.ts_ms,
            self.nonce,
            self.client_op,
            sha_hex(self.content.as_bytes()),
            sha_hex(self.cargo.as_deref().unwrap_or("").as_bytes()),
            sha_hex(tags.join("\n").as_bytes()),
        );
        // Only an envelope that carries it signs it: every envelope without one keeps its exact bytes.
        if let Some(a) = self.authored_ms {
            s.push_str(&format!("\nauthored:{a}"));
        }
        if let Some(p) = &self.prev {
            s.push_str(&format!("\nprev:{p}"));
        }
        s.into_bytes()
    }

    /// Build and sign an envelope. `now_ms` is injected so callers and tests control time.
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        sk: &SigningKey,
        channel: &str,
        author: &str,
        kind: Kind,
        content: &str,
        cargo: Option<String>,
        tags: Vec<String>,
        now_ms: i64,
    ) -> Envelope {
        let mut env = Envelope {
            channel: channel.to_string(),
            author: author.to_string(),
            kind,
            ts_ms: now_ms,
            nonce: random_hex(16),
            client_op: random_hex(16),
            content: content.to_string(),
            cargo,
            tags,
            authored_ms: None,
            prev: None,
            sig: String::new(),
        };
        env.sig = hex::encode(sk.sign(&env.signing_bytes()).to_bytes());
        env
    }

    /// Name the author's chain head and sign again (same `client_op`, fresh clock and nonce): the write the server
    /// stores only while `prev` is still that head.
    pub fn chain_to(mut self, prev: &str, sk: &SigningKey, now_ms: i64) -> Envelope {
        self.prev = Some(prev.to_string());
        self.resign(sk, now_ms)
    }

    /// Re-sign a queued envelope for delivery, keeping when it was written: `authored_ms` takes the first
    /// signing time (unless it already holds one). For a server that announces `authored_ms`.
    pub fn resign_authored(mut self, sk: &SigningKey, now_ms: i64) -> Envelope {
        self.authored_ms = Some(self.authored_ms.unwrap_or(self.ts_ms));
        self.resign(sk, now_ms)
    }

    /// Re-sign for a retry: fresh clock and nonce, the SAME `client_op`. If the first
    /// attempt did reach the server, the retry is recognised and nothing is written twice.
    pub fn resign(mut self, sk: &SigningKey, now_ms: i64) -> Envelope {
        self.ts_ms = now_ms;
        self.nonce = random_hex(16);
        self.sig = hex::encode(sk.sign(&self.signing_bytes()).to_bytes());
        self
    }

    /// Structural checks that need no key and no clock.
    pub fn validate(&self) -> Result<(), ProtoError> {
        if !handle_ok(&self.channel) {
            return Err(ProtoError::Invalid("channel"));
        }
        if !handle_ok(&self.author) {
            return Err(ProtoError::Invalid("author"));
        }
        if self.content.is_empty() || self.content.len() > MAX_CONTENT_BYTES {
            return Err(ProtoError::Invalid("content"));
        }
        if self
            .cargo
            .as_ref()
            .is_some_and(|c| c.len() > MAX_CARGO_BYTES)
        {
            return Err(ProtoError::Invalid("cargo"));
        }
        if self.nonce.len() < 32 || hex::decode(&self.nonce).is_err() {
            return Err(ProtoError::Invalid("nonce"));
        }
        // client_op is signed as a raw line and the tags are signed as one newline-joined digest, so a control
        // character in either would let two different envelopes sign the same bytes: ["a\nb"] and ["a", "b"] hashed
        // alike (Ramanujan's N2 audit of 23de599). Refusing them keeps
        // signing_bytes injective without changing a byte of any envelope already stored (none holds one).
        if self.client_op.is_empty() || self.client_op.len() > 128 || self.client_op.chars().any(char::is_control) {
            return Err(ProtoError::Invalid("client_op"));
        }
        if self.tags.len() > 32
            || self.tags.iter().any(|t| t.is_empty() || t.len() > 200 || t.chars().any(char::is_control))
        {
            return Err(ProtoError::Invalid("tags"));
        }
        if self.authored_ms.is_some_and(|a| a > self.ts_ms + MAX_SKEW_MS || a < self.ts_ms - MAX_AUTHORED_AGE_MS) {
            return Err(ProtoError::Invalid("authored_ms"));
        }
        if self.prev.as_deref().is_some_and(|p| !link_ok(p)) {
            return Err(ProtoError::Invalid("prev"));
        }
        Ok(())
    }

    /// Full verification against the registered key of `author` at server time `now_ms`.
    pub fn verify(&self, pubkey_hex: &str, now_ms: i64) -> Result<(), ProtoError> {
        self.validate()?;
        if (now_ms - self.ts_ms).abs() > MAX_SKEW_MS {
            return Err(ProtoError::Stale);
        }
        verify_sig(pubkey_hex, &self.signing_bytes(), &self.sig)
    }

    /// Verification of an envelope read back from storage: the signature and shape, not the clock
    /// (a stored line is as old as it is).
    pub fn verify_stored(&self, pubkey_hex: &str) -> Result<(), ProtoError> {
        self.validate()?;
        verify_sig(pubkey_hex, &self.signing_bytes(), &self.sig)
    }
}

/// One line of a chain's face (docs/CHAIN.md): a native observation with the key that verified it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChainRecord {
    pub seq: i64,
    pub id: String,
    pub pubkey: String,
    pub envelope: Envelope,
    /// A link withheld from this reader (docs/CHAIN.md "Withheld links"): the record is in a room the reader may not
    /// read, so the server gives only its place: `seq`, the author, `prev` and `sig`. The room, words and id are blanked,
    /// so its signature cannot be checked, but the links on both sides of it can: `prev` against the record before, and
    /// the next record's `prev` against this `sig`. Absent (false) on every record given whole.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub withheld: bool,
}

impl ChainRecord {
    /// This record as a withheld link: the place in the chain kept (author, kind, time, `prev`, `sig`), everything that
    /// says where it was written or what it says blanked. The id is replaced too, because an id names its room.
    pub fn withhold(self) -> ChainRecord {
        let e = self.envelope;
        ChainRecord {
            seq: self.seq,
            id: format!("withheld-{}", self.seq),
            pubkey: self.pubkey,
            envelope: Envelope {
                channel: String::new(),
                author: e.author,
                kind: e.kind,
                ts_ms: e.ts_ms,
                nonce: String::new(),
                client_op: String::new(),
                content: String::new(),
                cargo: None,
                tags: Vec::new(),
                authored_ms: None,
                prev: e.prev,
                sig: e.sig,
            },
            withheld: true,
        }
    }
}

/// What [`check_chain`] found. `breaks` is empty for a whole chain.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChainReport {
    pub records: usize,
    pub chained: usize,
    pub unchained: usize,
    /// Links given without their record (in a room this reader may not read): counted, linked, not signature-checked.
    #[serde(default)]
    pub withheld: usize,
    pub first_chained: Option<String>,
    pub from_genesis: bool,
    pub breaks: Vec<ChainBreak>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChainBreak {
    pub id: String,
    pub why: String,
}

/// Check one author's records in order, with nothing but the records: each signature against its own rebuilt
/// signing bytes, each id against its signature, each `prev` against the record before it. A first record that
/// names a predecessor outside the records is not a break (the face may start mid-chain).
pub fn check_chain(records: &[ChainRecord]) -> ChainReport {
    let mut r = ChainReport { records: records.len(), ..Default::default() };
    let author = records.first().map(|x| x.envelope.author.as_str());
    for (i, rec) in records.iter().enumerate() {
        let mut broke = |why: &str| r.breaks.push(ChainBreak { id: rec.id.clone(), why: why.to_string() });
        if Some(rec.envelope.author.as_str()) != author {
            broke("another author's record");
        }
        if rec.withheld {
            // Only the link is given: the signed bytes are not, so there is no signature or id to check here. Its `prev`
            // is still checked below, and the next record's `prev` is checked against its `sig`, so the chain stays whole.
            r.withheld += 1;
        } else {
            if rec.envelope.verify_stored(&rec.pubkey).is_err() {
                broke("bad signature");
            }
            if rec.id.rsplit('-').next() != Some(&link_of(&rec.envelope.sig)[..12]) {
                broke("id does not match its signature");
            }
        }
        match rec.envelope.prev.as_deref() {
            None => r.unchained += 1,
            Some(p) => {
                r.chained += 1;
                if r.first_chained.is_none() {
                    r.first_chained = Some(rec.id.clone());
                }
                if i == 0 {
                    r.from_genesis = p == GENESIS_PREV;
                } else if p != link_of(&records[i - 1].envelope.sig) {
                    r.breaks.push(ChainBreak { id: rec.id.clone(), why: "prev is not the record before it".into() });
                }
            }
        }
    }
    r
}

/// A change to the key registry. Must be signed by the owner, or by the current key of the
/// seat being changed. There is no unsigned path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyEvent {
    pub action: KeyAction,
    pub handle: String,
    /// Hex public key being registered. Empty for `revoke`.
    #[serde(default)]
    pub pubkey: String,
    pub ts_ms: i64,
    pub nonce: String,
    /// The seat whose key signed this event.
    pub signer: String,
    pub sig: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyAction {
    Add,
    Rotate,
    Revoke,
}

impl KeyAction {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyAction::Add => "add",
            KeyAction::Rotate => "rotate",
            KeyAction::Revoke => "revoke",
        }
    }
}

impl KeyEvent {
    pub fn signing_bytes(&self) -> Vec<u8> {
        format!(
            "{KEY_DOMAIN}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.action.as_str(),
            self.handle,
            self.pubkey,
            self.ts_ms,
            self.nonce,
            self.signer
        )
        .into_bytes()
    }

    pub fn sign(
        sk: &SigningKey,
        signer: &str,
        action: KeyAction,
        handle: &str,
        pubkey: &str,
        now_ms: i64,
    ) -> KeyEvent {
        let mut ev = KeyEvent {
            action,
            handle: handle.to_string(),
            pubkey: pubkey.to_string(),
            ts_ms: now_ms,
            nonce: random_hex(16),
            signer: signer.to_string(),
            sig: String::new(),
        };
        ev.sig = hex::encode(sk.sign(&ev.signing_bytes()).to_bytes());
        ev
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if !handle_ok(&self.handle) {
            return Err(ProtoError::Invalid("handle"));
        }
        if !handle_ok(&self.signer) {
            return Err(ProtoError::Invalid("signer"));
        }
        match self.action {
            KeyAction::Revoke => {}
            _ => {
                parse_pubkey(&self.pubkey)?;
            }
        }
        if self.nonce.len() < 32 || hex::decode(&self.nonce).is_err() {
            return Err(ProtoError::Invalid("nonce"));
        }
        Ok(())
    }

    pub fn verify(&self, signer_pubkey_hex: &str, now_ms: i64) -> Result<(), ProtoError> {
        self.validate()?;
        if (now_ms - self.ts_ms).abs() > MAX_SKEW_MS {
            return Err(ProtoError::Stale);
        }
        verify_sig(signer_pubkey_hex, &self.signing_bytes(), &self.sig)
    }
}

pub fn parse_pubkey(pubkey_hex: &str) -> Result<VerifyingKey, ProtoError> {
    let raw: [u8; 32] = hex::decode(pubkey_hex)
        .map_err(|_| ProtoError::BadHex("pubkey"))?
        .try_into()
        .map_err(|_| ProtoError::BadHex("pubkey"))?;
    VerifyingKey::from_bytes(&raw).map_err(|_| ProtoError::BadHex("pubkey"))
}

fn verify_sig(pubkey_hex: &str, msg: &[u8], sig_hex: &str) -> Result<(), ProtoError> {
    let vk = parse_pubkey(pubkey_hex)?;
    let raw: [u8; 64] = hex::decode(sig_hex)
        .map_err(|_| ProtoError::BadHex("sig"))?
        .try_into()
        .map_err(|_| ProtoError::BadHex("sig"))?;
    vk.verify(msg, &Signature::from_bytes(&raw))
        .map_err(|_| ProtoError::BadSignature)
}

pub fn pubkey_hex(sk: &SigningKey) -> String {
    hex::encode(sk.verifying_key().to_bytes())
}

pub fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

/// What a public seat signs to join an Oracle (`GET /public/register/challenge`, then `POST /public/register`): the
/// one-time nonce bound to the key that will hold the seat. The server (publicseats.rs) and `ox join` both use this.
pub fn register_challenge_message(nonce: &str, pubkey_hex: &str) -> Vec<u8> {
    format!("oracle-register/1\n{nonce}\n{}", pubkey_hex.to_lowercase()).into_bytes()
}

pub fn generate_key() -> SigningKey {
    use rand::RngCore;
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    SigningKey::from_bytes(&seed)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_moment_is_milliseconds_seconds_a_date_through_its_end_or_a_date_and_time() {
        assert_eq!(super::parse_when("1793664000000"), Some(1_793_664_000_000));
        assert_eq!(super::parse_when("1793664000"), Some(1_793_664_000_000), "seconds read as seconds");
        assert_eq!(super::parse_when("2026-11-03"), Some(super::parse_when("2026-11-04T00:00").unwrap() - 1), "a date is through its last millisecond");
        assert_eq!(super::iso_ms(super::parse_when("2026-11-03T15:30Z").unwrap()), "2026-11-03T15:30:00.000Z");
        assert_eq!(super::iso_ms(super::parse_when("2024-02-29T00:00:05").unwrap()), "2024-02-29T00:00:05.000Z", "a leap day");
        for bad in ["", "soon", "2026-13-01", "2026-11-03T25:00", "2026-11", "-5"] { assert_eq!(super::parse_when(bad), None, "{bad}"); }
    }

    use super::*;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }
    const NOW: i64 = 1_800_000_000_000;

    fn env(content: &str) -> Envelope {
        Envelope::sign(
            &key(),
            "room",
            "rocky",
            Kind::Say,
            content,
            None,
            vec![],
            NOW,
        )
    }

    #[test]
    fn roundtrip_hard_content() {
        for c in [
            "hello",
            "line one\nline two",
            "ends\n",
            "carriage\r\nreturn",
            "café résumé — ✓",
            "buzz 🐝 poke 👉",
            " ",
            &"x".repeat(60_000),
        ] {
            let e = env(c);
            assert_eq!(
                e.verify(&pubkey_hex(&key()), NOW),
                Ok(()),
                "content {:?}",
                &c[..c.len().min(12)]
            );
        }
    }

    #[test]
    fn golden_vector_file_is_what_this_crate_signs() {
        // testdata/say-v2-golden.json is the shared vector other implementations (the AXXIS web door)
        // test against. It is only a vector if THIS crate, the one canonical signer, agrees with it.
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/say-v2-golden.json")).unwrap();
        let e: Envelope = serde_json::from_value(v["envelope"].clone()).unwrap();
        assert_eq!(String::from_utf8(e.signing_bytes()).unwrap(), v["signing_bytes"].as_str().unwrap());
        let seed: [u8; 32] = hex::decode(v["seed_hex"].as_str().unwrap()).unwrap().try_into().unwrap();
        let sk = SigningKey::from_bytes(&seed);
        assert_eq!(pubkey_hex(&sk), v["pubkey_hex"].as_str().unwrap());
        assert_eq!(hex::encode(sk.sign(&e.signing_bytes()).to_bytes()), e.sig);
        assert_eq!(e.verify(v["pubkey_hex"].as_str().unwrap(), e.ts_ms), Ok(()));
    }

    #[test]
    fn signing_bytes_are_stable() {
        // Golden vector: any change to canonicalisation must be a deliberate, versioned change.
        let mut e = env("hello");
        e.nonce = "00".repeat(16);
        e.client_op = "op-1".into();
        let bytes = String::from_utf8(e.signing_bytes()).unwrap();
        assert_eq!(
            bytes,
            format!(
                "oracle-say/2\nroom\nrocky\nsay\n{NOW}\n{}\nop-1\n{}\n{}\n{}",
                "00".repeat(16),
                "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            )
        );
    }

    #[test]
    fn tampering_any_covered_field_fails() {
        let pk = pubkey_hex(&key());
        let base = Envelope::sign(
            &key(),
            "room",
            "rocky",
            Kind::Say,
            "hi",
            Some("{\"a\":1}".into()),
            vec!["b".into(), "a".into()],
            NOW,
        );
        type Tamper = Box<dyn Fn(&mut Envelope)>;
        let cases: Vec<Tamper> = vec![
            Box::new(|e| e.channel = "other".into()),
            Box::new(|e| e.author = "kin".into()),
            Box::new(|e| e.kind = Kind::State),
            Box::new(|e| e.ts_ms += 1),
            Box::new(|e| e.nonce = "11".repeat(16)),
            Box::new(|e| e.client_op = "x".into()),
            Box::new(|e| e.content = "hi!".into()),
            Box::new(|e| e.cargo = Some("{\"a\":2}".into())),
            Box::new(|e| e.tags.push("c".into())),
        ];
        for (i, f) in cases.iter().enumerate() {
            let mut e = base.clone();
            f(&mut e);
            assert_eq!(
                e.verify(&pk, NOW),
                Err(ProtoError::BadSignature),
                "case {i}"
            );
        }
        // tag ORDER is not significant
        let mut e = base.clone();
        e.tags.reverse();
        assert_eq!(e.verify(&pk, NOW), Ok(()));
    }

    #[test]
    fn wrong_key_and_stale_are_rejected() {
        let e = env("hi");
        let other = pubkey_hex(&SigningKey::from_bytes(&[9u8; 32]));
        assert_eq!(e.verify(&other, NOW), Err(ProtoError::BadSignature));
        assert_eq!(
            e.verify(&pubkey_hex(&key()), NOW + MAX_SKEW_MS + 1),
            Err(ProtoError::Stale)
        );
        assert_eq!(
            e.verify(&pubkey_hex(&key()), NOW - MAX_SKEW_MS - 1),
            Err(ProtoError::Stale)
        );
    }

    #[test]
    fn resign_keeps_client_op_and_verifies_at_the_new_time() {
        let e = env("queued while offline");
        let later = NOW + 3 * 60 * 60 * 1000;
        assert_eq!(e.verify(&pubkey_hex(&key()), later), Err(ProtoError::Stale));
        let r = e.clone().resign(&key(), later);
        assert_eq!(r.client_op, e.client_op);
        assert_ne!(r.nonce, e.nonce);
        assert_eq!(r.verify(&pubkey_hex(&key()), later), Ok(()));
    }

    #[test]
    fn structural_validation() {
        let mut e = env("hi");
        e.author = "Rocky!".into();
        assert_eq!(e.validate(), Err(ProtoError::Invalid("author")));
        let mut e = env("hi");
        e.nonce = "abcd".into();
        assert_eq!(e.validate(), Err(ProtoError::Invalid("nonce")));
        let mut e = env("hi");
        e.content = String::new();
        assert_eq!(e.validate(), Err(ProtoError::Invalid("content")));
    }

    #[test]
    fn a_tag_or_client_op_with_a_control_character_is_refused_so_signing_bytes_stay_injective() {
        // The mutant from Ramanujan's N2 audit: one tag holding a newline signs exactly what two tags sign.
        let two = Envelope::sign(&key(), "room", "rocky", Kind::Say, "hi", None, vec!["a".into(), "b".into()], NOW);
        let mut one = two.clone();
        one.tags = vec!["a\nb".into()];
        assert_eq!(one.signing_bytes(), two.signing_bytes(), "the framing alone cannot tell them apart");
        assert_eq!(one.validate(), Err(ProtoError::Invalid("tags")));
        assert!(one.verify_stored(&hex::encode(key().verifying_key().to_bytes())).is_err(), "nor can it pass as stored");
        assert_eq!(two.validate(), Ok(()));
        for bad in ["a\rb", "a\tb", "a\u{0}b", "a\u{7f}b", "a\u{85}b"] {
            let mut e = two.clone();
            e.tags = vec![bad.into()];
            assert_eq!(e.validate(), Err(ProtoError::Invalid("tags")), "{bad:?}");
        }
        // Negative control: a plain tag that merely differs is accepted and signs different bytes.
        let mut ab = two.clone();
        ab.tags = vec!["ab".into()];
        assert_eq!(ab.validate(), Ok(()));
        assert_ne!(ab.signing_bytes(), two.signing_bytes());
        let mut op = env("hi");
        op.client_op = "op\n0".into();
        assert_eq!(op.validate(), Err(ProtoError::Invalid("client_op")));
    }

    #[test]
    fn an_offline_write_keeps_its_authored_time_signed() {
        let e = env("written offline");
        // Without authored_ms the signed bytes are exactly the old ones: old servers and stored envelopes still verify.
        assert!(!String::from_utf8(e.signing_bytes()).unwrap().contains("authored:"));
        assert!(e.verify(&pubkey_hex(&key()), NOW).is_ok());
        assert_eq!(serde_json::to_value(&e).unwrap().get("authored_ms"), None, "absent on the wire too");
        // Delivered 20 minutes later: fresh ts_ms for the replay window, the first time kept and signed.
        let later = NOW + 20 * 60_000;
        let d = e.clone().resign_authored(&key(), later);
        assert_eq!((d.ts_ms, d.authored_ms, d.client_op.clone()), (later, Some(NOW), e.client_op.clone()));
        assert!(d.verify(&pubkey_hex(&key()), later).is_ok());
        // A second re-sign does not move it again.
        assert_eq!(d.clone().resign_authored(&key(), later + 60_000).authored_ms, Some(NOW));
        // It is covered by the signature: changing it breaks the envelope.
        let mut forged = d.clone();
        forged.authored_ms = Some(NOW - 86_400_000);
        assert!(forged.verify(&pubkey_hex(&key()), later).is_err());
        // It may not claim the future, nor more than 30 days back.
        let mut future = e.clone();
        future.authored_ms = Some(NOW + MAX_SKEW_MS + 1);
        assert!(matches!(future.resign(&key(), NOW).validate(), Err(ProtoError::Invalid("authored_ms"))));
        let mut ancient = e.clone();
        ancient.authored_ms = Some(NOW - MAX_AUTHORED_AGE_MS - 1);
        assert!(matches!(ancient.resign(&key(), NOW).validate(), Err(ProtoError::Invalid("authored_ms"))));
    }

    #[test]
    fn prev_is_signed_only_when_present() {
        let e = env("hello");
        assert_eq!(serde_json::to_value(&e).unwrap().get("prev"), None, "absent on the wire too");
        let old = String::from_utf8(e.signing_bytes()).unwrap();
        assert!(!old.contains("prev:"), "an unchained envelope signs exactly the old bytes");
        let c = e.clone().chain_to(GENESIS_PREV, &key(), NOW);
        assert_eq!(c.client_op, e.client_op, "chaining is a retry of the same write");
        assert!(String::from_utf8(c.signing_bytes()).unwrap().ends_with(&format!("\nprev:{GENESIS_PREV}")));
        assert_eq!(c.verify(&pubkey_hex(&key()), NOW), Ok(()));
        // Negative controls: a changed or removed prev no longer verifies; a malformed one is refused before any key.
        let mut moved = c.clone();
        moved.prev = Some(link_of(&e.sig));
        assert_eq!(moved.verify(&pubkey_hex(&key()), NOW), Err(ProtoError::BadSignature));
        let mut dropped = c.clone();
        dropped.prev = None;
        assert_eq!(dropped.verify(&pubkey_hex(&key()), NOW), Err(ProtoError::BadSignature));
        for bad in ["", "00", &GENESIS_PREV.to_uppercase().replace('0', "A"), &format!("{GENESIS_PREV}0")] {
            let mut b = c.clone();
            b.prev = Some(bad.to_string());
            assert_eq!(b.validate(), Err(ProtoError::Invalid("prev")), "{bad}");
        }
    }

    fn rec(e: Envelope, seq: i64) -> ChainRecord {
        let id = format!("OBS-room-20261020-{}", &link_of(&e.sig)[..12]);
        ChainRecord { seq, id, pubkey: pubkey_hex(&key()), envelope: e, withheld: false }
    }

    /// Four writes: one before chains existed, then three linked ones.
    fn chain() -> Vec<ChainRecord> {
        let mut out = vec![rec(env("before chains"), 1)];
        for (i, t) in ["one", "two", "three"].iter().enumerate() {
            let prev = link_of(&out.last().unwrap().envelope.sig);
            out.push(rec(env(t).chain_to(&prev, &key(), NOW), i as i64 + 2));
        }
        out
    }

    #[test]
    fn a_whole_chain_checks_and_every_edit_breaks_it() {
        let c = chain();
        let r = check_chain(&c);
        assert_eq!((r.records, r.chained, r.unchained, r.breaks.len()), (4, 3, 1, 0), "{r:?}");
        assert_eq!(r.first_chained.as_deref(), Some(c[1].id.as_str()));
        assert!(c[2].envelope.prev.as_deref().unwrap().starts_with(c[1].id.rsplit('-').next().unwrap()), "prev begins with the predecessor's id digest");
        // A face that starts mid-chain is not broken; one that starts at genesis says so.
        assert!(check_chain(&c[2..]).breaks.is_empty());
        let g = vec![rec(env("first").chain_to(GENESIS_PREV, &key(), NOW), 1)];
        assert!(check_chain(&g).from_genesis);
        // Negative controls: a removed record, a reordering, an edit, a wrong id, a stranger's record.
        let mut gap = c.clone();
        gap.remove(2);
        assert_eq!(check_chain(&gap).breaks, vec![ChainBreak { id: c[3].id.clone(), why: "prev is not the record before it".into() }]);
        let mut swapped = c.clone();
        swapped.swap(1, 2);
        assert_eq!(check_chain(&swapped).breaks.len(), 3, "{:?}", check_chain(&swapped));
        let mut edited = c.clone();
        edited[2].envelope.content = "two, edited".into();
        assert_eq!(check_chain(&edited).breaks[0].why, "bad signature");
        let mut renamed = c.clone();
        renamed[1].id = "OBS-room-20001020-000000000000".into();
        assert_eq!(check_chain(&renamed).breaks[0].why, "id does not match its signature");
        let mut mixed = c.clone();
        mixed[3].envelope.author = "mallory".into();
        assert!(check_chain(&mixed).breaks.iter().any(|b| b.why == "another author's record"));
    }

    #[test]
    fn key_event_roundtrip_and_tamper() {
        let owner = key();
        let new_pk = pubkey_hex(&SigningKey::from_bytes(&[3u8; 32]));
        let ev = KeyEvent::sign(&owner, "blaze", KeyAction::Add, "rocky", &new_pk, NOW);
        assert_eq!(ev.verify(&pubkey_hex(&owner), NOW), Ok(()));
        let mut bad = ev.clone();
        bad.handle = "kin".into();
        assert_eq!(
            bad.verify(&pubkey_hex(&owner), NOW),
            Err(ProtoError::BadSignature)
        );
        let mut bad = ev.clone();
        bad.pubkey = pubkey_hex(&SigningKey::from_bytes(&[4u8; 32]));
        assert_eq!(
            bad.verify(&pubkey_hex(&owner), NOW),
            Err(ProtoError::BadSignature)
        );
    }

    #[test]
    fn say_and_key_domains_cannot_be_confused() {
        let e = env("hi");
        assert!(e.signing_bytes().starts_with(b"oracle-say/2\n"));
        let ev = KeyEvent::sign(&key(), "blaze", KeyAction::Revoke, "rocky", "", NOW);
        assert!(ev.signing_bytes().starts_with(b"oracle-key/1\n"));
    }
}

/// The owner's local clock beside the Oracle clock (Blaze, 1 Nov 2026: "the Oracle needs a clock, a UTC clock plus a local
/// clock as well"). The offset is a setting, `±HH:MM` (ORACLE_LOCAL_OFFSET on the server), never a place: minutes east of
/// UTC, or None when unset or malformed, so a reader is never shown a wrong local time.
pub fn local_offset_minutes(setting: Option<&str>) -> Option<i32> {
    let s = setting?.trim();
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = rest.split_once(':')?;
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    (h <= 14 && m < 60 && h.to_string().len() <= 2 && m.to_string().len() <= 2).then_some(sign * (h * 60 + m))
}

/// The local clock as a reader needs it next to a UTC time: `10:50 (+05:30)`, or None without a valid offset.
pub fn local_clock(ms: i64, setting: Option<&str>) -> Option<String> {
    let off = local_offset_minutes(setting)?;
    let iso = iso_ms(ms + i64::from(off) * 60_000);
    let sign = if off < 0 { '-' } else { '+' };
    Some(format!("{} ({sign}{:02}:{:02})", &iso[11..16], off.abs() / 60, off.abs() % 60))
}

/// `local_clock` with the server's setting.
pub fn local_clock_env(ms: i64) -> Option<String> {
    local_clock(ms, std::env::var("ORACLE_LOCAL_OFFSET").ok().as_deref())
}

/// A moment written by a person or a tag (3 Nov 2026, facts valid in time): unix milliseconds (or seconds, under 10^11), or a
/// UTC date `2026-10-01`, or a date and time `2026-10-01T15:30[:05][Z]`. A date alone means through the end of that day (its
/// last millisecond): "valid until 1 Oct" holds on 1 Oct, "as of 1 Oct" sees what was written that day. None when it is
/// none of these.
pub fn parse_when(s: &str) -> Option<i64> {
    let s = s.trim();
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        let n: i64 = s.parse().ok()?;
        return Some(if n < 100_000_000_000 { n * 1000 } else { n });
    }
    let (date, time) = match s.split_once('T') { Some((d, t)) => (d, Some(t)), None => (s, None) };
    let d: Vec<i64> = date.split('-').map(|x| x.parse::<i64>().ok()).collect::<Option<Vec<_>>>()?;
    let [y, mo, da] = d[..] else { return None };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || !(1..=9999).contains(&y) { return None; }
    let (y2, m2) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + (153 * m2 + 2) / 5 + da - 1;
    let day = (era * 146_097 + doe - 719_468) * 86_400_000;
    match time {
        None => Some(day + 86_400_000 - 1),
        Some(t) => {
            let t: Vec<i64> = t.trim_end_matches('Z').split(':').map(|x| x.parse::<i64>().ok()).collect::<Option<Vec<_>>>()?;
            let (h, mi, se) = match t[..] { [h, mi] => (h, mi, 0), [h, mi, se] => (h, mi, se), _ => return None };
            if !(0..24).contains(&h) || !(0..60).contains(&mi) || !(0..60).contains(&se) { return None; }
            Some(day + h * 3_600_000 + mi * 60_000 + se * 1000)
        }
    }
}

/// Milliseconds since the epoch as ISO-8601 UTC with millisecond precision: `2026-09-20T18:21:07.412Z`.
/// Written out by hand so the wire format needs no date library (days-to-civil, Howard Hinnant).
/// The wall clock in unix milliseconds: the one clock the server, the MCP door and ox share (code audit, 4 Nov 2026: it
/// was written four times, and the store reached into the HTTP layer for its copy). A clock before 1970 reads as 0.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn iso_ms(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (
        doy - (153 * mp + 2) / 5 + 1,
        if mp < 10 { mp + 3 } else { mp - 9 },
    );
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

#[cfg(test)]
mod iso_tests {
    use super::iso_ms;
    #[test]
    fn known_instants() {
        assert_eq!(iso_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_ms(951_782_400_123), "2000-02-29T00:00:00.123Z"); // leap day
        assert_eq!(iso_ms(1_789_928_467_412), "2026-09-20T18:21:07.412Z");
        assert_eq!(iso_ms(-1), "1969-12-31T23:59:59.999Z");
    }

}

#[cfg(test)]
mod local_clock_tests {
    use super::{local_clock, local_offset_minutes};

    #[test]
    fn the_offset_is_a_setting_read_strictly() {
        assert_eq!(local_offset_minutes(Some("+05:30")), Some(330));
        assert_eq!(local_offset_minutes(Some("-04:00")), Some(-240));
        assert_eq!(local_offset_minutes(Some(" +00:00 ")), Some(0));
        for bad in [None, Some(""), Some("05:30"), Some("+5"), Some("+25:00"), Some("+05:60"), Some("IST"), Some("+0530")] {
            assert_eq!(local_offset_minutes(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_local_clock_sits_beside_utc_and_wraps_the_day() {
        let t = 1_793_510_400_000; // 2026-11-01T05:20:00Z
        assert_eq!(local_clock(t, Some("+05:30")).as_deref(), Some("10:50 (+05:30)"));
        assert_eq!(local_clock(t, Some("-08:00")).as_deref(), Some("21:20 (-08:00)"));
        assert_eq!(local_clock(t + 20 * 3_600_000, Some("+05:30")).as_deref(), Some("06:50 (+05:30)"));
        assert_eq!(local_clock(t, Some("nowhere")), None);
    }
}
