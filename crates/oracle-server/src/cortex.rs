//! The cortex widened (docs/CORTEX.md, Blaze's decision 2026-10-18): nodes, marks and links beside the claims of
//! `claims.rs`, which this file does not touch. A **node** is something that lasts and has a page (question, entity,
//! concept); a **mark** is evidence that joins a source span (words as they were said) to nodes, with labels, intents, a
//! project and a verdict; a **link** joins a mark or a node to a node. Nothing is stored here: every value is folded on
//! read from signed messages tagged `cortex:node`, `cortex:mark`, `cortex:link`, `cortex:verdict`. A message whose cargo
//! does not parse or names nothing known is listed under `rejected` with the reason, never dropped.
use crate::store::{Observation, Result, Store};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Noor's schema (Blaze, 25 Oct 2026, docs/CORTEX.md "The graph"): an `observation` is an approved span of someone's
/// words, its node id cut from where it was said, not from a title; a `tension` is two things that pull apart, open until
/// a `completes` link resolves it.
pub const KINDS: [&str; 5] = ["question", "entity", "concept", "observation", "tension"];
/// `said-by` joins an observation to the entity who said it (Blaze, 25 Oct: every node under its author, so seats minting
/// nodes cannot blur whose words they are); `refines` joins a later observation to the earlier one it changes (plans
/// are living: a refined observation reads Superseded, the newest step of the refinement is current).
/// `supports` (26 Oct): a premise under its conclusion, above all a Belief under the decision or rule it underpins.
pub const LINKS: [&str; 13] = ["about", "asks", "answers", "depends-on", "is-same-as", "part-of", "generates", "contradicts", "constrains", "completes", "said-by", "refines", "supports"];
pub const VERDICTS: [&str; 6] = ["approve", "reject", "note", "answered", "dropped", "open"];
pub const TAGS: [&str; 4] = ["cortex:node", "cortex:mark", "cortex:link", "cortex:verdict"];

/// A title folded for its id: lowercase, every run of other characters one `-`, trimmed.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for ch in title.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

/// A node's id: sha256 of `kind:` and its slug, so the same title names the same node from any writer.
pub fn nid_of(kind: &str, title: &str) -> String {
    oracle_proto::sha_hex(format!("{kind}:{}", slug(title)).as_bytes())
}

/// An observation's id: where it was said (the episode's message id and the span), so two different spans that read
/// alike stay two observations, and re-publishing one span is the same node.
pub fn observation_nid(message: &str, start: u64, end: u64) -> String {
    oracle_proto::sha_hex(format!("observation:{message}:{start}:{end}").as_bytes())
}

/// The label a node's place in the graph implies (Noor, "topology implies taxonomy"): nobody assigns it.
/// Tension: Decision once something completes it, else Question. Question nodes: Question. Otherwise, by its live links:
/// constrains anything → Rule; generates two or more → Vision; completes something → Solution; a step in a chain
/// (depends-on going out and coming in, or out to a node that itself depends on another) → Plan; generates one → Idea
/// that grew; any other link → Observation; no link at all → Idea.
/// Where the links decide nothing (a bare Observation or Idea), the first label its author approved stands, so a Rule
/// Blaze approved reads Rule until the graph says more.
pub fn form_with_labels(topology: &'static str, labels: &[String]) -> String {
    // The label its author approved stands (Blaze, 25 Oct: a Problem that constrains a plan is still a Problem); the
    // links decide only when nobody labelled it, and say Superseded when a later version replaced it.
    match (topology, labels.first()) {
        ("Superseded", _) => topology.to_owned(),
        (_, Some(l)) => l.clone(),
        _ => topology.to_owned(),
    }
}

pub fn form_of(kind: &str, out: &[Link], inbound: &[Link], completed: bool, chain_next: bool) -> &'static str {
    let n = |ls: &[Link], k: &str| ls.iter().filter(|l| l.kind == k).count();
    if n(inbound, "refines") > 0 {
        return "Superseded";
    }
    let out: Vec<Link> = out.iter().filter(|l| l.kind != "said-by" && l.kind != "refines").cloned().collect();
    let out = out.as_slice();
    let inbound: Vec<Link> = inbound.iter().filter(|l| l.kind != "said-by").cloned().collect();
    let inbound = inbound.as_slice();
    match kind {
        "tension" => if completed { "Decision" } else { "Question" },
        "question" => "Question",
        "entity" => "Entity",
        _ if n(out, "constrains") > 0 => "Rule",
        _ if n(out, "generates") >= 2 => "Vision",
        _ if n(out, "completes") > 0 => "Solution",
        _ if n(out, "asks") > 0 => "Question",
        _ if n(out, "depends-on") > 0 && (n(inbound, "depends-on") > 0 || chain_next) => "Plan",
        _ if out.is_empty() && inbound.is_empty() => "Idea",
        "concept" => "Concept",
        _ => "Observation",
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    pub verdict: String,
    pub by: String,
    pub signed_by: String,
    pub at_ms: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    /// The page the person acted in, when a seat signs for them (`cases-page`): the claim an attestation makes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Link {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub message: String,
    /// How central the target is to the source, 0 to 1 (cargo `weight`; Blaze, 26 Oct: "when there is something very
    /// core to AXXIS, we mark it as very important; mildly related at a lower score; and if we later connect it to
    /// something, we mark it up"). Set when the link is drawn; a later link record with the same from, to and kind
    /// replaces it, so re-weighing is one record. Absent means unweighed (read as [`DEFAULT_WEIGHT`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
}

/// The weight an unweighed link counts as: between related and strong.
pub const DEFAULT_WEIGHT: f64 = 0.5;

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub nid: String,
    pub kind: String,
    pub title: String,
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wikidata: Option<String>,
    pub room: String,
    pub minted_by: String,
    /// The record of another node that replaced this one (`supersedes:<a record of this node>`): a superseded node
    /// stays on record and cites nothing. A revision of the same node (same nid) never supersedes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    pub author: String,
    pub minted_ms: i64,
    /// An observation's words and where they were said: `{message, start, end}` in the episode, and the quote itself.
    #[serde(skip_serializing_if = "Value::is_null")]
    pub source: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    /// The label shown: the approved label, or Superseded, or (unlabelled) what its links imply. Worked out on every read.
    pub form: String,
    /// What its links alone imply (`form_of`), beside the approved label, so the two can be compared.
    pub shape: String,
    /// The labels its author approved (an observation's cargo `labels`): the fallback when the links decide nothing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Who drafted it, from an observation's cargo `drafted_by` (a seat; Brain cases since 25 Oct).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drafted_by: Option<String>,
    /// The drafter's interpretation, not the source quote or a verified execution receipt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub intents: Vec<String>,
    /// approve / reject, newest wins.
    pub verdict: Option<Verdict>,
    /// A question's state: open (the default), answered, dropped; newest wins.
    pub status: Option<Verdict>,
    pub notes: Vec<Verdict>,
    /// Live marks linked to this node, with the link's kind.
    pub marks: Vec<(String, String)>,
    pub out: Vec<Link>,
    #[serde(rename = "in")]
    pub inbound: Vec<Link>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Mark {
    pub id: String,
    pub room: String,
    /// Who acted: cargo `by` (the person who drew it in a page), else the signer.
    pub by: String,
    pub signed_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub created_ms: i64,
    pub source: Value,
    pub labels: Vec<String>,
    pub intents: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub origin: Value,
    pub supersedes: Vec<String>,
    pub superseded_by: Option<String>,
    pub verdict: Option<Verdict>,
    pub notes: Vec<Verdict>,
    pub links: Vec<Link>,
}

#[derive(Debug, Default, Serialize)]
pub struct Cortex {
    pub nodes: Vec<Node>,
    pub marks: Vec<Mark>,
    pub rejected: Vec<Value>,
}

fn field<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v[k].as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)).collect()).unwrap_or_default()
}

/// A record in the ledger a link may point at (Blaze, 25 Oct: link what is marked to what Oracle already observed):
/// an observation id such as `OBS-axxis-20260320-034`, or an imported id with a known prefix. The edge says how an
/// approved observation relates to that record (it generates, refines, contradicts ... it).
pub fn is_record_id(s: &str) -> bool {
    const PREFIXES: [&str; 6] = ["OBS-", "CE-", "action-", "learning-", "decision-", "context-"];
    s.len() <= 120 && PREFIXES.iter().any(|p| s.starts_with(p) && s.len() > p.len())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_nid(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Everything, folded from the ledger in sequence order.
pub fn fold(rows: &[Observation]) -> Cortex {
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    let mut marks: BTreeMap<String, Mark> = BTreeMap::new();
    let mut links: BTreeMap<(String, String, String), Option<Link>> = BTreeMap::new();
    let mut rejected = Vec::new();
    // Every node record to its node, and each node record's `supersedes:` claims (new record, new node, old record).
    let mut node_records: BTreeMap<String, String> = BTreeMap::new();
    let mut node_supersedes: Vec<(String, String, String)> = Vec::new();
    let reject = |rejected: &mut Vec<Value>, o: &Observation, why: &str| rejected.push(serde_json::json!({"id": o.id, "why": why}));
    let verdict_of = |o: &Observation, cargo: &Value, verdict: &str| Verdict {
        verdict: verdict.to_owned(),
        by: field(cargo, "by").unwrap_or(&o.author).to_owned(),
        signed_by: o.author.clone(),
        at_ms: o.created_ms,
        message: o.id.clone(),
        text: field(cargo, "text").map(str::to_owned),
        r#ref: field(cargo, "ref").map(str::to_owned),
        via: field(cargo, "via").map(str::to_owned),
    };
    for o in rows {
        let has = |t: &str| o.tags.iter().any(|x| x == t);
        let Some(cargo) = o.cargo.as_deref().and_then(|c| serde_json::from_str::<Value>(c).ok()) else {
            reject(&mut rejected, o, "cargo is not JSON");
            continue;
        };
        if has("cortex:node") {
            let (Some(kind), Some(title)) = (field(&cargo, "kind"), field(&cargo, "title")) else {
                reject(&mut rejected, o, "a node needs kind and title");
                continue;
            };
            if !KINDS.contains(&kind) || slug(title).is_empty() {
                reject(&mut rejected, o, "kind is not question, entity, concept, observation or tension, or the title has no letters");
                continue;
            }
            let source = &cargo["source"];
            let span = field(source, "message").zip(source["start"].as_u64().zip(source["end"].as_u64())).filter(|(_, (s, e))| s < e);
            if kind == "observation" && (span.is_none() || field(&cargo, "quote").is_none()) {
                reject(&mut rejected, o, "an observation needs source {message, start < end} and its quote");
                continue;
            }
            let nid = match span {
                Some((m, (st, en))) if kind == "observation" => observation_nid(m, st, en),
                _ => nid_of(kind, title),
            };
            node_records.insert(o.id.clone(), nid.clone());
            for old in o.tags.iter().filter_map(|t| t.strip_prefix("supersedes:")) {
                node_supersedes.push((o.id.clone(), nid.clone(), old.to_owned()));
            }
            match nodes.get_mut(&nid) {
                Some(n) => {
                    if n.title != title && !n.aliases.iter().any(|a| a == title) {
                        n.aliases.push(title.to_owned());
                    }
                    if n.wikidata.is_none() {
                        n.wikidata = field(&cargo, "wikidata").map(str::to_owned);
                    }
                    // A later record of the same node revises what can change after approval: the note, the labels, the
                    // intents (Brain cases, 3 Nov 2026: 18 approved notes went out with unfilled placeholders and could only be
                    // corrected beside the node). Rows come oldest first, so the newest field present wins; the first record
                    // stays the node's mint, and its source and quote never move (they are what the nid is made of).
                    if let Some(note) = field(&cargo, "note") {
                        n.note = Some(note.to_owned());
                    }
                    if n.kind == "observation" && cargo["labels"].is_array() {
                        n.labels = strings(&cargo["labels"]);
                    }
                    if cargo["intents"].is_array() {
                        n.intents = strings(&cargo["intents"]);
                    }
                }
                None => {
                    nodes.insert(nid.clone(), Node {
                        nid, kind: kind.into(), title: title.into(), aliases: vec![], wikidata: field(&cargo, "wikidata").map(str::to_owned),
                        room: o.channel.clone(), minted_by: o.id.clone(), superseded_by: None, author: o.author.clone(), minted_ms: o.created_ms,
                        source: if kind == "observation" { source.clone() } else { Value::Null },
                        quote: field(&cargo, "quote").filter(|_| kind == "observation").map(str::to_owned), form: String::new(),
                        labels: if kind == "observation" { strings(&cargo["labels"]) } else { vec![] }, shape: String::new(),
                        drafted_by: field(&cargo, "drafted_by").map(str::to_owned),
                        note: field(&cargo, "note").map(str::to_owned), speaker: field(&cargo, "speaker").map(str::to_owned),
                        project: field(&cargo, "project").map(str::to_owned), intents: strings(&cargo["intents"]),
                        verdict: None, status: None, notes: vec![], marks: vec![], out: vec![], inbound: vec![],
                    });
                }
            }
        } else if has("cortex:mark") {
            let source = &cargo["source"];
            let ok_source = field(source, "message").is_some()
                && source["start"].as_u64().zip(source["end"].as_u64()).is_some_and(|(s, e)| s < e);
            let labels = strings(&cargo["labels"]);
            if !ok_source || labels.is_empty() {
                reject(&mut rejected, o, "a mark needs source {message, start < end} and at least one label");
                continue;
            }
            let supersedes: Vec<String> = o.tags.iter().filter_map(|t| t.strip_prefix("supersedes:")).map(str::to_owned).collect();
            for old in &supersedes {
                if let Some(m) = marks.get_mut(old) {
                    m.superseded_by = Some(o.id.clone());
                }
            }
            marks.insert(o.id.clone(), Mark {
                id: o.id.clone(), room: o.channel.clone(), by: field(&cargo, "by").unwrap_or(&o.author).to_owned(), signed_by: o.author.clone(),
                via: field(&cargo, "via").map(str::to_owned), created_ms: o.created_ms, source: source.clone(), labels,
                intents: strings(&cargo["intents"]), project: field(&cargo, "project").map(str::to_owned), note: field(&cargo, "note").map(str::to_owned),
                origin: cargo["origin"].clone(), supersedes, superseded_by: None, verdict: None, notes: vec![], links: vec![],
            });
        } else if has("cortex:link") {
            let (Some(from), Some(to), Some(kind)) = (field(&cargo, "from"), field(&cargo, "to"), field(&cargo, "kind")) else {
                reject(&mut rejected, o, "a link needs from, to and kind");
                continue;
            };
            if !LINKS.contains(&kind) {
                reject(&mut rejected, o, "kind is not one of the twelve link kinds (docs/CORTEX.md)");
                continue;
            }
            let from_known = marks.contains_key(from) || nodes.contains_key(from);
            if !from_known || !(nodes.contains_key(to) || is_nid(to) || is_record_id(to)) || from == to {
                reject(&mut rejected, o, "from must be a known mark or node, to a node, and not the same");
                continue;
            }
            let key = (from.to_owned(), to.to_owned(), kind.to_owned());
            let removed = cargo["removed"].as_bool().unwrap_or(false);
            let weight = cargo["weight"].as_f64().filter(|w| w.is_finite()).map(|w| w.clamp(0.0, 1.0));
            links.insert(key, (!removed).then(|| Link { from: from.into(), to: to.into(), kind: kind.into(), message: o.id.clone(), weight }));
        } else if has("cortex:verdict") {
            let (Some(target), Some(v)) = (field(&cargo, "target"), field(&cargo, "verdict")) else {
                reject(&mut rejected, o, "a verdict needs target and verdict");
                continue;
            };
            if !VERDICTS.contains(&v) {
                reject(&mut rejected, o, "verdict is not approve, reject, note, answered, dropped or open");
                continue;
            }
            let vd = verdict_of(o, &cargo, v);
            if let Some(m) = marks.get_mut(target) {
                match v {
                    "approve" | "reject" => m.verdict = Some(vd),
                    "note" => m.notes.push(vd),
                    _ => { reject(&mut rejected, o, "answered, dropped and open are for question nodes"); }
                }
            } else if let Some(n) = nodes.get_mut(target) {
                match v {
                    "approve" | "reject" => n.verdict = Some(vd),
                    "note" => n.notes.push(vd),
                    _ if n.kind == "question" => n.status = Some(vd),
                    _ => { reject(&mut rejected, o, "answered, dropped and open are for question nodes"); }
                }
            } else {
                reject(&mut rejected, o, "the target is no known mark or node");
            }
        }
    }
    for (new, new_nid, old) in &node_supersedes {
        if let Some(old_nid) = node_records.get(old).filter(|o| *o != new_nid) {
            if let Some(n) = nodes.get_mut(old_nid) {
                n.superseded_by.get_or_insert_with(|| new.clone());
            }
        }
    }
    // Live links onto marks and nodes; a superseded mark keeps its links on record but leaves the node pages.
    let live: Vec<Link> = links.into_values().flatten().collect();
    for l in &live {
        if let Some(m) = marks.get_mut(&l.from) {
            m.links.push(l.clone());
        }
        if let Some(n) = nodes.get_mut(&l.from) {
            n.out.push(l.clone());
        }
    }
    for l in &live {
        let is_mark = marks.get(&l.from);
        if let Some(n) = nodes.get_mut(&l.to) {
            match is_mark {
                Some(m) if m.superseded_by.is_none() => n.marks.push((l.from.clone(), l.kind.clone())),
                Some(_) => {}
                None => n.inbound.push(l.clone()),
            }
        }
    }
    // The label each node's place implies: a tension is completed when a live `completes` link points at it; a node is
    // a step when the node it depends on itself depends on another.
    let depends: BTreeSet<&str> = live.iter().filter(|l| l.kind == "depends-on").map(|l| l.from.as_str()).collect();
    let completed: BTreeSet<&str> = live.iter().filter(|l| l.kind == "completes").map(|l| l.to.as_str()).collect();
    let forms: Vec<(String, &'static str)> = nodes.values().map(|n| {
        let chain_next = n.out.iter().any(|l| l.kind == "depends-on" && depends.contains(l.to.as_str()));
        (n.nid.clone(), form_of(&n.kind, &n.out, &n.inbound, completed.contains(n.nid.as_str()), chain_next))
    }).collect();
    for (nid, shape) in forms {
        if let Some(n) = nodes.get_mut(&nid) {
            n.form = form_with_labels(shape, &n.labels);
            n.shape = shape.to_owned();
        }
    }
    Cortex { nodes: nodes.into_values().collect(), marks: marks.into_values().collect(), rejected }
}

/// One Blaze-approved observation node that points at a record or a node (docs/CORTEX.md "Cited by"). A citation is a
/// reference (`about`, `generates`, `part-of` ...), never an endorsement: it says an approved reading links here.
#[derive(Debug, Clone, Serialize)]
pub struct Cite {
    /// The record that minted the citing node.
    pub node: String,
    pub nid: String,
    /// Who said the cited words, from the node's source span: `blaze` (his prompt), `claude` (a reply), or `unknown`.
    /// The speaker of the source, not the seat that published it.
    pub said_by: String,
    /// The episode the words come from (`source.chat`): the citation's family. Unknown families never merge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat: Option<String>,
    pub kinds: Vec<String>,
    /// The approval the node carries: the verdict record, who it names, who signed it, the page it names, and its
    /// [`Attestation`] class.
    pub approval: Value,
}

/// How an approval is known to be the owner's (docs/CORTEX.md "Cited by"), decided by the store against the key
/// registry in the same snapshot as the fold. Only `Owner` and `Attested` count; the others are reported, never counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Attestation {
    /// Signed by a key that held the owner role when it was written, naming that owner; the envelope verifies.
    Owner,
    /// Signed by a seat the owner registered as an attester (`oracle gateway add <seat> --routes attest`), naming an
    /// owner and the page he acted in (`via`); the envelope verifies under the seat's key.
    Attested,
    /// A seat approving in its own name: an agent's approval, never the owner's.
    Agent,
    /// Anything else: an unregistered seat naming the owner, no page named, an imported row, a key not held then, or an
    /// envelope that does not verify.
    Unverified,
}

impl Attestation {
    pub fn as_str(self) -> &'static str {
        match self {
            Attestation::Owner => "owner",
            Attestation::Attested => "attested",
            Attestation::Agent => "agent",
            Attestation::Unverified => "unverified",
        }
    }
    pub fn counts(self) -> bool {
        matches!(self, Attestation::Owner | Attestation::Attested)
    }
}

/// Inbound citations from Blaze-approved observation nodes, by target (a record id or a node id), from the live fold:
/// withdrawn links and archived nodes are already gone, a node whose newest verdict is not approve counts zero, and a
/// node citing one target through several links counts once with every kind it uses.
pub struct Citations {
    by_target: BTreeMap<String, BTreeMap<String, Cite>>,
    /// Approved nodes whose approval does not count (agent or unverified), by target, with the class: reported only.
    uncounted: BTreeMap<String, BTreeMap<String, Attestation>>,
    /// The record that minted each approved observation node, to its node id: a hit that is itself such a node is cited
    /// through its node id as well as its record id.
    nid_of_record: BTreeMap<String, String>,
}

impl Citations {
    /// `class` names each approval's [`Attestation`] by its verdict record; a verdict it does not name is unverified.
    pub fn build(c: &Cortex, class: &BTreeMap<String, Attestation>) -> Citations {
        let mut by_target: BTreeMap<String, BTreeMap<String, Cite>> = BTreeMap::new();
        let mut uncounted: BTreeMap<String, BTreeMap<String, Attestation>> = BTreeMap::new();
        let mut nid_of_record = BTreeMap::new();
        for n in c.nodes.iter().filter(|n| n.kind == "observation" && n.superseded_by.is_none()) {
            let Some(v) = n.verdict.as_ref().filter(|v| v.verdict == "approve") else { continue };
            let att = class.get(&v.message).copied().unwrap_or(Attestation::Unverified);
            if !att.counts() {
                for l in n.out.iter().filter(|l| l.kind != "said-by") {
                    uncounted.entry(l.to.clone()).or_default().insert(n.minted_by.clone(), att);
                }
                continue;
            }
            nid_of_record.insert(n.minted_by.clone(), n.nid.clone());
            let said_by = match (n.source["reply"].as_str().filter(|s| !s.is_empty()), n.source["prompt"].as_str()) {
                (Some(_), _) => "claude",
                (None, Some(_)) => "blaze",
                _ => "unknown",
            };
            let chat = n.source["chat"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
            for l in &n.out {
                if l.kind == "said-by" {
                    continue;
                }
                let cite = by_target.entry(l.to.clone()).or_default().entry(n.minted_by.clone()).or_insert_with(|| Cite {
                    node: n.minted_by.clone(), nid: n.nid.clone(), said_by: said_by.into(), chat: chat.clone(), kinds: vec![],
                    approval: serde_json::json!({"verdict": v.message, "by": v.by, "signed_by": v.signed_by, "via": v.via, "class": att.as_str()}),
                });
                if !cite.kinds.contains(&l.kind) {
                    cite.kinds.push(l.kind.clone());
                }
            }
        }
        Citations { by_target, uncounted, nid_of_record }
    }

    /// Who cites the record `id` (and, when it minted an approved observation, that node): `None` when no approved node
    /// links here at all. `count` is the counted nodes only (owner and attested); `attestation` gives each class.
    /// `by` lists at most [`CITE_LIST`] counted nodes: one node per family first, then the rest, each in node-id order.
    pub fn of(&self, id: &str) -> Option<Value> {
        let mut cites: BTreeMap<&str, Cite> = BTreeMap::new();
        let mut uncounted: BTreeMap<&str, Attestation> = BTreeMap::new();
        for t in std::iter::once(id).chain(self.nid_of_record.get(id).map(String::as_str)) {
            for (node, a) in self.uncounted.get(t).into_iter().flatten() {
                if node != id {
                    uncounted.insert(node.as_str(), *a);
                }
            }
        }
        for t in std::iter::once(id).chain(self.nid_of_record.get(id).map(String::as_str)) {
            for (node, c) in self.by_target.get(t).into_iter().flatten() {
                if node == id {
                    continue;
                }
                let e = cites.entry(node.as_str()).or_insert_with(|| Cite { kinds: vec![], ..c.clone() });
                for k in &c.kinds {
                    if !e.kinds.contains(k) {
                        e.kinds.push(k.clone());
                    }
                }
            }
        }
        if cites.is_empty() && uncounted.is_empty() {
            return None;
        }
        let class_count = |a: Attestation| {
            cites.values().filter(|c| c.approval["class"] == a.as_str()).count() + uncounted.values().filter(|x| **x == a).count()
        };
        let attestation = serde_json::json!({
            "owner": class_count(Attestation::Owner), "attested": class_count(Attestation::Attested),
            "agent": class_count(Attestation::Agent), "unverified": class_count(Attestation::Unverified),
        });
        let family = |c: &Cite| c.chat.clone().unwrap_or_else(|| format!("node:{}", c.node));
        let families: BTreeSet<String> = cites.values().map(family).collect();
        let mut seen = BTreeSet::new();
        let (mut first, mut rest): (Vec<&Cite>, Vec<&Cite>) = (vec![], vec![]);
        for c in cites.values() {
            if seen.insert(family(c)) { first.push(c) } else { rest.push(c) }
        }
        let by: Vec<&Cite> = first.into_iter().chain(rest).take(CITE_LIST).collect();
        Some(serde_json::json!({"status": "measured", "count": cites.len(), "families": families.len(), "by": by, "truncated": cites.len() > CITE_LIST, "attestation": attestation}))
    }
}

/// How many citing nodes a hit lists (the count is always whole).
pub const CITE_LIST: usize = 5;

/// Words that never make a concept match on their own and never count as the content of a query.
const STOP: [&str; 58] = [
    "a", "an", "the", "of", "to", "in", "on", "for", "and", "or", "is", "are", "was", "were", "be", "it", "its", "this",
    "that", "what", "which", "who", "how", "why", "when", "where", "do", "does", "did", "we", "our", "us", "i", "me",
    "my", "you", "your", "about", "with", "as", "at", "by", "from", "have", "has", "had", "best", "most", "all", "any",
    "so", "far", "up", "until", "now", "there", "can", "should",
];

/// Query words that name a form (an approved label): "our best breakthroughs", "what did we decide".
const FORM_WORDS: [(&str, &str); 25] = [
    ("breakthrough", "Breakthrough"), ("breakthroughs", "Breakthrough"), ("decision", "Decision"), ("decisions", "Decision"),
    ("decided", "Decision"), ("decide", "Decision"), ("rule", "Rule"), ("rules", "Rule"), ("vision", "Vision"),
    ("visions", "Vision"), ("plan", "Plan"), ("plans", "Plan"), ("discovery", "Discovery"), ("discoveries", "Discovery"),
    ("problem", "Problem"), ("problems", "Problem"), ("prediction", "Prediction"), ("predictions", "Prediction"),
    ("pattern", "Pattern"), ("patterns", "Pattern"), ("solution", "Solution"), ("solutions", "Solution"),
    ("belief", "Belief"), ("beliefs", "Belief"), ("believe", "Belief"), // Belief, the 16th label (Blaze, 26 Oct)
];

/// Lowercase words of a text: letters and digits, everything else a break.
pub fn words(text: &str) -> Vec<String> {
    text.to_lowercase().split(|ch: char| !ch.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}

/// The concept and entity layer of the hand-built graph, for recall (docs/SEARCH.md "Through the graph"). A query that
/// names a concept or an entity (its title or an alias) reaches the approved observations linked `about` it, whatever
/// words those observations use; a query that names a form ("breakthroughs") reaches the approved observations with
/// that label. Built from the same fold and approval classes as [`Citations`]: only owner and attested approvals count.
#[derive(Default)]
pub struct ConceptIndex {
    /// Each title and alias as its words, with the node it names; longest first.
    phrases: Vec<(Vec<String>, usize)>,
    /// Concept and entity nodes: (nid, title, is an entity).
    nodes: Vec<(String, String, bool)>,
    /// For each node, the records that minted the approved observations about it, with the link's weight.
    members: Vec<Vec<(String, f64)>>,
    /// Approved observations by label, the minting records.
    by_form: BTreeMap<String, Vec<String>>,
    /// How many approved observations link to each approved observation (any kind but `part-of`), by minting record:
    /// the graph's own measure of what later readings built on.
    pub degree: BTreeMap<String, usize>,
    /// The records that minted observations whose approval verifies as the owner's (owner or attested): the one test of
    /// "approved" recall uses (Emmy's review: never the unchecked `approved_by` field of a node's cargo).
    pub approved: BTreeSet<String>,
    /// Each approved observation's quote in its comparable form ([`comparable`]), with its minting record, for quotes of at
    /// least [`COPY_MIN`] characters: recall puts the verified node in place of any copy of the same passage (an import, an
    /// extract, a conversation index line) that matched better (Blaze, 30 Oct 2026: "the approved marks are actual verified
    /// cortex nodes that should hit on every oracle recall").
    pub quotes: Vec<(String, String)>,
    /// Each concept or entity node's [`certification`], in the order of `nodes`.
    pub cert: Vec<Certification>,
}

/// The shortest approved quote recall compares with copies: a shorter one ("Yes", "the state") would find itself everywhere.
pub const COPY_MIN: usize = 40;

/// A passage as recall compares it with its copies: lowercase words joined by one space, so an import's line breaks,
/// markdown and curly quotes do not hide that it is the same passage.
pub fn comparable(text: &str) -> String {
    words(text).join(" ")
}

/// What a query reaches through the graph: the nodes it named, the form it named, and the approved observations, best
/// first (the ones both a named node and the named form reach, then by the weight of their link to a named node, then by
/// degree).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct GraphMatch {
    pub nodes: Vec<String>,
    pub form: Option<String>,
    pub records: Vec<String>,
    /// The query's content words that no named node or form covers: with two or more the query is about more than the
    /// node it names ("dot is 1 bit not 153 bytes" names DOT but asks about something else), and recall keeps only the
    /// reached observations its words also match.
    pub unnamed: usize,
}

impl ConceptIndex {
    /// The certification of the concept or entity a [`GraphMatch`] names by `title`.
    pub fn certified(&self, title: &str) -> Option<Certification> {
        self.nodes.iter().position(|(_, t, _)| t == title).map(|i| self.cert[i])
    }

    pub fn build(c: &Cortex, class: &BTreeMap<String, Attestation>) -> ConceptIndex {
        let mut ix = ConceptIndex::default();
        let mut slot: BTreeMap<&str, usize> = BTreeMap::new();
        for n in c.nodes.iter().filter(|n| (n.kind == "concept" || n.kind == "entity") && n.superseded_by.is_none()) {
            slot.insert(n.nid.as_str(), ix.nodes.len());
            ix.nodes.push((n.nid.clone(), n.title.clone(), n.kind == "entity"));
            ix.members.push(vec![]);
            for t in std::iter::once(&n.title).chain(n.aliases.iter()) {
                let w = words(t);
                let content = w.iter().filter(|x| !STOP.contains(&x.as_str())).count();
                if content == 0 || t.chars().count() < 3 {
                    continue;
                }
                if !ix.phrases.iter().any(|(p, i)| *p == w && *i == ix.nodes.len() - 1) {
                    ix.phrases.push((w, ix.nodes.len() - 1));
                }
            }
        }
        ix.phrases.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        let mut approved: BTreeMap<&str, &str> = BTreeMap::new();
        for n in c.nodes.iter().filter(|n| n.kind == "observation" && n.superseded_by.is_none()) {
            let Some(v) = n.verdict.as_ref().filter(|v| v.verdict == "approve") else { continue };
            if !class.get(&v.message).copied().unwrap_or(Attestation::Unverified).counts() {
                continue;
            }
            approved.insert(n.nid.as_str(), n.minted_by.as_str());
            ix.approved.insert(n.minted_by.clone());
            if let Some(q) = n.quote.as_deref().map(comparable).filter(|q| q.chars().count() >= COPY_MIN) {
                ix.quotes.push((n.minted_by.clone(), q));
            }
        }
        let roots: BTreeSet<&str> = approved.keys().copied().collect();
        let by_nid: BTreeMap<&str, &Node> = c.nodes.iter().map(|n| (n.nid.as_str(), n)).collect();
        ix.cert = ix.nodes.iter().map(|(nid, _, _)| certification(by_nid[nid.as_str()], &by_nid, &roots)).collect();
        for n in c.nodes.iter().filter(|n| approved.contains_key(n.nid.as_str())) {
            for l in n.out.iter().filter(|l| l.kind == "about") {
                if let Some(&i) = slot.get(l.to.as_str()) {
                    if !ix.members[i].iter().any(|(m, _)| *m == n.minted_by) {
                        ix.members[i].push((n.minted_by.clone(), l.weight.unwrap_or(DEFAULT_WEIGHT)));
                    }
                }
            }
            for label in &n.labels {
                ix.by_form.entry(label.clone()).or_default().push(n.minted_by.clone());
            }
            let cited = n.inbound.iter().filter(|l| l.kind != "part-of" && approved.contains_key(l.from.as_str()))
                .map(|l| l.from.as_str()).collect::<BTreeSet<_>>().len();
            ix.degree.insert(n.minted_by.clone(), cited);
        }
        // Synonyms: concept or entity nodes joined by `is-same-as` (either way) share their members, so either name
        // reaches what is about both (a merge in Brain cases keeps the old name; docs/CORTEX.md).
        let mut group: Vec<usize> = (0..ix.nodes.len()).collect();
        fn root(g: &mut [usize], i: usize) -> usize { let mut i = i; while g[i] != i { g[i] = g[g[i]]; i = g[i]; } i }
        for n in c.nodes.iter().filter(|n| slot.contains_key(n.nid.as_str())) {
            for l in n.out.iter().filter(|l| l.kind == "is-same-as") {
                if let (Some(&a), Some(&b)) = (slot.get(n.nid.as_str()), slot.get(l.to.as_str())) {
                    let (ra, rb) = (root(&mut group, a), root(&mut group, b));
                    group[ra] = rb;
                }
            }
        }
        let mut shared: BTreeMap<usize, Vec<(String, f64)>> = BTreeMap::new();
        for i in 0..ix.nodes.len() {
            let r = root(&mut group, i);
            let e = shared.entry(r).or_default();
            for (m, w) in &ix.members[i] {
                match e.iter_mut().find(|(x, _)| x == m) {
                    Some(have) => have.1 = have.1.max(*w),
                    None => e.push((m.clone(), *w)),
                }
            }
        }
        for i in 0..ix.nodes.len() {
            let r = root(&mut group, i);
            ix.members[i] = shared[&r].clone();
        }
        ix
    }

    /// The nodes and form a query names, and the approved observations they reach. A one-word concept title counts only when
    /// it is the query's one content word (an entity's name always counts) ("state", "what is state"), so a common word inside a longer question ("Waze for
    /// knowledge") does not pull a whole concept in; a phrase inside a longer matched phrase is dropped ("agents" under "company of agents").
    pub fn reach(&self, query: &str) -> GraphMatch {
        let q = words(query);
        let form = q.iter().find_map(|w| FORM_WORDS.iter().find(|(k, _)| *k == w.as_str()).map(|(_, f)| (*f).to_owned()));
        let content: Vec<&String> = q.iter().filter(|w| !STOP.contains(&w.as_str()) && !FORM_WORDS.iter().any(|(k, _)| *k == w.as_str())).collect();
        let mut used = vec![false; q.len()];
        let mut hit_nodes: Vec<usize> = vec![];
        for (p, i) in &self.phrases {
            let content_words = p.iter().filter(|x| !STOP.contains(&x.as_str())).count();
            if content_words == 1 && p.len() == 1 && content.len() > 1 && !self.nodes[*i].2 {
                continue;
            }
            if p.len() > q.len() {
                continue;
            }
            for start in 0..=(q.len() - p.len()) {
                if q[start..start + p.len()] == p[..] && !used[start..start + p.len()].iter().any(|u| *u) {
                    used[start..start + p.len()].iter_mut().for_each(|u| *u = true);
                    if !hit_nodes.contains(i) {
                        hit_nodes.push(*i);
                    }
                    break;
                }
            }
        }
        let mut reached: BTreeMap<&str, (bool, bool)> = BTreeMap::new(); // record -> (by a node, by the form)
        let mut weight: BTreeMap<&str, f64> = BTreeMap::new(); // record -> its strongest link to a named node
        for i in &hit_nodes {
            for (r, w) in &self.members[*i] {
                reached.entry(r.as_str()).or_default().0 = true;
                let e = weight.entry(r.as_str()).or_insert(0.0);
                *e = e.max(*w);
            }
        }
        // A form alone leads only when the query is nothing but the form ("what is our best breakthrough"); with other words
        // and no named node ("the PostgreSQL deadlock problem") the form says nothing about the topic (Emmy's canary).
        let form = form.filter(|_| !hit_nodes.is_empty() || content.is_empty());
        if let Some(f) = &form {
            // A form alone ("our best breakthroughs") reaches every observation with it; with a named node, only its own.
            for r in self.by_form.get(f).into_iter().flatten() {
                if hit_nodes.is_empty() || reached.contains_key(r.as_str()) {
                    reached.entry(r.as_str()).or_default().1 = true;
                }
            }
            // With a named node the form narrows to what both reach, unless nothing has both: "what did we decide about
            // X" still answers with X when no Decision is about it.
            if !hit_nodes.is_empty() && reached.values().any(|(_, by_form)| *by_form) {
                reached.retain(|_, (_, by_form)| *by_form);
            }
        }
        let mut records: Vec<(&str, (bool, bool))> = reached.into_iter().collect();
        records.sort_by(|a, b| {
            let both = |x: &(bool, bool)| x.0 && x.1;
            both(&b.1).cmp(&both(&a.1))
                .then_with(|| weight.get(b.0).unwrap_or(&0.0).total_cmp(weight.get(a.0).unwrap_or(&0.0)))
                .then_with(|| self.degree.get(b.0).unwrap_or(&0).cmp(self.degree.get(a.0).unwrap_or(&0)))
                .then_with(|| b.0.cmp(a.0))
        });
        let unnamed = q.iter().zip(used.iter())
            .filter(|(w, u)| !**u && !STOP.contains(&w.as_str()) && !FORM_WORDS.iter().any(|(k, _)| *k == w.as_str()))
            .count();
        GraphMatch {
            nodes: hit_nodes.iter().map(|i| self.nodes[*i].1.clone()).collect(),
            form,
            records: records.into_iter().map(|(r, _)| r.to_owned()).collect(),
            unnamed,
        }
    }
}

/// A node's approve verdict and how it is attested; `None` when its newest verdict is not approve.
fn approval<'a>(n: &'a Node, class: &BTreeMap<String, Attestation>) -> Option<(&'a Verdict, Attestation)> {
    n.verdict.as_ref().filter(|v| v.verdict == "approve").map(|v| (v, class.get(&v.message).copied().unwrap_or(Attestation::Unverified)))
}

/// How much of a node the owner has approved: its state, the approved readings and the readings counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Certification {
    pub state: &'static str,
    pub approved: usize,
    pub readings: usize,
}

/// A node's certification, rolled up from its parts the way NVIDIA Auto Ontology rolls up a term (dal/terms.py `_certification`:
/// certified, partial or pending; Blaze, 3 Nov 2026: "add check_answerable and partial state to oracle"). A question, entity or
/// concept is `approved` when every current reading linked to it is approved, `partial` when some are, `pending` when none is. A
/// reading the owner rejected is out of the count (he said it is not this), and so is a superseded one. An observation is its own
/// verdict: `approved` (a verdict that counts, in `roots`), `rejected`, or `pending`.
pub fn certification(n: &Node, by_nid: &BTreeMap<&str, &Node>, roots: &BTreeSet<&str>) -> Certification {
    let rejected = |n: &Node| n.verdict.as_ref().is_some_and(|v| v.verdict == "reject");
    if n.kind == "observation" {
        let yes = roots.contains(n.nid.as_str());
        let state = if yes { "approved" } else if rejected(n) { "rejected" } else { "pending" };
        return Certification { state, approved: yes as usize, readings: 1 };
    }
    let readings: BTreeSet<&str> = n.inbound.iter().filter(|l| l.kind != "said-by").map(|l| l.from.as_str())
        .filter(|f| by_nid.get(f).is_some_and(|r| r.kind == "observation" && r.superseded_by.is_none() && (roots.contains(f) || !rejected(r))))
        .collect();
    let approved = readings.iter().filter(|f| roots.contains(*f)).count();
    let state = match approved {
        0 => "pending",
        a if a == readings.len() => "approved",
        _ => "partial",
    };
    Certification { state, approved, readings: readings.len() }
}

/// Nodes (one room, one kind, or titles and aliases containing `q`), with their live marks counted, newest first.
///
/// The Cortex is what Blaze approved (Blaze's ruling via Noor, 28 Oct 2026): by default
/// only an observation whose approve verdict counts (his own key, or attested for him by a registered seat such as the
/// Brain cases page) and a question, entity or concept an approved observation links to; `all` lists every node with its
/// `approval` (class owner, attested, agent or unverified), so drafts stay reachable but never read as approved.
/// The observations whose approve verdict counts (his own key, or attested for him): the roots of the Cortex.
fn approved_roots<'a>(c: &'a Cortex, class: &BTreeMap<String, Attestation>) -> BTreeSet<&'a str> {
    c.nodes.iter()
        .filter(|n| n.kind == "observation" && n.superseded_by.is_none() && approval(n, class).is_some_and(|(_, a)| a.counts()))
        .map(|n| n.nid.as_str()).collect()
}

/// A node in the Cortex: an approved observation, or a current question, entity or concept one links to. A superseded
/// question, entity or concept is history, not current cortex (Emmy's review of 518a18c): it stays out of the default
/// list, as ConceptIndex leaves it out, and shows with `superseded_by` under `all`.
fn in_cortex(n: &Node, approved: &BTreeSet<&str>) -> bool {
    approved.contains(n.nid.as_str())
        || (n.kind != "observation" && n.superseded_by.is_none()
            && n.inbound.iter().any(|l| l.kind != "said-by" && approved.contains(l.from.as_str())))
}

/// `GET /cortex?count=1` (schema cortex-count/1): what the brief counts, from one fold and no node rows (Rocky, 1 Nov
/// 2026: the brief read none of the cortex; the whole list is 2,487 nodes). `approved_nodes` is the default node list's
/// length (what Blaze approved and what it links to), by kind; `pending_marks` the marks with no verdict yet, not
/// superseded (`marks?pending=1`).
pub fn counts(s: &Store) -> Result<Value> {
    let (c, class) = s.cortex_with_classes()?;
    let approved = approved_roots(&c, &class);
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for n in c.nodes.iter().filter(|n| in_cortex(n, &approved)) {
        *by_kind.entry(n.kind.as_str()).or_default() += 1;
    }
    let pending = marks_of(&c, None, None, true).len();
    Ok(serde_json::json!({"schema": "cortex-count/1", "approved_nodes": by_kind.values().sum::<usize>(), "by_kind": by_kind,
        "approved_observations": approved.len(), "pending_marks": pending, "marks": c.marks.len()}))
}

/// The labels that make an approved reading law for every seat (Noor, 3 Nov 2026: "approved
/// readings labelled rule or preference load into every brief as LAW lines"; a Profile reading is how Blaze works, which every seat
/// needs as much as a rule).
pub const LAW_LABELS: [&str; 3] = ["Rule", "Profile", "Preference"];

/// One line of law from a reading: the note's INTENT line when it has one, else the note's first line, else the quote; at most
/// 240 bytes, cut on a character boundary.
fn law_line(n: &Node) -> String {
    let note = n.note.as_deref().unwrap_or("");
    let pick = note.lines().find_map(|l| l.trim().strip_prefix("INTENT:").map(str::trim))
        .or_else(|| note.lines().map(str::trim).find(|l| !l.is_empty()))
        .or(n.quote.as_deref()).unwrap_or(&n.title);
    let mut end = pick.len().min(240);
    while !pick.is_char_boundary(end) { end -= 1; }
    if end < pick.len() { format!("{}…", pick[..end].trim_end()) } else { pick.to_owned() }
}

/// `GET /cortex/law?top=n` (schema cortex-law/1): what Blaze approved and labelled Rule, Profile or Preference, one line each, the
/// readings most linked to first, then the newest. Only a verdict that counts (his key, or attested for him); a superseded reading
/// is out. `top` keeps the first n and `total` says how many there are.
pub fn law(s: &Store, top: Option<usize>) -> Result<Value> {
    let (c, class) = s.cortex_with_classes()?;
    let approved = approved_roots(&c, &class);
    let linked = |n: &Node| n.inbound.iter().filter(|l| l.kind != "said-by").count();
    let mut rows: Vec<&Node> = c.nodes.iter()
        .filter(|n| approved.contains(n.nid.as_str()) && n.labels.iter().any(|l| LAW_LABELS.contains(&l.as_str())))
        .collect();
    rows.sort_by(|a, b| linked(b).cmp(&linked(a)).then(b.minted_ms.cmp(&a.minted_ms)).then(a.nid.cmp(&b.nid)));
    let total = rows.len();
    let lines: Vec<Value> = rows.iter().take(top.unwrap_or(total)).map(|n| serde_json::json!({
        "code": n.minted_by, "nid": n.nid, "labels": n.labels, "line": law_line(n), "linked": linked(n), "minted_ms": n.minted_ms,
        "approval": approval(n, &class).map(|(v, _)| v.message.clone())})).collect();
    Ok(serde_json::json!({"schema": "cortex-law/1", "total": total, "shown": lines.len(), "labels": LAW_LABELS, "lines": lines}))
}

pub fn nodes(s: &Store, room: Option<&str>, kind: Option<&str>, q: Option<&str>, all: bool) -> Result<Value> {
    let (c, class) = s.cortex_with_classes()?;
    let approved = approved_roots(&c, &class);
    let in_cortex = |n: &Node| in_cortex(n, &approved);
    let q = q.map(|x| x.to_lowercase());
    let by_id: BTreeMap<&str, &Mark> = c.marks.iter().map(|m| (m.id.as_str(), m)).collect();
    let by_nid: BTreeMap<&str, &Node> = c.nodes.iter().map(|n| (n.nid.as_str(), n)).collect();
    let roots = &approved;
    let mut out: Vec<Value> = c.nodes.iter()
        .filter(|n| room.is_none_or(|r| n.room == r) && kind.is_none_or(|k| n.kind == k))
        .filter(|n| q.as_deref().is_none_or(|q| n.title.to_lowercase().contains(q) || n.aliases.iter().any(|a| a.to_lowercase().contains(q))))
        .filter(|n| all || in_cortex(n))
        .map(|n| {
            // What the node holds in the cortex: the approved readings that link to it (Noor, 31 Oct 2026, read every entity as
            // "0 approved marks": `approved` counts the old claim marks attached to a node, not Blaze's approved readings).
            let readings = n.inbound.iter().filter(|l| l.kind != "said-by" && approved.contains(l.from.as_str()))
                .map(|l| l.from.as_str()).collect::<BTreeSet<_>>().len();
            let ms: Vec<&&Mark> = n.marks.iter().filter_map(|(id, _)| by_id.get(id.as_str())).collect();
            let approved = ms.iter().filter(|m| m.verdict.as_ref().is_some_and(|v| v.verdict == "approve")).count();
            let rejected = ms.iter().filter(|m| m.verdict.as_ref().is_some_and(|v| v.verdict == "reject")).count();
            let cert = certification(n, &by_nid, roots);
            serde_json::json!({"nid": n.nid, "kind": n.kind, "form": n.form, "shape": n.shape, "title": n.title, "aliases": n.aliases, "room": n.room, "minted_ms": n.minted_ms,
                "quote": n.quote, "source": n.source, "out": n.out.len(), "in": n.inbound.len(),
                "readings": readings, "readings_total": cert.readings, "certification": cert.state, "marks": ms.len(), "approved": approved, "rejected": rejected,
                "verdict": n.verdict.as_ref().map(|v| v.verdict.clone()), "status": n.status.as_ref().map(|v| v.verdict.clone()).or_else(|| (n.kind == "question").then(|| "open".into())),
                "approval": approval(n, &class).map(|(v, a)| serde_json::json!({"verdict": v.message, "by": v.by, "signed_by": v.signed_by, "via": v.via, "class": a.as_str(), "counts": a.counts()})),
                "cortex": in_cortex(n), "superseded_by": n.superseded_by})
        }).collect();
    out.sort_by(|a, b| b["minted_ms"].as_i64().cmp(&a["minted_ms"].as_i64()));
    // A list of concepts, entities or questions reads strongest first: by the approved readings each holds.
    if kind.is_some_and(|k| k != "observation") {
        out.sort_by(|a, b| b["readings"].as_u64().cmp(&a["readings"].as_u64()));
    }
    // `approved_observations` counts the approved observations in this answer (after room, kind and q);
    // `approved_observations_total` counts them across the whole cortex.
    let shown = out.iter().filter(|n| n["kind"] == "observation" && approved.contains(n["nid"].as_str().unwrap_or(""))).count();
    Ok(serde_json::json!({"nodes": out, "count": out.len(), "all": all, "approved_observations": shown,
        "approved_observations_total": approved.len(), "rejected": c.rejected.len(), "rejected_why": c.rejected}))
}

/// One node's page: itself, its live marks (each with its source span), its links to and from other nodes.
pub fn node(s: &Store, nid: &str) -> Result<Option<Value>> {
    let c = fold(&s.tagged_any_live(&TAGS)?);
    let Some(n) = c.nodes.iter().find(|n| n.nid == nid || (nid.len() >= 8 && n.nid.starts_with(nid))) else { return Ok(None) };
    let wanted: BTreeSet<&str> = n.marks.iter().map(|(id, _)| id.as_str()).collect();
    let ms: Vec<&Mark> = c.marks.iter().filter(|m| wanted.contains(m.id.as_str())).collect();
    Ok(Some(serde_json::json!({"node": n, "marks": ms})))
}

/// The marks on one source message (live and superseded, oldest first), optionally of one room.
pub fn marks(s: &Store, room: Option<&str>, source: Option<&str>, pending: bool) -> Result<Value> {
    let c = fold(&s.tagged_any_live(&TAGS)?);
    let ms = marks_of(&c, room, source, pending);
    Ok(serde_json::json!({"marks": ms, "count": ms.len(), "pending": pending}))
}

/// The marks a reader asks for: of a room, on a source message, and with `pending` only those still waiting for a verdict
/// (not superseded, no verdict yet): what Blaze has to answer (27 Oct, live Brain cases).
pub fn marks_of<'a>(c: &'a Cortex, room: Option<&str>, source: Option<&str>, pending: bool) -> Vec<&'a Mark> {
    c.marks.iter()
        .filter(|m| room.is_none_or(|r| m.room == r) && source.is_none_or(|src| m.source["message"].as_str() == Some(src)))
        .filter(|m| !pending || (m.verdict.is_none() && m.superseded_by.is_none()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn obs(seq: i64, author: &str, tag: &str, cargo: Value, extra: &[&str]) -> Observation {
        let mut tags = vec![tag.to_owned()];
        tags.extend(extra.iter().map(|t| t.to_string()));
        Observation { seq, id: format!("OBS-brain-20261018-{seq:012x}"), channel: "brain".into(), author: author.into(), kind: "say".into(),
            content: "c".into(), cargo: Some(cargo.to_string()), tags, created_ms: 1_000 + seq, authored_ms: None, pubkey: String::new(), sig: String::new(), envelope: 2 }
    }
    fn id(seq: i64) -> String { format!("OBS-brain-20261018-{seq:012x}") }

    #[test]
    fn law_lines_take_the_intent_else_the_first_line_and_cut_on_a_character() {
        let mut n = Node { nid: "x".into(), kind: "observation".into(), title: "the title".into(), aliases: vec![], wikidata: None, room: "cortex".into(),
            minted_by: id(1), superseded_by: None, author: "brain".into(), minted_ms: 1, source: Value::Null, quote: Some("the quote".into()),
            form: String::new(), labels: vec!["Rule".into()], shape: String::new(), drafted_by: None,
            note: Some("FRAME: a ladder\nINTENT: Automate whatever is done twice.\nLINKS: m2155".into()), speaker: None, project: None,
            intents: vec![], verdict: None, status: None, notes: vec![], marks: vec![], out: vec![], inbound: vec![] };
        assert_eq!(law_line(&n), "Automate whatever is done twice.");
        n.note = Some("\n  One best option, never a menu.\nmore".into());
        assert_eq!(law_line(&n), "One best option, never a menu.", "no INTENT: the first line that holds words");
        n.note = None;
        assert_eq!(law_line(&n), "the quote");
        n.note = Some("é".repeat(200));
        let l = law_line(&n);
        assert!(l.ends_with('…') && l.len() <= 243, "cut at 240 bytes on a character boundary: {}", l.len());
    }

    #[test]
    fn a_later_record_of_the_same_node_revises_its_note_labels_and_intents_and_keeps_its_mint() {
        use serde_json::json as j;
        let ob = |seq: i64, note: Option<&str>, labels: Option<&[&str]>| {
            let mut c = j!({"kind": "observation", "title": "agents bid on tasks", "source": {"message": "OBS-cases-1", "start": 0, "end": 5},
                "quote": "agents bid on tasks", "intents": ["build"]});
            if let Some(n) = note { c["note"] = j!(n); }
            if let Some(l) = labels { c["labels"] = j!(l); }
            obs(seq, "brain", "cortex:node", c, &[])
        };
        let first = fold(&[ob(1, Some("Builds on {A}."), Some(&["Vision"]))]);
        let nid = observation_nid("OBS-cases-1", 0, 5);
        let node = |c: &Cortex| c.nodes.iter().find(|n| n.nid == nid).cloned().expect("the node");
        assert_eq!(node(&first).note.as_deref(), Some("Builds on {A}."), "control: one record, its own note");
        let c = fold(&[ob(1, Some("Builds on {A}."), Some(&["Vision"])), ob(2, Some("Builds on prompt #12."), Some(&["Vision", "Breakthrough"]))]);
        assert!(c.rejected.is_empty(), "{:?}", c.rejected);
        assert_eq!(c.nodes.iter().filter(|n| n.nid == nid).count(), 1, "one node, not two");
        let n = node(&c);
        assert_eq!(n.note.as_deref(), Some("Builds on prompt #12."), "the newer note wins");
        assert_eq!(n.labels, vec!["Vision", "Breakthrough"]);
        assert_eq!((n.minted_by.as_str(), n.quote.as_deref()), (id(1).as_str(), Some("agents bid on tasks")), "the mint and the quote stay");
        let kept = fold(&[ob(1, Some("Builds on prompt #12."), Some(&["Vision"])), ob(2, None, None)]);
        assert_eq!((node(&kept).note.as_deref(), node(&kept).labels.clone()), (Some("Builds on prompt #12."), vec!["Vision".to_owned()]),
            "a later record without the fields leaves them as they were");
    }

    #[test]
    fn a_node_is_approved_partial_or_pending_by_its_readings_and_a_rejected_reading_is_out_of_the_count() {
        use serde_json::json as j;
        let ob = |seq: i64, start: u64| obs(seq, "brain", "cortex:node", j!({"kind": "observation", "title": format!("reading {seq}"),
            "source": {"message": "OBS-cases-1", "start": start, "end": start + 5}, "quote": format!("reading {seq}")}), &[]);
        let link = |seq: i64, from: &str, to: &str| obs(seq, "brain", "cortex:link", j!({"from": from, "to": to, "kind": "about"}), &[]);
        let verdict = |seq: i64, target: &str, v: &str| obs(seq, "brain", "cortex:verdict", j!({"target": target, "verdict": v, "by": "blaze", "via": "cases-page"}), &[]);
        let o = |start: u64| observation_nid("OBS-cases-1", start, start + 5);
        let (agents, state, pixar) = (nid_of("concept", "company of agents"), nid_of("concept", "state"), nid_of("entity", "Pixar"));
        let rows = vec![
            obs(1, "brain", "cortex:node", j!({"kind": "concept", "title": "company of agents"}), &[]),
            obs(2, "brain", "cortex:node", j!({"kind": "concept", "title": "state"}), &[]),
            obs(3, "brain", "cortex:node", j!({"kind": "entity", "title": "Pixar"}), &[]),
            ob(4, 0), ob(5, 10), ob(6, 20), ob(7, 30), ob(8, 40),
            link(9, &o(0), &agents), link(10, &o(10), &agents), link(11, &o(20), &agents), link(12, &o(30), &state), link(13, &o(40), &pixar),
            verdict(14, &o(0), "approve"), verdict(15, &o(20), "reject"), verdict(16, &o(30), "approve"),
        ];
        let c = fold(&rows);
        assert!(c.rejected.is_empty(), "{:?}", c.rejected);
        let class: BTreeMap<String, Attestation> = [14, 15, 16].iter().map(|s| (id(*s), Attestation::Owner)).collect();
        let ix = ConceptIndex::build(&c, &class);
        let cert = |t: &str| ix.certified(t).map(|c| (c.state, c.approved, c.readings));
        assert_eq!(cert("company of agents"), Some(("partial", 1, 2)), "one approved, one waiting; the rejected one does not count");
        assert_eq!(cert("state"), Some(("approved", 1, 1)));
        assert_eq!(cert("Pixar"), Some(("pending", 0, 1)), "nothing approved yet");
        assert_eq!(cert("no such node"), None);
        // An agent's approval is not the owner's: the reading stays pending, and the concept with it.
        let agent: BTreeMap<String, Attestation> = [(id(14), Attestation::Agent), (id(15), Attestation::Owner), (id(16), Attestation::Owner)].into_iter().collect();
        assert_eq!(ConceptIndex::build(&c, &agent).certified("company of agents").map(|c| c.state), Some("pending"));
        // Observations are their own verdict.
        let roots: BTreeSet<&str> = approved_roots(&c, &class);
        let by_nid: BTreeMap<&str, &Node> = c.nodes.iter().map(|n| (n.nid.as_str(), n)).collect();
        let of = |nid: &str| certification(by_nid[nid], &by_nid, &roots).state;
        assert_eq!((of(&o(0)), of(&o(10)), of(&o(20))), ("approved", "pending", "rejected"));
    }

    #[test]
    fn a_query_reaches_approved_observations_through_concepts_entities_and_forms() {
        use serde_json::json as j;
        let ob = |seq: i64, start: u64, title: &str, labels: &[&str]| obs(seq, "brain", "cortex:node", j!({"kind": "observation", "title": title,
            "source": {"message": "OBS-cases-1", "start": start, "end": start + 5}, "quote": title, "labels": labels}), &[]);
        let link = |seq: i64, from: &str, to: &str, kind: &str| obs(seq, "brain", "cortex:link", j!({"from": from, "to": to, "kind": kind}), &[]);
        let verdict = |seq: i64, target: &str| obs(seq, "brain", "cortex:verdict", j!({"target": target, "verdict": "approve", "by": "blaze", "via": "cases-page"}), &[]);
        let (o1, o2, o3, o4) = (observation_nid("OBS-cases-1", 0, 5), observation_nid("OBS-cases-1", 10, 15),
            observation_nid("OBS-cases-1", 20, 25), observation_nid("OBS-cases-1", 30, 35));
        let (company, state, pixar) = (nid_of("concept", "company of agents"), nid_of("concept", "state"), nid_of("entity", "Pixar"));
        let rows = vec![
            obs(1, "brain", "cortex:node", j!({"kind": "concept", "title": "company of agents"}), &[]),
            obs(2, "brain", "cortex:node", j!({"kind": "concept", "title": "agent company"}), &[]),
            obs(3, "brain", "cortex:node", j!({"kind": "concept", "title": "state"}), &[]),
            obs(4, "brain", "cortex:node", j!({"kind": "entity", "title": "Pixar"}), &[]),
            ob(5, 0, "agents bid on tasks", &["Breakthrough"]), ob(6, 10, "a seat per role", &["Vision"]),
            ob(7, 20, "a draft nobody approved", &["Vision"]), ob(8, 30, "the world should have a state", &["Breakthrough"]),
            link(9, &o1, &company, "about"), link(10, &o2, &company, "about"), link(11, &o3, &company, "about"),
            link(12, &o2, &o1, "answers"), link(13, &o4, &state, "about"), link(14, &o2, &pixar, "about"),
            verdict(15, &o1), verdict(16, &o2), verdict(17, &o4), link(18, &nid_of("concept", "agent company"), &company, "is-same-as"),
        ];
        let c = fold(&rows);
        assert!(c.rejected.is_empty(), "{:?}", c.rejected);
        let mut class: BTreeMap<String, Attestation> = [15, 16, 17].iter().map(|s| (id(*s), Attestation::Owner)).collect();
        let ix = ConceptIndex::build(&c, &class);
        let rec = |seq: i64| id(seq);
        let r = ix.reach("what have we decided about the company of agents");
        assert_eq!(r.nodes, vec!["company of agents"]);
        assert_eq!((r.form.as_deref(), r.records.clone()), (Some("Decision"), vec![rec(5), rec(6)]), "approved only, the one another reading answers first; no Decision about it, so the form does not empty it");
        assert_eq!(ix.reach("agent company").records, vec![rec(5), rec(6)], "a synonym joined by is-same-as reaches the same");
        assert_eq!(ix.reach("pixar").records, vec![rec(6)], "an entity reaches what is about it");
        assert_eq!(ix.reach("pixar").unnamed, 0, "the query is the entity: it leads with what is about it");
        assert_eq!(ix.reach("pixar lighting pipeline budget").unnamed, 3, "the entity plus three other words: recall narrows to what those words match");
        let b = ix.reach("what is our best breakthrough");
        assert_eq!((b.form.as_deref(), b.records.clone()), (Some("Breakthrough"), vec![rec(5), rec(8)]), "a form alone lists its observations");
        assert_eq!(ix.reach("breakthroughs in the company of agents").records, vec![rec(5)], "a node and a form: only what both reach");
        assert_eq!(ix.reach("what is state").records, vec![rec(8)], "a one-word title in a short query counts");
        assert!(ix.reach("state of the art pixar").nodes == vec!["Pixar"], "a one-word title beside other content words is not a match; a named entity is");
        let long = ix.reach("how do we keep the state of every agent in a big team");
        assert!(long.nodes.is_empty() && long.records.is_empty(), "a common word inside a longer question pulls nothing in: {long:?}");
        assert!(ix.reach("tell me something new").records.is_empty());
        let off = ix.reach("the postgresql deadlock problem");
        assert!(off.records.is_empty() && off.form.is_none(), "a form beside an unnamed topic leads nothing: {off:?}");
        assert!(ix.reach("decision about a topic nobody marked").records.is_empty());
        class.insert(id(16), Attestation::Agent);
        assert_eq!(ConceptIndex::build(&c, &class).reach("company of agents").records, vec![rec(5)], "a seat's own approval does not count");
        assert_eq!(ix.degree.get(&rec(5)), Some(&1));
        assert!(ix.approved.contains(&rec(5)) && !ix.approved.contains(&rec(7)), "only verified approvals are approved");
        // A weight on the link outranks degree, and a later link record re-weighs it (Blaze: mark it up when it proves core).
        let mut rows = rows.clone();
        rows.push(obs(19, "brain", "cortex:link", j!({"from": o2, "to": company, "kind": "about", "weight": 1.0}), &[]));
        rows.push(obs(20, "brain", "cortex:link", j!({"from": o1, "to": company, "kind": "about", "weight": 0.2}), &[]));
        let ix = ConceptIndex::build(&fold(&rows), &[15, 16, 17].iter().map(|s| (id(*s), Attestation::Owner)).collect());
        assert_eq!(ix.reach("company of agents").records, vec![rec(6), rec(5)], "core first, mention last");
    }

    #[test]
    fn noors_graph_observations_links_and_labels_from_topology() {
        use serde_json::json as j;
        let ob = |seq: i64, start: u64, title: &str| obs(seq, "brain", "cortex:node", j!({"kind": "observation", "title": title,
            "source": {"message": "OBS-cases-1", "start": start, "end": start + 5}, "quote": title}), &[]);
        let (vision, a, b, plan1, plan2, plan3, rule, lone) = (observation_nid("OBS-cases-1", 0, 5), observation_nid("OBS-cases-1", 10, 15),
            observation_nid("OBS-cases-1", 20, 25), observation_nid("OBS-cases-1", 30, 35), observation_nid("OBS-cases-1", 40, 45),
            observation_nid("OBS-cases-1", 50, 55), observation_nid("OBS-cases-1", 60, 65), observation_nid("OBS-cases-1", 70, 75));
        let tension = nid_of("tension", "Is the orchestrator the personal agent?");
        let link = |seq: i64, from: &str, to: &str, kind: &str| obs(seq, "brain", "cortex:link", j!({"from": from, "to": to, "kind": kind}), &[]);
        let mut rows = vec![ob(1, 0, "augment myself"), ob(2, 10, "agent team"), ob(3, 20, "orchestrator"), ob(4, 30, "company"), ob(5, 40, "efficiency"),
            ob(6, 50, "personal agent"), ob(7, 60, "not for the product"), ob(8, 70, "an idea alone"),
            obs(9, "brain", "cortex:node", j!({"kind": "tension", "title": "Is the orchestrator the personal agent?"}), &[]),
            obs(10, "brain", "cortex:node", j!({"kind": "observation", "title": "no quote", "source": {"message": "OBS-cases-1", "start": 80, "end": 85}}), &[])];
        rows.extend([link(11, &vision, &a, "generates"), link(12, &vision, &b, "generates"), link(13, &plan1, &plan2, "depends-on"),
            link(14, &plan2, &plan3, "depends-on"), link(15, &rule, &vision, "constrains"), link(16, &tension, &b, "about")]);
        let c = fold(&rows);
        let form = |nid: &str| c.nodes.iter().find(|n| n.nid == nid).map(|n| n.form.clone()).unwrap();
        assert_eq!(form(&vision), "Vision");
        assert_eq!(form(&plan1), "Plan", "the first step: depends on a node that depends on another");
        assert_eq!(form(&plan2), "Plan");
        assert_eq!(form(&plan3), "Observation", "the end of the chain depends on nothing");
        assert_eq!(form(&rule), "Rule");
        assert_eq!(form(&lone), "Idea");
        assert_eq!(form(&tension), "Question", "an open tension");
        assert!(c.rejected.iter().any(|r| r["why"].as_str().unwrap().contains("quote")), "an observation without its quote is refused");
        assert_eq!(c.nodes.iter().find(|n| n.nid == vision).unwrap().quote.as_deref(), Some("augment myself"));
        rows.push(link(17, &a, &tension, "completes"));
        let c = fold(&rows);
        assert_eq!(c.nodes.iter().find(|n| n.nid == tension).unwrap().form, "Decision", "a completed tension is a decision");
        assert_eq!(c.nodes.iter().find(|n| n.nid == a).unwrap().form, "Solution");
        assert_eq!(form_with_labels("Observation", &["Rule".to_owned()]), "Rule", "an approved label stands where links decide nothing");
        assert_eq!(form_with_labels("Rule", &["Problem".to_owned()]), "Problem", "an approved label stands even where the links say otherwise");
        assert_eq!(form_with_labels("Superseded", &["Plan".to_owned()]), "Superseded", "a replaced version says so");
        assert_eq!(form_with_labels("Idea", &[]), "Idea");
        let blaze = nid_of("entity", "Blaze");
        rows.push(obs(19, "brain", "cortex:node", j!({"kind": "entity", "title": "Blaze"}), &[]));
        rows.push(link(20, &lone, &blaze, "said-by"));
        rows.push(link(21, &plan3, &rule, "refines"));
        let c = fold(&rows);
        let form = |nid: &str| c.nodes.iter().find(|n| n.nid == nid).map(|n| n.form.clone()).unwrap();
        assert_eq!(form(&lone), "Idea", "who said it is not a link that shapes the label");
        assert_eq!(form(&rule), "Superseded", "a refined observation gives way to the one that refines it");
        assert_eq!(c.nodes.iter().find(|n| n.nid == blaze).unwrap().inbound.len(), 1, "the speaker's page lists what they said");
        rows.push(link(22, &a, "OBS-axxis-20260320-034", "generates"));
        let c = fold(&rows);
        assert!(c.nodes.iter().find(|n| n.nid == a).unwrap().out.iter().any(|l| l.to == "OBS-axxis-20260320-034"), "a link may point at a ledger record");
        rows.push(link(23, &a, "not a record; drop tables", "generates"));
        assert!(fold(&rows).rejected.iter().any(|r| r["id"].as_str().unwrap().ends_with("17")), "a target that is neither a node nor a record id is refused");
        rows.push(link(18, &a, &b, "explains"));
        assert!(fold(&rows).rejected.iter().any(|r| r["why"].as_str().unwrap().contains("twelve link kinds")), "an unknown link kind is refused");
    }

    #[test]
    fn the_same_title_is_one_node_and_others_become_aliases() {
        assert_eq!(slug("  Zero-setup  deployment! "), "zero-setup-deployment");
        assert_eq!(nid_of("concept", "Zero setup deployment"), nid_of("concept", "zero-setup DEPLOYMENT"));
        assert_ne!(nid_of("concept", "Replit"), nid_of("entity", "Replit"));
        let c = fold(&[obs(1, "rocky", "cortex:node", serde_json::json!({"kind": "concept", "title": "Zero setup deployment"}), &[]),
                       obs(2, "emmy", "cortex:node", serde_json::json!({"kind": "concept", "title": "zero-setup deployment", "wikidata": "Q1"}), &[])]);
        assert_eq!(c.nodes.len(), 1);
        assert_eq!((c.nodes[0].title.as_str(), c.nodes[0].aliases.clone(), c.nodes[0].wikidata.as_deref()), ("Zero setup deployment", vec!["zero-setup deployment".to_owned()], Some("Q1")));
    }

    #[test]
    fn a_mark_links_words_to_nodes_and_a_superseding_mark_takes_its_place() {
        let q = nid_of("question", "How do platforms deploy without setup?");
        let rows = vec![
            obs(1, "rocky", "cortex:node", serde_json::json!({"kind": "question", "title": "How do platforms deploy without setup?"}), &[]),
            obs(2, "rocky", "cortex:mark", serde_json::json!({"source": {"system": "timemachine", "message": "u40", "start": 10, "end": 50, "sha256": "x"},
                "labels": ["Question"], "intents": ["Compete", "Learn"], "project": "AXXIS", "origin": {"system": "cases", "mark": 36}}), &[]),
            obs(3, "rocky", "cortex:link", serde_json::json!({"from": id(2), "to": q, "kind": "asks"}), &[]),
            obs(4, "rocky", "cortex:mark", serde_json::json!({"source": {"system": "timemachine", "message": "u40", "start": 0, "end": 50, "sha256": "y"},
                "labels": ["Question", "Pattern"], "by": "blaze", "via": "cases-page"}), &[&format!("supersedes:{}", id(2))]),
            obs(5, "rocky", "cortex:link", serde_json::json!({"from": id(4), "to": q, "kind": "asks"}), &[]),
            obs(6, "rocky", "cortex:verdict", serde_json::json!({"target": id(4), "verdict": "approve", "by": "blaze", "via": "cases-page"}), &[]),
        ];
        let c = fold(&rows);
        assert!(c.rejected.is_empty(), "{:?}", c.rejected);
        let n = &c.nodes[0];
        assert_eq!(n.marks, vec![(id(4), "asks".to_owned())], "the superseded mark leaves the node's page");
        let old = c.marks.iter().find(|m| m.id == id(2)).unwrap();
        assert_eq!(old.superseded_by.as_deref(), Some(id(4).as_str()));
        assert_eq!(old.links.len(), 1, "its link stays on record");
        let new = c.marks.iter().find(|m| m.id == id(4)).unwrap();
        assert_eq!((new.by.as_str(), new.signed_by.as_str(), new.via.as_deref()), ("blaze", "rocky", Some("cases-page")));
        assert_eq!(new.verdict.as_ref().map(|v| (v.verdict.as_str(), v.by.as_str(), v.signed_by.as_str())), Some(("approve", "blaze", "rocky")));
    }

    #[test]
    fn links_are_removed_by_a_newer_message_and_bad_messages_are_listed_not_dropped() {
        let a = nid_of("concept", "memory");
        let rows = vec![
            obs(1, "rocky", "cortex:node", serde_json::json!({"kind": "concept", "title": "memory"}), &[]),
            obs(2, "rocky", "cortex:mark", serde_json::json!({"source": {"message": "u1", "start": 0, "end": 5}, "labels": ["Vision"]}), &[]),
            obs(3, "rocky", "cortex:link", serde_json::json!({"from": id(2), "to": a, "kind": "about"}), &[]),
            obs(4, "rocky", "cortex:link", serde_json::json!({"from": id(2), "to": a, "kind": "about", "removed": true}), &[]),
            obs(5, "rocky", "cortex:node", serde_json::json!({"kind": "vibe", "title": "x"}), &[]),
            obs(6, "rocky", "cortex:mark", serde_json::json!({"source": {"message": "u1", "start": 5, "end": 5}, "labels": ["Vision"]}), &[]),
            obs(7, "rocky", "cortex:link", serde_json::json!({"from": "OBS-nope", "to": a, "kind": "about"}), &[]),
            obs(8, "rocky", "cortex:verdict", serde_json::json!({"target": a, "verdict": "answered"}), &[]),
            obs(9, "rocky", "cortex:verdict", serde_json::json!({"target": id(2), "verdict": "maybe"}), &[]),
        ];
        let mut bad = rows[4].clone();
        bad.cargo = Some("not json".into());
        let mut all = rows.clone();
        all.push(Observation { seq: 10, id: id(10), ..bad });
        let c = fold(&all);
        assert!(c.nodes[0].marks.is_empty(), "a removed link is gone from the page");
        let why: Vec<&str> = c.rejected.iter().filter_map(|r| r["why"].as_str()).collect();
        assert_eq!(why.len(), 6, "{why:?}");
        assert!(why.iter().any(|w| w.contains("question nodes")), "answered is for questions only");
    }

    #[test]
    fn a_question_has_a_status_beside_its_verdict() {
        let q = nid_of("question", "Is it worth building?");
        let c = fold(&[
            obs(1, "rocky", "cortex:node", serde_json::json!({"kind": "question", "title": "Is it worth building?"}), &[]),
            obs(2, "rocky", "cortex:verdict", serde_json::json!({"target": q, "verdict": "approve", "by": "blaze"}), &[]),
            obs(3, "rocky", "cortex:verdict", serde_json::json!({"target": q, "verdict": "answered", "ref": "OBS-x", "by": "blaze"}), &[]),
            obs(4, "rocky", "cortex:verdict", serde_json::json!({"target": q, "verdict": "open", "by": "blaze"}), &[]),
        ]);
        let n = &c.nodes[0];
        assert_eq!(n.verdict.as_ref().map(|v| v.verdict.as_str()), Some("approve"));
        assert_eq!(n.status.as_ref().map(|v| v.verdict.as_str()), Some("open"), "the newest status wins");
    }
}
