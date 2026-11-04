//! `ox edit` (docs/EDIT.md): the pure parts, tested without a server. An edit is a new record that supersedes the old one in
//! the same room; it keeps what places the record (its thread, subject, type, intent, addressees, refs) and drops what the
//! server derives or what belonged to the old record alone (its own supersedes, reactions, archive marks).
use anyhow::{Context, Result};
use serde_json::{json, Value};

/// The tag prefixes an edit carries over from the record it replaces.
const KEPT: [&str; 8] = ["reply:", "subject:", "type:", "intent:", "to:", "ref:", "facet:", "ask:"];

/// The old record's placing tags, then `supersedes:<id>`; duplicates once.
pub fn kept_tags(tags: &[Value], of: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for t in tags.iter().filter_map(Value::as_str) {
        if KEPT.iter().any(|p| t.starts_with(p)) && !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    }
    out.push(format!("supersedes:{of}"));
    out
}

/// The old cargo (when it is an object) with `edit: {of, why}`; any other cargo is kept under `edit.cargo`.
pub fn cargo(old: Option<Value>, of: &str, why: Option<&str>) -> Value {
    let mut edit = json!({"of": of});
    if let Some(w) = why.map(str::trim).filter(|w| !w.is_empty()) {
        edit["why"] = json!(w);
    }
    match old {
        Some(Value::Object(mut m)) => {
            m.insert("edit".into(), edit);
            Value::Object(m)
        }
        Some(other) => {
            edit["cargo"] = other;
            json!({"edit": edit})
        }
        None => json!({"edit": edit}),
    }
}

/// What changed, in lines: `-2 +3 lines` (a line kept counts in neither).
pub fn change(old: &str, new: &str) -> String {
    let o: Vec<&str> = old.lines().collect();
    let n: Vec<&str> = new.lines().collect();
    let gone = o.iter().filter(|l| !n.contains(l)).count();
    let added = n.iter().filter(|l| !o.contains(l)).count();
    format!("-{gone} +{added} lines")
}

/// The old text in $VISUAL or $EDITOR (vi when neither is set), and what the editor saved.
pub fn in_editor(old: &str) -> Result<String> {
    use std::io::IsTerminal;
    anyhow::ensure!(std::io::stdin().is_terminal(), "no terminal for an editor: give the new text with --text or --file");
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
    let path = std::env::temp_dir().join(format!("ox-edit-{}.md", std::process::id()));
    std::fs::write(&path, old).context("writing the draft")?;
    let status = std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&path).status().context("starting the editor")?;
    let text = std::fs::read_to_string(&path).context("reading the draft back");
    let _ = std::fs::remove_file(&path);
    anyhow::ensure!(status.success(), "the editor exited with {status}: nothing written");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_edit_keeps_what_places_the_record_and_supersedes_it() {
        let tags = json!(["reply:OBS-a", "intent:decision", "supersedes:OBS-older", "react:done", "to:blaze", "intent:decision", "envelope:1"]);
        assert_eq!(kept_tags(tags.as_array().unwrap(), "OBS-b"), ["reply:OBS-a", "intent:decision", "to:blaze", "supersedes:OBS-b"]);
        assert_eq!(kept_tags(&[], "OBS-b"), ["supersedes:OBS-b"]);
    }
    #[test]
    fn the_cargo_keeps_the_old_fields_and_says_what_was_edited_and_why() {
        assert_eq!(cargo(Some(json!({"options": ["a", "b"]})), "OBS-b", Some(" wrong date ")), json!({"options": ["a", "b"], "edit": {"of": "OBS-b", "why": "wrong date"}}));
        assert_eq!(cargo(None, "OBS-b", Some("")), json!({"edit": {"of": "OBS-b"}}), "an empty why is no why");
        assert_eq!(cargo(Some(json!([1, 2])), "OBS-b", None), json!({"edit": {"of": "OBS-b", "cargo": [1, 2]}}), "a cargo that is not an object is kept whole");
    }
    #[test]
    fn the_change_is_counted_in_lines() {
        assert_eq!(change("a\nb\nc", "a\nB\nc\nd"), "-1 +2 lines");
        assert_eq!(change("same", "same"), "-0 +0 lines");
    }
}
