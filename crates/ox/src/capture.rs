//! Session capture for coding agents (the competitor map's agent-adapters and session-extraction rows, 4 Nov 2026). A hook in
//! Claude Code, Codex or Cursor runs `ox capture --hook <agent>` when a session compacts or ends. The hook hands over its
//! JSON on stdin; this file reads it, puts one line in the spool and returns at once: Codex gives a SessionEnd hook one to
//! three seconds, and a hook must never hold an agent up. A detached `ox capture --drain` then reads each spooled session's
//! transcript from where the last capture stopped, asks the Oracle's `/extract` for the durable things in it, and signs
//! each one as this seat's memory. The pure parts live here so they are tested without a server.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const AGENTS: [&str; 3] = ["claude-code", "codex", "cursor"];
/// A turn longer than this is cut: a pasted log or file says little that its first part does not.
pub const TURN_MAX: usize = 2_000;
/// Less new text than this is not worth a model call.
pub const MIN_NEW: usize = 200;
/// One /extract call reads this much (the server's MAX_INPUT); a capture sends at most `MAX_PIECES` of them, the newest.
pub const PIECE: usize = 8_000;
pub const MAX_PIECES: usize = 6;

/// The text in pieces of at most `PIECE` characters, cut between turns where it can be, oldest first; only the newest
/// `MAX_PIECES` are kept (a capture of a long session reads its end, and says how much it left).
pub fn pieces(text: &str) -> (Vec<String>, usize) {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for turn in text.split("\n\n") {
        let n = turn.chars().count();
        if !cur.is_empty() && cur.chars().count() + 2 + n > PIECE {
            out.push(std::mem::take(&mut cur));
        }
        if n > PIECE {
            out.push(turn.chars().take(PIECE).collect());
            continue;
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        cur.push_str(turn);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    let left = out.len().saturating_sub(MAX_PIECES);
    (out.split_off(left), left)
}

/// What a hook hands over, as kept in the spool.
#[derive(Debug, PartialEq)]
pub struct Spooled {
    pub agent: String,
    pub session: String,
    pub transcript: Option<PathBuf>,
    pub event: String,
    pub last_message: Option<String>,
}

/// The hook's stdin, read the way each agent writes it: Claude Code and Codex send `session_id`, Cursor `conversation_id`;
/// all three send `transcript_path` (Cursor's may be null) and `hook_event_name`; Codex's Stop adds `last_assistant_message`.
pub fn from_hook(agent: &str, stdin: &str) -> Result<Spooled> {
    if !AGENTS.contains(&agent) {
        bail!("--hook is one of {}", AGENTS.join(", "));
    }
    let v: Value = serde_json::from_str(stdin.trim()).context("the hook's stdin is not JSON")?;
    let session = v["session_id"].as_str().or(v["conversation_id"].as_str()).context("the hook names no session")?;
    Ok(Spooled {
        agent: agent.to_string(),
        session: session.to_string(),
        transcript: v["transcript_path"].as_str().filter(|p| !p.is_empty()).map(PathBuf::from),
        event: v["hook_event_name"].as_str().unwrap_or("").to_string(),
        last_message: v["last_assistant_message"].as_str().map(str::to_string),
    })
}

/// A session id as a file name: only what is safe on every system.
pub fn safe(id: &str) -> String {
    let s: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(80).collect();
    if s.is_empty() { "session".into() } else { s }
}

/// Capture may hold private transcript text before redaction. Restrict its directories
/// independently of the caller's umask or the enclosing ox home's permissions.
pub fn private_dir(home: &Path, child: Option<&str>) -> Result<PathBuf> {
    let root = home.join("capture");
    for path in std::iter::once(root.clone()).chain(child.map(|c| root.join(c))) {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)] {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        let meta = std::fs::symlink_metadata(&path)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            bail!("capture directory must be a real directory: {}", path.display());
        }
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(child.map_or(root.clone(), |c| root.join(c)))
}

/// Put one capture in the spool (`<home>/capture/queue/<ms>-<session>-<nonce>.json`) and return its path.
pub fn spool(home: &Path, s: &Spooled, now_ms: i64) -> Result<PathBuf> {
    let dir = private_dir(home, Some("queue"))?;
    let file = dir.join(format!("{now_ms}-{}-{}.json", safe(&s.session), oracle_proto::random_hex(8)));
    let v = json!({"agent": s.agent, "session": s.session, "transcript": s.transcript, "event": s.event,
        "last_message": s.last_message, "at": now_ms});
    crate::write_durable(&file, &serde_json::to_string(&v)?)?;
    Ok(file)
}

pub fn unspool(v: &Value) -> Option<Spooled> {
    Some(Spooled {
        agent: v["agent"].as_str()?.to_string(),
        session: v["session"].as_str()?.to_string(),
        transcript: v["transcript"].as_str().map(PathBuf::from),
        event: v["event"].as_str().unwrap_or("").to_string(),
        last_message: v["last_message"].as_str().map(str::to_string),
    })
}

/// How many transcript lines of a session were already read (`<home>/capture/sessions/<session>.json`).
pub fn done_lines(home: &Path, session: &str) -> usize {
    std::fs::read_to_string(home.join("capture").join("sessions").join(format!("{}.json", safe(session))))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["lines"].as_u64())
        .unwrap_or(0) as usize
}

pub fn set_done_lines(home: &Path, session: &str, lines: usize, now_ms: i64) -> Result<()> {
    let dir = private_dir(home, Some("sessions"))?;
    crate::write_durable(&dir.join(format!("{}.json", safe(session))), &json!({"lines": lines, "at": now_ms}).to_string())?;
    Ok(())
}

/// The text of one content value: a string, or the text parts of a list (tool calls and results are left out).
fn text_of(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| matches!(p["type"].as_str(), None | Some("text" | "input_text" | "output_text")))
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// One transcript line as (who, text), whichever agent wrote it: Claude Code (`type` user/assistant, `message.content`),
/// Codex (`payload` of type message with `role` and `content`), or a plain `role`/`content` line. Anything else is None.
pub fn turn(line: &str) -> Option<(&'static str, String)> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v["isMeta"] == true || v["isSidechain"] == true {
        return None;
    }
    let m = if v["payload"].is_object() { &v["payload"] } else if v["message"].is_object() { &v["message"] } else { &v };
    let role = m["role"].as_str().or(v["type"].as_str()).or(v["role"].as_str())?;
    let who = match role {
        "user" => "person",
        "assistant" => "agent",
        _ => return None,
    };
    let text = text_of(&m["content"]).trim().to_string();
    // a user line that only carries a tool result, and the harness's own injected blocks, are not the person speaking
    if text.is_empty() || text.starts_with("<system-reminder>") || text.starts_with("<command-") || text.starts_with("Caveat:") {
        return None;
    }
    let text = if text.chars().count() > TURN_MAX {
        format!("{} …", text.chars().take(TURN_MAX).collect::<String>())
    } else {
        text
    };
    Some((who, text))
}

/// The turns after the first `done` lines of a transcript, as text for the extractor, and the transcript's line count.
/// A transcript that is not JSON lines is read as plain text.
pub fn new_text(transcript: &str, done: usize) -> (String, usize) {
    let lines: Vec<&str> = transcript.lines().collect();
    let total = lines.len();
    let fresh = &lines[done.min(total)..];
    let json_lines = lines.iter().take(20).filter(|l| l.trim_start().starts_with('{')).count();
    if json_lines == 0 {
        return (fresh.join("\n"), total);
    }
    let text = fresh.iter().filter_map(|l| turn(l)).map(|(who, t)| format!("{who}: {t}")).collect::<Vec<_>>().join("\n\n");
    (text, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_at_the_same_time_keep_separate_private_files() {
        let home = std::env::temp_dir().join(format!("ox-capture-private-{}", oracle_proto::random_hex(8)));
        let s = from_hook("codex", r#"{"session_id":"private-session","last_assistant_message":"private fixture text"}"#).unwrap();
        let first = spool(&home, &s, 7).unwrap();
        let second = spool(&home, &s, 7).unwrap();
        assert_ne!(first, second, "simultaneous hook events must not overwrite each other");
        set_done_lines(&home, &s.session, 3, 7).unwrap();
        crate::capture_log(&home, "private diagnostic");
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            for dir in ["capture", "capture/queue", "capture/sessions"] {
                assert_eq!(std::fs::metadata(home.join(dir)).unwrap().permissions().mode() & 0o777, 0o700, "{dir}");
            }
            for file in [first.clone(), second, home.join("capture/sessions/private-session.json"), home.join("capture/log")] {
                assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600, "{file:?}");
            }
            // Previously created capture directories are tightened too.
            std::fs::set_permissions(home.join("capture"), std::fs::Permissions::from_mode(0o755)).unwrap();
            private_dir(&home, None).unwrap();
            assert_eq!(std::fs::metadata(home.join("capture")).unwrap().permissions().mode() & 0o777, 0o700);
        }
        assert!(std::fs::read_to_string(first).unwrap().contains("private fixture text"));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn capture_refuses_a_redirected_directory() {
        let home = std::env::temp_dir().join(format!("ox-capture-link-{}", oracle_proto::random_hex(8)));
        std::fs::create_dir_all(home.join("outside")).unwrap();
        std::os::unix::fs::symlink(home.join("outside"), home.join("capture")).unwrap();
        let s = from_hook("codex", r#"{"session_id":"s","last_assistant_message":"private fixture"}"#).unwrap();
        assert!(spool(&home, &s, 1).is_err());
        assert_eq!(std::fs::read_dir(home.join("outside")).unwrap().count(), 0);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn each_agent_s_hook_is_read() {
        let cc = from_hook("claude-code", r#"{"session_id":"abc-1","transcript_path":"/t/abc.jsonl","hook_event_name":"PreCompact"}"#).unwrap();
        assert_eq!((cc.session.as_str(), cc.event.as_str()), ("abc-1", "PreCompact"));
        assert_eq!(cc.transcript, Some(PathBuf::from("/t/abc.jsonl")));
        let cursor = from_hook("cursor", r#"{"conversation_id":"c9","transcript_path":null,"hook_event_name":"sessionEnd"}"#).unwrap();
        assert_eq!((cursor.session.as_str(), cursor.transcript), ("c9", None));
        let codex = from_hook("codex", r#"{"session_id":"x","transcript_path":"/r.jsonl","hook_event_name":"Stop","last_assistant_message":"done"}"#).unwrap();
        assert_eq!(codex.last_message.as_deref(), Some("done"));
        assert!(from_hook("vim", "{}").is_err(), "an unknown agent is refused");
        assert!(from_hook("codex", r#"{"transcript_path":"/r"}"#).is_err(), "no session, nothing spooled");
        assert!(from_hook("codex", "not json").is_err());
    }

    #[test]
    fn transcripts_of_each_shape_become_turns() {
        let claude = [
            r#"{"type":"user","message":{"role":"user","content":"We ship oracle-core v0.1.0 on Friday."}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Noted: Friday."},{"type":"tool_use","name":"Bash","input":{}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#,
            r#"{"type":"system","content":"compacted"}"#,
        ]
        .join("\n");
        let (text, total) = new_text(&claude, 0);
        assert_eq!(total, 5);
        assert_eq!(text, "person: We ship oracle-core v0.1.0 on Friday.\n\nagent: Noted: Friday.");
        let codex = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Use pnpm here."}]}}
{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]}}
{"type":"response_item","payload":{"type":"function_call","name":"shell"}}"#;
        assert_eq!(new_text(codex, 0).0, "person: Use pnpm here.");
        assert_eq!(new_text("plain notes\nsecond line", 1), ("second line".to_string(), 2), "plain text is read as it is");
    }

    #[test]
    fn a_capture_reads_only_what_came_after_the_last_one() {
        let t = (0..4).map(|i| format!(r#"{{"type":"user","message":{{"role":"user","content":"turn {i}"}}}}"#)).collect::<Vec<_>>().join("\n");
        assert_eq!(new_text(&t, 3).0, "person: turn 3");
        assert_eq!(new_text(&t, 9).0, "", "past the end: nothing new");
        let home = std::env::temp_dir().join(format!("ox-capture-{}", std::process::id()));
        assert_eq!(done_lines(&home, "s/../1"), 0);
        set_done_lines(&home, "s/../1", 4, 1).unwrap();
        assert_eq!(done_lines(&home, "s/../1"), 4);
        let s = from_hook("codex", r#"{"session_id":"s/../1","hook_event_name":"SessionEnd"}"#).unwrap();
        let f = spool(&home, &s, 7).unwrap();
        assert!(f.starts_with(home.join("capture/queue")), "a session id cannot climb out of the spool: {f:?}");
        let back = unspool(&serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap()).unwrap();
        assert_eq!(back, s);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_long_session_goes_in_pieces_cut_between_turns_and_the_newest_are_kept() {
        let turn = |i: usize| format!("person: {}", i.to_string().repeat(3_000));
        let text = (0..20).map(turn).collect::<Vec<_>>().join("\n\n");
        let (p, left) = pieces(&text);
        assert_eq!(p.len(), MAX_PIECES);
        assert!(left > 0, "older pieces are counted, not silently lost");
        assert!(p.iter().all(|x| x.chars().count() <= PIECE));
        assert!(p.last().unwrap().ends_with(&turn(19)), "the newest turn is in the last piece");
        assert!(p.iter().all(|x| x.starts_with("person: ")), "a piece starts on a turn");
        assert_eq!(pieces("person: short"), (vec!["person: short".to_string()], 0));
        assert_eq!(pieces("").0.len(), 0);
    }

    #[test]
    fn a_long_turn_is_cut() {
        let long = "x".repeat(TURN_MAX + 50);
        let (_, t) = turn(&json!({"role": "user", "content": long}).to_string()).unwrap();
        assert_eq!(t.chars().count(), TURN_MAX + 2);
    }
}
