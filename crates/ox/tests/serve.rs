#![cfg(unix)]
//! `ox serve` as a real process, against a real Oracle: the gates Emmy set for the service
//!  that only show at the process level.

use ed25519_dalek::SigningKey;
use oracle_proto::{keyfile, pubkey_hex, KeyAction, KeyEvent};
use oracle_server::store::{Scope, Store};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

/// An Oracle with seats rocky (key 2) and mallory (key 3), and a seat home for each on disk.
struct World {
    dir: PathBuf,
    rocky: PathBuf,
    mallory: PathBuf,
}

fn world(name: &str) -> World {
    let now = now_ms();
    let store = Store::memory().unwrap();
    store.init_owner("blaze", &pubkey_hex(&sk(1)), now).unwrap();
    let mut tokens = vec![];
    for (h, k) in [("rocky", 2), ("mallory", 3)] {
        store.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), now), now).unwrap();
        tokens.push(store.mint_token(h, Scope::Write, "test", now).unwrap());
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
            let l = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(l, oracle_server::http::router(oracle_server::http::App::new(store))).await.unwrap();
        });
    });
    let dir = std::env::temp_dir().join(format!("oxit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let seat = |h: &str, k: u8, token: &str| {
        let home = dir.join(h);
        std::fs::create_dir_all(&home).unwrap();
        keyfile::create(&home.join("identity"), &sk(k)).unwrap();
        let cfg = json!({"url": url, "handle": h, "token": token, "key_path": home.join("identity").display().to_string()});
        std::fs::write(home.join("config.json"), cfg.to_string()).unwrap();
        home
    };
    World { rocky: seat("rocky", 2, &tokens[0]), mallory: seat("mallory", 3, &tokens[1]), dir }
}

struct Served(Child);
impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve(w: &World, run: &Path) -> Served {
    let child = Command::new(env!("CARGO_BIN_EXE_ox"))
        .args(["--home", w.rocky.to_str().unwrap(), "serve", "--seat", w.rocky.to_str().unwrap(), "--seat", w.mallory.to_str().unwrap(), "--run", run.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_up(run, &[&w.rocky, &w.mallory]);
    Served(child)
}

/// Up when it answers hello to the capabilities it published: a socket alone is not enough (capabilities are
/// published after it is bound), and a capability file left by an earlier, killed service is not proof either.
fn wait_up(run: &Path, homes: &[&Path]) {
    let up = |home: &Path| -> bool {
        let Ok(c) = std::fs::read_to_string(home.join("service.cap")) else { return false };
        let Ok(mut s) = UnixStream::connect(run.join("oracle.sock")) else { return false };
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        if writeln!(s, "{}", json!({"cap": c, "op": "hello"})).is_err() {
            return false;
        }
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).is_ok() && line.contains("\"ok\":true")
    };
    let t = Instant::now();
    while !homes.iter().all(|h| up(h)) {
        assert!(t.elapsed() < Duration::from_secs(15), "ox serve did not come up");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn cap(home: &Path) -> String {
    std::fs::read_to_string(home.join("service.cap")).unwrap()
}

fn ask(run: &Path, req: Value) -> Value {
    let mut s = UnixStream::connect(run.join("oracle.sock")).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    // A service at its connection limit answers `busy` and closes at once, possibly before this write: the
    // refusal is still there to read.
    let _ = writeln!(s, "{req}");
    let mut line = String::new();
    let _ = BufReader::new(s).read_line(&mut line);
    serde_json::from_str(&line).unwrap_or(Value::Null)
}

fn count(run: &Path, home: &Path, word: &str) -> usize {
    let v = ask(run, json!({"cap": cap(home), "op": "get", "path": format!("/search?q={word}&channel=svc&author=rocky")}));
    v["value"]["items"].as_array().map_or(0, |a| a.len())
}

#[test]
fn a_second_service_fails_without_touching_the_first() {
    let w = world("second");
    let run = w.dir.join("run");
    let _first = serve(&w, &run);
    let before = (cap(&w.rocky), cap(&w.mallory));
    let second = Command::new(env!("CARGO_BIN_EXE_ox"))
        .args(["--home", w.rocky.to_str().unwrap(), "serve", "--seat", w.rocky.to_str().unwrap(), "--run", run.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!second.status.success(), "a second service must refuse to start");
    assert!(String::from_utf8_lossy(&second.stderr).contains("another Oracle service"), "{}", String::from_utf8_lossy(&second.stderr));
    assert_eq!((cap(&w.rocky), cap(&w.mallory)), before, "the first service's capabilities are untouched");
    assert_eq!(ask(&run, json!({"cap": before.0, "op": "hello"}))["seat"], "rocky", "and its clients still work");
}

#[test]
fn a_write_retried_after_a_kill_is_one_event() {
    let w = world("kill");
    let run = w.dir.join("run");
    let first = serve(&w, &run);
    let req = |c: &str| json!({"cap": c, "op": "write", "op_id": "kill-test-0001", "room": "svc", "text": "svckilled"});
    let a = ask(&run, req(&cap(&w.rocky)));
    let id = a["stored"]["id"].clone();
    assert!(id.is_string(), "{a}");
    // The client never saw that reply (say it died); the service is killed outright and started again.
    drop(first);
    let _again = serve(&w, &run);
    let b = ask(&run, req(&cap(&w.rocky)));
    assert_eq!((b["stored"]["id"].clone(), b["replayed"].clone()), (id, json!(true)), "{b}");
    assert_eq!(count(&run, &w.rocky, "svckilled"), 1);
    // A stale socket file from the killed process did not stop the restart (checked above), and the old
    // capability is dead: a restart issues new ones.
}

#[test]
fn an_oversized_line_is_refused_and_the_service_keeps_serving() {
    let w = world("big");
    let run = w.dir.join("run");
    let _s = serve(&w, &run);
    let mut s = UnixStream::connect(run.join("oracle.sock")).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    // 5 MiB with no newline: more than one frame may hold.
    let chunk = vec![b'x'; 1 << 20];
    for _ in 0..5 {
        if s.write_all(&chunk).is_err() {
            break;
        }
    }
    let mut reply = String::new();
    let _ = BufReader::new(&s).read_line(&mut reply);
    assert!(reply.contains("too_large"), "{reply}");
    // The connection is closed after the refusal.
    let mut rest = Vec::new();
    assert_eq!(BufReader::new(&s).read_to_end(&mut rest).unwrap_or(0), 0);
    assert_eq!(ask(&run, json!({"cap": cap(&w.rocky), "op": "hello"}))["seat"], "rocky");
}

#[test]
fn idle_connections_cannot_starve_another_seat() {
    let w = world("idle");
    let run = w.dir.join("run");
    let _s = serve(&w, &run);
    // Rocky's limit of bound connections, all idle after one hello.
    let mut held = vec![];
    // wait_up's own probe holds a rocky slot until the service sees it close, so a bound hello may briefly be busy.
    let t = Instant::now();
    while held.len() < 16 {
        let mut s = UnixStream::connect(run.join("oracle.sock")).unwrap();
        writeln!(s, "{}", json!({"cap": cap(&w.rocky), "op": "hello"})).unwrap();
        let mut line = String::new();
        BufReader::new(s.try_clone().unwrap()).read_line(&mut line).unwrap();
        if line.contains("\"code\":\"busy\"") && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        assert!(line.contains("\"ok\":true"), "{line}");
        held.push(s);
    }
    // The 17th for rocky is turned away...
    let over = ask(&run, json!({"cap": cap(&w.rocky), "op": "hello"}));
    assert_eq!(over["code"], "busy", "{over}");
    // ...connections that never identify fill their own pool...
    for _ in 0..16 {
        held.push(UnixStream::connect(run.join("oracle.sock")).unwrap());
    }
    std::thread::sleep(Duration::from_millis(200));
    // ...and mallory is still served once an unnamed slot frees (they are dropped after 5 s of silence).
    let t = Instant::now();
    loop {
        let m = ask(&run, json!({"cap": cap(&w.mallory), "op": "hello"}));
        if m["seat"] == "mallory" {
            break;
        }
        assert_eq!(m["code"], "busy", "{m}");
        assert!(t.elapsed() < Duration::from_secs(10), "mallory starved");
        std::thread::sleep(Duration::from_millis(250));
    }
    drop(held);
}

#[test]
fn a_seat_home_is_served_once_whatever_the_run_directory() {
    let w = world("twohomes");
    let _first = serve(&w, &w.dir.join("run-a"));
    let before = (cap(&w.rocky), cap(&w.mallory));
    let other = w.dir.join("run-b");
    let second = Command::new(env!("CARGO_BIN_EXE_ox"))
        .args(["--home", w.rocky.to_str().unwrap(), "serve", "--seat", w.rocky.to_str().unwrap(), "--run", other.to_str().unwrap()])
        .stdout(Stdio::null()).stderr(Stdio::piped()).output().unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already served"), "{}", String::from_utf8_lossy(&second.stderr));
    assert_eq!((cap(&w.rocky), cap(&w.mallory)), before, "the first service's capabilities are untouched");
    assert!(!other.join("oracle.sock").exists(), "the second never bound a socket");
}

#[test]
fn a_crash_between_acceptance_and_the_journal_is_still_one_event() {
    let w = world("crash");
    let run = w.dir.join("run");
    // The service dies right after the Oracle accepted the write, before it records the outcome.
    let mut faulty = Command::new(env!("CARGO_BIN_EXE_ox"))
        .args(["--home", w.rocky.to_str().unwrap(), "serve", "--seat", w.rocky.to_str().unwrap(), "--run", run.to_str().unwrap()])
        .env("OX_FAULT", "crash-after-accept")
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    wait_up(&run, &[&w.rocky]);
    let req = |c: &str| json!({"cap": c, "op": "write", "op_id": "crash-op-0001", "room": "svc", "text": "svccrash"});
    let mut s = UnixStream::connect(run.join("oracle.sock")).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    writeln!(s, "{}", req(&cap(&w.rocky))).unwrap();
    let mut line = String::new();
    let _ = BufReader::new(s).read_line(&mut line);
    assert!(line.is_empty(), "no reply: the service died mid-write ({line})");
    assert!(!faulty.wait().unwrap().success(), "it aborted");
    // The record holds the envelope with no outcome. Restarted cleanly, the same op lands once with the
    // original message.
    let rec: Value = serde_json::from_str(&std::fs::read_to_string(w.rocky.join("ops/crash-op-0001.json")).unwrap()).unwrap();
    assert!(rec["outcome"].is_null() && rec["envelope"]["client_op"].is_string(), "{rec}");
    let _again = serve(&w, &run);
    let r = ask(&run, req(&cap(&w.rocky)));
    assert!(r["stored"]["id"].is_string(), "{r}");
    assert_eq!(r["stored"]["duplicate"], true, "the Oracle had it already: {r}");
    assert_eq!(count(&run, &w.rocky, "svccrash"), 1);
}

fn ox_cli(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ox")).arg("--home").arg(home).args(args).env_remove("OX_DIRECT").output().unwrap()
}

#[test]
fn the_cli_goes_through_the_service_and_works_without_it() {
    let w = world("cli");
    let run = w.dir.join("run");
    let served = serve(&w, &run);
    let out = ox_cli(&w.rocky, &["say", "svc", "svcviaservice"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // Journaled once: one op record.
    let records = |h: &Path| std::fs::read_dir(h.join("ops")).unwrap().filter_map(|e| e.ok()).filter(|e| e.path().extension().is_some_and(|x| x == "json")).count();
    assert_eq!(records(&w.rocky), 1, "one op record");
    assert_eq!(count(&run, &w.rocky, "svcviaservice"), 1);
    // Reads come back the same way.
    let read = ox_cli(&w.rocky, &["read", "svc"]);
    assert!(String::from_utf8_lossy(&read.stdout).contains("svcviaservice"), "{}", String::from_utf8_lossy(&read.stdout));
    // Without the service, ox speaks to the Oracle directly: Oracle works with no service at all.
    drop(served);
    let direct = ox_cli(&w.rocky, &["say", "svc", "svcdirect"]);
    assert!(direct.status.success(), "{}", String::from_utf8_lossy(&direct.stderr));
    // Direct writes keep the same journal (so a retried op can never become a second write).
    assert_eq!(records(&w.rocky), 2, "the direct write is journaled too");
    let read = ox_cli(&w.rocky, &["read", "svc"]);
    assert!(String::from_utf8_lossy(&read.stdout).contains("svcdirect"));
}

#[test]
fn an_mcp_server_whose_ox_was_updated_becomes_the_new_ox_and_answers_on() {
    let w = world("stale");
    let bin = w.dir.join("ox-copy");
    std::fs::copy(env!("CARGO_BIN_EXE_ox"), &bin).unwrap();
    // A binary written a moment ago can still be held open for writing by another test thread's fork (ETXTBSY,
    // "text file busy", errno 26 on Linux): retry the spawn on exactly that.
    let spawn = || Command::new(&bin)
        .arg("--home").arg(&w.rocky).arg("mcp")
        .env("OX_DIRECT", "1")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
        .spawn();
    let mut mcp = {
        let mut tries = 0;
        loop {
            match spawn() {
                Ok(c) => break c,
                Err(e) if e.raw_os_error() == Some(26) && tries < 50 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("spawn {}: {e}", bin.display()),
            }
        }
    };
    let mut stdin = mcp.stdin.take().unwrap();
    let mut stdout = BufReader::new(mcp.stdout.take().unwrap());
    let told = std::cell::Cell::new(false);
    let mut call = |id: i64, msg: Value| -> Value {
        let mut m = msg;
        m["jsonrpc"] = json!("2.0");
        m["id"] = json!(id);
        writeln!(stdin, "{m}").unwrap();
        // A notification is not the reply: note it and read on.
        loop {
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["method"] == "notifications/tools/list_changed" {
                told.set(true);
                continue;
            }
            return v;
        }
    };
    call(1, json!({"method": "initialize", "params": {}}));
    let fresh = call(2, json!({"method": "tools/call", "params": {"name": "oracle_tasks", "arguments": {}}}));
    assert!(!fresh["result"]["content"][0]["text"].as_str().unwrap_or("").contains("out of date"), "{fresh}");
    // ox is updated on disk (a new file renamed over the old, as an install does).
    let new = w.dir.join("ox-new");
    std::fs::copy(env!("CARGO_BIN_EXE_ox"), &new).unwrap();
    std::fs::write(&new, [std::fs::read(&new).unwrap(), b"\n".to_vec()].concat()).unwrap();
    std::fs::rename(&new, &bin).unwrap();
    let pid = mcp.id();
    // Blaze, 2026-10-03: an install must not leave every seat's MCP refusing until someone runs /mcp. The server execs
    // the new ox in place (same process, same pipes) and the request that found it out of date is answered by it.
    assert!(!told.get(), "control: no tools-changed notice before the install");
    let after = call(3, json!({"method": "tools/call", "params": {"name": "oracle_tasks", "arguments": {}}}));
    // The successor first tells the client its tools changed, so tools shipped since appear without a reconnect.
    assert!(told.get(), "the successor announced notifications/tools/list_changed");
    assert_eq!(after["id"], 3, "{after}");
    assert!(!after["result"]["content"][0]["text"].as_str().unwrap_or("").contains("out of date"), "{after}");
    assert_eq!(mcp.id(), pid);
    // Proof it really is the new program, not the old one answering: on Linux the old image would read "(deleted)".
    #[cfg(target_os = "linux")]
    {
        let exe = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap();
        assert!(!exe.to_string_lossy().ends_with("(deleted)"), "still the replaced program: {}", exe.display());
    }
    assert!(mcp.try_wait().unwrap().is_none(), "the server is still running, as the new ox");
    // ...and keeps answering.
    assert!(call(4, json!({"method": "tools/list"}))["result"]["tools"].is_array());
    // NEGATIVE CONTROL: a replacement that cannot run (not executable) is refused as before, never a silent death.
    let broken = w.dir.join("ox-broken");
    std::fs::write(&broken, b"not a program").unwrap();
    std::fs::rename(&broken, &bin).unwrap();
    let refused = call(5, json!({"method": "tools/call", "params": {"name": "oracle_tasks", "arguments": {}}}));
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert!(refused["result"]["content"][0]["text"].as_str().unwrap().contains("reconnect"), "{refused}");
    let _ = mcp.kill();
    let _ = mcp.wait();
}

/// `ox serve` for rocky alone, told to inject `fault` (debug builds only).
fn serve_with_fault(w: &World, run: &Path, fault: &str) -> Served {
    let child = Command::new(env!("CARGO_BIN_EXE_ox"))
        .args(["--home", w.rocky.to_str().unwrap(), "serve", "--seat", w.rocky.to_str().unwrap(), "--run", run.to_str().unwrap()])
        .env("OX_FAULT", fault)
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    wait_up(run, &[&w.rocky]);
    Served(child)
}

/// A write whose reply never usefully arrives is reported as unknown with its op id, is never re-signed as a
/// second write, and the same op retried later is the original message.
fn unknown_outcome_is_one_event(name: &str, fault: &str, extra_env: &[(&str, &str)]) {
    let w = world(name);
    let run = w.dir.join("run");
    let op = format!("{name}-op-0001");
    let word = format!("svc{name}");
    {
        let _faulty = serve_with_fault(&w, &run, fault);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ox"));
        cmd.arg("--home").arg(&w.rocky).args(["--op-id", &op, "say", "svc", &word]).env_remove("OX_DIRECT");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "an unknown outcome is not success");
        assert!(err.contains("outcome unknown") && err.contains(&op), "{err}");
        // Written once by the service; no direct, newly signed copy.
        assert_eq!(count(&run, &w.rocky, &word), 1, "{fault}: one event, not two");
    }
    // The fault gone, the same op is the same write.
    let _clean = serve(&w, &run);
    let again = ox_cli(&w.rocky, &["--op-id", &op, "say", "svc", &word]);
    assert!(again.status.success(), "{}", String::from_utf8_lossy(&again.stderr));
    assert_eq!(count(&run, &w.rocky, &word), 1, "{fault}: still one event after the retry");
}

#[test]
fn a_lost_reply_is_unknown_never_a_second_write() {
    unknown_outcome_is_one_event("lost", "drop-reply-after-write", &[]);
}

#[test]
fn a_garbled_reply_is_unknown_never_a_second_write() {
    unknown_outcome_is_one_event("garbled", "garbage-reply-after-write", &[]);
}

#[test]
fn a_reply_that_times_out_is_unknown_never_a_second_write() {
    unknown_outcome_is_one_event("late", "stall-reply-after-write", &[("OX_SERVICE_TIMEOUT_MS", "400")]);
}

#[test]
fn a_service_on_another_protocol_is_refused_not_bypassed() {
    let w = world("proto");
    let run = w.dir.join("run");
    let _s = serve_with_fault(&w, &run, "protocol-mismatch");
    let out = ox_cli(&w.rocky, &["say", "svc", "svcmismatch"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("speaks oracle-local/0") && err.contains("nothing was done"), "{err}");
    // Nothing went around it to the Oracle directly.
    let direct = Command::new(env!("CARGO_BIN_EXE_ox")).arg("--home").arg(&w.rocky).args(["read", "svc"]).env("OX_DIRECT", "1").output().unwrap();
    assert!(!String::from_utf8_lossy(&direct.stdout).contains("svcmismatch"));
    // Reads are refused the same way.
    assert!(!ox_cli(&w.rocky, &["read", "svc"]).status.success());
}

#[test]
fn an_unknown_write_retried_after_the_service_stopped_is_still_one_event() {
    // Emmy, PR 54 second review: uncertain → service unavailable → the advertised retry → one event, and the
    // original receipt.
    let w = world("crossinv");
    let run = w.dir.join("run");
    let op = "crossinv-op-0001";
    {
        let _faulty = serve_with_fault(&w, &run, "drop-reply-after-write");
        let out = ox_cli(&w.rocky, &["--op-id", op, "say", "svc", "svccrossinv"]);
        assert!(String::from_utf8_lossy(&out.stderr).contains("outcome unknown"), "{}", String::from_utf8_lossy(&out.stderr));
    } // the service stops here
    let rec: Value = serde_json::from_str(&std::fs::read_to_string(w.rocky.join(format!("ops/{op}.json"))).unwrap()).unwrap();
    let original = rec["outcome"]["stored"]["id"].as_str().expect("the service recorded what the Oracle stored").to_string();
    // No service now: ox speaks directly, through the same journal, and resolves the op to its recorded outcome.
    let again = ox_cli(&w.rocky, &["--json", "--op-id", op, "say", "svc", "svccrossinv"]);
    assert!(again.status.success(), "{}", String::from_utf8_lossy(&again.stderr));
    assert!(String::from_utf8_lossy(&again.stdout).contains(&original), "the original receipt: {}", String::from_utf8_lossy(&again.stdout));
    let direct = Command::new(env!("CARGO_BIN_EXE_ox")).arg("--home").arg(&w.rocky).args(["--json", "read", "svc"]).env("OX_DIRECT", "1").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&direct.stdout).matches("svccrossinv").count(), 1, "one event on the Oracle");
    // An op whose record holds the envelope but no outcome (cut off before it was recorded) re-delivers that same
    // envelope directly: still one event.
    let op2 = "crossinv-op-0002";
    let mut cut = rec.clone();
    cut["outcome"] = Value::Null;
    std::fs::write(w.rocky.join(format!("ops/{op2}.json")), cut.to_string()).unwrap();
    let replay = ox_cli(&w.rocky, &["--json", "--op-id", op2, "say", "svc", "svccrossinv"]);
    assert!(replay.status.success() && String::from_utf8_lossy(&replay.stdout).contains(&original), "{}", String::from_utf8_lossy(&replay.stdout));
    let direct = Command::new(env!("CARGO_BIN_EXE_ox")).arg("--home").arg(&w.rocky).args(["--json", "read", "svc"]).env("OX_DIRECT", "1").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&direct.stdout).matches("svccrossinv").count(), 1);
}

#[test]
fn a_running_service_that_refuses_is_not_bypassed() {
    let w = world("refusing");
    let run = w.dir.join("run");
    let _s = serve(&w, &run);
    // The service runs but does not know this capability (as if it were stale): it refuses, and ox does not go
    // around it.
    std::fs::write(w.rocky.join("service.cap"), "0".repeat(64)).unwrap();
    let out = ox_cli(&w.rocky, &["say", "svc", "svcrefused"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("refused"), "{}", String::from_utf8_lossy(&out.stderr));
    let direct = Command::new(env!("CARGO_BIN_EXE_ox")).arg("--home").arg(&w.rocky).args(["read", "svc"]).env("OX_DIRECT", "1").output().unwrap();
    assert!(!String::from_utf8_lossy(&direct.stdout).contains("svcrefused"));
}

#[test]
fn direct_writes_reject_invalid_operation_ids_before_touching_the_journal() {
    let w = world("invalid-op");
    for op in ["../escaped-operation", "/tmp/escaped-operation", "short", "has space in operation"] {
        let out = Command::new(env!("CARGO_BIN_EXE_ox"))
            .arg("--home").arg(&w.rocky)
            .args(["--op-id", op, "say", "svc", "must not be written"])
            .env("OX_DIRECT", "1").output().unwrap();
        assert!(!out.status.success(), "invalid operation id accepted: {op}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("write needs an op_id"));
        assert!(!w.rocky.join("ops").exists(), "invalid id touched the journal");
    }
}
