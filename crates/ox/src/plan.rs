//! Plan heads (docs/PLANS.md): a State head whose data is a `plan/1` page. The pure parts live here so they are tested
//! without a server: the subject, validation, the state a plan implies, one step moved, the note, the text.
use anyhow::{bail, Result};
use serde_json::{json, Value};

pub const STATUSES: [&str; 4] = ["proposed", "doing", "done", "dropped"];

/// `plan.<room>.<name>`; room and name are `[a-z0-9][a-z0-9_-]{0,47}`.
pub fn subject(room: &str, name: &str) -> Result<String> {
    for (what, v) in [("room", room), ("name", name)] {
        let ok = !v.is_empty() && v.len() <= 48 && v.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && v.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !ok { bail!("ox plan: the {what} is [a-z0-9][a-z0-9_-]{{0,47}}, not {v:?}"); }
    }
    Ok(format!("plan.{room}.{name}"))
}

/// A plan/1 page (docs/rooms/STRUCTURED-MESSAGES.md in AXXIS): problems and steps with unique ids and titles, every
/// `fixes` naming a problem, every status one of the four.
pub fn validate(v: &Value) -> Result<()> {
    if v["schema"] != "plan/1" { bail!("the page's schema is {}, not plan/1", v["schema"]); }
    if v["title"].as_str().is_none_or(|t| t.trim().is_empty()) { bail!("the plan has no title"); }
    let mut ids = std::collections::HashSet::new();
    let problems = v["problems"].as_array().filter(|a| !a.is_empty()).ok_or_else(|| anyhow::anyhow!("the plan names no problems"))?;
    for p in problems {
        let id = p["id"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow::anyhow!("a problem has no id"))?;
        if p["title"].as_str().is_none_or(|t| t.trim().is_empty()) { bail!("problem {id} has no title"); }
        if !ids.insert(id.to_string()) { bail!("the id {id} is used twice"); }
    }
    let steps = v["steps"].as_array().filter(|a| !a.is_empty()).ok_or_else(|| anyhow::anyhow!("the plan has no steps"))?;
    for s in steps {
        let id = s["id"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow::anyhow!("a step has no id"))?;
        if s["title"].as_str().is_none_or(|t| t.trim().is_empty()) { bail!("step {id} has no title"); }
        if !ids.insert(id.to_string()) { bail!("the id {id} is used twice"); }
        if let Some(st) = s.get("status") {
            if !st.as_str().is_some_and(|x| STATUSES.contains(&x)) { bail!("step {id}: status is one of {}, not {st}", STATUSES.join(", ")); }
        }
        for f in s["fixes"].as_array().into_iter().flatten() {
            let f = f.as_str().unwrap_or("");
            if !problems.iter().any(|p| p["id"] == f) { bail!("step {id} fixes {f:?}, which names no problem"); }
        }
    }
    Ok(())
}

/// The head's state from the steps: done when every step is done or dropped, running when one is doing, else open.
pub fn state(v: &Value) -> &'static str {
    let steps: Vec<&str> = v["steps"].as_array().into_iter().flatten().map(|s| s["status"].as_str().unwrap_or("proposed")).collect();
    if !steps.is_empty() && steps.iter().all(|s| *s == "done" || *s == "dropped") { "done" }
    else if steps.contains(&"doing") { "running" }
    else { "open" }
}

/// One step moved: `S2=doing`. The step must exist and the status be one of the four; the rest of the page is kept.
pub fn step(v: &Value, mv: &str) -> Result<Value> {
    let (id, status) = mv.split_once('=').ok_or_else(|| anyhow::anyhow!("--step is <step id>=<status>, e.g. S2=doing"))?;
    if !STATUSES.contains(&status) { bail!("the status is one of {}, not {status:?}", STATUSES.join(", ")); }
    let mut out = v.clone();
    let steps = out["steps"].as_array_mut().ok_or_else(|| anyhow::anyhow!("the plan has no steps"))?;
    let Some(s) = steps.iter_mut().find(|s| s["id"] == id) else { bail!("no step {id} in this plan") };
    s["status"] = json!(status);
    Ok(out)
}

/// The revision's note: the title and the counts.
pub fn note(v: &Value) -> String {
    let steps = v["steps"].as_array().cloned().unwrap_or_default();
    let count = |st: &str| steps.iter().filter(|s| s["status"].as_str().unwrap_or("proposed") == st).count();
    format!("{} · {} problems · {} steps ({} done, {} doing, {} proposed{})", v["title"].as_str().unwrap_or("plan"),
        v["problems"].as_array().map_or(0, |a| a.len()), steps.len(), count("done"), count("doing"), count("proposed"),
        match count("dropped") { 0 => String::new(), n => format!(", {n} dropped") })
}

/// The newest revision's cargo (subject, state, owner, next_touch, data) out of a `/search?tag=subject:…` answer,
/// state records with a plan/1 page only.
pub fn newest_cargo(v: &Value) -> Option<Value> {
    let rows = v["hits"].as_array().or_else(|| v["items"].as_array())?;
    rows.iter()
        .filter(|h| h["kind"].as_str().is_none_or(|k| k == "state"))
        .max_by_key(|h| h["created_ms"].as_i64().unwrap_or(0))
        .and_then(|h| h["cargo"].as_str().and_then(|c| serde_json::from_str::<Value>(c).ok()))
        .filter(|c| c["data"]["schema"] == "plan/1")
}

/// The newest revision's plan page.
pub fn newest_data(v: &Value) -> Option<Value> {
    newest_cargo(v).and_then(|c| c.get("data").cloned())
}

/// The plan as text: the title, the problems, the steps with status and what each fixes.
pub fn text(v: &Value) -> String {
    let mut out = format!("PLAN  {}{}\n", v["title"].as_str().unwrap_or("?"), v["subtitle"].as_str().map(|s| format!(" · {s}")).unwrap_or_default());
    if let Some(l) = v["lede"].as_str() { out += &format!("      {l}\n"); }
    out += "PROBLEMS\n";
    for p in v["problems"].as_array().into_iter().flatten() {
        out += &format!("  {:<4} {}{}\n", p["id"].as_str().unwrap_or("?"), p["title"].as_str().unwrap_or(""), p["group"].as_str().map(|g| format!(" [{g}]")).unwrap_or_default());
    }
    out += &format!("STEPS{}\n", v["steps_title"].as_str().map(|t| format!(" · {t}")).unwrap_or_default());
    for s in v["steps"].as_array().into_iter().flatten() {
        let fixes: Vec<&str> = s["fixes"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        out += &format!("  {:<4} {:<9} {}{}\n", s["id"].as_str().unwrap_or("?"), s["status"].as_str().unwrap_or("proposed"), s["title"].as_str().unwrap_or(""),
            if fixes.is_empty() { String::new() } else { format!(" · fixes {}", fixes.join(", ")) });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn page() -> Value {
        json!({"schema": "plan/1", "title": "Search", "problems": [{"id": "R1", "title": "dump"}, {"id": "R2", "title": "noise"}],
               "steps": [{"id": "S1", "title": "model", "fixes": ["R1"], "status": "doing"}, {"id": "S2", "title": "tab", "fixes": ["R1", "R2"]}]})
    }
    #[test]
    fn a_subject_is_plan_room_name_and_refuses_bad_names() {
        assert_eq!(subject("oracle", "search").unwrap(), "plan.oracle.search");
        for (r, n) in [("Oracle", "x"), ("oracle", ""), ("oracle", "a b"), ("-x", "y")] { assert!(subject(r, n).is_err(), "{r} {n}"); }
    }
    #[test]
    fn a_valid_plan_passes_and_each_fault_is_named() {
        validate(&page()).unwrap();
        let mut dup = page(); dup["steps"][1]["id"] = json!("R1");
        assert!(validate(&dup).unwrap_err().to_string().contains("used twice"));
        let mut orphan = page(); orphan["steps"][0]["fixes"] = json!(["R9"]);
        assert!(validate(&orphan).unwrap_err().to_string().contains("names no problem"));
        let mut st = page(); st["steps"][0]["status"] = json!("maybe");
        assert!(validate(&st).unwrap_err().to_string().contains("status is one of"));
        let mut no = page(); no["problems"] = json!([]);
        assert!(validate(&no).unwrap_err().to_string().contains("names no problems"));
        let mut schema = page(); schema["schema"] = json!("doc/1");
        assert!(validate(&schema).is_err());
    }
    #[test]
    fn the_state_follows_the_steps_and_a_step_moves_in_a_copy() {
        let p = page();
        assert_eq!(state(&p), "running");
        let moved = step(&p, "S1=done").unwrap();
        assert_eq!(moved["steps"][0]["status"], "done");
        assert_eq!(p["steps"][0]["status"], "doing", "the original is untouched");
        assert_eq!(state(&moved), "open", "S2 is still proposed");
        let all = step(&moved, "S2=dropped").unwrap();
        assert_eq!(state(&all), "done");
        assert!(step(&p, "S9=done").unwrap_err().to_string().contains("no step S9"));
        assert!(step(&p, "S1=maybe").is_err());
        assert!(step(&p, "S1").is_err());
        assert_eq!(note(&all), "Search · 2 problems · 2 steps (1 done, 0 doing, 0 proposed, 1 dropped)");
    }
    #[test]
    fn newest_data_takes_the_newest_state_record_with_a_plan() {
        let hit = |id: &str, ms: i64, data: Value| json!({"id": id, "kind": "state", "created_ms": ms, "cargo": json!({"subject": "plan.o.s", "state": "open", "next_touch": format!("2026-10-2{ms}"), "data": data}).to_string()});
        let v = json!({"hits": [hit("a", 1, page()), hit("b", 3, json!({"schema": "plan/1", "title": "newer", "problems": [], "steps": []})), hit("c", 2, page())]});
        assert_eq!(newest_data(&v).unwrap()["title"], "newer");
        assert_eq!(newest_cargo(&v).unwrap()["next_touch"], "2026-10-23", "a step move keeps the previous revision's date");
        assert!(newest_data(&json!({"hits": []})).is_none());
        let text = text(&page());
        assert!(text.contains("S1   doing     model · fixes R1") && text.contains("R2   noise"), "{text}");
    }
}
