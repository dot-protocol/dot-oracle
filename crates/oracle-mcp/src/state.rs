//! Verify the resolver's evidence without selecting a second set of heads.
use anyhow::{anyhow, bail, Result};
use oracle_proto::Envelope;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// Read-time orientation, not permission to write or evidence of freshness.
/// Reuse the authoritative State; never select a head or invent a destination.
pub fn orientation(state: &Value) -> Value {
    let heads = state["heads"].as_array().map(Vec::len).unwrap_or(0);
    let diagnostics = ["bad", "unknown", "missing_parents"].iter().any(|k|
        state[*k].as_array().is_some_and(|v| !v.is_empty()));
    let uncertain = diagnostics || state["trustworthy"] == false || state["complete"] == false
        || state["coverage"]["sql_complete"] == false
        || matches!(state["state"].as_str(), Some("degraded" | "incomplete"));
    let (status, action) = if uncertain || heads > 1 {
        ("unresolved", "Read the State evidence and reconcile its missing, conflicting or unverified revisions before relying on its directions.")
    } else if heads == 0 {
        ("missing", "Ask the room owner to publish purpose, current and next through the existing State editor; ordinary messages remain available for coordination.")
    } else {
        ("available", "Read the cited State head before acting; a signed description is not proof that its claims are current.")
    };
    let missing: Vec<&str> = if status == "available" {
        ["purpose", "current", "next"].into_iter().filter(|k|
            state["fields"][*k].as_str().is_none_or(|v| v.trim().is_empty())).collect()
    } else { vec![] };
    let status = if !missing.is_empty() { "partial" } else { status };
    let action = if status == "partial" {
        "Ask the room owner to fill the missing orientation fields through the existing State editor, preserving the other fields."
    } else { action };
    json!({"status": status, "missing_fields": missing, "heads": state["heads"],
        "action": action, "freshness": "not_assessed", "enforcement": "advisory"})
}

pub fn orientation_text(value: &Value) -> String {
    let missing = value["missing_fields"].as_array().into_iter().flatten()
        .filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
    format!("ORIENTATION {}{} — {}\n", value["status"].as_str().unwrap_or("unknown"),
        if missing.is_empty() { String::new() } else { format!(" (missing {missing})") },
        value["action"].as_str().unwrap_or(""))
}

/// Read one authoritative snapshot. Keys are current registrations, including revoked
/// keys for historical signatures; this is not a historical key/role registry.
pub fn read(door: &dyn crate::Door, room: &str, as_of_seq: Option<i64>) -> Result<Value> {
    if as_of_seq.is_some_and(|c| c < 1) {
        bail!("as_of_seq must be a positive exclusive seq cursor");
    }
    let cursor = as_of_seq.map(|c| format!("?as_of_seq={c}")).unwrap_or_default();
    let state = door.get(&format!("/rooms/{}/state{cursor}", crate::enc(room)))?;
    let registry = door.get("/keys")?;
    let keys = registry["keys"].as_array().ok_or_else(|| anyhow!("the key registry did not answer"))?;
    verify(state, room, as_of_seq, keys)
}

fn revision(e: &Value, room: &str, keys: &[Value]) -> Result<Value> {
    let row = &e["row"];
    let id = e["id"].as_str().ok_or_else(|| anyhow!("no evidence id"))?;
    if row["id"] != e["id"] || row["seq"] != e["seq"] || row["channel"].as_str() != Some(room) {
        bail!("evidence source row differs from its id, seq or room");
    }
    let raw = row["cargo"].as_str().ok_or_else(|| anyhow!("no revision cargo"))?;
    let mut cargo: Value = serde_json::from_str(raw)?;
    let tags = row["tags"].as_array().ok_or_else(|| anyhow!("no source tags"))?;
    if !tags.iter().any(|t| t == "room-context:v1") {
        bail!("source row is not tagged room-context:v1");
    }
    if row["envelope"].as_i64() == Some(1) {
        if e["native"].as_bool() != Some(false) || !e["envelope"].is_null() {
            bail!("imported evidence claims a native envelope");
        }
        if !["signed:ok", "cargo-signed:ok"].iter().all(|tag| tags.iter().any(|t| t == tag)) {
            bail!("imported without signed:ok and cargo-signed:ok");
        }
        if let Some(inner) = cargo["cargo"].as_str() {
            cargo = serde_json::from_str(inner)?;
        }
    } else {
        if e["native"].as_bool() != Some(true) {
            bail!("native evidence has no native envelope");
        }
        let author = row["author"].as_str().ok_or_else(|| anyhow!("no author"))?;
        let pubkey = row["pubkey"].as_str().ok_or_else(|| anyhow!("no source key"))?;
        if !keys.iter().any(|k| k["handle"].as_str() == Some(author) && k["pubkey"].as_str() == Some(pubkey)) {
            bail!("signed by a key never registered to {author}");
        }
        let env: Envelope = serde_json::from_value(e["envelope"].clone())?;
        env.verify_stored(pubkey).map_err(|err| anyhow!("{err}"))?;
        if env.author != author || env.channel != room || env.cargo.as_deref() != Some(raw)
            || row["content"].as_str() != Some(env.content.as_str())
            || row["kind"].as_str() != Some(env.kind.as_str())
            || row["sig"].as_str() != Some(env.sig.as_str())
        {
            bail!("stored envelope differs from the source row");
        }
        if e.get("signed_ms").is_some_and(|v| v.as_i64() != Some(env.ts_ms)) {
            bail!("signed timestamp differs from the envelope");
        }
    }
    if cargo["schema"] != "room-context/1" || cargo["channel"].as_str() != Some(room)
        || !cargo["fields"].is_object()
        || !cargo["parents"].as_array().is_some_and(|p| p.iter().all(Value::is_string))
    {
        bail!("not a well-formed room-context/1 revision of #{room}");
    }
    for key in ["parents", "fields", "adopts"] {
        let projected = if key == "adopts" { cargo.get(key).filter(|a| a.is_object()).cloned().unwrap_or(Value::Null) } else { cargo[key].clone() };
        if e.get(key).is_some_and(|value| value != &projected) {
            bail!("evidence {key} projection differs from revision cargo");
        }
    }
    Ok(json!({"id": id, "parents": cargo["parents"], "fields": cargo["fields"], "adopts": cargo.get("adopts").filter(|a| a.is_object())}))
}

/// Verify signed bytes and selected-field parity. The server alone folds lineage,
/// archive marks and the head set. A verification failure keeps the diagnostics and
/// returned heads visible, but withholds fields and cannot claim complete/trustworthy.
pub fn verify(mut state: Value, room: &str, requested: Option<i64>, keys: &[Value]) -> Result<Value> {
    if state["schema"] != "room-context/1" || state["channel"].as_str() != Some(room) {
        bail!("#{room} did not answer with an authoritative State");
    }
    let cursor = state["as_of_seq"].as_i64().filter(|c| *c > 0).ok_or_else(|| anyhow!("no exclusive State cursor"))?;
    let through = state["read_through_seq"].as_i64().ok_or_else(|| anyhow!("no State read boundary"))?;
    if cursor > through.saturating_add(1) || requested.is_some_and(|c| c != cursor) {
        bail!("State cursor differs from the requested or available boundary");
    }
    let heads = state["heads"].as_array().ok_or_else(|| anyhow!("no State heads"))?.clone();
    let evidence = state["evidence"].as_array().ok_or_else(|| anyhow!("no State evidence"))?.clone();
    if state["revisions"].as_u64() != Some(evidence.len() as u64) {
        bail!("State revision count differs from evidence coverage");
    }
    let mut seen = HashSet::new();
    let mut verified = HashMap::new();
    let mut failed = Vec::new();
    let (mut native, mut imported) = (0usize, 0usize);
    for e in &evidence {
        let id = e["id"].as_str().ok_or_else(|| anyhow!("no evidence id"))?;
        let seq = e["seq"].as_i64().ok_or_else(|| anyhow!("no evidence seq"))?;
        if seq < 1 || seq >= cursor || !seen.insert(id.to_owned()) {
            bail!("State evidence is duplicated or outside its exclusive cursor");
        }
        match revision(e, room, keys) {
            Ok(p) => {
                if e["native"] == true { native += 1 } else { imported += 1 }
                verified.insert(id.to_owned(), p);
            }
            Err(err) => failed.push(json!({"id": id, "why": format!("client verification: {err:#}")})),
        }
    }
    let mut head_ids = HashSet::new();
    for h in &heads {
        let id = h.as_str().ok_or_else(|| anyhow!("non-string State head"))?;
        if !head_ids.insert(id) { bail!("duplicate State head"); }
        if !verified.contains_key(id) && !failed.iter().any(|f| f["id"].as_str() == Some(id)) {
            failed.push(json!({"id": id, "why": "client verification: returned head has no verified revision evidence"}));
        }
    }
    let status = state["state"].as_str().ok_or_else(|| anyhow!("no State status"))?;
    if !["empty", "alive", "degraded", "incomplete"].contains(&status) {
        bail!("unknown State status");
    }
    if (status == "alive" && heads.len() != 1) || (status == "empty" && !heads.is_empty()) {
        bail!("State status contradicts the returned heads");
    }
    if status == "alive" {
        if let Some(p) = verified.get(heads[0].as_str().unwrap()) {
            let id = heads[0].as_str().unwrap();
            if state["fields"] != p["fields"] || state["adoptions"][id] != p["adopts"] {
                failed.push(json!({"id": id, "why": "client verification: selected fields or adoption differ from signed cargo"}));
            }
        }
    } else if !state["fields"].is_null() {
        bail!("a non-alive State carries selected fields");
    }
    if !failed.is_empty() {
        state["bad"].as_array_mut().ok_or_else(|| anyhow!("no State diagnostics"))?.extend(failed.clone());
        state["state"] = json!("degraded");
        state["fields"] = Value::Null;
        state["complete"] = json!(false);
        state["trustworthy"] = json!(false);
    }
    state["client_verification"] = json!({"native_verified": native, "import_attested": imported, "failures": failed,
        "trust": "current registered keys; imported attestation tags; server alone selects heads and archives"});
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn orientation_does_not_turn_missing_or_conflicting_evidence_into_directions() {
        let ready = json!({"state":"alive", "heads":["head"], "trustworthy":true,
            "complete":true, "fields":{"purpose":"Memory", "current":"Testing", "next":"Review"}});
        assert_eq!(orientation(&ready)["status"], "available");
        assert_eq!(orientation(&ready)["freshness"], "not_assessed");
        let mut partial = ready.clone(); partial["fields"]["next"] = json!("  ");
        assert_eq!(orientation(&partial)["missing_fields"], json!(["next"]));
        assert_eq!(orientation(&partial)["status"], "partial");
        let empty = json!({"state":"empty", "heads":[], "fields":null});
        assert_eq!(orientation(&empty)["status"], "missing");
        for key in ["trustworthy", "complete"] {
            let mut broken = ready.clone(); broken[key] = json!(false);
            assert_eq!(orientation(&broken)["status"], "unresolved");
        }
        let mut conflict = ready.clone(); conflict["heads"] = json!(["a", "b"]);
        assert_eq!(orientation(&conflict)["status"], "unresolved");
        let mut truncated = ready.clone(); truncated["coverage"] = json!({"sql_complete":false});
        assert_eq!(orientation(&truncated)["status"], "unresolved");
        let mut corrupt_empty = empty; corrupt_empty["bad"] = json!([{"id":"bad"}]);
        assert_eq!(orientation(&corrupt_empty)["status"], "unresolved");
        assert_eq!(ready["fields"]["next"], "Review");
    }
    fn imported() -> Value {
        let cargo = json!({"schema": "room-context/1", "channel": "room", "parents": [], "fields": {"goal": "utf8 λ"}}).to_string();
        json!({"schema": "room-context/1", "channel": "room", "state": "alive", "as_of_seq": 2, "read_through_seq": 1,
            "heads": ["old"], "fields": {"goal": "utf8 λ"}, "adoptions": {}, "revisions": 1, "bad": [], "complete": true, "trustworthy": true,
            "evidence": [{"id": "old", "seq": 1, "native": false, "envelope": null,
                "row": {"id": "old", "seq": 1, "channel": "room", "envelope": 1, "cargo": cargo,
                    "tags": ["room-context:v1", "signed:ok", "cargo-signed:ok"]}}]})
    }
    #[test]
    fn cursor_and_coverage_failures_do_not_become_an_alive_state() {
        let v = imported();
        assert_eq!(verify(v.clone(), "room", Some(2), &[]).unwrap()["state"], "alive");
        assert!(verify(v.clone(), "room", Some(1), &[]).is_err());
        let mut future = v.clone(); future["as_of_seq"] = json!(3);
        assert!(verify(future, "room", None, &[]).is_err());
        let mut missing = v.clone(); missing["evidence"] = json!([]);
        assert!(verify(missing, "room", None, &[]).is_err());
        let mut duplicate = v.clone(); duplicate["evidence"].as_array_mut().unwrap().push(v["evidence"][0].clone()); duplicate["revisions"] = json!(2);
        assert!(verify(duplicate, "room", None, &[]).is_err());
        let mut out = v.clone(); out["evidence"][0]["seq"] = json!(2);
        assert!(verify(out, "room", None, &[]).is_err());
        let mut edited = v; edited["evidence"][0]["row"]["seq"] = json!(9);
        let answer = verify(edited, "room", None, &[]).unwrap();
        assert_eq!(answer["state"], "degraded");
        assert_eq!(answer["complete"], false);
        assert!(answer["fields"].is_null());
    }
    #[test]
    fn imported_wrapper_is_explicit_and_unsigned_selected_fields_are_withheld() {
        let mut v = imported();
        let inner = v["evidence"][0]["row"]["cargo"].as_str().unwrap().to_owned();
        v["evidence"][0]["row"]["cargo"] = json!(json!({"cargo": inner}).to_string());
        assert_eq!(verify(v.clone(), "room", None, &[]).unwrap()["client_verification"]["import_attested"], 1);
        v["fields"]["goal"] = json!("unverified projection");
        let answer = verify(v, "room", None, &[]).unwrap();
        assert_eq!(answer["state"], "degraded");
        assert_eq!(answer["trustworthy"], false);
    }
}
