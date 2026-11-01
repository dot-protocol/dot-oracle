//! Marks on Oracle observations (Blaze, 27 Oct 2026: Brain cases become a native part of Oracle, made in live conversations and
//! held for his approval; docs/CORTEX.md "Marks on Oracle observations"). A seat in a chat names an observation it can read and the
//! exact words in it; this turns that into the `cortex:mark` message the cortex fold already reads (source `{system: "oracle",
//! message, start, end, sha256}`, labels, intents, note), the `cortex:link` messages that give it lineage, and the `cortex:verdict`
//! message that is Blaze's answer. `ox mark`, `ox verdict` and the MCP tools `oracle_mark`, `oracle_verdict` are this one code.

use anyhow::{bail, Result};
use serde_json::{json, Value};

/// The fourteen tests of docs/BRAIN-CASES.md, plus the three working labels the cases page uses.
pub const LABELS: [&str; 17] = [
    "Question", "Vision", "Problem", "Plan", "Decision", "Rule", "Discovery", "Breakthrough", "Solution", "Oracle-core design",
    "Pattern", "Claim", "Prediction", "Profile", "Belief", "Idea", "For Rocky",
];
/// The thirteen link kinds of the cortex (oracle-server cortex.rs LINKS).
pub const LINK_KINDS: [&str; 13] = ["about", "asks", "answers", "depends-on", "is-same-as", "part-of", "generates", "contradicts", "constrains", "completes", "said-by", "refines", "supports"];
pub const VERDICTS: [&str; 3] = ["approve", "reject", "note"];
/// Where marks, links and verdicts are written (docs/CORTEX.md: Brain cases write to #brain).
pub const ROOM: &str = "brain";

/// The span of `quote` in `content` (its `nth` occurrence, 1-based): char offsets, as the Time Machine's marks are, and the sha256 of
/// the span's bytes. The quote must be found whole; a mark is never made on words that are not there.
pub fn locate(content: &str, quote: &str, nth: usize) -> Result<(usize, usize, String)> {
    let quote = quote.trim();
    if quote.is_empty() {
        bail!("the quote is empty");
    }
    let mut from = 0usize;
    let mut found = None;
    for _ in 0..nth.max(1) {
        match content[from..].find(quote) {
            Some(i) => {
                found = Some(from + i);
                from = from + i + quote.len();
            }
            None => {
                found = None;
                break;
            }
        }
    }
    let Some(byte) = found else {
        bail!("the quote is not in the observation{}: {:?}", if nth > 1 { format!(" {nth} times") } else { String::new() }, quote.chars().take(60).collect::<String>());
    };
    let start = content[..byte].chars().count();
    let end = start + quote.chars().count();
    use sha2::Digest;
    Ok((start, end, hex::encode(sha2::Sha256::digest(quote.as_bytes()))))
}

/// One `kind:target` link as the CLI and the MCP take it.
pub fn parse_link(s: &str) -> Result<(String, String)> {
    let (kind, to) = s.split_once(':').ok_or_else(|| anyhow::anyhow!("a link is kind:target, e.g. about:OBS-… or generates:m1830"))?;
    if !LINK_KINDS.contains(&kind) {
        bail!("link kind {kind:?} is not one of {}", LINK_KINDS.join(", "));
    }
    let to = to.trim();
    if to.is_empty() || to.len() > 120 || to.chars().any(char::is_whitespace) {
        bail!("link target {to:?} is not an id");
    }
    Ok((kind.to_owned(), to.to_owned()))
}

/// What a mark is: validated inputs made into the message's face, cargo and tags.
pub struct MarkMessage {
    pub face: String,
    pub cargo: Value,
    pub tags: Vec<String>,
    pub links: Vec<(String, String)>,
    pub start: usize,
    pub end: usize,
}

#[allow(clippy::too_many_arguments)]
pub fn mark_message(observation: &str, content: &str, quote: &str, nth: usize, labels: &[String], intents: &[String], note: Option<&str>, links: &[String], by: Option<&str>) -> Result<MarkMessage> {
    if !observation.starts_with("OBS-") || observation.len() > 80 || observation.chars().any(char::is_whitespace) {
        bail!("the observation is an OBS-… id");
    }
    if labels.is_empty() {
        bail!("a mark needs at least one label: {}", LABELS.join(", "));
    }
    for l in labels {
        if !LABELS.contains(&l.as_str()) {
            bail!("label {l:?} is not one of {}", LABELS.join(", "));
        }
    }
    let links: Vec<(String, String)> = links.iter().map(|l| parse_link(l)).collect::<Result<_>>()?;
    if links.is_empty() {
        bail!("a mark needs at least one lineage link (kind:target): what it is about, answers, refines or generates; find it with ox recall");
    }
    let (start, end, sha256) = locate(content, quote, nth)?;
    let quote = quote.trim();
    let short: String = quote.chars().take(80).collect();
    let face = format!("mark {}: \"{}{}\" on {observation}", labels.join("+"), short, if quote.chars().count() > 80 { "…" } else { "" });
    let mut cargo = json!({
        "schema": "cortex-mark/1",
        "source": {"system": "oracle", "message": observation, "start": start, "end": end, "sha256": sha256},
        "quote": quote, "labels": labels, "intents": intents, "origin": {"system": "ox"},
    });
    if let Some(n) = note.map(str::trim).filter(|n| !n.is_empty()) {
        cargo["note"] = json!(n);
    }
    if let Some(b) = by {
        cargo["by"] = json!(b);
    }
    let tags = vec!["cortex:mark".to_owned(), "schema:cortex-mark/1".to_owned(), format!("about:{observation}")];
    Ok(MarkMessage { face, cargo, tags, links, start, end })
}

/// A link message from a mark (its id) to a node, mark or record.
pub fn link_message(from: &str, kind: &str, to: &str) -> (String, Value, Vec<String>) {
    (format!("link: {from} —{kind}→ {to}"), json!({"schema": "cortex-link/1", "from": from, "to": to, "kind": kind}), vec!["cortex:link".to_owned(), "schema:cortex-link/1".to_owned()])
}

/// Blaze's answer on a mark or node.
pub fn verdict_message(target: &str, verdict: &str, text: Option<&str>, by: Option<&str>) -> Result<(String, Value, Vec<String>)> {
    if !VERDICTS.contains(&verdict) {
        bail!("verdict is approve, reject or note");
    }
    if target.is_empty() || target.len() > 120 || target.chars().any(char::is_whitespace) {
        bail!("the target is a mark id (OBS-…) or a node id");
    }
    let mut cargo = json!({"schema": "cortex-verdict/1", "target": target, "verdict": verdict});
    if let Some(t) = text.map(str::trim).filter(|t| !t.is_empty()) {
        cargo["text"] = json!(t);
    }
    if let Some(b) = by {
        cargo["by"] = json!(b);
    }
    let face = format!("verdict {verdict} on {target}{}", text.map(|t| format!(": {t}")).unwrap_or_default());
    Ok((face, cargo, vec!["cortex:verdict".to_owned(), "schema:cortex-verdict/1".to_owned(), format!("about:{target}")]))
}

/// The content of an observation as `/search?id=` answers it.
pub fn content_of(v: &Value) -> Option<String> {
    let o = v.get("observation").unwrap_or(v);
    o.get("content").and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_is_located_by_chars_and_hashed_and_a_missing_quote_is_refused() {
        let c = "héllo world, héllo again";
        let (s, e, sha) = locate(c, "héllo", 2).unwrap();
        assert_eq!((s, e), (13, 18), "char offsets, not bytes");
        assert_eq!(sha.len(), 64);
        assert!(locate(c, "héllo", 3).is_err());
        assert!(locate(c, "nothing", 1).is_err());
        assert!(locate(c, "  ", 1).is_err());
    }

    #[test]
    fn a_mark_needs_a_label_and_a_lineage_link_and_carries_the_span() {
        let m = mark_message("OBS-noor-20261027-abc", "Blaze: the harness is the product.", "the harness is the product", 1, &["Vision".into()], &[], Some("why"), &["about:OBS-x".into(), "refines:m1830".into()], None).unwrap();
        assert_eq!(m.cargo["source"]["system"], "oracle");
        assert_eq!((m.start, m.end), (7, 33));
        assert_eq!(m.links.len(), 2);
        assert!(m.tags.contains(&"cortex:mark".to_string()));
        assert!(mark_message("OBS-x", "a b", "a", 1, &[], &[], None, &["about:OBS-y".into()], None).is_err(), "no label");
        assert!(mark_message("OBS-x", "a b", "a", 1, &["Vision".into()], &[], None, &[], None).is_err(), "no link");
        assert!(mark_message("OBS-x", "a b", "a", 1, &["Hunch".into()], &[], None, &["about:OBS-y".into()], None).is_err(), "unknown label");
        assert!(parse_link("loves:OBS-y").is_err());
        assert!(mark_message("not an id", "a b", "a", 1, &["Vision".into()], &[], None, &["about:OBS-y".into()], None).is_err());
    }

    #[test]
    fn a_verdict_is_one_of_three() {
        let (face, cargo, tags) = verdict_message("OBS-brain-1", "approve", Some("yes"), Some("blaze")).unwrap();
        assert_eq!(cargo["verdict"], "approve");
        assert!(face.starts_with("verdict approve on OBS-brain-1"));
        assert!(tags.contains(&"cortex:verdict".to_string()));
        assert!(verdict_message("OBS-brain-1", "maybe", None, None).is_err());
        assert_eq!(content_of(&serde_json::json!({"observation": {"content": "x"}})).as_deref(), Some("x"));
    }
}
