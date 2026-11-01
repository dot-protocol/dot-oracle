//! Brain cases through the cortex (Blaze, 28 Oct 2026: "give Noor the tools to read brain cases and draft marks and update your
//! marks"; "approval will be by me"; "everything we are building with marks, searchable and linked in the Oracle MCP cortex tool").
//! The chats and their marks live in the Time Machine's cases store on core (docs/BRAIN-CASES.md); oracle-server reads them for an
//! allowed seat (`GET /cortex/cases`, `GET /cortex/case/<n>`) and writes a draft as that seat (`POST /cortex/drafts`). A draft
//! carries the five fields, intent, frame, state, method and lineage, and waits for Blaze; there is no approve here. `ox case` and
//! the MCP `oracle_cortex` ops `cases`, `case` and `draft` are this one code.

use crate::marks::{parse_link, LABELS};
use anyhow::{bail, Result};
use serde_json::{json, Value};

/// The five fields every draft carries (Blaze, 28 Oct 2026: "intent frame state method lineage").
pub const FIELDS: [&str; 5] = ["intent", "frame", "state", "method", "lineage"];

/// The server path for a read: `cases` (the chats, filtered by `which`: To do, drafted, reviewed ...) or `case` (one chat whole).
pub fn path(op: &str, a: &Value) -> Result<String> {
    match op {
        "cases" => {
            let mut q = vec![format!("limit={}", a["limit"].as_u64().unwrap_or(50).clamp(1, 500))];
            if let Some(o) = a["offset"].as_u64() {
                q.push(format!("offset={o}"));
            }
            if let Some(w) = a["which"].as_str().filter(|w| !w.is_empty()) {
                q.push(format!("state={}", crate::enc(w)));
            }
            Ok(format!("/cortex/cases?{}", q.join("&")))
        }
        "case" => {
            let n = a["n"].as_str().map(str::to_owned).or_else(|| a["n"].as_u64().map(|n| n.to_string())).unwrap_or_default();
            let n = n.trim().trim_start_matches('#');
            if n.is_empty() || n.len() > 64 || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                bail!("n is a chat number (146) or a conversation id");
            }
            Ok(format!("/cortex/case/{n}"))
        }
        _ => bail!("op is cases or case"),
    }
}

/// A draft's body, checked here so a seat hears what is wrong before anything is sent.
pub fn draft(a: &Value) -> Result<Value> {
    let text = |k: &str| a[k].as_str().map(str::trim).unwrap_or("").to_owned();
    let list = |k: &str| -> Vec<String> { a[k].as_array().map(|v| v.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default() };
    let (case, quote) = (text("case"), a["quote"].as_str().unwrap_or("").to_owned());
    if case.is_empty() {
        bail!("case is the prompt's uuid, as op case lists it (a reply's words are marked on its prompt, with msg)");
    }
    if quote.trim().is_empty() {
        bail!("quote is the exact words, as they stand in the message");
    }
    let labels = list("labels");
    if labels.is_empty() {
        bail!("a draft needs at least one label: {}", LABELS[..16].join(", "));
    }
    for l in &labels {
        if l == "For Rocky" || !LABELS.contains(&l.as_str()) {
            bail!("label {l:?} is not one of {}", LABELS[..16].join(", "));
        }
    }
    let mut fields = serde_json::Map::new();
    let mut missing = vec![];
    for f in FIELDS {
        let v = text(f);
        if v.is_empty() {
            missing.push(f);
        }
        fields.insert(f.into(), json!(v));
    }
    if !missing.is_empty() {
        bail!("a draft carries all five fields; missing: {} (write \"none\" where a field does not apply)", missing.join(", "));
    }
    let links = list("links");
    if links.is_empty() {
        bail!("a draft needs at least one lineage link, kind:target (about:OBS-…, part-of:m1875, generates:OBS-…)");
    }
    for l in &links {
        parse_link(l)?;
    }
    let mut b = json!({"case": case, "quote": quote, "labels": labels, "fields": fields, "links": links,
                       "nth": a["nth"].as_u64().unwrap_or(1), "intents": list("intents"), "about": list("about"), "entities": list("entities")});
    for k in ["msg", "project"] {
        if let Some(v) = a[k].as_str().map(str::trim).filter(|v| !v.is_empty()) {
            b[k] = json!(v);
        }
    }
    if let Some(s) = a["supersedes"].as_str().map(|s| s.trim_start_matches('m').to_owned()).or_else(|| a["supersedes"].as_u64().map(|n| n.to_string())) {
        let n: u64 = s.parse().map_err(|_| anyhow::anyhow!("supersedes is a draft's mark id (m1910)"))?;
        b["supersedes"] = json!(n);
    }
    Ok(b)
}

fn short(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s }
}

/// What a read or a draft answers, for a reader: the chats one per line; one chat whole (every prompt with its case uuid, every
/// reply with its msg uuid, then every live mark with its author, labels, verdict, words, note and links); a draft's receipt.
pub fn text(op: &str, v: &Value) -> String {
    let mut out = String::new();
    match op {
        "cases" => {
            out.push_str(&format!("{} chats\n", v["count"]));
            for s in v["sessions"].as_array().into_iter().flatten() {
                out.push_str(&format!("#{}  {}  {}  {} marks, {} reviewed  {} prompts  {}\n", s["n"], s["first_at"].as_str().unwrap_or("").get(..10).unwrap_or(""),
                                      s["state"].as_str().unwrap_or("?"), s["marks"], s["reviewed"], s["prompts"], s["conversation"].as_str().unwrap_or("")));
            }
        }
        "case" => {
            out.push_str(&format!("#{}  {}  {} prompts  ({})\n", v["n"], v["conversation"].as_str().unwrap_or(""), v["prompts"], v["state"].as_str().unwrap_or("")));
            let msgs = v["messages"].as_array().cloned().unwrap_or_default();
            let prompt_of = |owner: &Value| msgs.iter().find(|m| &m["case_n"] == owner && !owner.is_null()).and_then(|m| m["uuid"].as_str()).unwrap_or("?").to_owned();
            for m in &msgs {
                if m["sender"] == "human" {
                    out.push_str(&format!("\n── PROMPT {}  case={}\n", m["case_n"], m["uuid"].as_str().unwrap_or("?")));
                } else {
                    out.push_str(&format!("\n── REPLY to {}  case={}  msg={}\n", m["owner_n"], prompt_of(&m["owner_n"]), m["uuid"].as_str().unwrap_or("?")));
                }
                out.push_str(m["text"].as_str().unwrap_or(""));
                out.push('\n');
            }
            let marks = v["marks"].as_array().cloned().unwrap_or_default();
            out.push_str(&format!("\n── MARKS ({})\n", marks.len()));
            for k in &marks {
                let labels: Vec<&str> = k["labels"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let verdict = k["review"]["verdict"].as_str().unwrap_or("draft");
                let links: Vec<&str> = k["links"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                out.push_str(&format!("m{}  by {}  [{}]  {}  case={}{}\n  «{}»\n", k["id"], k["by"].as_str().unwrap_or("?"), labels.join(" + "), verdict,
                                      k["case_key"].as_str().unwrap_or("?"), k["msg"].as_str().map(|m| format!(" msg={m}")).unwrap_or_default(), short(k["quote"].as_str().unwrap_or(""), 200)));
                if let Some(t) = k["review"]["text"].as_str().filter(|t| !t.is_empty()) {
                    out.push_str(&format!("  Blaze: {t}\n"));
                }
                out.push_str(&format!("  {}\n", short(k["note"].as_str().unwrap_or(""), 400)));
                if !links.is_empty() {
                    out.push_str(&format!("  links {}\n", links.join(" ")));
                }
            }
        }
        _ => out.push_str(&format!("drafted m{} on case {} (#{}), held for Blaze's verdict\n", v["mark"], v["case"].as_str().unwrap_or("?"), v["ordinal"])),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Value {
        json!({"case": "019c9095-01f3-79fe-b06e-1687a96934d3", "quote": "Benchmark is just the 0.1% that was measured.", "labels": ["Discovery"],
               "intent": "what benchmarks miss", "frame": "the arena", "state": "open rooms rank by check", "method": "none", "lineage": "2 Feb m972",
               "links": ["about:m972"], "supersedes": "m1910"})
    }

    #[test]
    fn a_draft_carries_the_five_fields_and_a_lineage_link() {
        let b = draft(&good()).unwrap();
        assert_eq!(b["fields"]["frame"], "the arena");
        assert_eq!(b["supersedes"], 1910);
        for f in FIELDS {
            let mut a = good();
            a[f] = json!("");
            assert!(draft(&a).unwrap_err().to_string().contains(f), "{f} missing must be refused");
        }
        let mut a = good();
        a["links"] = json!([]);
        assert!(draft(&a).is_err());
    }

    #[test]
    fn a_seat_never_drafts_for_rocky_or_an_unknown_label() {
        for l in ["For Rocky", "Approved", ""] {
            let mut a = good();
            a["labels"] = json!([l]);
            assert!(draft(&a).is_err(), "{l:?} must be refused");
        }
    }

    #[test]
    fn a_case_is_read_by_number_or_conversation_id_only() {
        assert_eq!(path("case", &json!({"n": 146})).unwrap(), "/cortex/case/146");
        assert_eq!(path("case", &json!({"n": "#146"})).unwrap(), "/cortex/case/146");
        assert!(path("case", &json!({"n": "../api/marks"})).is_err());
        assert_eq!(path("cases", &json!({"which": "drafted", "limit": 5})).unwrap(), "/cortex/cases?limit=5&state=drafted");
    }
}
