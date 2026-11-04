//! The cortex's atom (Noor's Round 43, decided in the room, 2026-10-10): a CLAIM, a falsifiable statement with a type, a
//! status, provenance into the ledger and typed edges to other claims. Nothing here is stored: a claim is minted, linked and
//! moved by signed messages (tags `claim:mint`, `claim:edge`, `claim:status`, `claim:source`, the fields in cargo), and the table is folded
//! from them on every read, so the ledger stays the only thing trusted and the table can never drift from it.
//!
//!   mint    cargo {statement, type, status, sources?, row?}      cid = sha256 of the statement, whitespace collapsed
//!   edge    cargo {from, to, kind}                               kind: supports · contradicts · supersedes · is-same-as · cites
//!   status  cargo {cid, status, why?}                            the newest status wins; every move stays in history
//!   source  cargo {cid, sources?, remove?, why?}                 add sources, or take off one that was wrong (4 Nov 2026);
//!                                                                 a removal is listed on the claim with its record and why
//!
//! Cascading refutation (Noor's TASK-OAI-1, 27 Oct: ETH refuted, and hundreds of hardness results rest on it). A claim rests on
//! another when it `cites` it or is `supports`-ed by it. When a claim it rests on, directly or through others, is refuted after
//! the claim's own last status move, the claim *stands* open: `standing` is "open" and `fallen` names the refuted root and the
//! path to it, "root claim <cid> refuted — re-verification required". Nothing is written for this: the signed status stays as it
//! was, and a newer status move on the claim (its re-verification) is what clears it. A refuted claim stays refuted.
//!
//! A cid may be named by a unique prefix of at least 8 hex. A message whose cargo does not parse, or names a claim that
//! does not exist, is listed under `rejected` with the reason: never silently dropped.
use crate::store::{Observation, Result, Store};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

pub const TYPES: [&str; 6] = ["standard-theorem", "our-derivation", "conjecture", "bounded-computation", "external", "unresolved"];
pub const STATUSES: [&str; 3] = ["verified", "refuted", "open"];
pub const EDGES: [&str; 5] = ["supports", "contradicts", "supersedes", "is-same-as", "cites"];
pub const TAGS: [&str; 4] = ["claim:mint", "claim:edge", "claim:status", "claim:source"];

/// The statement as hashed: trimmed, every run of whitespace one space. Case and punctuation are the author's.
pub fn canonical(statement: &str) -> String {
    statement.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn cid_of(statement: &str) -> String {
    oracle_proto::sha_hex(canonical(statement).as_bytes())
}

#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    pub kind: String,
    pub cid: String,
    /// The message that asserted the edge.
    pub by: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Move {
    pub status: String,
    pub by: String,
    pub author: String,
    pub at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceRemoved {
    pub source: String,
    pub by: String,
    pub author: String,
    pub at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Claim {
    pub cid: String,
    pub statement: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub room: String,
    pub minted_by: String,
    pub author: String,
    pub minted_ms: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// Sources taken off after they were added (a wrong DOI, a dead link): never erased, said with the signed record that
    /// removed each and why (Ramanujan, 30 Oct 2026: the superseded PRZZ claim carried a wrong DOI beside the right one).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources_removed: Vec<SourceRemoved>,
    /// The minting room's own label for the row (e.g. Round 53's "B12").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row: Option<String>,
    pub out: Vec<Edge>,
    #[serde(rename = "in")]
    pub inbound: Vec<Edge>,
    pub history: Vec<Move>,
    /// Superseded by a claim that is not itself refuted: shown under its successor.
    pub superseded_by: Option<String>,
    /// The status a reader should act on: `status`, unless a claim this one rests on fell after its last move (then "open").
    pub standing: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallen: Option<Fallen>,
}

/// Why a claim stands open: the refuted root, when it fell, and the chain from this claim down to it.
#[derive(Debug, Clone, Serialize)]
pub struct Fallen {
    pub root: String,
    pub at_ms: i64,
    pub path: Vec<String>,
    pub why: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Claims {
    pub claims: Vec<Claim>,
    pub rejected: Vec<Value>,
}

fn field<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v[k].as_str().map(str::trim).filter(|s| !s.is_empty())
}

/// Every claim, folded from the ledger in sequence order.
pub fn fold(rows: &[Observation]) -> Claims {
    let mut by_cid: BTreeMap<String, Claim> = BTreeMap::new();
    let mut rejected = Vec::new();
    let reject = |rejected: &mut Vec<Value>, o: &Observation, why: &str| rejected.push(serde_json::json!({"id": o.id, "why": why}));
    let resolve = |by_cid: &BTreeMap<String, Claim>, name: &str| -> std::result::Result<String, String> {
        let name = name.trim().to_ascii_lowercase();
        if name.len() < 8 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{name} is not a cid (at least 8 hex)"));
        }
        let hits: Vec<&String> = by_cid.keys().filter(|k| k.starts_with(&name)).collect();
        match hits.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(format!("no claim {name}")),
            _ => Err(format!("{name} names {} claims", hits.len())),
        }
    };
    for o in rows {
        let Some(cargo) = o.cargo.as_deref().and_then(|c| serde_json::from_str::<Value>(c).ok()) else {
            reject(&mut rejected, o, "cargo is not JSON");
            continue;
        };
        if o.tags.iter().any(|t| t == "claim:mint") {
            let (Some(statement), Some(kind), Some(status)) = (field(&cargo, "statement"), field(&cargo, "type"), field(&cargo, "status")) else {
                reject(&mut rejected, o, "a mint needs statement, type and status");
                continue;
            };
            if !TYPES.contains(&kind) || !STATUSES.contains(&status) {
                reject(&mut rejected, o, "type or status is not one of the named values");
                continue;
            }
            let cid = cid_of(statement);
            if by_cid.contains_key(&cid) {
                reject(&mut rejected, o, "already minted: the same statement has one cid");
                continue;
            }
            let sources = cargo["sources"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            by_cid.insert(cid.clone(), Claim {
                cid,
                statement: canonical(statement),
                kind: kind.to_owned(),
                status: status.to_owned(),
                room: o.channel.clone(),
                minted_by: o.id.clone(),
                author: o.author.clone(),
                minted_ms: o.created_ms,
                sources,
                sources_removed: Vec::new(),
                row: field(&cargo, "row").map(str::to_owned),
                out: vec![],
                inbound: vec![],
                history: vec![Move { status: status.to_owned(), by: o.id.clone(), author: o.author.clone(), at_ms: o.created_ms, why: None }],
                standing: status.to_owned(),
                fallen: None,
                superseded_by: None,
            });
        } else if o.tags.iter().any(|t| t == "claim:edge") {
            let (Some(from), Some(to), Some(kind)) = (field(&cargo, "from"), field(&cargo, "to"), field(&cargo, "kind")) else {
                reject(&mut rejected, o, "an edge needs from, to and kind");
                continue;
            };
            if !EDGES.contains(&kind) {
                reject(&mut rejected, o, "kind is not one of supports, contradicts, supersedes, is-same-as, cites");
                continue;
            }
            match (resolve(&by_cid, from), resolve(&by_cid, to)) {
                (Ok(f), Ok(t)) if f != t => {
                    by_cid.get_mut(&f).unwrap().out.push(Edge { kind: kind.into(), cid: t.clone(), by: o.id.clone() });
                    by_cid.get_mut(&t).unwrap().inbound.push(Edge { kind: kind.into(), cid: f, by: o.id.clone() });
                }
                (Ok(_), Ok(_)) => reject(&mut rejected, o, "an edge from a claim to itself"),
                (Err(e), _) | (_, Err(e)) => reject(&mut rejected, o, &e),
            }
        } else if o.tags.iter().any(|t| t == "claim:status") {
            let (Some(name), Some(status)) = (field(&cargo, "cid"), field(&cargo, "status")) else {
                reject(&mut rejected, o, "a status needs cid and status");
                continue;
            };
            if !STATUSES.contains(&status) {
                reject(&mut rejected, o, "status is not verified, refuted or open");
                continue;
            }
            match resolve(&by_cid, name) {
                Ok(c) => {
                    let claim = by_cid.get_mut(&c).unwrap();
                    claim.status = status.to_owned();
                    claim.history.push(Move { status: status.into(), by: o.id.clone(), author: o.author.clone(), at_ms: o.created_ms, why: field(&cargo, "why").map(str::to_owned) });
                }
                Err(e) => reject(&mut rejected, o, &e),
            }
        } else if o.tags.iter().any(|t| t == "claim:source") {
            // A source added after the mint (Blaze, 30 Oct 2026), kept once each, and only from the claim's own room: a seat
            // writing in one room never edits another room's claims. `remove` takes off a source the claim carries, matched
            // exactly; the removal stays on the claim with its record. Removing one it does not carry is refused, whole, with
            // the reason: a typo must not pass for a correction.
            let strs = |k: &str| -> Vec<String> {
                cargo[k].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()).unwrap_or_default()
            };
            let (add, remove) = (strs("sources"), strs("remove"));
            let Some(name) = field(&cargo, "cid").filter(|_| !add.is_empty() || !remove.is_empty()) else {
                reject(&mut rejected, o, "a source needs cid and sources or remove");
                continue;
            };
            match resolve(&by_cid, name) {
                Ok(c) => {
                    let claim = by_cid.get_mut(&c).unwrap();
                    if claim.room != o.channel {
                        reject(&mut rejected, o, "a source is added or removed in the claim's own room");
                        continue;
                    }
                    if let Some(missing) = remove.iter().find(|r| !claim.sources.contains(r)) {
                        reject(&mut rejected, o, &format!("the claim carries no source {missing}"));
                        continue;
                    }
                    for r in &remove {
                        claim.sources.retain(|x| x != r);
                        claim.sources_removed.push(SourceRemoved { source: r.clone(), by: o.id.clone(), author: o.author.clone(), at_ms: o.created_ms, why: field(&cargo, "why").map(str::to_owned) });
                    }
                    for s in add {
                        if !claim.sources.contains(&s) {
                            claim.sources.push(s);
                        }
                    }
                }
                Err(e) => reject(&mut rejected, o, &e),
            }
        }
    }
    // A claim superseded by one that is not refuted reads under its successor (the openfab pattern, Round 50).
    let live: BTreeMap<String, String> = by_cid.iter().map(|(k, c)| (k.clone(), c.status.clone())).collect();
    for c in by_cid.values_mut() {
        c.superseded_by = c.inbound.iter().find(|e| e.kind == "supersedes" && live.get(&e.cid).is_some_and(|s| s != "refuted")).map(|e| e.cid.clone());
    }
    cascade(&mut by_cid);
    Claims { claims: by_cid.into_values().collect(), rejected }
}

/// What each claim rests on: the claims it cites, and the claims that support it.
fn bases(c: &Claim) -> impl Iterator<Item = &String> {
    c.out.iter().filter(|e| e.kind == "cites").map(|e| &e.cid).chain(c.inbound.iter().filter(|e| e.kind == "supports").map(|e| &e.cid))
}

fn last_move_ms(c: &Claim) -> i64 {
    c.history.last().map_or(c.minted_ms, |m| m.at_ms)
}

/// Sets `standing` and `fallen` on every claim (module docs). The earliest fall wins; cycles end because a claim is resolved once.
fn cascade(by_cid: &mut BTreeMap<String, Claim>) {
    fn fall_of(cid: &str, by_cid: &BTreeMap<String, Claim>, memo: &mut BTreeMap<String, Option<Fallen>>, visiting: &mut Vec<String>) -> Option<Fallen> {
        if let Some(f) = memo.get(cid) {
            return f.clone();
        }
        let c = by_cid.get(cid)?;
        if visiting.iter().any(|v| v == cid) {
            return None;
        }
        visiting.push(cid.to_owned());
        let mut best: Option<Fallen> = None;
        for b in bases(c) {
            let Some(base) = by_cid.get(b) else { continue };
            // The base's own fall: refuted outright (when), or standing on a fall of its own.
            let own = (base.status == "refuted").then(|| Fallen { root: base.cid.clone(), at_ms: last_move_ms(base), path: vec![base.cid.clone()], why: String::new() });
            let deeper = fall_of(b, by_cid, memo, visiting).map(|mut f| {
                f.path.insert(0, base.cid.clone());
                f
            });
            for f in [own, deeper].into_iter().flatten() {
                if best.as_ref().is_none_or(|x| f.at_ms < x.at_ms) {
                    best = Some(f);
                }
            }
        }
        visiting.pop();
        // A move on this claim after the fall is its re-verification; a refuted claim does not stand on anything.
        let best = best.filter(|f| c.status != "refuted" && last_move_ms(c) < f.at_ms).map(|mut f| {
            f.why = format!("root claim {} refuted — re-verification required", &f.root[..12.min(f.root.len())]);
            f
        });
        memo.insert(cid.to_owned(), best.clone());
        best
    }
    let mut memo = BTreeMap::new();
    let cids: Vec<String> = by_cid.keys().cloned().collect();
    for cid in &cids {
        let f = fall_of(cid, by_cid, &mut memo, &mut Vec::new());
        let c = by_cid.get_mut(cid).unwrap();
        c.standing = if f.is_some() { "open".into() } else { c.status.clone() };
        c.fallen = f;
    }
}

/// The claims, optionally of one room, one status, or one cid (or prefix), oldest first.
pub fn read(s: &Store, room: Option<&str>, status: Option<&str>, cid: Option<&str>) -> Result<Claims> {
    let rows = s.tagged_any(&TAGS)?;
    let mut all = fold(&rows);
    all.claims.retain(|c| room.is_none_or(|r| c.room == r) && status.is_none_or(|st| c.status == st) && cid.is_none_or(|p| c.cid.starts_with(&p.to_ascii_lowercase())));
    all.claims.sort_by_key(|c| c.minted_ms);
    Ok(all)
}
