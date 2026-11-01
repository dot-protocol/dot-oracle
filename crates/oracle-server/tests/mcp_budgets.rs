//! Every MCP answer honours its budget (Blaze's law COMPRESS, DON'T EXTEND; Rocky's live pass, 1 Nov 2026 07:15Z:
//! `oracle_inbox mine limit=5` answered 86 KB, `oracle_task owner=rocky limit=5` 129 KB, `oracle_read` showed links but
//! not tags, `oracle_cortex op=nodes top=5` 11 nodes, `oracle_room_state` every field). Each check runs against a real
//! server through `POST /mcp`, the door a connector uses, over a seeded store.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};
use serde_json::{json, Value};

const NOW: i64 = 1_790_000_000_000;

fn key() -> SigningKey {
    SigningKey::from_bytes(&[1; 32])
}

struct Fixture {
    s: Store,
    at: i64,
}

impl Fixture {
    fn new() -> Self {
        let s = Store::memory().unwrap();
        s.init_owner("blaze", &pubkey_hex(&key()), 1).unwrap();
        Self { s, at: NOW }
    }
    fn put(&mut self, room: &str, kind: Kind, text: &str, cargo: Option<Value>, tags: &[&str]) -> String {
        self.put_as("blaze", &key(), room, kind, text, cargo, tags)
    }
    #[allow(clippy::too_many_arguments)]
    fn put_as(&mut self, who: &str, k: &SigningKey, room: &str, kind: Kind, text: &str, cargo: Option<Value>, tags: &[&str]) -> String {
        self.at += 1;
        let e = Envelope::sign(k, room, who, kind, text, cargo.map(|c| c.to_string()), tags.iter().map(|t| t.to_string()).collect(), self.at);
        self.s.write(&Principal { handle: who.into(), scope: Scope::Write }, &e, self.at).unwrap().id
    }
}

/// A real server over the fixture's store; `call(tool, args)` is one `tools/call` through `POST /mcp` as blaze.
struct Served {
    base: String,
    auth: String,
    _runtime: tokio::runtime::Runtime,
}

impl Served {
    fn start(f: Fixture) -> Self {
        let token = f.s.mint_token("blaze", Scope::Read, "budgets-test", f.at).unwrap();
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let base = format!("http://{}", socket.local_addr().unwrap());
        let mut app = oracle_server::http::App::new(f.s);
        app.self_url = base.clone();
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(socket).unwrap();
            axum::serve(listener, oracle_server::http::router(app)).await.unwrap();
        });
        Served { base, auth: format!("Bearer {token}"), _runtime: runtime }
    }
    fn get(&self, path: &str) -> Value {
        ureq::get(&format!("{}{path}", self.base)).set("Authorization", &self.auth).call().unwrap().into_json().unwrap()
    }
    /// The whole JSON-RPC answer, as a harness receives it, and its size in bytes.
    fn call(&self, tool: &str, args: Value) -> (Value, usize) {
        let body = ureq::post(&format!("{}/mcp", self.base)).set("Authorization", &self.auth)
            .send_json(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}}))
            .unwrap().into_string().unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_ne!(v["result"]["isError"], true, "{tool}: {v}");
        (v["result"].clone(), body.len())
    }
}

/// 3. `oracle_read` returns each message's tags beside its links; the HTTP read it uses already carried them.
#[test]
fn a_room_read_carries_every_messages_tags_beside_its_links() {
    let mut f = Fixture::new();
    let root = f.put("oracle", Kind::Say, "the root", None, &["intent:question", "ask:reply"]);
    let reply = format!("reply:{root}");
    let id = f.put("oracle", Kind::Say, "the receipt", None, &["intent:receipt", &reply, "receipt", "type:oracle.receipt"]);
    let served = Served::start(f);
    let http = served.get("/search?channel=oracle&limit=5");
    let row = http["items"].as_array().unwrap().iter().find(|i| i["id"] == id.as_str()).unwrap().clone();
    assert!(row["tags"].as_array().unwrap().iter().any(|t| t == "receipt"), "the HTTP read carries tags: {row}");
    let (r, _) = served.call("oracle_read", json!({"room": "oracle", "limit": 5}));
    let m = r["structuredContent"]["messages"].as_array().unwrap().iter().find(|m| m["id"] == id.as_str()).unwrap().clone();
    assert_eq!(m["links"], json!([reply]), "{m}");
    let tags: Vec<&str> = m["tags"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    for t in ["intent:receipt", reply.as_str(), "receipt", "type:oracle.receipt"] {
        assert!(tags.contains(&t), "{t} missing from {m}");
    }
    assert!(m["seq"].is_i64() && m["intent"] == "receipt" && m["author"] == "blaze", "the existing fields stay: {m}");
    assert!(r["content"][0]["text"].as_str().unwrap().contains("tags: receipt type:oracle.receipt"), "{r}");
}

/// A Blaze-approved observation (his own key signs the node and the verdict) titled `title`.
fn approved_reading(f: &mut Fixture, n: usize, title: &str) -> String {
    let message = format!("OBS-cases-20261101-{n:012x}");
    f.put("cortex", Kind::Say, title, Some(json!({"kind": "observation", "title": title, "quote": title, "source": {"message": message, "start": 0, "end": 9}})), &["cortex:node"]);
    let nid = oracle_server::cortex::observation_nid(&message, 0, 9);
    f.put("cortex", Kind::Say, "approved", Some(json!({"target": nid, "verdict": "approve", "by": "blaze", "via": "cases-page"})), &["cortex:verdict"]);
    nid
}

/// 4. `oracle_cortex op=nodes`: `top` bounds the nodes returned, after the list's ranking; the rest is counted.
#[test]
fn cortex_nodes_answer_at_most_top_and_count_what_was_cut() {
    let mut f = Fixture::new();
    for n in 0..11 {
        approved_reading(&mut f, n, &format!("oracle reading {n}"));
    }
    approved_reading(&mut f, 99, "something else");
    let served = Served::start(f);
    let whole = served.get("/cortex?q=oracle");
    assert_eq!((whole["count"].as_u64(), whole["nodes"].as_array().map(Vec::len)), (Some(11), Some(11)), "without top, every match");
    let http = served.get("/cortex?q=oracle&top=5");
    let first: Vec<Value> = whole["nodes"].as_array().unwrap()[..5].iter().map(|n| n["nid"].clone()).collect();
    let kept: Vec<Value> = http["nodes"].as_array().unwrap().iter().map(|n| n["nid"].clone()).collect();
    assert_eq!(kept, first, "the first five of the same ranked list");
    assert_eq!((http["count"].as_u64(), http["shown"].as_u64(), http["cut"].as_u64()), (Some(11), Some(5), Some(6)));
    let (r, bytes) = served.call("oracle_cortex", json!({"op": "nodes", "q": "oracle", "top": 5}));
    let text = r["content"][0]["text"].as_str().unwrap();
    assert_eq!(text.lines().filter(|l| l.contains("oracle reading")).count(), 5, "{text}");
    assert!(text.contains("top 5 shown, 6 more"), "{text}");
    assert!(bytes < 2_000, "{bytes} bytes");
}

/// 5. `oracle_room_state` read with `fields`: only those fields, with heads, revisions and room; unknown names are named.
#[test]
fn a_state_read_picks_its_fields_through_the_server() {
    let mut f = Fixture::new();
    let long = "b".repeat(1_400);
    let mut sets: Vec<String> = ["current=budgets landing", "next=the release"].iter().map(|s| s.to_string()).collect();
    sets.extend(["purpose", "goal", "scope", "constraints", "questions", "acceptance"].iter().map(|k| format!("{k}={long}")));
    let rev = oracle_mcp::state_revision("oracle", &[], &json!({}), &sets).unwrap();
    let head = f.put("oracle", Kind::Say, "State updated", Some(rev), &["room-context:v1", "intent:status"]);
    let served = Served::start(f);
    let (whole, whole_bytes) = served.call("oracle_room_state", json!({"room": "oracle"}));
    assert_eq!(whole["structuredContent"]["fields"]["goal"].as_str().map(str::len), Some(1_400), "{whole}");
    let (r, bytes) = served.call("oracle_room_state", json!({"room": "oracle", "fields": "next,current,colour"}));
    let v = &r["structuredContent"];
    assert_eq!(v["fields"], json!({"next": "the release", "current": "budgets landing"}), "{v}");
    assert_eq!((v["unknown_fields"].clone(), v["room"].clone(), v["heads"].clone(), v["revisions"].clone()), (json!(["colour"]), json!("oracle"), json!([head]), json!(1)));
    assert!(bytes < 1_500 && whole_bytes > 16_000, "{bytes} bytes picked, {whole_bytes} whole");
}

/// 2. `oracle_task` in its list form: `limit` bounds the heads, newest-touched first; the budget says what was cut.
#[test]
fn the_task_list_answers_its_limit_newest_touched_first() {
    let mut f = Fixture::new();
    let note = "a head's note, long enough to cost something on every line of a list that nobody bounded";
    for i in 0..60 {
        let subject = format!("work.{i:02}");
        f.put("board", Kind::State, note, Some(json!({"subject": subject, "state": "noted", "owner": "blaze"})), &[&format!("subject:{subject}")]);
    }
    let served = Served::start(f);
    let (all, all_bytes) = served.call("oracle_task", json!({}));
    let v = &all["structuredContent"];
    assert_eq!((v["heads"].as_array().map(Vec::len), v["total"].as_u64()), (Some(50), Some(60)), "unset is fifty: {v}");
    assert_eq!(v["budget"], json!({"rows": 50, "cut": 10, "more": "offset: 50 for the next 10"}));
    let (r, bytes) = served.call("oracle_task", json!({"owner": "blaze", "limit": 5}));
    let subjects: Vec<&str> = r["structuredContent"]["heads"].as_array().unwrap().iter().filter_map(|h| h["subject"].as_str()).collect();
    assert_eq!(subjects, ["work.59", "work.58", "work.57", "work.56", "work.55"], "newest-touched first");
    assert_eq!(r["structuredContent"]["budget"]["cut"], 55);
    assert!(bytes < 3_000 && all_bytes > 5 * bytes, "{bytes} bytes for five, {all_bytes} for fifty");
}

/// 1. `oracle_inbox`: `limit` bounds the ask rows and what is addressed to me; `budget` says what was cut and how to get it.
#[test]
fn the_inbox_answers_its_limit_in_every_list() {
    let mut f = Fixture::new();
    // Asked today (after the inbox began on 8 Oct 2026), by a second seat, of the owner.
    f.at = oracle_server::http::now_ms() - 30 * 60_000;
    let noor = SigningKey::from_bytes(&[5; 32]);
    f.s.apply_key_event(&KeyEvent::sign(&key(), "blaze", KeyAction::Add, "noor", &pubkey_hex(&noor), f.at), f.at).unwrap();
    let card = "which seats are live, and which of them answered the last round of asks before the release";
    for i in 0..40 {
        f.put_as("noor", &noor, "axxis", Kind::Say, &format!("@blaze {card}? ({i})"), None, &["to:blaze", "ask:reply"]);
    }
    let served = Served::start(f);
    let (whole, whole_bytes) = served.call("oracle_inbox", json!({"mine": true, "limit": 200}));
    assert_eq!(whole["structuredContent"]["asks"]["open"].as_array().map(Vec::len), Some(40), "{whole}");
    let (r, bytes) = served.call("oracle_inbox", json!({"mine": true, "limit": 5}));
    let v = &r["structuredContent"];
    assert_eq!(v["asks"]["open"].as_array().map(Vec::len), Some(5), "{v}");
    assert!(v["for_me"].as_array().map_or(0, Vec::len) <= 5, "{v}");
    assert_eq!(v["asks"]["counts"]["open"], 40);
    assert!(v["budget"]["cut"].as_u64().unwrap() >= 35 && v["budget"]["more"].as_str().unwrap().contains("limit: up to 200"), "{}", v["budget"]);
    assert!(r["content"][0]["text"].as_str().unwrap().contains("ASKS TO ME · 40 open"), "{r}");
    let (d, _) = served.call("oracle_inbox", json!({}));
    assert_eq!(d["structuredContent"]["asks"]["open"].as_array().map(Vec::len), Some(20), "unset is twenty");
    assert!(bytes * 4 < whole_bytes, "{bytes} bytes at limit 5, {whole_bytes} at 200");
}

/// Item 6, `oracle_brief`: the asks capped at ten with the rest counted, and the cortex counted on the server: what Blaze
/// approved (the default node list's length) and the marks still waiting on his verdict.
#[test]
fn the_brief_counts_the_cortex_and_lists_ten_asks() {
    let mut f = Fixture::new();
    f.at = oracle_server::http::now_ms() - 30 * 60_000;
    let noor = SigningKey::from_bytes(&[5; 32]);
    f.s.apply_key_event(&KeyEvent::sign(&key(), "blaze", KeyAction::Add, "noor", &pubkey_hex(&noor), f.at), f.at).unwrap();
    for i in 0..15 {
        f.put_as("noor", &noor, "axxis", Kind::Say, &format!("@blaze is ask {i} settled?"), None, &["to:blaze", "ask:reply"]);
    }
    for n in 0..4 {
        approved_reading(&mut f, n, &format!("an approved reading {n}"));
    }
    // A draft (no verdict) is not in the Cortex.
    f.put("cortex", Kind::Say, "a draft", Some(json!({"kind": "observation", "title": "a draft", "quote": "a draft",
        "source": {"message": "OBS-cases-20001101-dddddddddddd", "start": 0, "end": 7}})), &["cortex:node"]);
    // Three marks: one approved, two waiting on a verdict.
    let mark = |f: &mut Fixture, start: u64| f.put("brain", Kind::Say, "a mark", Some(json!({"source": {"message": "u40", "start": start, "end": start + 5}, "labels": ["Claim"]})), &["cortex:mark"]);
    let settled = mark(&mut f, 0);
    mark(&mut f, 10);
    mark(&mut f, 20);
    f.put("brain", Kind::Say, "approved", Some(json!({"target": settled, "verdict": "approve", "by": "blaze"})), &["cortex:verdict"]);
    let served = Served::start(f);
    assert!(served.get("/whoami")["features"].as_array().unwrap().iter().any(|x| x == "cortex_count"));
    let counted = served.get("/cortex?count=1");
    let listed = served.get("/cortex");
    assert_eq!((counted["schema"].as_str(), counted["approved_nodes"].as_u64(), counted["pending_marks"].as_u64()), (Some("cortex-count/1"), Some(4), Some(2)), "{counted}");
    assert_eq!(counted["approved_nodes"], listed["count"], "the same count as the default node list");
    assert!(counted.get("nodes").is_none(), "no rows");
    assert_eq!(served.get("/cortex/marks?pending=1")["count"], counted["pending_marks"]);
    let (r, _) = served.call("oracle_brief", json!({}));
    let v = &r["structuredContent"];
    assert_eq!(v["cortex"], json!({"approved_nodes": 4, "pending_marks": 2}), "{v}");
    assert_eq!((v["asks"]["rows"].as_array().map(Vec::len), v["asks"]["more"].as_u64()), (Some(10), Some(5)), "{}", v["asks"]);
    assert_eq!(v["local"], served.get("/whoami")["local"], "the brief's local clock is the server's");
    assert!(r["content"][0]["text"].as_str().unwrap().contains("CORTEX     4 approved nodes · 2 marks waiting"), "{r}");
    let (full, _) = served.call("oracle_brief", json!({"full": true}));
    assert_eq!(full["structuredContent"]["asks"]["rows"].as_array().map(Vec::len), Some(15));
}
