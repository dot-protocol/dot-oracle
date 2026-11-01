//! `ox mcp` — this seat, as an MCP server over stdio.
//!
//! The third door onto the same Oracle, after the HTTP API and the CLI, for harnesses that
//! speak the Model Context Protocol. It runs on the agent's own machine, so the seat's key
//! never leaves it: every write is signed here, exactly as `ox say` signs it. A tool result
//! never claims a write the server did not accept — an unreachable Oracle yields "QUEUED",
//! with the file, and `oracle_flush` delivers it.
//!
//! The tools themselves live in `oracle-mcp`, shared with the server's `POST /mcp`; this file
//! is only the door: `Ox` as a [`Door`], and the stdio loop. Wire: JSON-RPC 2.0, one message
//! per line on stdin/stdout. Nothing else is ever printed to stdout.

use super::Ox;
use anyhow::Result;
use oracle_mcp::{Door, Written};
use oracle_proto::{pubkey_hex, Kind};
use serde_json::{json, Value};
use std::io::Write;

impl Door for Ox {
    fn handle(&self) -> &str {
        &self.cfg.handle
    }
    fn get(&self, path: &str) -> Result<Value> {
        Ox::get(self, path)
    }
    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        Ox::post(self, path, body)
    }
    fn get_bytes(&self, path: &str) -> Result<(String, Vec<u8>)> {
        let (body, ctype) = self.bytes("GET", path, None, None)?;
        Ok((ctype, body))
    }
    fn write(&self, kind: Kind, room: &str, text: &str, cargo: Option<String>, tags: Vec<String>, op_id: Option<String>) -> Result<Written> {
        self.try_write(kind, room, text, cargo, tags, op_id)
    }
    fn flush(&self) -> Result<(usize, usize)> {
        Ox::flush(self)
    }
    fn state(&self, room: &str) -> Result<Value> {
        self.room_state(room)
    }
    fn state_as_of(&self, room: &str, as_of: i64) -> Result<Value> {
        self.room_state_at(room, Some(as_of))
    }
    fn recorded_write(&self, op_id: &str) -> Result<Option<Value>> {
        self.recorded_op(op_id)
    }
    fn queued(&self) -> usize {
        if let Some(Ok(v)) = self.ask_service(json!({"op": "queued"})) {
            return v["queued"].as_u64().unwrap_or(0) as usize;
        }
        std::fs::read_dir(self.outbox())
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .count()
            })
            .unwrap_or(0)
    }
    fn identity(&self) -> String {
        format!(
            "key {}… · {}",
            self.key()
                .map(|k| pubkey_hex(&k))
                .unwrap_or_else(|_| "????????".into())
                .get(..8)
                .unwrap_or("????????"),
            self.base()
        )
    }
}

/// The `ox` program this process was started from: its path, read once at start (after an install replaces the file,
/// Linux reports the running image as "<path> (deleted)", which has no metadata and cannot be exec'd), and that file's
/// size and modification time then. When `ox` is replaced, the path names a new file and the stamp changes.
struct Started {
    path: Option<std::path::PathBuf>,
    stamp: Option<(u64, std::time::SystemTime)>,
}

fn stamp_of(path: &Option<std::path::PathBuf>) -> Option<(u64, std::time::SystemTime)> {
    let m = std::fs::metadata(path.as_ref()?).ok()?;
    Some((m.len(), m.modified().ok()?))
}

impl Started {
    fn now() -> Started {
        let path = std::env::current_exe().ok();
        let stamp = stamp_of(&path);
        Started { path, stamp }
    }
}

/// True when the `ox` at the path this process started from is no longer that program (an install replaced it).
fn replaced(started: &Started) -> bool {
    started.stamp.is_some() && stamp_of(&started.path) != started.stamp
}

/// The env var a replaced server hands its successor: the request that found it out of date, plus any input it had
/// already read but not answered. The successor answers those first, so the client never sees the switch.
const REPLAY: &str = "OX_MCP_REPLAY";

/// Become the new `ox`: exec the program now at `exe` with this process's own arguments, on the same stdin and stdout
/// (Blaze, 2026-10-03: installing ox must not leave every seat's MCP refusing until a person runs /mcp). The tools
/// keep no session state between calls, so the successor serves the same client from the next line on. Returns only
/// if the exec failed; the caller then refuses the call as before.
#[cfg(unix)]
fn become_new(exe: &std::path::Path, replay: &str) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(exe).args(std::env::args_os().skip(1)).env(REPLAY, replay).exec()
}

/// Why this MCP server must not answer tools any more, if it must not: the program on disk was updated after it
/// started, or the machine's Oracle service speaks another protocol. An MCP client keeps a server process for
/// the whole session, so without this an agent runs old code for days (three on Blaze's Mac ran from 26 Sep to
/// 1 Oct) while the CLI beside it is new.
fn stale(ox: &Ox, started: &Started) -> Option<String> {
    if replaced(started) {
        return Some("this Oracle MCP server is out of date: ox was updated on disk after it started. Reconnect the oracle-core MCP server (in Claude Code: /mcp, then reconnect) to load the new version. Nothing was done.".into());
    }
    match ox.ask_service(json!({"op": "hello"})) {
        Some(Err(crate::service::Unanswered::Incompatible(why))) => return Some(why),
        Some(Ok(v)) if v["ok"] == true && v["protocol"] != crate::service::PROTOCOL => {
            return Some(format!("the Oracle service speaks {} and this MCP server speaks {}: reconnect the oracle-core MCP server to load the matching version. Nothing was done.",
                v["protocol"].as_str().unwrap_or("?"), crate::service::PROTOCOL));
        }
        _ => {}
    }
    None
}

/// One doorbell look (`ox doorbell-tick`): the seat's open asks and the rooms' news, for the session in this environment.
pub fn doorbell_tick(ox: &Ox) {
    if let Some(bell) = crate::doorbell::Doorbell::from_env(&ox.home) {
        // The server's own derived list of open asks waiting on this seat: not a page of raw messages.
        if let Ok(open) = ox.get("/asks?state=open") {
            let _ = bell.tick(&open, &|p: &str| ox.get(p).ok(), crate::now_ms());
        }
    }
}

/// The notification that makes an MCP client list the tools again.
fn list_changed() -> Value {
    json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
}

/// This stdio server tells its client when its tools change (a successor after an install), so it says so at initialize.
fn announce_list_changes(msg: &Value, mut reply: Value) -> Value {
    if msg["method"] == "initialize" && reply["result"]["capabilities"]["tools"].is_object() {
        reply["result"]["capabilities"]["tools"]["listChanged"] = json!(true);
    }
    reply
}

pub fn serve(ox: &Ox) -> Result<()> {
    let (stdin, mut stdout) = (std::io::stdin(), std::io::stdout());
    let started = Started::now();
    // The doorbell for the Claude Code session that started this process, if one did (shadow: it logs, never sends).
    // Each look is a fresh `ox doorbell-tick` process run from this executable's path, so installing a new ox changes
    // the doorbell on its next look; this long-lived process never needs a restart for it (Blaze, 2026-10-01: "how
    // many more times will I have to do this?").
    if crate::doorbell::Doorbell::from_env(&ox.home).is_some() {
        if let Ok(exe) = std::env::current_exe() {
            // The look runs as this seat: a server started as `ox --home <seat> mcp` (every seat on core) passes its
            // home on, or the child would read ~/.oracle and fail silently (found on core, 2026-10-03).
            let home = ox.home.clone();
            std::thread::spawn(move || loop {
                let child = std::process::Command::new(&exe).arg("--home").arg(&home).arg("doorbell-tick").env_remove(REPLAY)
                    .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
                if let Ok(mut c) = child {
                    // A look that hangs is killed rather than piling up behind the next one.
                    let t = std::time::Instant::now();
                    while matches!(c.try_wait(), Ok(None)) && t.elapsed() < std::time::Duration::from_secs(60) {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                    if matches!(c.try_wait(), Ok(None)) {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(15));
            });
        }
    }
    // Lines a predecessor read but did not answer (see `become_new`), first.
    let mut queued: std::collections::VecDeque<String> =
        std::env::var(REPLAY).unwrap_or_default().lines().map(String::from).collect();
    // A successor runs new code, but the client still holds the old tool list from its initialize: tell it to list the
    // tools again, or a tool shipped since (oracle_claims, oracle_hallway) never appears (Jobs, 2026-10-10).
    if std::env::var_os(REPLAY).is_some() {
        writeln!(stdout, "{}", list_changed())?;
        stdout.flush()?;
    }
    let mut input = std::io::BufReader::new(stdin.lock());
    loop {
        let line = match queued.pop_front() {
            Some(l) => l,
            None => {
                let mut l = String::new();
                if std::io::BufRead::read_line(&mut input, &mut l)? == 0 {
                    break;
                }
                l
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) if msg["method"] == "tools/call" && stale(ox, &started).is_some() => {
                #[cfg(unix)]
                if replaced(&started) {
                    if let Some(exe) = &started.path {
                        // Hand over this request, anything still queued, and what is buffered unread.
                        let mut carry: Vec<String> = vec![line.trim_end().to_string()];
                        carry.extend(queued.drain(..));
                        carry.extend(String::from_utf8_lossy(input.buffer())
                            .lines().map(String::from));
                        stdout.flush()?;
                        let _ = become_new(exe, &carry.join("\n"));
                    }
                }
                Some(json!({"jsonrpc": "2.0", "id": msg["id"],
                    "result": {"content": [{"type": "text", "text": stale(ox, &started).unwrap_or_default()}], "isError": true}}))
            }
            Ok(msg) => oracle_mcp::handle(ox, &msg, false).map(|r| announce_list_changes(&msg, r)),
            Err(e) => Some(
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}}),
            ),
        };
        if let Some(r) = reply {
            writeln!(stdout, "{r}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_mcp_retry_uses_real_journal_and_stores_one_observation() {
        let server = crate::signed_read_tests::oracle();
        let first = server.ox(2);
        let args = json!({"room":"retry-fixture","text":"Review this unchanged message","ask":"reply","by":"2h","expires":"7d","op_id":"mcp-relative-http-01"});
        let receipt = oracle_mcp::call(&first, "oracle_say", &args).unwrap();
        let path = first.home.join("ops/mcp-relative-http-01.json");
        let mut record: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let original_id = record["outcome"]["stored"]["id"].as_str().unwrap().to_owned();
        assert!(receipt.contains(&original_id));
        let original_digest = record["digest"].clone();
        // The server committed, but the client never learned its outcome.
        record["outcome"] = Value::Null;
        std::fs::write(&path, record.to_string()).unwrap();
        drop(first);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let restarted = server.ox(2);
        let replay = oracle_mcp::call(&restarted, "oracle_say", &args).unwrap();
        assert!(replay.contains(&original_id), "{replay}");
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["digest"], original_digest);
        let rows = Door::get(&restarted, "/search?channel=retry-fixture&limit=20").unwrap();
        assert_eq!(rows["items"].as_array().unwrap().len(), 1, "one event after HTTP replay");
        let mut changed = args; changed["by"] = json!("3h");
        assert!(oracle_mcp::call(&restarted, "oracle_say", &changed).unwrap_err().to_string().contains("different write"));
    }

    // Points nowhere: the door is exercised without an Oracle; the tools have their own tests in oracle-mcp.
    fn ox() -> Ox {
        let home = std::env::temp_dir().join(format!("ox-mcp-test-{}", oracle_proto::random_hex(6)));
        Ox {
            home,
            cfg: super::super::Config {
                url: "http://127.0.0.1:1".into(),
                fallback_urls: vec![],
                handle: "rocky".into(),
                token: "t".into(),
                key_path: "/nonexistent".into(),
            },
            json: false,
            live: std::cell::Cell::new(0),
            via: None,
            incompatible: None,
            op_id: None,
        }
    }

    #[test]
    fn ox_is_a_door_for_its_own_seat_with_no_queue_until_something_is_queued() {
        let o = ox();
        assert_eq!(Door::handle(&o), "rocky");
        assert_eq!(o.queued(), 0);
        assert!(o.identity().ends_with("http://127.0.0.1:1"), "{}", o.identity());
        let r = oracle_mcp::handle(&o, &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}), false).unwrap();
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
        assert_eq!(announce_list_changes(&init, r.clone())["result"]["capabilities"]["tools"]["listChanged"], true, "stdio says it announces tool changes");
        assert_eq!(list_changed()["method"], "notifications/tools/list_changed");
        // Control: any other reply passes through unchanged.
        let other = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        assert_eq!(announce_list_changes(&other, json!({"x": 1})), json!({"x": 1}));
        assert_eq!(r["result"]["serverInfo"]["title"], "Oracle · rocky");
        // The stdio door lists the clean tool set, not the old names.
        let r = oracle_mcp::handle(&o, &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}), false).unwrap();
        assert!(!r["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "room_say"));
    }

    #[test]
    fn a_write_through_the_door_with_no_key_is_an_error_not_a_receipt() {
        let o = ox();
        let r = oracle_mcp::handle(&o, &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "oracle_say", "arguments": {"room": "r", "text": "hi"}}}), false).unwrap();
        assert_eq!(r["result"]["isError"], true, "{r}");
    }
}
