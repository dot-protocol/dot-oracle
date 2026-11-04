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
        if let Some(r) = s.get("rank") {
            if !r.as_u64().is_some_and(|n| (1..=100_000).contains(&n)) { bail!("step {id}: rank is a whole number from 1, not {r}"); }
        }
        if let Some(a) = s.get("after") {
            let list = a.as_array().ok_or_else(|| anyhow::anyhow!("step {id}: after is a list of step ids"))?;
            for x in list {
                let x = x.as_str().filter(|x| !x.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("step {id}: an after entry is empty"))?;
                match x.split_once(':') {
                    Some((plan, st)) => {
                        if plan.split('.').count() != 3 || !plan.starts_with("plan.") || st.is_empty() {
                            bail!("step {id}: after {x:?} names another plan's step as plan.<room>.<name>:<step>");
                        }
                    }
                    None if x == id => bail!("step {id} waits on itself"),
                    None if !steps.iter().any(|o| o["id"] == x) => bail!("step {id} waits on {x}, which is no step of this plan"),
                    None => {}
                }
            }
        }
    }
    if let Some(c) = cycle(steps) { bail!("the steps wait on each other in a circle: {}", c.join(" -> ")); }
    Ok(())
}

/// What a step waits on (`after`): a step of this plan (`S3`) or of another plan (`plan.oracle.search:T14`).
pub fn after_of(s: &Value) -> Vec<&str> {
    s["after"].as_array().into_iter().flatten().filter_map(Value::as_str).collect()
}

/// A circle among the steps of one plan through `after`, as the ids walked, or None.
fn cycle(steps: &[Value]) -> Option<Vec<String>> {
    let id = |s: &Value| s["id"].as_str().unwrap_or("").to_string();
    let local = |s: &Value| after_of(s).into_iter().filter(|x| !x.contains(':')).map(str::to_string).collect::<Vec<_>>();
    let edges: std::collections::BTreeMap<String, Vec<String>> = steps.iter().map(|s| (id(s), local(s))).collect();
    fn walk(n: &str, edges: &std::collections::BTreeMap<String, Vec<String>>, path: &mut Vec<String>, done: &mut std::collections::BTreeSet<String>) -> Option<Vec<String>> {
        if let Some(i) = path.iter().position(|p| p == n) {
            let mut c = path[i..].to_vec();
            c.push(n.to_string());
            return Some(c);
        }
        if done.contains(n) { return None; }
        path.push(n.to_string());
        for m in edges.get(n).into_iter().flatten() {
            if let Some(c) = walk(m, edges, path, done) { return Some(c); }
        }
        path.pop();
        done.insert(n.to_string());
        None
    }
    let mut done = std::collections::BTreeSet::new();
    edges.keys().find_map(|n| walk(n, &edges, &mut vec![], &mut done))
}

/// One step's `rank` or `after` set in a copy: `S2=3` / `S2=` (rank), `S2=S1,plan.oracle.search:T14` / `S2=` (after); an
/// empty value clears it. The page is validated after the change.
pub fn set(v: &Value, field: &str, mv: &str) -> Result<Value> {
    let (id, val) = mv.split_once('=').ok_or_else(|| anyhow::anyhow!("--{field} is <step id>=<value>, e.g. S2={}", if field == "rank" { "3" } else { "S1" }))?;
    let mut out = v.clone();
    let steps = out["steps"].as_array_mut().ok_or_else(|| anyhow::anyhow!("the plan has no steps"))?;
    let Some(s) = steps.iter_mut().find(|s| s["id"] == id) else { bail!("no step {id} in this plan") };
    let obj = s.as_object_mut().ok_or_else(|| anyhow::anyhow!("step {id} is not an object"))?;
    if val.trim().is_empty() {
        obj.remove(field);
    } else if field == "rank" {
        obj.insert("rank".into(), json!(val.trim().parse::<u64>().map_err(|_| anyhow::anyhow!("--rank {id}={val}: a whole number"))?));
    } else {
        obj.insert("after".into(), json!(val.split(',').map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>()));
    }
    validate(&out)?;
    Ok(out)
}

/// Every open step of every plan (proposed or doing) as one pipeline: ranked steps first by rank, then the unranked ones in
/// each plan's own order; each with its owner and what it still waits on (an `after` step that is not done or dropped, read
/// across plans). `plans` is (subject, page).
pub fn pipeline(plans: &[(String, Value)]) -> String {
    let finished = |st: &str| st == "done" || st == "dropped";
    let status_of = |subject: &str, step: &str| -> Option<String> {
        plans.iter().find(|(s, _)| s == subject)?.1["steps"].as_array()?.iter().find(|x| x["id"] == step)
            .map(|x| x["status"].as_str().unwrap_or("proposed").to_string())
    };
    let mut rows: Vec<(u64, usize, String, &Value)> = vec![];
    let (mut done, mut total) = (0, 0);
    for (pi, (subject, page)) in plans.iter().enumerate() {
        for (si, st) in page["steps"].as_array().into_iter().flatten().enumerate() {
            total += 1;
            if finished(st["status"].as_str().unwrap_or("proposed")) { done += 1; continue; }
            rows.push((st["rank"].as_u64().unwrap_or(u64::MAX), pi * 10_000 + si, subject.clone(), st));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let ranked = rows.iter().filter(|r| r.0 != u64::MAX).count();
    let mut out = format!("PIPELINE  {} open steps in {} plans ({ranked} ranked), {done} of {total} done or dropped\n", rows.len(), plans.len());
    let mut header = false;
    for (rank, _, subject, st) in &rows {
        if *rank == u64::MAX && !header {
            out += "UNRANKED, in each plan's order\n";
            header = true;
        }
        let waits: Vec<String> = after_of(st).into_iter().filter_map(|a| {
            let (p, s) = a.split_once(':').unwrap_or((subject.as_str(), a));
            let st = status_of(p, s);
            match st.as_deref() {
                Some(x) if finished(x) => None,
                Some(x) => Some(format!("{a} ({x})")),
                None => Some(format!("{a} (not found)")),
            }
        }).collect();
        out += &format!("  {:<5} {:<26} {:<6} {:<8} {:<10} {}{}\n",
            if *rank == u64::MAX { "-".into() } else { format!("#{rank}") }, subject, st["id"].as_str().unwrap_or("?"),
            st["status"].as_str().unwrap_or("proposed"), st["owner"].as_str().unwrap_or("-"), st["title"].as_str().unwrap_or(""),
            if waits.is_empty() { String::new() } else { format!(" · waits on {}", waits.join(", ")) });
    }
    out
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
        let after = after_of(s);
        out += &format!("  {:<4} {:<9} {}{}{}{}\n", s["id"].as_str().unwrap_or("?"), s["status"].as_str().unwrap_or("proposed"),
            s["rank"].as_u64().map(|r| format!("#{r} ")).unwrap_or_default(), s["title"].as_str().unwrap_or(""),
            if fixes.is_empty() { String::new() } else { format!(" · fixes {}", fixes.join(", ")) },
            if after.is_empty() { String::new() } else { format!(" · after {}", after.join(", ")) });
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
    fn rank_and_after_are_checked_and_a_circle_is_refused() {
        let mut p = page();
        p["steps"][1]["rank"] = json!(2);
        p["steps"][1]["after"] = json!(["S1", "plan.oracle.search:T14"]);
        validate(&p).unwrap();
        let mut bad = p.clone(); bad["steps"][0]["rank"] = json!(0);
        assert!(validate(&bad).unwrap_err().to_string().contains("rank is a whole number"));
        let mut bad = p.clone(); bad["steps"][1]["after"] = json!(["S9"]);
        assert!(validate(&bad).unwrap_err().to_string().contains("no step of this plan"));
        let mut bad = p.clone(); bad["steps"][1]["after"] = json!(["search:T14"]);
        assert!(validate(&bad).unwrap_err().to_string().contains("plan.<room>.<name>:<step>"));
        let mut bad = p.clone(); bad["steps"][1]["after"] = json!(["S2"]);
        assert!(validate(&bad).unwrap_err().to_string().contains("waits on itself"));
        let mut circle = p.clone(); circle["steps"][0]["after"] = json!(["S2"]);
        assert!(validate(&circle).unwrap_err().to_string().contains("S1 -> S2 -> S1"), "{}", validate(&circle).unwrap_err());
        let set_rank = set(&page(), "rank", "S2=3").unwrap();
        assert_eq!(set_rank["steps"][1]["rank"], 3);
        assert!(set(&set_rank, "rank", "S2=").unwrap()["steps"][1].get("rank").is_none(), "an empty value clears it");
        assert!(set(&page(), "after", "S1=S2").unwrap()["steps"][0]["after"] == json!(["S2"]));
        assert!(set(&set(&page(), "after", "S1=S2").unwrap(), "after", "S2=S1").is_err(), "a move that makes a circle is refused");
        assert!(set(&page(), "rank", "S2=x").is_err());
        assert!(text(&p).contains("S2   proposed  #2 tab · fixes R1, R2 · after S1, plan.oracle.search:T14"), "{}", text(&p));
    }
    #[test]
    fn the_pipeline_orders_by_rank_across_plans_and_names_what_each_step_waits_on() {
        let a = json!({"schema": "plan/1", "title": "A", "problems": [{"id": "R1", "title": "x"}], "steps": [
            {"id": "S1", "title": "first in A", "status": "doing", "rank": 2, "owner": "rocky"},
            {"id": "S2", "title": "waits on B", "rank": 1, "after": ["plan.b.b:T1", "S1"]},
            {"id": "S3", "title": "finished", "status": "done", "rank": 3},
            {"id": "S4", "title": "unranked in A"}]});
        let b = json!({"schema": "plan/1", "title": "B", "problems": [{"id": "R1", "title": "y"}], "steps": [
            {"id": "T1", "title": "done in B", "status": "done"}, {"id": "T2", "title": "unranked in B", "after": ["plan.z.z:Q1"]}]});
        let t = pipeline(&[("plan.a.a".into(), a), ("plan.b.b".into(), b)]);
        let lines: Vec<&str> = t.lines().collect();
        assert!(lines[0].starts_with("PIPELINE  4 open steps in 2 plans (2 ranked), 2 of 6 done or dropped"), "{t}");
        assert!(lines[1].contains("#1") && lines[1].contains("waits on B") && lines[1].ends_with("waits on S1 (doing)"), "B's T1 is done, so only S1 is left: {t}");
        assert!(lines[2].contains("#2") && lines[2].contains("first in A") && lines[2].contains("rocky"));
        assert_eq!(lines[3], "UNRANKED, in each plan's order");
        assert!(lines[4].contains("unranked in A") && lines[5].contains("unranked in B") && lines[5].ends_with("waits on plan.z.z:Q1 (not found)"), "{t}");
        assert!(!t.contains("finished"), "done steps are counted, not listed");
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
