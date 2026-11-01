//! Deterministic entity primer. Every section is evidence, never generated biography.
use crate::{
    cortex::{self, Node},
    privaterooms,
    store::{Result, Store, StoreError},
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

fn clip(s: &str, bytes: usize) -> String {
    let mut end = s.len().min(bytes);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].replace(['\n', '\r'], " ")
}
fn evidence(n: &Node) -> Value {
    json!({"nid":n.nid,"room":n.room,"text":clip(n.quote.as_deref().unwrap_or(&n.title), 240),
        "source":n.source,"approval":n.verdict.as_ref().map(|v| &v.message)})
}

pub fn brief(s: &Store, name: &str, seat: &str) -> Result<Value> {
    if name.trim().is_empty() || name.len() > 160 {
        return Err(StoreError::Rejected(
            "entity name must be 1 to 160 bytes".into(),
        ));
    }
    let hidden = privaterooms::hidden_for(&s.private_rooms_table()?, seat);
    let scope = s.seat_rooms_for(seat)?;
    let visible =
        |room: &str| !hidden.contains(room) && scope.as_ref().is_none_or(|r| r.may_read(room));
    let source_visible = |id: &str| privaterooms::room_of_id(id).is_none_or(&visible);
    let (_, classes) = s.cortex_with_classes()?;
    // Filter before folding: a hidden link or verdict cannot affect a public section.
    let rows = s
        .tagged_any_live(&cortex::TAGS)?
        .into_iter()
        .filter(|o| visible(&o.channel))
        .collect::<Vec<_>>();
    let c = cortex::fold(&rows);
    let nodes: Vec<&Node> = c
        .nodes
        .iter()
        .filter(|n| {
            n.superseded_by.is_none() && n.source["message"].as_str().is_none_or(&source_visible)
        })
        .collect();
    let name = cortex::slug(name);
    let exact = nodes
        .iter()
        .any(|n| n.kind == "entity" && cortex::slug(&n.title) == name);
    let found: Vec<_> = nodes
        .iter()
        .copied()
        .filter(|n| {
            n.kind == "entity"
                && (if exact {
                    cortex::slug(&n.title) == name
                } else {
                    n.aliases.iter().any(|a| cortex::slug(a) == name)
                })
        })
        .collect();
    if found.len() > 1 {
        return Err(StoreError::Conflict(
            "entity name matches multiple visible entities; use an exact title".into(),
        ));
    }
    let entity = found.first().ok_or(StoreError::NotFound)?;
    let linked = |n: &&Node| n.out.iter().any(|l| l.to == entity.nid);
    let mut readings: Vec<&Node> = nodes
        .iter()
        .copied()
        .filter(|n| n.kind == "observation")
        .filter(linked)
        .filter(|n| {
            n.verdict.as_ref().is_some_and(|v| {
                v.verdict == "approve" && classes.get(&v.message).is_some_and(|a| a.counts())
            })
        })
        .collect();
    readings.sort_by(|a, b| b.minted_ms.cmp(&a.minted_ms).then(a.nid.cmp(&b.nid)));
    let charter = readings
        .iter()
        .rev()
        .find(|n| n.labels.iter().any(|l| l.eq_ignore_ascii_case("profile")))
        .map(|n| evidence(n));
    let positions: Vec<Value> = readings
        .iter()
        .filter(|n| !n.labels.iter().any(|l| l.eq_ignore_ascii_case("profile")))
        .take(3)
        .map(|n| {
            let mut v = evidence(n);
            v["relation"] =
                json!(if n
                    .speaker
                    .as_deref()
                    .is_some_and(
                        |speaker| cortex::slug(speaker) == cortex::slug(&entity.title)
                            || entity
                                .aliases
                                .iter()
                                .any(|a| cortex::slug(a) == cortex::slug(speaker))
                    )
                    || n.out
                        .iter()
                        .any(|l| l.kind == "said-by" && l.to == entity.nid)
                {
                    "by"
                } else {
                    "about"
                });
            v
        })
        .collect();
    let entities: BTreeMap<&str, &Node> = nodes
        .iter()
        .copied()
        .filter(|n| n.kind == "entity")
        .map(|n| (n.nid.as_str(), n))
        .collect();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for n in &readings {
        let targets: BTreeSet<&str> = n
            .out
            .iter()
            .filter(|l| {
                l.kind != "said-by" && l.to != entity.nid && entities.contains_key(l.to.as_str())
            })
            .map(|l| l.to.as_str())
            .collect();
        for id in targets {
            *counts.entry(id).or_default() += 1;
        }
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let connections: Vec<Value> = counts.into_iter().take(3).map(|(id,count)|json!({"nid":id,"title":entities[id].title,"room":entities[id].room,"shared_approved_readings":count})).collect();
    let mut questions: Vec<&Node> = nodes
        .iter()
        .copied()
        .filter(|n| n.kind == "question" && n.status.as_ref().is_none_or(|v| v.verdict == "open"))
        .filter(linked)
        .collect();
    questions.sort_by(|a, b| b.minted_ms.cmp(&a.minted_ms).then(a.nid.cmp(&b.nid)));
    let mut questions: Vec<Value> = questions.iter().take(3).map(|n| evidence(n)).collect();
    // An entity is not authority to inspect a seat's personal queue. Only exact
    // registered names (including owner-maintained seat aliases) may map to one.
    let names = s.seat_names()?;
    let target = names.iter().find(|handle| handle.eq_ignore_ascii_case(&entity.title));
    let mut addressed_asks = "no exact registered seat";
    if let Some(target) = target {
        let now = crate::store::now_ms_pub();
        match s.cross_seat(seat, target, "asks", true, now) {
            Err(StoreError::Forbidden(_)) => addressed_asks = "not authorized for this seat",
            Err(e) => return Err(e),
            Ok(()) => {
                let a = crate::asks::asks_for(s, target, None, crate::asks::StateFilter::Open, now)?;
                addressed_asks = if a.capped { "bounded candidate scan; older asks may be missing" } else { "bounded open-ask preview" };
                let aliases = s.seat_aliases();
                let self_addressed = a.im_waiting_on.into_iter().filter(|a|
                    a.held_by.iter().any(|h| crate::alias::same(&aliases, h, target)));
                let mut rows = a.waiting_on_me.into_iter().chain(a.acked).chain(a.stuck).chain(a.later).chain(self_addressed)
                    .filter(|a| visible(&a.room)).collect::<Vec<_>>();
                rows.sort_by(|a,b| b.created_ms.cmp(&a.created_ms).then(a.id.cmp(&b.id)));
                rows.dedup_by(|a,b| a.id == b.id);
                let mut asks: Vec<Value> = rows.into_iter().take(3).map(|a|
                    json!({"id":a.id,"room":a.room,"text":a.card,"state":a.state,"kind":"addressed_ask"})).collect();
                asks.extend(questions);
                asks.truncate(3);
                questions = asks;
            }
        }
    }
    let last = s.entity_mentions(&entity.title, &hidden, scope.as_ref())?.first().map(|o|
        json!({"id":o.id,"room":o.channel,"at_ms":o.created_ms,"text":clip(&o.content,160),"status":"mention, not an approved position"}));
    // A byte budget is deliberately conservative: <=500 UTF-8 bytes cannot exceed
    // 500 byte-level tokens. Full structured evidence remains available via JSON.
    let lines = [
        ("Entity", entity.title.clone()),
        (
            "Charter",
            charter
                .as_ref()
                .map(|v| {
                    format!(
                        "[{}] {}",
                        &v["nid"].as_str().unwrap()[..8],
                        v["text"].as_str().unwrap_or("")
                    )
                })
                .unwrap_or_else(|| "not recorded".into()),
        ),
        (
            "Positions",
            positions
                .iter()
                .map(|v| {
                    format!(
                        "{} [{}]: {}",
                        v["relation"].as_str().unwrap_or("question"),
                        &v["nid"].as_str().unwrap_or("")[..8],
                        clip(v["text"].as_str().unwrap_or(""), 25)
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        ),
        (
            "Connections",
            connections
                .iter()
                .filter_map(|v| v["title"].as_str())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        (
            "Last seen",
            last.as_ref()
                .map(|v| {
                    format!(
                        "{} [{}]",
                        oracle_proto::iso_ms(v["at_ms"].as_i64().unwrap_or(0)),
                        v["id"]
                            .as_str()
                            .unwrap_or("")
                            .rsplit('-')
                            .next()
                            .unwrap_or("")
                    )
                })
                .unwrap_or_default(),
        ),
        (
            "Open questions",
            if questions.is_empty() && addressed_asks == "not authorized for this seat" {
                "addressed asks unavailable; no linked questions".to_string()
            } else {
                questions.iter().filter_map(|v| v["text"].as_str()).collect::<Vec<_>>().join("; ")
            },
        ),
    ];
    let primer = lines
        .iter()
        .map(|(label, value)| {
            format!(
                "{label}: {}\n",
                clip(
                    if value.is_empty() {
                        "not recorded"
                    } else {
                        value
                    },
                    65
                )
            )
        })
        .collect::<String>();
    debug_assert!(primer.len() <= 500);
    Ok(
        json!({"schema":"entity-brief/1","entity":{"nid":entity.nid,"title":entity.title,"room":entity.room},
        "charter":charter,"positions":positions,"connections":connections,"last_seen":last,"open_questions":questions,"addressed_asks":addressed_asks,
        "primer":primer,"primer_byte_limit":500,"scope":"visible approved linked readings; open questions combine authorized addressed asks and linked cortex questions; bounded preview, not a complete inbox; last_seen is an exact-name lexical mention"}),
    )
}
