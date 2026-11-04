//! The three updates a message takes that are not a new message, as the room already folds them: a mark (`react:<mark>` replying to it), an annotation
//! (`annotates:<id>` + `set:k=v`) and an archive. ONE definition, validated, shared by `ox react|annotate|archive` and the MCP tools, so a model cannot write
//! a malformed update that the CLI would have refused. The server's `check_annotation` and the fold stay the authority: nothing here grants anything.
use anyhow::{bail, Result};
use serde_json::{json, Value};

/// seen · go · like · no, and what a seat did with an ask to it (Blaze, 2026-10-10): ack (I have it; stop asking me),
/// stuck (I cannot move it; say why), done (I did it), later (not now: it comes back by itself at a time; Blaze, 23 Oct:
/// "give me a option to defer things"). Each is that seat's alone; the server's MARKS is the same list.
pub const REACTIONS: [(&str, &str); 8] = [
    ("seen", "👁 seen"),
    ("go", "✓ go"),
    ("like", "♥"),
    ("no", "✕ no"),
    ("ack", "● ack"),
    ("stuck", "■ stuck"),
    ("done", "✔ done"),
    ("later", "◷ later"),
];

pub fn is_obs_id(id: &str) -> bool {
    id.starts_with("OBS-")
        && id.len() <= 160
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn reaction(kind: &str, target: &str, why: Option<&str>) -> Result<(String, Vec<String>)> {
    if kind == "later" {
        bail!("later needs the time it comes back: --until 4h (or 30m, 2d, 1w, or epoch milliseconds)");
    }
    mark(kind, target, why)
}

/// `later`: the ask leaves the marking seat's waiting list until `until_ms`, then asks again by itself (HEY's Bubble Up).
pub fn later(target: &str, until_ms: i64, now_ms: i64, why: Option<&str>) -> Result<(String, Vec<String>)> {
    if until_ms <= now_ms {
        bail!("--until is in the past: later brings an ask back at a future time");
    }
    if until_ms - now_ms > 366 * 86_400_000 {
        bail!("--until is more than a year away: say no, or done, instead");
    }
    let (face, mut tags) = mark("later", target, why)?;
    tags.push(format!("until:{until_ms}"));
    Ok((face, tags))
}

/// A duration (`30m`, `4h`, `2d`, `1w`) or epoch milliseconds, as the absolute time it names.
pub fn until_of(s: &str, now_ms: i64) -> Result<i64> {
    let s = s.trim();
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && s.len() >= 12 {
        return Ok(s.parse()?);
    }
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = n.parse().map_err(|_| anyhow::anyhow!("--until is a duration (30m, 4h, 2d, 1w) or epoch milliseconds"))?;
    let ms = match unit { "m" => 60_000, "h" => 3_600_000, "d" => 86_400_000, "w" => 7 * 86_400_000, _ => bail!("--until is a duration (30m, 4h, 2d, 1w) or epoch milliseconds") };
    Ok(now_ms + n * ms)
}

fn mark(kind: &str, target: &str, why: Option<&str>) -> Result<(String, Vec<String>)> {
    let Some((_, glyph)) = REACTIONS.iter().find(|(k, _)| *k == kind) else {
        bail!("a mark is seen, go, like, no, ack, stuck, done or later");
    };
    let why = why.map(str::trim).filter(|w| !w.is_empty());
    if why.is_some_and(|w| w.len() > 600) {
        bail!("--why is at most 600 characters");
    }
    let face = match why {
        Some(w) => format!("{glyph} — {w}"),
        None => glyph.to_string(),
    };
    // A message by its id, or a card by its board id (the card's own task:<id> thread).
    let link = if is_obs_id(target) {
        format!("reply:{target}")
    } else if !target.is_empty()
        && target.len() <= 120
        && target
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        format!("task:{target}")
    } else {
        bail!("mark a message (OBS-…) or a card (its board id)");
    };
    Ok((face, vec![link, format!("react:{kind}")]))
}

pub fn annotation(id: &str, sets: &[String], confidence: Option<f64>) -> Result<Vec<String>> {
    if !id.starts_with("OBS-")
        || id.len() > 160
        || id
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    {
        bail!("annotate a message id: OBS-…");
    }
    if sets.is_empty() {
        bail!(
            "say what changes: --set intent=decision, --set ask=act, --set item:R1=requirement …"
        );
    }
    let mut tags = vec![
        format!("annotates:{id}"),
        "intent:status".into(),
        "ttl:0".into(),
    ];
    if let Some(c) = confidence {
        if !(0.0..=1.0).contains(&c) {
            bail!("--confidence is between 0 and 1");
        }
        tags.push(format!("confidence:{c:.2}"));
    }
    for kv in sets {
        let Some((k, v)) = kv.split_once('=') else {
            bail!("--set is key=value: {kv}");
        };
        let key_ok = !k.is_empty()
            && k.len() <= 40
            && k.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b':');
        if !key_ok || v.trim().is_empty() || v.len() > 150 {
            bail!(
                "--set {kv}: a short key (letters, digits, - _ :) and a value of 1–150 characters"
            );
        }
        // Items are their own tag (item:R1=requirement), as the classifier and the room read them.
        tags.push(if let Some(item) = k.strip_prefix("item:") {
            format!("item:{item}={v}")
        } else {
            format!("set:{k}={v}")
        });
    }
    if tags.len() > 32 {
        bail!("at most {} --set per annotation", 32 - 4);
    }
    Ok(tags)
}

pub fn archive(room: &str, id: &str, note: Option<&str>) -> Result<(String, Vec<String>, Value)> {
    if !is_obs_id(id) {
        bail!("archive a message id: OBS-…");
    }
    let mut face = "Archived: out of the view.".to_string();
    if let Some(n) = note.map(str::trim).filter(|n| !n.is_empty()) {
        face = format!("{face} {n}");
    }
    let cargo = json!({"schema": "room-reply/1", "channel": room, "replyTo": id, "text": face});
    Ok((
        face,
        vec![
            format!("archives:{id}"),
            format!("reply:{id}"),
            "intent:status".into(),
        ],
        cargo,
    ))
}

/// A claim write (Round 43, the cortex): mint, edge, status or source, as the tags and cargo the server's claims fold reads.
/// Validated here exactly as the fold validates, so a model cannot write a claim the table would reject.
pub fn claim(op: &str, a: &Value) -> Result<(String, Vec<String>, Value)> {
    const TYPES: [&str; 6] = ["standard-theorem", "our-derivation", "conjecture", "bounded-computation", "external", "unresolved"];
    const STATUSES: [&str; 3] = ["verified", "refuted", "open"];
    const EDGES: [&str; 5] = ["supports", "contradicts", "supersedes", "is-same-as", "cites"];
    let get = |k: &str| a[k].as_str().map(str::trim).filter(|s| !s.is_empty());
    let need = |k: &str| get(k).ok_or_else(|| anyhow::anyhow!("a claim {op} needs {k}"));
    let hex = |k: &str| -> Result<String> {
        let v = need(k)?.to_ascii_lowercase();
        if v.len() < 8 || !v.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("{k} is a cid or a prefix of at least 8 hex");
        }
        Ok(v)
    };
    match op {
        "mint" => {
            let (statement, kind, status) = (need("statement")?, need("type")?, need("status")?);
            if !TYPES.contains(&kind) {
                bail!("type is one of {}", TYPES.join(", "));
            }
            if !STATUSES.contains(&status) {
                bail!("status is verified, refuted or open");
            }
            let mut cargo = json!({"statement": statement, "type": kind, "status": status});
            if let Some(src) = a["sources"].as_array() {
                cargo["sources"] = json!(src);
            }
            if let Some(r) = get("row") {
                cargo["row"] = json!(r);
            }
            Ok((format!("claim ({kind}, {status}): {statement}"), vec!["claim:mint".into()], cargo))
        }
        "edge" => {
            let (from, to, kind) = (hex("from")?, hex("to")?, need("kind")?);
            if !EDGES.contains(&kind) {
                bail!("kind is one of {}", EDGES.join(", "));
            }
            Ok((format!("claim {from} {kind} {to}"), vec!["claim:edge".into()], json!({"from": from, "to": to, "kind": kind})))
        }
        "status" => {
            let (cid, status) = (hex("cid")?, need("status")?);
            if !STATUSES.contains(&status) {
                bail!("status is verified, refuted or open");
            }
            let mut cargo = json!({"cid": cid, "status": status});
            if let Some(w) = get("why") {
                cargo["why"] = json!(w);
            }
            Ok((format!("claim {cid} is {status}{}", get("why").map(|w| format!(": {w}")).unwrap_or_default()), vec!["claim:status".into()], cargo))
        }
        "source" => {
            // A source added to a claim already minted (Blaze, 30 Oct 2026: claims editable through Oracle). A claim's words
            // are its cid, so re-minting to add a reference would make a new claim and cut every link to the old one.
            // `remove` takes off a source that was wrong (a bad DOI); the claim keeps the removal with this record and `why`.
            let cid = hex("cid")?;
            let list = |k: &str| -> Vec<String> { a[k].as_array().map(|v| v.iter().filter_map(|s| s.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()).unwrap_or_default() };
            let (src, rm) = (list("sources"), list("remove"));
            let n = src.len() + rm.len();
            if n == 0 || n > 10 || src.iter().chain(&rm).any(|s| s.len() > 500) {
                bail!("source needs 1 to 10 sources to add or remove (OBS ids or dated URLs), each at most 500 characters");
            }
            let mut face = format!("claim {cid} source:");
            if !src.is_empty() {
                face.push_str(&format!(" + {}", src.join(" · ")));
            }
            if !rm.is_empty() {
                face.push_str(&format!(" − {}", rm.join(" · ")));
            }
            let mut cargo = json!({"cid": cid});
            if !src.is_empty() {
                cargo["sources"] = json!(src);
            }
            if !rm.is_empty() {
                cargo["remove"] = json!(rm);
            }
            if let Some(w) = get("why") {
                cargo["why"] = json!(w);
                face.push_str(&format!(" ({w})"));
            }
            Ok((face, vec!["claim:source".into()], cargo))
        }
        _ => bail!("op is mint, edge, status or source"),
    }
}

#[cfg(test)]
mod later_tests {
    use super::*;
    const NOW: i64 = 1_800_000_000_000;
    #[test]
    fn later_needs_a_future_time_and_says_it_in_a_tag() {
        assert!(reaction("later", "OBS-x-1", None).unwrap_err().to_string().contains("--until"));
        let (face, tags) = later("OBS-x-1", NOW + 3_600_000, NOW, Some("after the release")).unwrap();
        assert!(face.starts_with("◷ later"));
        assert_eq!(tags, vec!["reply:OBS-x-1".to_string(), "react:later".into(), format!("until:{}", NOW + 3_600_000)]);
        assert!(later("OBS-x-1", NOW - 1, NOW, None).is_err(), "the past is refused");
        assert!(later("OBS-x-1", NOW + 400 * 86_400_000, NOW, None).is_err(), "over a year is refused");
        assert_eq!(until_of("30m", NOW).unwrap(), NOW + 1_800_000);
        assert_eq!(until_of("2d", NOW).unwrap(), NOW + 2 * 86_400_000);
        assert_eq!(until_of("1792800000000", NOW).unwrap(), 1_792_800_000_000);
        assert!(until_of("soon", NOW).is_err() && until_of("5y", NOW).is_err());
    }
}

#[cfg(test)]
mod source_removal_tests {
    use super::claim;
    use serde_json::json;

    #[test]
    fn a_source_write_can_remove_with_a_reason_and_still_needs_something_to_do() {
        let (face, tags, cargo) = claim("source", &json!({"cid": "ccd22744f23a", "remove": ["https://doi.org/wrong"], "why": "Crossref"})).unwrap();
        assert_eq!(tags, vec!["claim:source".to_string()]);
        assert_eq!(cargo, json!({"cid": "ccd22744f23a", "remove": ["https://doi.org/wrong"], "why": "Crossref"}));
        assert!(face.contains("− https://doi.org/wrong") && face.contains("(Crossref)"), "{face}");
        // Negative control: neither a source to add nor one to remove is refused before anything is signed.
        assert!(claim("source", &json!({"cid": "ccd22744f23a"})).is_err());
        assert!(claim("source", &json!({"cid": "ccd22744f23a", "sources": [], "remove": []})).is_err());
    }
}
