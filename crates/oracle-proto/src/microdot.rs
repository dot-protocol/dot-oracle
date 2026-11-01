//! Microdot: a signed, chained record as a picture (`docs/MICRODOT.md`).
//!
//! Law one is the document; the Python reference (`tools/microdot/mdot.py`) and this module read the same
//! bytes to the same `dot_id`s (the vectors under `tools/microdot/vectors/` are the gate). A dot is its canonical
//! bytes: a 160-byte header, a 64-byte Ed25519 seal over the header, and the zlib-compressed body. A face is a
//! picture that regenerates those bytes; it is read in any of the eight symmetries of its square, and the seal
//! says which way is up. A v4 composite names its components by `dot_id` and places each tile around its own,
//! turned by the relation (the case marker); a tile turned the wrong way is a broken sentence and is refused.
//!
//! Minting here and minting in Python give the same header and seal; the compressed body may differ between
//! deflate implementations, so a dot minted on one side is verified, not re-minted, on the other (the
//! compression-malleability debt noted in the document; v4.1 hashes what is sealed).

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

/// Pixels per module in the reference face.
pub const MODULE: usize = 4;
/// Border width in modules (the finder).
pub const BORDER: usize = 4;
/// The border blue.
pub const BLUE: [u8; 3] = [77, 91, 206];
pub const HEADER_LEN: usize = 160;
pub const SEAL_LEN: usize = 64;
pub const PREFIX_LEN: usize = HEADER_LEN + SEAL_LEN;
/// v5 (docs/MICRODOT.md §7): a 152-byte semantic header, a 64-byte seal, then a transport wrapper
/// `enc:u8 comp:u32 stream` that is neither sealed nor hashed.
pub const HEADER5_LEN: usize = 152;
pub const PREFIX5_LEN: usize = HEADER5_LEN + SEAL_LEN;
pub const WRAP5_LEN: usize = PREFIX5_LEN + 5;
pub const SEAL5_TAG: &[u8] = b"MDOT5-SEAL\0";
pub const ID5_TAG: &[u8] = b"MDOT5-ID\0";
pub const MAX_ORIG: usize = 16 << 20;
pub const ZLIB_SLACK: usize = 1024;
pub const MAX_COMPONENTS: usize = 256;
pub const MAX_PIXELS: usize = 64 << 20;
pub const MAX_DIM: usize = 1 << 15;
pub const MAX_MODULES_SIDE: usize = 4096;
pub const PREVKIND_NAMES: [&str; 3] = ["none", "microdot", "oracle"];
pub const CLAIM_MARK: &[u8] = b"---mdot-claim/1\n";
pub const CLAIM_END: &[u8] = b"\n---\n";
pub const CLAIM_KEYS: [&str; 10] = [
    "evidence", "status", "scope", "harness", "source", "corpus", "query", "date", "coverage", "exclusions",
];
pub const EVIDENCE: [&str; 4] = ["recompute", "bench", "absent", "none"];
pub const STATUS: [&str; 4] = ["claim", "verified", "refuted", "superseded"];
pub const REL_NAMES: [&str; 8] = [
    "root",
    "reply",
    "supersedes",
    "cites",
    "part-of",
    "is-same-as",
    "depends-on",
    "about",
];
/// A symmetry of the square: (quarter turns clockwise, mirrored left-right first).
pub type Sym = (u8, bool);
/// A v4 layout: (rel, dot_id) per component, in order.
pub type Layout = Vec<(u8, [u8; 32])>;
/// The eight symmetries, in the reference's order.
pub const SYM: [Sym; 8] = [
    (0, false),
    (1, false),
    (2, false),
    (3, false),
    (0, true),
    (1, true),
    (2, true),
    (3, true),
];
/// The case marker of each relation 0..7.
pub const REL_SYM: [Sym; 8] = SYM;

#[derive(Debug, thiserror::Error)]
pub enum MicrodotError {
    #[error("no MDOT header")]
    NoHeader,
    #[error("length {0} != 224+comp {1}")]
    Length(usize, u32),
    #[error("seal does not verify")]
    Seal,
    #[error("body does not inflate")]
    Inflate,
    #[error("what != sha256(body)")]
    What,
    #[error("layout malformed")]
    Layout,
    #[error("component is not the one the layout names")]
    Component,
    #[error("bad png: {0}")]
    Png(&'static str),
    /// v5: the first failing part of the ordered predicate, by name (docs/MICRODOT.md §7), with a detail.
    #[error("{0}: {1}")]
    Part(&'static str, String),
}

impl MicrodotError {
    /// The part's name for a v5 refusal (`magic`, `ver`, …, `claim`), or the legacy message.
    pub fn part(&self) -> String {
        match self {
            MicrodotError::Part(p, _) => (*p).to_string(),
            other => other.to_string(),
        }
    }
}

/// A v5 claim's signed front matter (keys from CLAIM_KEYS, in that order).
pub type Claim = Vec<(String, String)>;

/// One parsed, verified dot.
#[derive(Debug, Clone)]
pub struct Dot {
    pub dot_id: [u8; 32],
    pub ver: u16,
    pub flags: u16,
    pub suite: u8,
    pub when_ms: u64,
    pub orig: u32,
    pub comp: u32,
    pub what: [u8; 32],
    pub prev: [u8; 32],
    pub who: [u8; 32],
    pub next: [u8; 32],
    pub body: Vec<u8>,
    /// A v4 composite's layout: (rel, dot_id) per component.
    pub layout: Option<Layout>,
    pub markdown: Option<String>,
    pub canon: Vec<u8>,
    /// v5 only: the predecessor kind (0 none, 1 microdot id, 2 Oracle chain head), the transport encoding
    /// (0 raw, 1 zlib) and the claim front matter when the body carries the marker.
    pub prevkind: u8,
    pub enc: u8,
    pub claim: Option<Claim>,
}

impl Dot {
    pub fn id_hex(&self) -> String {
        hex::encode(self.dot_id)
    }
}

pub fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

fn zlib(body: &[u8]) -> Vec<u8> {
    let mut e = ZlibEncoder::new(Vec::new(), Compression::best());
    e.write_all(body).expect("zlib into memory");
    e.finish().expect("zlib into memory")
}

fn inflate(comp: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    ZlibDecoder::new(comp).read_to_end(&mut out).ok()?;
    Some(out)
}

/// A dot's canonical bytes: header + seal + zlib(body). `suite` is 0 (Ed25519 + sha256 + zlib).
pub fn pack(
    seed: &[u8; 32],
    body: &[u8],
    when_ms: u64,
    prev: [u8; 32],
    next: [u8; 32],
    ver: u16,
    flags: u16,
) -> Vec<u8> {
    let comp = zlib(body);
    let key = SigningKey::from_bytes(seed);
    let mut h = Vec::with_capacity(HEADER_LEN);
    h.extend_from_slice(b"MDOT");
    h.extend_from_slice(&ver.to_be_bytes());
    h.extend_from_slice(&flags.to_be_bytes());
    h.push(0); // suite
    h.push(0); // pad
    h.extend_from_slice(&when_ms.to_be_bytes());
    h.extend_from_slice(&(body.len() as u32).to_be_bytes());
    h.extend_from_slice(&(comp.len() as u32).to_be_bytes());
    h.extend_from_slice(&sha256(body));
    h.extend_from_slice(&prev);
    h.extend_from_slice(&[0u8; 6]);
    h.extend_from_slice(&key.verifying_key().to_bytes());
    h.extend_from_slice(&next);
    debug_assert_eq!(h.len(), HEADER_LEN);
    let seal = key.sign(&h).to_bytes();
    let mut c = h;
    c.extend_from_slice(&seal);
    c.extend_from_slice(&comp);
    c
}

/// A plain v3 dot.
pub fn pack_v3(seed: &[u8; 32], body: &[u8], when_ms: u64, prev: [u8; 32]) -> Vec<u8> {
    pack(seed, body, when_ms, prev, [0u8; 32], 3, 0)
}

fn arr32(b: &[u8]) -> [u8; 32] {
    let mut a = [0u8; 32];
    a.copy_from_slice(b);
    a
}

/// Canonical bytes -> a verified dot: the seal over the header, `what` against the inflated body, the lengths.
pub fn parse(c: &[u8]) -> Result<Dot, MicrodotError> {
    parse_v34(c, true)
}

fn parse_v34(c: &[u8], seal: bool) -> Result<Dot, MicrodotError> {
    if c.len() < PREFIX_LEN || &c[..4] != b"MDOT" {
        return Err(MicrodotError::NoHeader);
    }
    let h = &c[..HEADER_LEN];
    let ver = u16::from_be_bytes([h[4], h[5]]);
    let flags = u16::from_be_bytes([h[6], h[7]]);
    let suite = h[8];
    let when_ms = u64::from_be_bytes(h[10..18].try_into().expect("8 bytes"));
    let orig = u32::from_be_bytes(h[18..22].try_into().expect("4 bytes"));
    let comp = u32::from_be_bytes(h[22..26].try_into().expect("4 bytes"));
    if c.len() != PREFIX_LEN + comp as usize {
        return Err(MicrodotError::Length(c.len(), comp));
    }
    let what = arr32(&h[26..58]);
    let prev = arr32(&h[58..90]);
    let who = arr32(&h[96..128]);
    let next = arr32(&h[128..160]);
    if seal {
        let vk = VerifyingKey::from_bytes(&who).map_err(|_| MicrodotError::Seal)?;
        let sig = Signature::from_bytes(c[HEADER_LEN..PREFIX_LEN].try_into().expect("64 bytes"));
        vk.verify(h, &sig).map_err(|_| MicrodotError::Seal)?;
    }
    let body = inflate(&c[PREFIX_LEN..]).ok_or(MicrodotError::Inflate)?;
    if body.len() != orig as usize || sha256(&body) != what {
        return Err(MicrodotError::What);
    }
    let (layout, markdown) = if ver >= 4 && flags & 1 == 1 {
        let (l, m) = parse_layout(&body).ok_or(MicrodotError::Layout)?;
        (Some(l), Some(m))
    } else {
        (None, None)
    };
    Ok(Dot {
        dot_id: sha256(c),
        ver,
        flags,
        suite,
        when_ms,
        orig,
        comp,
        what,
        prev,
        who,
        next,
        body,
        layout,
        markdown,
        canon: c.to_vec(),
        prevkind: 0,
        enc: 1,
        claim: None,
    })
}

/// Stored bytes of any version -> the dot, by its own version's rule: v5 through `parse_v5`, v3/v4 as before.
/// `magic` and an unknown `ver` are refused by name.
pub fn parse_any(c: &[u8]) -> Result<Dot, MicrodotError> {
    if c.len() < 6 || &c[..4] != b"MDOT" {
        return Err(MicrodotError::Part("magic", String::new()));
    }
    match u16::from_be_bytes([c[4], c[5]]) {
        5 => parse_v5(c),
        3 | 4 => parse(c),
        v => Err(MicrodotError::Part("ver", v.to_string())),
    }
}

/// A dot made elsewhere, for import (docs/MICRODOT.md §8; Noor, 31 Oct 2026): the dot and whether its seal holds. The body
/// must still match its `what` hash and the header its layout; only a failed seal is reported instead of refused, so a test
/// encoder's dot or a demo key's dot can enter Oracle saying exactly what it is. v5 dots are read as `parse_any` reads them.
pub fn parse_reporting_seal(c: &[u8]) -> Result<(Dot, bool), MicrodotError> {
    match parse_any(c) {
        Ok(d) => Ok((d, true)),
        Err(MicrodotError::Seal) if c.len() >= 6 && matches!(u16::from_be_bytes([c[4], c[5]]), 3 | 4) => Ok((parse_v34(c, false)?, false)),
        Err(e) => Err(e),
    }
}

/// The id of stored bytes by their own version's rule; only a dot that parses has an id.
pub fn dot_id_of(c: &[u8]) -> Result<[u8; 32], MicrodotError> {
    Ok(parse_any(c)?.dot_id)
}

// ---------------------------------------------------------------- v5: semantic bytes, transport bytes, claims

fn zlib_level(body: &[u8], level: u32) -> Vec<u8> {
    let mut e = ZlibEncoder::new(Vec::new(), Compression::new(level));
    e.write_all(body).expect("zlib into memory");
    e.finish().expect("zlib into memory")
}

/// Front matter + markdown -> a claim body. Keys must be from CLAIM_KEYS; the caller's values are its case.
pub fn claim_text(fields: &[(&str, &str)], markdown: &str) -> Result<Vec<u8>, MicrodotError> {
    for (i, (k, _)) in fields.iter().enumerate() {
        if !CLAIM_KEYS.contains(k) {
            return Err(MicrodotError::Part("claim", format!("unknown claim key {k}")));
        }
        if fields[..i].iter().any(|(fk, _)| fk == k) {
            return Err(MicrodotError::Part("claim", format!("duplicate key {k}")));
        }
    }
    let mut out = CLAIM_MARK.to_vec();
    for key in CLAIM_KEYS {
        if let Some((_, v)) = fields.iter().find(|(k, _)| *k == key) {
            out.extend_from_slice(format!("{key}: {v}\n").as_bytes());
        }
    }
    out.extend_from_slice(b"---\n");
    out.extend_from_slice(markdown.as_bytes());
    Ok(out)
}

fn status_bits(status: &str) -> u16 {
    STATUS.iter().position(|s| *s == status).unwrap_or(0) as u16
}

/// Body bytes -> (claim or None, markdown). A body without the marker is plain (and must have inflection 0).
pub fn parse_claim(body: &[u8], flags: u16) -> Result<(Option<Claim>, String), MicrodotError> {
    let claim = |d: String| MicrodotError::Part("claim", d);
    if !body.starts_with(CLAIM_MARK) {
        if (flags >> 1) & 3 != 0 {
            return Err(claim("inflection bits without a claim marker".into()));
        }
        return Ok((None, String::from_utf8_lossy(body).into_owned()));
    }
    let rest = &body[CLAIM_MARK.len() - 1..]; // keep the marker's newline so "\n---\n" can close an empty front matter
    let end = rest
        .windows(CLAIM_END.len())
        .position(|w| w == CLAIM_END)
        .ok_or_else(|| claim("no closing ---".into()))?;
    let fm = &rest[1..=end];
    if fm.contains(&b'\r') {
        return Err(claim("CR in front matter".into()));
    }
    let mut lines: Vec<&[u8]> = fm.split(|b| *b == b'\n').collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.len() > 16 {
        return Err(claim("more than 16 lines".into()));
    }
    let mut fields: Claim = Vec::new();
    for ln in lines {
        let sep = ln.windows(2).position(|w| w == b": ").ok_or_else(|| claim("line is not key: value".into()))?;
        let k = std::str::from_utf8(&ln[..sep]).map_err(|_| claim("key is not ascii".into()))?;
        if !CLAIM_KEYS.contains(&k) {
            return Err(claim(format!("unknown key {k}")));
        }
        if fields.iter().any(|(fk, _)| fk == k) {
            return Err(claim(format!("duplicate key {k}")));
        }
        let v = &ln[sep + 2..];
        if v.len() > 1024 || v.iter().any(|b| *b < 32 || *b == 0x7f) {
            return Err(claim(format!("bad value for {k}")));
        }
        let val = std::str::from_utf8(v).map_err(|_| claim(format!("bad utf-8 in {k}")))?;
        if val.chars().any(|ch| ch.is_control() || ch == '\u{2028}' || ch == '\u{2029}') {
            return Err(claim(format!("control character in {k}")));
        }
        fields.push((k.to_string(), val.to_string()));
    }
    let get = |k: &str| fields.iter().find(|(fk, _)| fk == k).map(|(_, v)| v.as_str());
    for k in ["evidence", "status", "scope"] {
        if get(k).is_none() {
            return Err(claim(format!("missing {k}")));
        }
    }
    let evidence = get("evidence").unwrap_or("");
    let status = get("status").unwrap_or("");
    if !EVIDENCE.contains(&evidence) {
        return Err(claim("evidence not in table".into()));
    }
    if !STATUS.contains(&status) {
        return Err(claim("status not in table".into()));
    }
    let needs: &[&str] = match evidence {
        "recompute" | "bench" => &["harness", "source"],
        "absent" => &["corpus", "query", "date"],
        _ => &[],
    };
    for k in needs {
        if get(k).is_none() {
            return Err(claim(format!("{evidence} needs {k}")));
        }
    }
    if (flags >> 1) & 3 != status_bits(status) {
        return Err(claim("inflection bits disagree with status".into()));
    }
    let md = String::from_utf8_lossy(&rest[end + CLAIM_END.len()..]).into_owned();
    Ok((Some(fields), md))
}

/// A v5 dot's stored bytes: header(152) + seal(64) + enc(1) + comp(4) + stream. `claim`, when given, is
/// written as front matter and sets the inflection bits from its status. `force_raw` skips zlib; otherwise zlib
/// is kept only when `comp <= orig + ZLIB_SLACK`.
#[allow(clippy::too_many_arguments)]
pub fn pack_v5(
    seed: &[u8; 32],
    body: &[u8],
    when_ms: u64,
    prev: [u8; 32],
    prevkind: u8,
    next: [u8; 32],
    composite: bool,
    claim: Option<&[(&str, &str)]>,
    level: u32,
    force_raw: bool,
) -> Result<Vec<u8>, MicrodotError> {
    let mut flags: u16 = if composite { 1 } else { 0 };
    let mut body = body.to_vec();
    if let Some(fields) = claim {
        let status = fields.iter().find(|(k, _)| *k == "status").map(|(_, v)| *v).unwrap_or("claim");
        flags |= status_bits(status) << 1;
        let utf8 = |b: &[u8]| -> Result<String, MicrodotError> {
            std::str::from_utf8(b).map(|t| t.to_string()).map_err(|_| MicrodotError::Part("claim", "markdown is not UTF-8".into()))
        };
        if composite {
            let (lay, _) = parse_layout(&body).ok_or(MicrodotError::Layout)?;
            let n = 6 + 33 * lay.len();
            let md = utf8(&body[n..])?;
            let mut b = body[..n].to_vec();
            b.extend_from_slice(&claim_text(fields, &md)?);
            body = b;
        } else {
            body = claim_text(fields, &utf8(&body)?)?;
        }
    }
    if body.len() > MAX_ORIG {
        return Err(MicrodotError::Part("limits", format!("orig {}", body.len())));
    }
    if prevkind == 0 && prev != [0u8; 32] {
        return Err(MicrodotError::Part("prevkind", "kind 0 needs a zero prev".into()));
    }
    let key = SigningKey::from_bytes(seed);
    let mut h = Vec::with_capacity(HEADER5_LEN);
    h.extend_from_slice(b"MDOT");
    h.extend_from_slice(&5u16.to_be_bytes());
    h.extend_from_slice(&flags.to_be_bytes());
    h.push(0); // suite
    h.push(prevkind);
    h.extend_from_slice(&when_ms.to_be_bytes());
    h.extend_from_slice(&(body.len() as u32).to_be_bytes());
    h.extend_from_slice(&sha256(&body));
    h.extend_from_slice(&prev);
    h.extend_from_slice(&[0u8; 2]);
    h.extend_from_slice(&key.verifying_key().to_bytes());
    h.extend_from_slice(&next);
    debug_assert_eq!(h.len(), HEADER5_LEN);
    let seal = key.sign(&[SEAL5_TAG, h.as_slice()].concat()).to_bytes();
    let stream = if force_raw { Vec::new() } else { zlib_level(&body, level) };
    let (enc, stream) = if !stream.is_empty() && stream.len() <= body.len() + ZLIB_SLACK { (1u8, stream) } else { (0u8, body.clone()) };
    let mut c = h;
    c.extend_from_slice(&seal);
    c.push(enc);
    c.extend_from_slice(&(stream.len() as u32).to_be_bytes());
    c.extend_from_slice(&stream);
    Ok(c)
}

/// The v5 id: sha256 over the domain tag, the semantic header and the seal; never over the stream.
pub fn dot_id_v5(c: &[u8]) -> [u8; 32] {
    sha256(&[ID5_TAG, &c[..PREFIX5_LEN]].concat())
}

/// A v5 dot's semantic canonical bytes: header + seal + body.
pub fn semantic_bytes(c: &[u8]) -> Result<Vec<u8>, MicrodotError> {
    let d = parse_v5(c)?;
    Ok([&c[..PREFIX5_LEN], d.body.as_slice()].concat())
}

/// Stored bytes of a v5 dot -> the dot, or the first failing part by name, in the order of docs/MICRODOT.md §7:
/// magic, ver, suite, flags, prevkind, pad, limits, seal, enc, comp, stream, inflate, orig, what, layout, claim.
pub fn parse_v5(c: &[u8]) -> Result<Dot, MicrodotError> {
    let fail = |p: &'static str, d: String| Err(MicrodotError::Part(p, d));
    if c.len() < WRAP5_LEN || &c[..4] != b"MDOT" {
        return fail("magic", String::new());
    }
    let h = &c[..HEADER5_LEN];
    let ver = u16::from_be_bytes([h[4], h[5]]);
    if ver != 5 {
        return fail("ver", ver.to_string());
    }
    let flags = u16::from_be_bytes([h[6], h[7]]);
    let suite = h[8];
    if suite != 0 {
        return fail("suite", suite.to_string());
    }
    if flags & !0x7 != 0 {
        return fail("flags", format!("{flags:#x}"));
    }
    let prevkind = h[9];
    let when_ms = u64::from_be_bytes(h[10..18].try_into().expect("8 bytes"));
    let orig = u32::from_be_bytes(h[18..22].try_into().expect("4 bytes"));
    let what = arr32(&h[22..54]);
    let prev = arr32(&h[54..86]);
    if prevkind > 2 || (prevkind == 0) != (prev == [0u8; 32]) {
        return fail("prevkind", prevkind.to_string());
    }
    if h[86..88] != [0, 0] {
        return fail("pad", String::new());
    }
    let who = arr32(&h[88..120]);
    let next = arr32(&h[120..152]);
    if orig as usize > MAX_ORIG {
        return fail("limits", format!("orig {orig}"));
    }
    let vk = VerifyingKey::from_bytes(&who).map_err(|_| MicrodotError::Part("seal", "who is not a key".into()))?;
    let sig = Signature::from_bytes(c[HEADER5_LEN..PREFIX5_LEN].try_into().expect("64 bytes"));
    if vk.verify(&[SEAL5_TAG, h].concat(), &sig).is_err() {
        return fail("seal", String::new());
    }
    let enc = c[PREFIX5_LEN];
    let comp = u32::from_be_bytes(c[PREFIX5_LEN + 1..WRAP5_LEN].try_into().expect("4 bytes"));
    if enc > 1 {
        return fail("enc", enc.to_string());
    }
    if enc == 0 && comp != orig {
        return fail("comp", format!("raw comp {comp} != orig {orig}"));
    }
    if enc == 1 && comp as usize > orig as usize + ZLIB_SLACK {
        return fail("comp", format!("zlib comp {comp} > orig + {ZLIB_SLACK}"));
    }
    if c.len() != WRAP5_LEN + comp as usize {
        return fail("stream", format!("{} bytes present, comp {comp}", c.len() - WRAP5_LEN));
    }
    let stream = &c[WRAP5_LEN..];
    let body = if enc == 1 {
        // inflate capped at orig + 1: a byte over the cap, an error, a short output or unused input refuse here
        let mut dec = ZlibDecoder::new(stream);
        let mut out = Vec::with_capacity(orig as usize + 1);
        let mut buf = [0u8; 8192];
        loop {
            let n = match dec.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => return fail("inflate", "zlib error".into()),
            };
            out.extend_from_slice(&buf[..n]);
            if out.len() > orig as usize {
                return fail("inflate", "over orig".into());
            }
        }
        if dec.total_in() as usize != stream.len() {
            return fail("inflate", "unused input inside comp".into());
        }
        out
    } else {
        stream.to_vec()
    };
    if body.len() != orig as usize {
        return fail("orig", format!("{} != {orig}", body.len()));
    }
    if sha256(&body) != what {
        return fail("what", String::new());
    }
    let (layout, text_start) = if flags & 1 == 1 {
        let (l, _) = parse_layout(&body).ok_or_else(|| MicrodotError::Part("layout", String::new()))?;
        if l.len() > MAX_COMPONENTS {
            return fail("layout", "too many components".into());
        }
        let n = 6 + 33 * l.len();
        (Some(l), n)
    } else {
        (None, 0)
    };
    let (claim, markdown) = parse_claim(&body[text_start..], flags)?;
    Ok(Dot {
        dot_id: dot_id_v5(c),
        ver,
        flags,
        suite,
        when_ms,
        orig,
        comp,
        what,
        prev,
        who,
        next,
        body,
        layout,
        markdown: Some(markdown),
        canon: c.to_vec(),
        prevkind,
        enc,
        claim,
    })
}

/// A v4 body: `MSET` n:u16 n × (rel:u8 dot_id[32]) markdown.
pub fn layout(components: &[(u8, [u8; 32])], markdown: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + components.len() * 33 + markdown.len());
    out.extend_from_slice(b"MSET");
    out.extend_from_slice(&(components.len() as u16).to_be_bytes());
    for (rel, id) in components {
        out.push(*rel);
        out.extend_from_slice(id);
    }
    out.extend_from_slice(markdown.as_bytes());
    out
}

pub fn parse_layout(body: &[u8]) -> Option<(Layout, String)> {
    if body.len() < 6 || &body[..4] != b"MSET" {
        return None;
    }
    let n = u16::from_be_bytes([body[4], body[5]]) as usize;
    let mut p = 6;
    let mut comps: Layout = Vec::with_capacity(n);
    for _ in 0..n {
        if p + 33 > body.len() {
            return None;
        }
        let rel = body[p];
        if rel > 7 {
            return None;
        }
        comps.push((rel, arr32(&body[p + 1..p + 33])));
        p += 33;
    }
    Some((comps, String::from_utf8_lossy(&body[p..]).into_owned()))
}

/// A composite's canonical bytes over its components (canonical bytes, in order).
/// Every component must parse under its own version's rule (v3, v4 or v5); a component that does not is refused
/// as `Component`, never hashed raw (Emmy, 21 Oct 2026: only a dot that parses has an id).
pub fn pack_composite(
    seed: &[u8; 32],
    components: &[(u8, &[u8])],
    markdown: &str,
    when_ms: u64,
    prev: [u8; 32],
) -> Result<Vec<u8>, MicrodotError> {
    let mut named: Layout = Vec::with_capacity(components.len());
    for (r, c) in components {
        if *r > 7 {
            return Err(MicrodotError::Layout);
        }
        named.push((*r, dot_id_of(c).map_err(|_| MicrodotError::Component)?));
    }
    Ok(pack(seed, &layout(&named, markdown), when_ms, prev, [0u8; 32], 4, 1))
}

// ---------------------------------------------------------------- PNG in and out

pub type Px = [u8; 3];
pub type Rows = Vec<Vec<Px>>;

fn crc32(data: &[u8]) -> u32 {
    let mut c: u32 = 0xffff_ffff;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 == 1 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut tc = Vec::with_capacity(4 + data.len());
    tc.extend_from_slice(kind);
    tc.extend_from_slice(data);
    out.extend_from_slice(&tc);
    out.extend_from_slice(&crc32(&tc).to_be_bytes());
}

/// RGB rows -> PNG bytes (8-bit RGB, filter 0, zlib best).
pub fn png_write(rows: &Rows, text: Option<&str>) -> Vec<u8> {
    let h = rows.len();
    let w = rows.first().map(|r| r.len()).unwrap_or(0);
    let mut raw = Vec::with_capacity(h * (1 + 3 * w));
    for row in rows {
        raw.push(0);
        for px in row {
            raw.extend_from_slice(px);
        }
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    if let Some(t) = text {
        let mut d = b"Comment\0".to_vec();
        d.extend(t.bytes().map(|b| if b.is_ascii() { b } else { b'?' }));
        chunk(&mut out, b"tEXt", &d);
    }
    chunk(&mut out, b"IDAT", &zlib(&raw));
    chunk(&mut out, b"IEND", b"");
    out
}

/// PNG bytes -> (w, h, RGB rows). 8-bit, filters 0 to 4, non-interlaced; grey and alpha channels are
/// folded to RGB. Camera colour is not tolerated: the blue is matched exactly, as in the reference.
/// Refuse a canvas before its rows exist: each side ≤ MAX_DIM and the product ≤ MAX_PIXELS.
pub fn canvas_ok(w: usize, h: usize) -> Result<(), MicrodotError> {
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM || w.checked_mul(h).is_none_or(|p| p > MAX_PIXELS) {
        return Err(MicrodotError::Png("dimensions"));
    }
    Ok(())
}

/// PNG bytes -> (w, h, RGB rows), bounded before anything is decompressed or allocated (docs/MICRODOT.md §7):
/// the signature, one 13-byte IHDR, 8-bit, colour types 0/2/4/6, no interlace, sides ≤ 32768 and ≤ 64 M pixels,
/// every chunk inside the file with its CRC, the IDAT total bounded, an IEND of length zero and nothing after it,
/// inflate capped at the exact raw length with no unused input, filters 0 to 4. Grey and alpha fold to RGB.
pub fn png_read(data: &[u8]) -> Result<(usize, usize, Rows), MicrodotError> {
    if data.len() < 33 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(MicrodotError::Png("signature"));
    }
    let (mut w, mut h, mut bpp, mut expect) = (0usize, 0usize, 0usize, 0usize);
    let mut have_ihdr = false;
    let mut idat: Vec<&[u8]> = Vec::new();
    let mut idat_len = 0usize;
    let mut seen_end = false;
    let mut i = 8;
    while i + 12 <= data.len() && !seen_end {
        let n = u32::from_be_bytes(data[i..i + 4].try_into().expect("4 bytes")) as usize;
        let kind = &data[i + 4..i + 8];
        if n > (1usize << 31) - 1 || i + 12 + n > data.len() {
            return Err(MicrodotError::Png("chunk length"));
        }
        let c = &data[i + 8..i + 8 + n];
        let crc = u32::from_be_bytes(data[i + 8 + n..i + 12 + n].try_into().expect("4 bytes"));
        if crc != crc32(&data[i + 4..i + 8 + n]) {
            return Err(MicrodotError::Png("crc"));
        }
        if kind == b"IHDR" {
            if have_ihdr || n != 13 {
                return Err(MicrodotError::Png("IHDR"));
            }
            have_ihdr = true;
            w = u32::from_be_bytes(c[0..4].try_into().expect("4 bytes")) as usize;
            h = u32::from_be_bytes(c[4..8].try_into().expect("4 bytes")) as usize;
            let (depth, ctype) = (c[8], c[9]);
            if depth != 8 || !matches!(ctype, 0 | 2 | 4 | 6) || c[10] != 0 || c[11] != 0 || c[12] != 0 {
                return Err(MicrodotError::Png("IHDR unsupported"));
            }
            canvas_ok(w, h)?;
            bpp = match ctype {
                0 => 1,
                2 => 3,
                4 => 2,
                _ => 4,
            };
            expect = (1 + w * bpp) * h; // ≤ (1 + 4·32768)·32768, no overflow after canvas_ok
        } else if kind == b"IDAT" {
            if !have_ihdr {
                return Err(MicrodotError::Png("IDAT before IHDR"));
            }
            idat_len += n;
            if idat_len > expect + expect / 1000 + 1024 {
                return Err(MicrodotError::Png("IDAT length"));
            }
            idat.push(c);
        } else if kind == b"IEND" {
            if n != 0 {
                return Err(MicrodotError::Png("IEND"));
            }
            seen_end = true;
        }
        i += 12 + n;
    }
    if !seen_end {
        return Err(MicrodotError::Png("IEND"));
    }
    if i != data.len() {
        return Err(MicrodotError::Png("container end"));
    }
    if !have_ihdr || idat.is_empty() {
        return Err(MicrodotError::Png("no image"));
    }
    let joined = idat.concat();
    let mut dec = ZlibDecoder::new(joined.as_slice());
    let mut raw = Vec::with_capacity(expect + 1);
    let mut buf = [0u8; 16384];
    loop {
        match dec.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > expect {
                    return Err(MicrodotError::Png("raw length"));
                }
            }
            Err(_) => return Err(MicrodotError::Png("inflate")),
        }
    }
    if raw.len() != expect || dec.total_in() as usize != joined.len() {
        return Err(MicrodotError::Png("raw length"));
    }
    let s = w * bpp;
    let mut rows: Rows = Vec::with_capacity(h);
    let mut prev = vec![0u8; s];
    let mut k = 0;
    for _ in 0..h {
        let f = raw[k];
        if f > 4 {
            return Err(MicrodotError::Png("filter"));
        }
        let mut cur = raw[k + 1..k + 1 + s].to_vec();
        k += 1 + s;
        for x in 0..s {
            let a = if x >= bpp { cur[x - bpp] as i32 } else { 0 };
            let b = prev[x] as i32;
            let cc = if x >= bpp { prev[x - bpp] as i32 } else { 0 };
            let add = match f {
                1 => a,
                2 => b,
                3 => (a + b) / 2,
                4 => {
                    let pp = a + b - cc;
                    let (pa, pb, pc) = ((pp - a).abs(), (pp - b).abs(), (pp - cc).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        cc
                    }
                }
                _ => 0,
            };
            cur[x] = ((cur[x] as i32 + add) & 255) as u8;
        }
        let row: Vec<Px> = cur
            .chunks(bpp)
            .map(|p| match bpp {
                1 | 2 => [p[0], p[0], p[0]],
                _ => [p[0], p[1], p[2]],
            })
            .collect();
        rows.push(row);
        prev = cur;
    }
    Ok((w, h, rows))
}

// ---------------------------------------------------------------- faces: grid <-> modules

pub type Mods = Vec<Vec<Px>>;

/// Canonical bytes -> the smallest square module matrix holding them, zero padded; bounded before it exists.
pub fn modules_of(c: &[u8]) -> Result<Mods, MicrodotError> {
    let n = c.len().div_ceil(3);
    let mut gs = 1;
    while gs * gs < n {
        gs += 1;
    }
    if gs > MAX_MODULES_SIDE || gs * gs > MAX_PIXELS {
        return Err(MicrodotError::Png("modules per side"));
    }
    let mut padded = c.to_vec();
    padded.resize(gs * gs * 3, 0);
    Ok((0..gs)
        .map(|y| {
            (0..gs)
                .map(|x| {
                    let i = (y * gs + x) * 3;
                    [padded[i], padded[i + 1], padded[i + 2]]
                })
                .collect()
        })
        .collect())
}

/// A module matrix -> its tile: the modules at `MODULE` pixels inside a `BORDER`-module blue frame; the
/// rendered side is bounded before any row exists.
pub fn tile_pixels(mods: &Mods) -> Result<Rows, MicrodotError> {
    let gs = mods.len();
    if gs > MAX_MODULES_SIDE {
        return Err(MicrodotError::Png("modules per side"));
    }
    let size = (gs + 2 * BORDER) * MODULE;
    canvas_ok(size, size)?;
    let mut px = vec![vec![BLUE; size]; size];
    for (y, row) in mods.iter().enumerate() {
        for (x, m) in row.iter().enumerate() {
            for dy in 0..MODULE {
                for dx in 0..MODULE {
                    px[(y + BORDER) * MODULE + dy][(x + BORDER) * MODULE + dx] = *m;
                }
            }
        }
    }
    Ok(px)
}

fn rot_cw<T: Copy>(m: &[Vec<T>]) -> Vec<Vec<T>> {
    // new[i][j] = m[n-1-j][i]
    let h = m.len();
    let w = m.first().map(|r| r.len()).unwrap_or(0);
    (0..w)
        .map(|i| (0..h).map(|j| m[h - 1 - j][i]).collect())
        .collect()
}

fn mirror_lr<T: Copy>(m: &[Vec<T>]) -> Vec<Vec<T>> {
    m.iter().map(|r| r.iter().rev().copied().collect()).collect()
}

/// Mirror left-right first (if asked), then `rot` quarter turns clockwise.
pub fn transform<T: Copy>(m: &[Vec<T>], (rot, mirror): Sym) -> Vec<Vec<T>> {
    let mut out: Vec<Vec<T>> = if mirror { mirror_lr(m) } else { m.to_vec() };
    for _ in 0..(rot % 4) {
        out = rot_cw(&out);
    }
    out
}

fn untransform<T: Copy>(m: &[Vec<T>], (rot, mirror): Sym) -> Vec<Vec<T>> {
    let mut out = m.to_vec();
    for _ in 0..((4 - rot % 4) % 4) {
        out = rot_cw(&out);
    }
    if mirror {
        out = mirror_lr(&out);
    }
    out
}

/// The symmetry R such that `observed = frame ∘ R`: a component tile was made by R, then the whole picture was
/// turned by `frame` (the composite tile's own reading). Found by composition on a generic matrix, so mirrors
/// compose correctly (the dihedral group is not commutative).
pub fn relative(frame: Sym, observed: Sym) -> Option<Sym> {
    let x: Vec<Vec<u8>> = vec![vec![1, 2, 3], vec![4, 5, 6], vec![7, 8, 9]];
    let target = transform(&x, observed);
    SYM.into_iter()
        .find(|&r| transform(&transform(&x, r), frame) == target)
}

fn bytes_of(mods: &Mods) -> Vec<u8> {
    mods.iter().flatten().flatten().copied().collect()
}

/// A module matrix in any orientation -> the dot and the symmetry it was read in. Tries the eight.
pub fn read_tile(mods: &Mods) -> Option<(Dot, Sym)> {
    read_tile_with(mods, |c| parse_any(c).ok().map(|d| (d, true))).map(|(d, s, _)| (d, s))
}

/// [`read_tile`] for import: a tile whose seal fails still reads, with `false` beside it ([`parse_reporting_seal`]).
pub fn read_tile_reporting(mods: &Mods) -> Option<(Dot, Sym, bool)> {
    read_tile_with(mods, |c| parse_reporting_seal(c).ok())
}

fn read_tile_with(mods: &Mods, read: impl Fn(&[u8]) -> Option<(Dot, bool)>) -> Option<(Dot, Sym, bool)> {
    for sym in SYM {
        let m = untransform(mods, sym);
        let raw = bytes_of(&m);
        if raw.len() < WRAP5_LEN.min(PREFIX_LEN) || &raw[..4] != b"MDOT" {
            continue;
        }
        let end = if u16::from_be_bytes([raw[4], raw[5]]) == 5 {
            if raw.len() < WRAP5_LEN {
                continue;
            }
            WRAP5_LEN + u32::from_be_bytes(raw[PREFIX5_LEN + 1..WRAP5_LEN].try_into().expect("4 bytes")) as usize
        } else {
            if raw.len() < PREFIX_LEN {
                continue;
            }
            PREFIX_LEN + u32::from_be_bytes(raw[22..26].try_into().expect("4 bytes")) as usize
        };
        if raw.len() < end {
            continue;
        }
        if let Some((d, sealed)) = read(&raw[..end]) {
            return Some((d, sym, sealed));
        }
    }
    None
}

/// Blue-bordered squares in a picture -> module matrices (the module size is read from the border).
pub fn tiles_in(rows: &Rows) -> Vec<Mods> {
    let h = rows.len();
    let w = rows.first().map(|r| r.len()).unwrap_or(0);
    if w.checked_mul(h).is_none_or(|p| p > MAX_PIXELS) {
        return Vec::new();
    }
    let mut seen = vec![vec![false; w]; h];
    let mut tiles = Vec::new();
    for y0 in 0..h {
        for x0 in 0..w {
            if seen[y0][x0] || rows[y0][x0] != BLUE {
                continue;
            }
            let mut stack = vec![(y0, x0)];
            seen[y0][x0] = true;
            let (mut top, mut left, mut bottom, mut right) = (y0, x0, y0, x0);
            while let Some((y, x)) = stack.pop() {
                top = top.min(y);
                bottom = bottom.max(y);
                left = left.min(x);
                right = right.max(x);
                let around = [
                    (y + 1, x),
                    (y.wrapping_sub(1), x),
                    (y, x + 1),
                    (y, x.wrapping_sub(1)),
                ];
                for (ny, nx) in around {
                    if ny < h && nx < w && !seen[ny][nx] && rows[ny][nx] == BLUE {
                        seen[ny][nx] = true;
                        stack.push((ny, nx));
                    }
                }
            }
            let side = right - left + 1;
            if side != bottom - top + 1 || side < (2 * BORDER + 1) * 2 {
                continue;
            }
            let mut t = 0;
            while t < side / 2 && rows[top + t][left + t] == BLUE {
                t += 1;
            }
            if t == 0 || t % BORDER != 0 {
                continue;
            }
            let module = t / BORDER;
            let inner = side - 2 * t;
            if inner == 0 || inner % module != 0 {
                continue;
            }
            let gs = inner / module;
            if gs > MAX_MODULES_SIDE {
                continue;
            }
            let mods: Mods = (0..gs)
                .map(|y| {
                    (0..gs)
                        .map(|x| rows[top + t + y * module + module / 2][left + t + x * module + module / 2])
                        .collect()
                })
                .collect();
            tiles.push(mods);
        }
    }
    tiles
}

/// The linear face of one dot: its bytes in one blue-bordered grid, as PNG bytes.
pub fn face_png(canon: &[u8], text: Option<&str>) -> Vec<u8> {
    png_write(&tile_pixels(&modules_of(canon).expect("a dot fits a face")).expect("a dot fits a face"), text)
}

/// A face as a result: the bounded form of `face_png` for input a stranger supplied.
pub fn try_face_png(canon: &[u8], text: Option<&str>) -> Result<Vec<u8>, MicrodotError> {
    Ok(png_write(&tile_pixels(&modules_of(canon)?)?, text))
}

/// The heptapod face of a v4 composite: its own tile in the centre, each component tile around it turned by its
/// relation; `components` are canonical bytes in the layout's order.
pub fn semagram_png(
    composite: &[u8],
    components: &[&[u8]],
    text: Option<&str>,
) -> Result<Vec<u8>, MicrodotError> {
    let d = parse_any(composite)?;
    let lay = d.layout.ok_or(MicrodotError::Layout)?;
    if lay.len() != components.len() || lay.len() > MAX_COMPONENTS {
        return Err(MicrodotError::Component);
    }
    let mut tiles = vec![tile_pixels(&modules_of(composite)?)?];
    for ((rel, id), comp) in lay.iter().zip(components) {
        if dot_id_of(comp).map_err(|_| MicrodotError::Component)? != *id {
            return Err(MicrodotError::Component);
        }
        tiles.push(tile_pixels(&transform(&modules_of(comp)?, REL_SYM[*rel as usize]))?);
    }
    let gap = 2 * MODULE;
    let cell = tiles.iter().map(|t| t.len()).max().unwrap_or(0) + gap;
    let mut ring: Vec<(i64, i64)> = vec![(0, 0)];
    let mut k: i64 = 1;
    while ring.len() < tiles.len() {
        for dy in -k..=k {
            for dx in -k..=k {
                if dx.abs().max(dy.abs()) == k {
                    ring.push((dx, dy));
                }
            }
        }
        k += 1;
    }
    ring.truncate(tiles.len());
    let span = (2 * (k - 1) + 1) as usize;
    let w = span * cell + gap;
    canvas_ok(w, w)?;
    let mut px: Rows = vec![vec![[255, 255, 255]; w]; w];
    for (t, (dx, dy)) in tiles.iter().zip(ring) {
        let ox = gap + (dx + k - 1) as usize * cell + (cell - gap - t.len()) / 2;
        let oy = gap + (dy + k - 1) as usize * cell + (cell - gap - t.len()) / 2;
        for (y, row) in t.iter().enumerate() {
            px[oy + y][ox..ox + row.len()].copy_from_slice(row);
        }
    }
    Ok(png_write(&px, text))
}

/// One tile the reader found.
#[derive(Debug, Clone)]
pub struct Found {
    pub dot_id: String,
    pub ver: u16,
    pub read_in: Sym,
}

/// One component the composite names, and what the picture holds for it.
#[derive(Debug, Clone)]
pub struct Checked {
    pub rel: &'static str,
    pub dot_id: String,
    pub present: bool,
    pub case_ok: bool,
    pub read_in: Option<Sym>,
    pub expected: Sym,
}

/// What a face says: every tile read in any orientation; the composite's components matched by `dot_id` and
/// their case markers checked relative to the composite's frame.
#[derive(Debug, Clone, Default)]
pub struct Reading {
    pub ok: bool,
    pub why: Option<String>,
    pub dots: Vec<Found>,
    pub unreadable: usize,
    pub composite: Option<String>,
    pub markdown: Option<String>,
    pub components: Vec<Checked>,
    pub complete: bool,
}

pub fn read_image(png: &[u8]) -> Result<Reading, MicrodotError> {
    let (_, _, rows) = png_read(png)?;
    Ok(read_rows(&rows))
}

pub fn read_rows(rows: &Rows) -> Reading {
    let mut found: Vec<(Dot, Sym)> = Vec::new();
    let mut unreadable = 0;
    for mods in tiles_in(rows) {
        match read_tile(&mods) {
            Some(f) => found.push(f),
            None => unreadable += 1,
        }
    }
    let mut out = Reading {
        ok: true,
        dots: found
            .iter()
            .map(|(d, s)| Found { dot_id: d.id_hex(), ver: d.ver, read_in: *s })
            .collect(),
        unreadable,
        complete: true,
        ..Default::default()
    };
    if unreadable > 0 {
        out.ok = false;
        out.why = Some(format!("{unreadable} tile(s) verify in no orientation"));
        return out;
    }
    let Some((d0, sym0)) = found.iter().find(|(d, _)| d.layout.is_some()).cloned() else {
        out.ok = !found.is_empty();
        if found.is_empty() {
            out.why = Some("no dot found".into());
        }
        return out;
    };
    let lay = d0.layout.clone().unwrap_or_default();
    out.composite = Some(d0.id_hex());
    out.markdown = d0.markdown.clone();
    for (rel, id) in &lay {
        let expected = REL_SYM[*rel as usize];
        let name = REL_NAMES[*rel as usize];
        match found.iter().find(|(d, _)| d.dot_id == *id) {
            None => {
                out.complete = false;
                out.components.push(Checked {
                    rel: name,
                    dot_id: hex::encode(id),
                    present: false,
                    case_ok: false,
                    read_in: None,
                    expected,
                });
            }
            Some((_, sym)) => {
                let r = relative(sym0, *sym);
                let case_ok = r == Some(expected);
                if !case_ok {
                    out.ok = false;
                    out.why = Some(format!(
                        "case marker broken on {}: read in {:?}, {} is {:?}",
                        &hex::encode(id)[..12],
                        r,
                        name,
                        expected
                    ));
                }
                out.components.push(Checked {
                    rel: name,
                    dot_id: hex::encode(id),
                    present: true,
                    case_ok,
                    read_in: r,
                    expected,
                });
            }
        }
    }
    if let Some((d, _)) = found
        .iter()
        .find(|(d, _)| d.dot_id != d0.dot_id && !lay.iter().any(|(_, id)| *id == d.dot_id))
    {
        out.ok = false;
        out.why = Some(format!("unlisted tile {}", &d.id_hex()[..12]));
    }
    if !out.complete && out.ok {
        let missing = out.components.iter().filter(|c| !c.present).count();
        out.why = Some(format!("incomplete: {missing} component(s) not in the picture"));
    }
    out
}

#[cfg(test)]
mod import_tests {
    use super::*;

    // Noor, 31 Oct 2026: Blaze's Round 41 dot is sealed by a demo key and a test encoder's dot fails its seal; both enter
    // Oracle saying so, and a body that does not match its hash never does.
    #[test]
    fn a_dot_made_elsewhere_reads_with_its_seal_reported_and_a_bad_body_is_still_refused() {
        let good = pack_v3(&[7u8; 32], b"the faces rule", 1_791_000_000_000, [0u8; 32]);
        let (d, sealed) = parse_reporting_seal(&good).unwrap();
        assert!(sealed && d.body == b"the faces rule");
        let mut forged = good.clone();
        forged[HEADER_LEN] ^= 1;
        assert!(matches!(parse_any(&forged), Err(MicrodotError::Seal)), "the strict reader still refuses it");
        let (d, sealed) = parse_reporting_seal(&forged).unwrap();
        assert!(!sealed && d.body == b"the faces rule", "the import reader reads it and says the seal failed");
        let mut changed = good.clone();
        let n = changed.len();
        changed[n - 1] ^= 1;
        assert!(parse_reporting_seal(&changed).is_err(), "a body that does not match its hash is refused either way");
        let mods = modules_of(&forged).unwrap();
        assert!(read_tile(&mods).is_none() && read_tile_reporting(&mods).is_some_and(|(_, _, s)| !s));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(name: &str) -> Vec<u8> {
        let p = format!("{}/../../tools/microdot/vectors/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
    }

    /// A vector made from our own records (Blaze's Round 41 image, the #culture semagram of 21 Oct): it lives in the
    /// private tree only, so its test reads it when it is there. The public tree tests the same reader paths on
    /// public-v3.png and public-semagram.png (tools/microdot/make_public_vectors.py, synthetic text, a test seed).
    fn private_vector(name: &str) -> Option<Vec<u8>> {
        let p = format!("{}/../../tools/microdot/vectors/{name}", env!("CARGO_MANIFEST_DIR"));
        let v = std::fs::read(&p).ok();
        if v.is_none() {
            eprintln!("{name}: a private vector, not in this tree; the public vectors cover this path");
        }
        v
    }

    #[test]
    fn public_v3_vector_reads_byte_exact() {
        let r = read_image(&vector("public-v3.png")).expect("png");
        assert!(r.ok, "{:?}", r.why);
        assert_eq!(r.dots.len(), 1);
        assert!(r.dots[0].dot_id.starts_with("dc7cb76c6e235180"), "{}", r.dots[0].dot_id);
        assert_eq!(r.dots[0].ver, 3);
    }

    #[test]
    fn public_semagram_vector_reads_whole() {
        let r = read_image(&vector("public-semagram.png")).expect("png");
        assert!(r.ok, "{:?}", r.why);
        let c = r.composite.as_deref().expect("a composite");
        assert!(c.starts_with("0154da33abe9a80e"), "{c}");
        assert!(r.complete);
        assert_eq!(r.components.len(), 3);
        assert!(r.components.iter().all(|c| c.present && c.case_ok));
        assert_eq!(r.dots.len(), 4);
    }

    fn seed(n: u8) -> [u8; 32] {
        sha256(&[b"microdot-test-seed-".as_slice(), &[n]].concat())
    }

    #[test]
    fn round41_vector_reads_byte_exact() {
        let Some(png) = private_vector("round41-v321.png") else { return };
        let r = read_image(&png).expect("png");
        assert!(r.ok, "{:?}", r.why);
        assert_eq!(r.dots.len(), 1);
        assert!(r.dots[0].dot_id.starts_with("e4d708e4450fd1e5"), "{}", r.dots[0].dot_id);
        assert_eq!(r.dots[0].ver, 3);
    }

    #[test]
    fn culture_semagram_vector_reads_whole() {
        let Some(png) = private_vector("culture-semagram-20261021.png") else { return };
        let r = read_image(&png).expect("png");
        assert!(r.ok, "{:?}", r.why);
        let c = r.composite.as_deref().expect("a composite");
        assert!(c.starts_with("9190d4aee2d5162e"), "{c}");
        assert!(r.complete);
        assert_eq!(r.components.len(), 3);
        assert!(r.components.iter().all(|c| c.present && c.case_ok));
        assert!(r.dots.len() == 4);
    }

    fn chain() -> (Vec<Vec<u8>>, Vec<u8>) {
        let s = seed(1);
        let a = pack_v3(&s, b"# one\n", 1_700_000_000_000, [0u8; 32]);
        let b = pack_v3(&s, b"# two\n", 1_700_000_001_000, sha256(&a));
        let c = pack_v3(&s, b"# three\n", 1_700_000_002_000, sha256(&b));
        let comp = pack_composite(
            &seed(2),
            &[(0, a.as_slice()), (1, b.as_slice()), (6, c.as_slice())],
            "the sentence",
            1_700_000_003_000,
            [0u8; 32],
        )
        .expect("three verified components");
        (vec![a, b, c], comp)
    }

    #[test]
    fn chain_and_composite_round_trip_both_faces() {
        let (parts, comp) = chain();
        for p in &parts {
            let d = parse(p).expect("verifies");
            let r = read_image(&face_png(p, None)).expect("png");
            assert!(r.ok && r.dots[0].dot_id == d.id_hex());
        }
        let d = parse(&comp).expect("composite verifies");
        assert_eq!(d.markdown.as_deref(), Some("the sentence"));
        assert_eq!(d.layout.as_ref().map(|l| l.len()), Some(3));
        assert_eq!(parse(&parts[1]).unwrap().prev, sha256(&parts[0]));
        let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
        let png = semagram_png(&comp, &refs, Some("test")).expect("semagram");
        let r = read_image(&png).expect("png");
        assert!(r.ok, "{:?}", r.why);
        assert_eq!(r.composite.as_deref(), Some(d.id_hex().as_str()));
        assert!(r.complete && r.components.iter().all(|c| c.case_ok));
        let linear = read_image(&face_png(&comp, None)).expect("png");
        assert!(linear.ok && !linear.complete, "{:?}", linear.why);
    }

    #[test]
    fn the_whole_picture_turned_or_mirrored_reads_the_same() {
        let (parts, comp) = chain();
        let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
        let png = semagram_png(&comp, &refs, None).expect("semagram");
        let (_, _, rows) = png_read(&png).expect("png");
        let base = read_rows(&rows);
        for sym in [(1, false), (2, false), (3, false), (0, true), (1, true), (3, true)] {
            let turned = transform(&rows, sym);
            let r = read_rows(&turned);
            assert!(r.ok, "{sym:?}: {:?}", r.why);
            assert_eq!(r.composite, base.composite);
            assert!(r.complete && r.components.iter().all(|c| c.case_ok), "{sym:?}");
        }
    }

    #[test]
    fn a_tile_turned_the_wrong_way_is_refused_by_name() {
        let (parts, comp) = chain();
        let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
        let png = semagram_png(&comp, &refs, None).expect("semagram");
        let (_, _, mut rows) = png_read(&png).expect("png");
        // find the tile of part 2 (rel 1: one turn) and give it two turns instead
        let want = sha256(&parts[1]);
        let tile = tile_pixels(&transform(&modules_of(&parts[1]).unwrap(), (1, false))).unwrap();
        let wrong = tile_pixels(&transform(&modules_of(&parts[1]).unwrap(), (2, false))).unwrap();
        let n = tile.len();
        let mut hit = false;
        'outer: for y in 0..rows.len() - n {
            for x in 0..rows[0].len() - n {
                if rows[y][x] == BLUE && (0..n).all(|i| rows[y + i][x..x + n] == tile[i][..]) {
                    for i in 0..n {
                        rows[y + i][x..x + n].copy_from_slice(&wrong[i]);
                    }
                    hit = true;
                    break 'outer;
                }
            }
        }
        assert!(hit, "part 2's tile is in the picture");
        let r = read_rows(&rows);
        assert!(!r.ok);
        let why = r.why.unwrap_or_default();
        assert!(why.starts_with("case marker broken on"), "{why}");
        assert!(why.contains(&hex::encode(want)[..12]), "{why}");
    }

    #[test]
    fn a_flipped_byte_makes_a_tile_unreadable() {
        let (parts, _) = chain();
        let mut c = parts[0].clone();
        c[200] ^= 0x01; // inside the seal
        let r = read_image(&face_png(&c, None)).expect("png");
        assert!(!r.ok && r.unreadable == 1, "{:?}", r.why);
        assert!(parse(&c).is_err());
    }

    #[test]
    fn an_unlisted_tile_is_refused() {
        let (parts, comp) = chain();
        let stranger = pack_v3(&seed(3), b"# not named\n", 1_700_000_009_000, [0u8; 32]);
        let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
        let png = semagram_png(&comp, &refs, None).expect("semagram");
        let (_, _, rows) = png_read(&png).expect("png");
        let extra = tile_pixels(&modules_of(&stranger).unwrap()).unwrap();
        let n = extra.len();
        let gap = 2 * MODULE;
        let mut big: Rows = vec![vec![[255, 255, 255]; rows[0].len() + n + 2 * gap]; rows.len().max(n + 2 * gap)];
        for (y, row) in rows.iter().enumerate() {
            big[y][..row.len()].copy_from_slice(row);
        }
        for (y, row) in extra.iter().enumerate() {
            let ox = rows[0].len() + gap;
            big[gap + y][ox..ox + n].copy_from_slice(row);
        }
        let r = read_rows(&big);
        assert!(!r.ok);
        assert!(r.why.unwrap_or_default().starts_with("unlisted tile"));
    }

    // ------------------------------------------------------------ v5 (docs/MICRODOT.md §7)

    fn v5_vectors() -> serde_json::Value {
        let p = format!("{}/../../tools/microdot/vectors/v5.json", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_slice(&std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"))).expect("v5.json")
    }

    fn part(c: &[u8]) -> String {
        parse_any(c).map(|_| "ok".to_string()).unwrap_or_else(|e| e.part())
    }

    #[test]
    fn v5_frozen_vectors_read_to_their_frozen_ids() {
        let v = v5_vectors();
        let mut ids = std::collections::HashSet::new();
        for x in v["vectors"].as_array().expect("vectors") {
            let c = hex::decode(x["stored_hex"].as_str().unwrap()).unwrap();
            let d = parse_any(&c).unwrap_or_else(|e| panic!("{}: {e}", x["name"]));
            assert_eq!(d.id_hex(), x["dot_id"].as_str().unwrap(), "{}", x["name"]);
            assert_eq!(hex::encode(sha256(&semantic_bytes(&c).unwrap())), x["semantic_sha256"].as_str().unwrap(), "{}", x["name"]);
            assert_eq!(d.enc as u64, x["enc"].as_u64().unwrap());
            if x["name"].as_str().unwrap().starts_with("plain") {
                ids.insert(d.id_hex());
            }
        }
        assert_eq!(ids.len(), 1, "plain-zlib9, plain-zlib1 and plain-raw share one id");
    }

    #[test]
    fn v5_one_body_one_key_any_compressor_one_id() {
        let s = seed(5);
        let body = "# the Room Protocol\n".to_string() + &"One question, followed honestly, in the company of the people who followed it before. ".repeat(40);
        let dots: Vec<Vec<u8>> = [1u32, 6, 9]
            .iter()
            .map(|l| pack_v5(&s, body.as_bytes(), 1_792_600_000_000, [0u8; 32], 0, [0u8; 32], false, None, *l, false).unwrap())
            .chain(std::iter::once(pack_v5(&s, body.as_bytes(), 1_792_600_000_000, [0u8; 32], 0, [0u8; 32], false, None, 9, true).unwrap()))
            .collect();
        let ids: std::collections::HashSet<String> = dots.iter().map(|c| parse_any(c).unwrap().id_hex()).collect();
        assert_eq!(ids.len(), 1);
        let comps: std::collections::HashSet<u32> = dots.iter().map(|c| parse_any(c).unwrap().comp).collect();
        assert!(comps.len() >= 2, "different compressed lengths");
        assert_eq!(parse_any(&dots[3]).unwrap().enc, 0);
        let seals: std::collections::HashSet<Vec<u8>> = dots.iter().map(|c| c[HEADER5_LEN..PREFIX5_LEN].to_vec()).collect();
        assert_eq!(seals.len(), 1);
        let other = pack_v5(&seed(6), body.as_bytes(), 1_792_600_000_000, [0u8; 32], 0, [0u8; 32], false, None, 9, false).unwrap();
        assert!(!ids.contains(&parse_any(&other).unwrap().id_hex()), "a second key differs (control)");
        // the same body minted by Python (vector plain-zlib9) and by Rust give one semantic hash
        let v = v5_vectors();
        let py = hex::decode(v["vectors"][0]["stored_hex"].as_str().unwrap()).unwrap();
        let py_seed = sha256(b"rocky-v5-seed-2026-10-21");
        let rs = pack_v5(&py_seed, body.as_bytes(), 1_792_600_000_000, [0u8; 32], 0, [0u8; 32], false, None, 9, false).unwrap();
        assert_eq!(parse_any(&rs).unwrap().id_hex(), parse_any(&py).unwrap().id_hex(), "Python and Rust mint one id");
    }

    fn resign5(c: &[u8], s: &[u8; 32], edit: impl Fn(&mut Vec<u8>)) -> Vec<u8> {
        let mut h = c[..HEADER5_LEN].to_vec();
        edit(&mut h);
        let key = SigningKey::from_bytes(s);
        let seal = key.sign(&[SEAL5_TAG, h.as_slice()].concat()).to_bytes();
        [h.as_slice(), &seal, &c[PREFIX5_LEN..]].concat()
    }

    #[test]
    fn v5_refusals_by_name() {
        let s = seed(7);
        let body = b"# a body of some length, enough to compress a little, repeated: ".repeat(8);
        let ok = pack_v5(&s, &body, 1_792_600_000_000, [0u8; 32], 0, [0u8; 32], false, None, 9, false).unwrap();
        assert_eq!(part(&ok), "ok");
        let n = ok.len();
        assert_eq!(part(&ok[..n - 3]), "stream");
        assert_eq!(part(&[ok.as_slice(), b"xx"].concat()), "stream");
        let mut b = ok.clone();
        b[WRAP5_LEN + 5] ^= 1;
        assert!(matches!(part(&b).as_str(), "inflate" | "what"));
        let mut s2 = ok.clone();
        s2[HEADER5_LEN + 3] ^= 1;
        assert_eq!(part(&s2), "seal");
        assert_eq!(part(&[b"MDOX", &ok[4..]].concat()), "magic");
        assert_eq!(part(&resign5(&ok, &s, |h| h[4..6].copy_from_slice(&6u16.to_be_bytes()))), "ver");
        assert_eq!(part(&resign5(&ok, &s, |h| h[8] = 1)), "suite");
        assert_eq!(part(&resign5(&ok, &s, |h| h[6..8].copy_from_slice(&8u16.to_be_bytes()))), "flags");
        assert_eq!(part(&resign5(&ok, &s, |h| h[9] = 3)), "prevkind");
        assert_eq!(part(&resign5(&ok, &s, |h| h[9] = 1)), "prevkind", "kind 1 with a zero prev");
        assert_eq!(part(&resign5(&ok, &s, |h| h[54] = 1)), "prevkind", "kind 0 with a nonzero prev");
        assert_eq!(part(&resign5(&ok, &s, |h| h[87] = 1)), "pad");
        assert_eq!(part(&resign5(&ok, &s, |h| h[18..22].copy_from_slice(&((MAX_ORIG + 1) as u32).to_be_bytes()))), "limits");
        let mut e = ok.clone();
        e[PREFIX5_LEN] = 2;
        assert_eq!(part(&e), "enc");
        let mut big = ok.clone();
        big[PREFIX5_LEN + 1..WRAP5_LEN].copy_from_slice(&((body.len() + 2000) as u32).to_be_bytes());
        assert_eq!(part(&big), "comp");
        let raw = pack_v5(&s, &body, 1_792_600_000_000, [0u8; 32], 0, [0u8; 32], false, None, 9, true).unwrap();
        let mut r2 = raw.clone();
        r2[PREFIX5_LEN + 1..WRAP5_LEN].copy_from_slice(&((body.len() - 1) as u32).to_be_bytes());
        assert_eq!(part(&r2), "comp");
        let with_stream = |st: &[u8]| [&ok[..PREFIX5_LEN + 1], &(st.len() as u32).to_be_bytes()[..], st].concat();
        let good = zlib_level(&body, 9);
        let mut early = good[..good.len() - 8].to_vec();
        early.extend_from_slice(&[0u8; 8]);
        assert_eq!(part(&with_stream(&early)), "inflate");
        assert_eq!(part(&with_stream(&[good.as_slice(), &[0u8; 4]].concat())), "inflate", "unused input inside comp");
        let longer = zlib_level(&[body.as_slice(), b"!"].concat(), 9);
        assert_eq!(part(&with_stream(&longer)), "inflate");
        let swapped = zlib_level(&[b"another body of the same length", &body[31..]].concat(), 9);
        assert_eq!(part(&with_stream(&swapped)), "what");
    }

    fn claim_dot(s: &[u8; 32], txt: &[u8], flags: u16) -> Vec<u8> {
        let key = SigningKey::from_bytes(s);
        let mut h = Vec::new();
        h.extend_from_slice(b"MDOT");
        h.extend_from_slice(&5u16.to_be_bytes());
        h.extend_from_slice(&flags.to_be_bytes());
        h.push(0);
        h.push(0);
        h.extend_from_slice(&1_792_600_000_000u64.to_be_bytes());
        h.extend_from_slice(&(txt.len() as u32).to_be_bytes());
        h.extend_from_slice(&sha256(txt));
        h.extend_from_slice(&[0u8; 32]);
        h.extend_from_slice(&[0u8; 2]);
        h.extend_from_slice(&key.verifying_key().to_bytes());
        h.extend_from_slice(&[0u8; 32]);
        let seal = key.sign(&[SEAL5_TAG, h.as_slice()].concat()).to_bytes();
        [h.as_slice(), &seal, &[0u8], &(txt.len() as u32).to_be_bytes(), txt].concat()
    }

    #[test]
    fn v5_claims_and_plain_bodies() {
        let s = seed(8);
        let fields: &[(&str, &str)] = &[("evidence", "recompute"), ("status", "verified"), ("scope", "307 clones, day 2"), ("harness", "3e92ba88"), ("source", "GSE140802")];
        let cl = pack_v5(&s, b"# run 3\nthree arms", 1_792_600_000_000, [0u8; 32], 0, [0u8; 32], false, Some(fields), 9, false).unwrap();
        let d = parse_any(&cl).unwrap();
        assert_eq!((d.flags >> 1) & 3, 1);
        let c = d.claim.expect("a claim");
        assert_eq!(c.iter().find(|(k, _)| k == "evidence").map(|(_, v)| v.as_str()), Some("recompute"));
        assert!(d.markdown.unwrap().starts_with("# run 3"));
        let plain = pack_v5(&s, b"note: colons: and key: value lines\nscope: not a claim", 1_792_600_000_000, [0u8; 32], 0, [0u8; 32], false, None, 9, false).unwrap();
        assert!(parse_any(&plain).unwrap().claim.is_none(), "colon text without the marker is plain");
        assert_eq!(part(&claim_dot(&s, b"plain words", 1 << 1)), "claim", "inflection without a marker");
        let base = "evidence: absent\nstatus: claim\nscope: planaria\ncorpus: GEO\nquery: planarian spatial clonal\ndate: 2026-10-21\n";
        let good = [CLAIM_MARK, base.as_bytes(), b"---\nbody"].concat();
        assert_eq!(part(&claim_dot(&s, &good, 0)), "ok");
        let bad: Vec<(&str, Vec<u8>, u16)> = vec![
            ("duplicate", [CLAIM_MARK, base.as_bytes(), b"evidence: none\n---\nx"].concat(), 0),
            ("unknown key", [CLAIM_MARK, base.as_bytes(), b"tier: 1\n---\nx"].concat(), 0),
            ("absent without corpus", [CLAIM_MARK, b"evidence: absent\nstatus: claim\nscope: s\nquery: q\ndate: d\n---\nx"].concat(), 0),
            ("missing scope", [CLAIM_MARK, b"evidence: none\nstatus: claim\n---\nx"].concat(), 0),
            ("inflection disagrees", good.clone(), 1 << 1),
            ("no closing", [CLAIM_MARK, base.as_bytes(), b"body"].concat(), 0),
            ("CR", [CLAIM_MARK, base.replacen('\n', "\r\n", 1).as_bytes(), b"---\nx"].concat(), 0),
            ("recompute without harness", [CLAIM_MARK, b"evidence: recompute\nstatus: claim\nscope: s\nsource: x\n---\nx"].concat(), 0),
            ("too long", [CLAIM_MARK, format!("evidence: none\nstatus: claim\nscope: {}\n", "x".repeat(1025)).as_bytes(), b"---\nx"].concat(), 0),
            ("DEL", [CLAIM_MARK, b"evidence: none\nstatus: claim\nscope: s\x7f\n---\nx"].concat(), 0),
            ("C1 control", [CLAIM_MARK, "evidence: none\nstatus: claim\nscope: s\u{85}\n---\nx".as_bytes()].concat(), 0),
            ("U+2028", [CLAIM_MARK, "evidence: none\nstatus: claim\nscope: s\u{2028}\n---\nx".as_bytes()].concat(), 0),
        ];
        for (name, txt, flags) in bad {
            assert_eq!(part(&claim_dot(&s, &txt, flags)), "claim", "{name}");
        }
    }

    #[test]
    fn v5_predecessors_composites_and_faces() {
        let s = seed(9);
        let body = b"# a body";
        let prev = sha256(b"some predecessor");
        let k1 = pack_v5(&s, body, 1_792_600_000_000, prev, 1, [0u8; 32], false, None, 9, false).unwrap();
        let k2 = pack_v5(&s, body, 1_792_600_000_000, prev, 2, [0u8; 32], false, None, 9, false).unwrap();
        let (d1, d2) = (parse_any(&k1).unwrap(), parse_any(&k2).unwrap());
        assert!(d1.prevkind == 1 && d2.prevkind == 2 && d1.dot_id != d2.dot_id);
        assert_ne!(d1.dot_id, sha256(&k1), "a v5 id is not sha256 of the stored bytes");
        assert_eq!(dot_id_of(&k1).unwrap(), d1.dot_id);
        // a v5 composite of two v5 children with a claim in its markdown, named by verified ids
        let a = pack_v5(&s, b"# child one", 1_792_600_000_010, [0u8; 32], 0, [0u8; 32], false, None, 9, false).unwrap();
        let b = pack_v5(&s, b"# child two", 1_792_600_000_011, dot_id_of(&a).unwrap(), 1, [0u8; 32], false, None, 9, false).unwrap();
        let lay = layout(&[(0, dot_id_of(&a).unwrap()), (4, dot_id_of(&b).unwrap())], "# three");
        let comp = pack_v5(&s, &lay, 1_792_600_000_012, dot_id_of(&b).unwrap(), 1, [0u8; 32], true, Some(&[("evidence", "none"), ("status", "claim"), ("scope", "two")]), 9, false).unwrap();
        let dc = parse_any(&comp).unwrap();
        assert_eq!(dc.layout.as_ref().map(|l| l.len()), Some(2));
        assert!(dc.claim.is_some() && dc.markdown.as_deref() == Some("# three"));
        let png = semagram_png(&comp, &[&a, &b], None).expect("semagram");
        let (_, _, rows) = png_read(&png).expect("png");
        let base = read_rows(&rows);
        assert!(base.ok && base.complete && base.composite.as_deref() == Some(dc.id_hex().as_str()), "{:?}", base.why);
        for sym in SYM {
            let r = read_rows(&transform(&rows, sym));
            assert!(r.ok && r.complete && r.components.iter().all(|c| c.case_ok), "{sym:?}: {:?}", r.why);
        }
        assert!(semagram_png(&comp, &[&b, &a], None).is_err(), "components out of order are refused");
        let linear = read_image(&face_png(&a, None)).unwrap();
        assert!(linear.ok && linear.dots[0].ver == 5 && linear.dots[0].dot_id == parse_any(&a).unwrap().id_hex());
        // raw transport reads to the same id from a face
        let raw = pack_v5(&s, b"# child one", 1_792_600_000_010, [0u8; 32], 0, [0u8; 32], false, None, 9, true).unwrap();
        assert_eq!(read_image(&face_png(&raw, None)).unwrap().dots[0].dot_id, parse_any(&a).unwrap().id_hex());
        // 14 MiB of pseudo-random bytes exceed orig + 1 KiB under zlib and fall back to raw by themselves
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let rnd: Vec<u8> = (0..14usize << 20).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; (x >> 24) as u8 }).collect();
        assert!(zlib_level(&rnd, 1).len() > rnd.len() + ZLIB_SLACK, "the vector really exceeds orig + 1 KiB");
        let inc = pack_v5(&s, &rnd, 1_792_600_000_000, [0u8; 32], 0, [0u8; 32], false, None, 1, false).unwrap();
        let di = parse_any(&inc).unwrap();
        assert!(di.enc == 0 && di.comp as usize == rnd.len());
    }

    #[test]
    fn mint_helpers_refuse_what_the_contract_refuses() {
        let s = seed(11);
        let fields: &[(&str, &str)] = &[("evidence", "none"), ("status", "claim"), ("scope", "s")];
        // 1. a claim's markdown must be UTF-8; arbitrary bytes stay legal without a claim
        let bad = pack_v5(&s, &[0xff, 0xfe], 1, [0u8; 32], 0, [0u8; 32], false, Some(fields), 9, false);
        assert!(matches!(bad, Err(MicrodotError::Part("claim", _))), "{bad:?}");
        let plain = pack_v5(&s, &[0xff, 0xfe], 1, [0u8; 32], 0, [0u8; 32], false, None, 9, false).expect("bytes without a claim");
        assert_eq!(parse_any(&plain).unwrap().body, vec![0xff, 0xfe]);
        let lay = layout(&[(0, [7u8; 32])], "");
        let bad_c = pack_v5(&s, &[lay.as_slice(), &[0xffu8, 0xfe]].concat(), 1, [0u8; 32], 0, [0u8; 32], true, Some(fields), 9, false);
        assert!(matches!(bad_c, Err(MicrodotError::Part("claim", _))), "{bad_c:?}");
        // 2. duplicate keys refuse in the helper and through pack_v5
        let dup: &[(&str, &str)] = &[("evidence", "none"), ("status", "claim"), ("scope", "first"), ("scope", "second")];
        assert!(matches!(claim_text(dup, ""), Err(MicrodotError::Part("claim", _))));
        assert!(matches!(pack_v5(&s, b"# x", 1, [0u8; 32], 0, [0u8; 32], false, Some(dup), 9, false), Err(MicrodotError::Part("claim", _))));
        // 3. a composite names only components that parse; mixed v4 and v5 components are fine
        assert!(matches!(pack_composite(&s, &[(0, b"not a microdot")], "", 1, [0u8; 32]), Err(MicrodotError::Component)));
        let v3 = pack_v3(&s, b"# v3", 1, [0u8; 32]);
        let v5 = pack_v5(&s, b"# v5", 2, [0u8; 32], 0, [0u8; 32], false, None, 9, false).unwrap();
        let mut broken = v5.clone();
        broken[HEADER5_LEN + 1] ^= 1;
        assert!(matches!(pack_composite(&s, &[(0, v3.as_slice()), (1, broken.as_slice())], "", 3, [0u8; 32]), Err(MicrodotError::Component)));
        let comp = pack_composite(&s, &[(0, v3.as_slice()), (1, v5.as_slice())], "mixed", 3, [0u8; 32]).expect("mixed v3 and v5");
        let d = parse_any(&comp).unwrap();
        assert_eq!(d.layout.as_ref().unwrap()[0].1, sha256(&v3));
        assert_eq!(d.layout.as_ref().unwrap()[1].1, dot_id_of(&v5).unwrap());
        assert!(matches!(pack_composite(&s, &[(9, v3.as_slice())], "", 3, [0u8; 32]), Err(MicrodotError::Layout)));
    }

    #[test]
    fn hostile_pngs_refuse_by_name_before_allocating() {
        fn ch(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            chunk(&mut out, kind, data);
            out
        }
        fn ihdr(w: u32, h: u32, il: u8) -> Vec<u8> {
            let mut c = Vec::new();
            c.extend_from_slice(&w.to_be_bytes());
            c.extend_from_slice(&h.to_be_bytes());
            c.extend_from_slice(&[8, 2, 0, 0, il]);
            ch(b"IHDR", &c)
        }
        let sig = b"\x89PNG\r\n\x1a\n".to_vec();
        let good = [sig.clone(), ihdr(2, 2, 0), ch(b"IDAT", &zlib(&[0u8; 14])), ch(b"IEND", b"")].concat();
        assert_eq!(read_image(&good).unwrap().why.as_deref(), Some("no dot found"));
        let mut badcrc = [sig.clone(), ihdr(2, 2, 0)].concat();
        let mut idat = ch(b"IDAT", &zlib(&[0u8; 14]));
        let l = idat.len();
        idat[l - 1] ^= 1;
        badcrc.extend(idat);
        badcrc.extend(ch(b"IEND", b""));
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("dimensions", [sig.clone(), ihdr(100_000, 100_000, 0), ch(b"IDAT", &zlib(&[0u8; 10])), ch(b"IEND", b"")].concat()),
            ("chunk length", [sig.clone(), ihdr(2, 2, 0), (1u32 << 30).to_be_bytes().to_vec(), b"IDAT".to_vec(), vec![b'x'; 10]].concat()),
            ("IDAT before IHDR", [sig.clone(), ch(b"IDAT", b"x"), ch(b"IEND", b"")].concat()),
            ("IHDR unsupported", [sig.clone(), ihdr(2, 2, 1), ch(b"IDAT", b"x"), ch(b"IEND", b"")].concat()),
            ("filter", [sig.clone(), ihdr(2, 1, 0), ch(b"IDAT", &zlib(&[7, 0, 0, 0, 0, 0, 0])), ch(b"IEND", b"")].concat()),
            ("raw length", [sig.clone(), ihdr(2, 2, 0), ch(b"IDAT", &zlib(&[0u8; 7])), ch(b"IEND", b"")].concat()),
            ("raw length", [sig.clone(), ihdr(2, 2, 0), ch(b"IDAT", &[zlib(&[0u8; 14]), vec![0, 0]].concat()), ch(b"IEND", b"")].concat()),
            ("IDAT length", [sig.clone(), ihdr(2, 2, 0), ch(b"IDAT", &[0u8; 4000]), ch(b"IEND", b"")].concat()),
            ("inflate", [sig.clone(), ihdr(2, 2, 0), ch(b"IDAT", &[&[0x78u8, 0x9c][..], &[0xffu8; 20]].concat()), ch(b"IEND", b"")].concat()),
            ("signature", [b"GIF89a".to_vec(), vec![0u8; 40]].concat()),
            ("crc", badcrc),
            ("IEND", [sig.clone(), ihdr(2, 2, 0), ch(b"IDAT", &zlib(&[0u8; 14]))].concat()),
            ("IEND", [sig.clone(), ihdr(2, 2, 0), ch(b"IDAT", &zlib(&[0u8; 14])), ch(b"IEND", b"x")].concat()),
            ("container end", [good.clone(), b"trailing".to_vec()].concat()),
        ];
        for (name, data) in cases {
            match read_image(&data) {
                Err(MicrodotError::Png(p)) => assert_eq!(p, name, "{name}"),
                other => panic!("{name}: {other:?}"),
            }
        }
        // rendered size squared against MAX_PIXELS: 4096 modules at 4 px would be 269 M pixels
        let row = vec![[0u8, 0, 0]; 4096];
        let big: Mods = vec![row; 4096];
        assert!(matches!(tile_pixels(&big), Err(MicrodotError::Png("dimensions"))));
        assert!(matches!(modules_of(&vec![0u8; 3 * 4097 * 4097]), Err(MicrodotError::Png("modules per side"))));
        assert!(matches!(canvas_ok(40_000, 1), Err(MicrodotError::Png("dimensions"))));
    }

    #[test]
    fn relative_composes_in_the_dihedral_group() {
        for frame in SYM {
            for r in SYM {
                let x: Vec<Vec<u8>> = vec![vec![1, 2, 3], vec![4, 5, 6], vec![7, 8, 9]];
                let observed_m = transform(&transform(&x, r), frame);
                let observed = SYM.into_iter().find(|&s| transform(&x, s) == observed_m).expect("closed");
                assert_eq!(relative(frame, observed), Some(r), "frame {frame:?} r {r:?}");
            }
        }
    }
}
