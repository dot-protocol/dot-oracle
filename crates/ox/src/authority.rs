//! `ox authority sign-request`: this seat signs one DOT authority Request v1 (dot-terminal crates/authority) with its
//! own key, so a DOT broker (the dot-iris rail) can verify who sent a browser command against an owner's grant whose
//! subject is this seat's public key (Iris grant contract, #dot).
//!
//! Only this one message type is signed. The bytes are the authority crate's own: the domain
//! `DOT-AUTH-REQUEST-ED25519-V1\0`, then the typed request re-encoded as JSON in its fixed field order. The input is
//! decoded into that type first (unknown fields refused), never signed as given, so a caller cannot make the seat
//! sign anything else. Oracle's own envelopes are signed over different bytes; the domain keeps the two apart.
use anyhow::{bail, Result};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Read;

pub const REQUEST_DOMAIN: &[u8] = b"DOT-AUTH-REQUEST-ED25519-V1\0";

/// dot-terminal `authority::Request`, field for field and in the same order (the order is part of the signed bytes).
#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u8,
    pub grant_id: [u8; 32],
    pub run: [u8; 32],
    pub nonce: [u8; 32],
    pub audience: String,
    pub resource: String,
    pub action: String,
    pub body_sha256: [u8; 32],
}

/// The authority crate's `valid_text`: what a grant's audience, resource and action may be.
fn valid_text(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
}

pub fn request_message(r: &Request) -> Result<Vec<u8>> {
    let mut bytes = REQUEST_DOMAIN.to_vec();
    bytes.extend(serde_json::to_vec(r)?);
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `{"subject": <public key hex>, "signature": <hex>}` for one Request v1 given as JSON.
/// The most a request may be: one cap for stdin and files alike.
pub const MAX_INPUT: u64 = 64 * 1024;

/// Read at most MAX_INPUT bytes and refuse anything longer, so nothing past the cap can hide behind a valid request
/// (Emmy, PR 113 review: a take() alone signs the first 64 KiB and silently drops the rest).
pub fn read_bounded(r: impl Read) -> Result<String> {
    let mut buf = Vec::new();
    r.take(MAX_INPUT + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_INPUT {
        bail!("a request is at most {MAX_INPUT} bytes; this input is longer, nothing was signed");
    }
    Ok(String::from_utf8(buf)?)
}

pub fn sign_request(key: &SigningKey, input: &str) -> Result<Value> {
    let r: Request = serde_json::from_str(input)
        .map_err(|e| anyhow::anyhow!("not a DOT authority Request v1: {e}"))?;
    if r.version != 1 {
        bail!("only Request version 1 is signed");
    }
    if ![&r.audience, &r.resource, &r.action]
        .iter()
        .all(|s| valid_text(s))
    {
        bail!("audience, resource and action are 1 to 256 characters with no control characters");
    }
    let sig = key.sign(&request_message(&r)?);
    Ok(json!({"subject": hex(key.verifying_key().as_bytes()), "signature": hex(&sig.to_bytes())}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier};

    fn request() -> Request {
        Request {
            version: 1,
            grant_id: [1; 32],
            run: [2; 32],
            nonce: [3; 32],
            audience: "dot-iris/mac".into(),
            resource: "https://example.com:443".into(),
            action: "read".into(),
            body_sha256: [4; 32],
        }
    }

    #[test]
    fn signs_the_authority_crates_exact_bytes() {
        // The bytes dot-terminal authority::request_message produces for this request: the domain, then the
        // fields in declaration order with byte arrays as JSON number arrays. A change here breaks every broker.
        let arr = |n: u8| format!("[{}]", vec![n.to_string(); 32].join(","));
        let want = format!(
            "DOT-AUTH-REQUEST-ED25519-V1\0{{\"version\":1,\"grant_id\":{},\"run\":{},\"nonce\":{},\"audience\":\"dot-iris/mac\",\"resource\":\"https://example.com:443\",\"action\":\"read\",\"body_sha256\":{}}}",
            arr(1), arr(2), arr(3), arr(4)
        );
        assert_eq!(request_message(&request()).unwrap(), want.as_bytes());
        let key = SigningKey::from_bytes(&[7; 32]);
        let out = sign_request(&key, &serde_json::to_string(&request()).unwrap()).unwrap();
        let sig: [u8; 64] = (0..64)
            .map(|i| {
                u8::from_str_radix(&out["signature"].as_str().unwrap()[2 * i..2 * i + 2], 16)
                    .unwrap()
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        key.verifying_key()
            .verify(want.as_bytes(), &Signature::from_bytes(&sig))
            .unwrap();
        assert_eq!(out["subject"], hex(key.verifying_key().as_bytes()));
    }

    #[test]
    fn input_past_the_cap_is_refused_not_cut() {
        let at_cap = " ".repeat(MAX_INPUT as usize);
        assert_eq!(
            read_bounded(at_cap.as_bytes()).unwrap().len(),
            MAX_INPUT as usize
        );
        // a valid request padded to the cap with a suffix after it: refused whole, never signed as its first part
        let over = format!("{}{}x", "{}", " ".repeat(MAX_INPUT as usize - 2));
        assert!(read_bounded(over.as_bytes()).is_err());
    }

    #[test]
    fn signs_nothing_but_a_request_v1() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let mut v = serde_json::to_value(request()).unwrap();
        v["extra"] = json!(1);
        assert!(sign_request(&key, &v.to_string()).is_err(), "unknown field");
        let mut v = serde_json::to_value(request()).unwrap();
        v["version"] = json!(2);
        assert!(sign_request(&key, &v.to_string()).is_err(), "version 2");
        for (field, bad) in [("action", ""), ("resource", "a\nb")] {
            let mut v = serde_json::to_value(request()).unwrap();
            v[field] = json!(bad);
            assert!(sign_request(&key, &v.to_string()).is_err(), "{field}");
        }
        assert!(
            sign_request(&key, r#"{"room":"dot","text":"hello"}"#).is_err(),
            "an Oracle message"
        );
        let mut v = serde_json::to_value(request()).unwrap();
        v["nonce"] = json!([1, 2, 3]);
        assert!(sign_request(&key, &v.to_string()).is_err(), "short nonce");
    }
}
