//! WORLD STATE (`GET /world`, schema world/1; Blaze, 1 Nov 2026, via Noor; Noor's schema):
//! a seeded store yields each of the six sections past its bound; the page is checked
//! over HTTP and through POST /mcp (`oracle_brief where=world` and the `brief` verb), under 6,000 bytes of text, and a
//! private room's records never appear.
#![allow(clippy::too_many_arguments)] // the seed helpers take the record whole: who, key, room, kind, text, cargo, tags, time
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};
use serde_json::{json, Value};

fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

struct Seed {
    s: Store,
    at: i64,
}

impl Seed {
    fn put(&mut self, who: &str, n: u8, room: &str, kind: Kind, text: &str, cargo: Option<Value>, tags: &[&str]) -> String {
        self.at += 1;
        self.put_at(who, n, room, kind, text, cargo, tags, self.at)
    }
    #[allow(clippy::too_many_arguments)]
    fn put_at(&self, who: &str, n: u8, room: &str, kind: Kind, text: &str, cargo: Option<Value>, tags: &[&str], at: i64) -> String {
        let e = Envelope::sign(&key(n), room, who, kind, text, cargo.map(|c| c.to_string()), tags.iter().map(|t| t.to_string()).collect(), at);
        self.s.write(&Principal { handle: who.into(), scope: Scope::Write }, &e, at).unwrap().id
    }
}

const SEATS: [(&str, u8); 3] = [("rocky", 3), ("noor", 5), ("emmy", 6)];

fn seeded() -> (Seed, Vec<String>) {
    let now = oracle_server::http::now_ms();
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&key(1)), now - 20 * 86_400_000).unwrap();
    for (h, n) in SEATS {
        s.apply_key_event(&KeyEvent::sign(&key(1), "blaze", KeyAction::Add, h, &pubkey_hex(&key(n)), now - 19 * 86_400_000), now - 19 * 86_400_000).unwrap();
    }
    let mut f = Seed { s, at: now - 60 * 60_000 };
    let mut ids = vec![];
    // A private room: nothing of it reaches the page.
    f.put("blaze", 1, "oracle", Kind::Say, "[Private rooms]", Some(json!({"schema": "private-rooms/1", "rooms": {"secret": ["blaze"]}})), &["schema:private-rooms/1"]);
    f.put("blaze", 1, "secret", Kind::State, "the secret project", Some(json!({"subject": "secret.project", "state": "running", "owner": "blaze", "next_touch": "2026-12-01"})), &["subject:secret.project"]);
    f.put("blaze", 1, "secret", Kind::Say, "a secret decision", None, &["memory:decision"]);
    // NORTH STAR: rocky's canon; rocky's and emmy's identity heads (an older rocky head first: only the newest counts).
    ids.push(f.put("rocky", 3, "rocky", Kind::Say, "canon v3", Some(json!({"schema": "canon/1", "version": 3, "rules": [{"id": "no-trailers", "kind": "safety", "text": "no trailers", "sources": ["OBS-x"]}, {"id": "compress", "kind": "law", "text": "compress", "sources": ["OBS-y"]}], "open": []})), &["canon"]));
    for (who, n, purpose) in [("rocky", 3, "An old purpose. Replaced."), ("rocky", 3, "Rocky builds AXXIS with Blaze. Second sentence."), ("emmy", 6, "Emmy reviews every contract")] {
        let id = f.put(who, n, who, Kind::Say, "identity", Some(json!({"schema": "identity-head/1", "seat": who, "purpose": purpose, "mission": "m", "persona_ref": "OBS-p"})), &["identity"]);
        ids.push(id);
    }
    // NOW: two plans with doing steps, and the queue (seven items, five shown).
    for (room, name, steps) in [("oracle", "search", json!([{"id": "S1", "title": "the model", "status": "doing", "owner": "rocky"}, {"id": "S2", "title": "the tab", "status": "proposed"}])),
                                ("axxis", "jobs", json!([{"id": "S1", "title": "the day view", "status": "done"}, {"id": "S2", "title": "the queue lane", "status": "doing"}]))] {
        let subject = format!("plan.{room}.{name}");
        ids.push(f.put("rocky", 3, room, Kind::State, &format!("{name} plan"), Some(json!({"subject": subject, "state": "running", "owner": "rocky", "next_touch": "2026-11-08",
            "data": {"schema": "plan/1", "title": name, "steps": steps}})), &[&format!("subject:{subject}"), "schema:plan/1"]));
    }
    let items: Vec<Value> = (1..=7).map(|i| json!({"subject": format!("queue.item{i}"), "title": format!("item {i}")})).collect();
    ids.push(f.put("rocky", 3, "rocky", Kind::Say, "queue", Some(json!({"schema": "queue/1", "by": "rocky", "items": items})), &["queue"]));
    // ACTIVE PROJECTS: fourteen running and one blocked; one done is not active.
    for i in 0..14 {
        let subject = format!("work.{i:02}");
        f.put("rocky", 3, "board", Kind::State, &format!("Working on {i}. More detail."), Some(json!({"subject": subject, "state": "running", "owner": "rocky", "next_touch": "2026-11-05"})), &[&format!("subject:{subject}")]);
    }
    ids.push(f.put("noor", 5, "board", Kind::State, "Blocked on CI. Waiting.", Some(json!({"subject": "work.blocked", "state": "blocked", "owner": "noor", "next_touch": "2026-11-03",
        "needs": ["core-ci"], "blocks": "the release", "check": "core-ci status"})), &["subject:work.blocked"]));
    f.put("rocky", 3, "board", Kind::State, "finished", Some(json!({"subject": "work.done", "state": "done", "owner": "rocky"})), &["subject:work.done"]);
    // KEY DECISIONS: ten this week (eight shown), one eleven days old.
    f.put_at("rocky", 3, "memory", Kind::Say, "An old decision.", None, &["memory:decision"], now - 11 * 86_400_000);
    for i in 0..10 {
        f.put("rocky", 3, "memory", Kind::Say, &format!("Decision {i}: keep the budgets. Because."), None, &["memory:decision"]);
    }
    // ACTIVE SEATS: rocky and emmy have a State; noor has none.
    for (who, n, current) in [("rocky", 3, "Building the world page. Then the release."), ("emmy", 6, "Reviewing the budgets")] {
        let rev = oracle_mcp::state_revision(who, &[], &json!({}), &[format!("current={current}"), "purpose=a seat".into()]).unwrap();
        ids.push(f.put(who, n, who, Kind::Say, "State updated", Some(rev), &["room-context:v1", "intent:status"]));
    }
    // OPEN QUESTIONS: three decide asks to blaze, one answered.
    let mut asks = vec![];
    for i in 0..3 {
        asks.push(f.put("noor", 5, "oracle", Kind::Say, &format!("@blaze which way for question {i}?"), Some(json!({"options": ["one", "two"], "recommendation": {"option": 1, "why": "simpler"}})), &["to:blaze", "ask:decide"]));
    }
    let reply = format!("reply:{}", asks[1]);
    f.put("blaze", 1, "oracle", Kind::Say, "one", None, &[&reply, "choice:1"]);
    ids.extend(asks);
    (f, ids)
}

struct Served {
    base: String,
    auth: String,
    _rt: tokio::runtime::Runtime,
}

fn serve(f: Seed) -> Served {
    let token = f.s.mint_token("blaze", Scope::Read, "world-test", f.at).unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let mut app = oracle_server::http::App::new(f.s);
    app.self_url = base.clone();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    rt.spawn(async move {
        let listener = tokio::net::TcpListener::from_std(socket).unwrap();
        axum::serve(listener, oracle_server::http::router(app)).await.unwrap();
    });
    Served { base, auth: format!("Bearer {token}"), _rt: rt }
}

impl Served {
    fn get(&self, path: &str) -> Value {
        ureq::get(&format!("{}{path}", self.base)).set("Authorization", &self.auth).call().unwrap().into_json().unwrap()
    }
    fn call(&self, tool: &str, args: Value) -> Value {
        let v: Value = ureq::post(&format!("{}/mcp", self.base)).set("Authorization", &self.auth)
            .send_json(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}})).unwrap().into_json().unwrap();
        v["result"].clone()
    }
}

#[test]
fn the_world_page_composes_six_bounded_sections_from_signed_heads() {
    let (f, ids) = seeded();
    let served = serve(f);
    let w = served.get("/world");
    assert_eq!(w["schema"], "world/1", "{w}");
    // NORTH STAR
    let ns = &w["north_star"];
    assert_eq!((ns["canon"]["version"].as_i64(), ns["canon"]["rules"].as_u64(), ns["canon"]["id"].as_str()), (Some(3), Some(2), Some(ids[0].as_str())));
    let purposes: Vec<(&str, &str)> = ns["identities"].as_array().unwrap().iter().map(|i| (i["seat"].as_str().unwrap(), i["purpose"].as_str().unwrap())).collect();
    assert_eq!(purposes, [("emmy", "Emmy reviews every contract"), ("rocky", "Rocky builds AXXIS with Blaze.")], "one line per seat, its newest head, the first sentence");
    // NOW
    let nw = &w["now_section"];
    let doing: Vec<(&str, &str)> = nw["steps"].as_array().unwrap().iter().map(|s| (s["plan"].as_str().unwrap(), s["step"].as_str().unwrap())).collect();
    assert_eq!(doing, [("plan.axxis.jobs", "S2"), ("plan.oracle.search", "S1")], "every doing step, newest plan first");
    assert_eq!(nw["steps"][1]["owner"], "rocky");
    assert_eq!((nw["queue"]["items"].as_array().map(Vec::len), nw["queue"]["more"].as_u64(), nw["queue"]["items"][0]["subject"].as_str()), (Some(5), Some(2), Some("queue.item1")));
    // ACTIVE PROJECTS: twelve of fifteen, the newest first, nothing done, nothing private, no plan.
    let p = &w["projects"];
    assert_eq!((p["items"].as_array().map(Vec::len), p["more"].as_u64()), (Some(12), Some(3)), "{p}");
    assert_eq!((p["items"][0]["subject"].as_str(), p["items"][0]["state"].as_str(), p["items"][0]["note"].as_str()), (Some("work.blocked"), Some("blocked"), Some("Blocked on CI.")));
    assert!(p["items"].as_array().unwrap().iter().all(|i| i["state"] != "done" && i["room"] != "secret" && !i["subject"].as_str().unwrap().starts_with("plan.")));
    // KEY DECISIONS: eight of the ten this week, newest first; the old one and the private one are not counted.
    let d = &w["decisions"];
    assert_eq!((d["items"].as_array().map(Vec::len), d["more"].as_u64(), d["items"][0]["text"].as_str()), (Some(8), Some(2), Some("Decision 9: keep the budgets.")), "{d}");
    // ACTIVE SEATS: the two with a State, newest written first.
    let seats: Vec<(&str, &str)> = w["seats"]["items"].as_array().unwrap().iter().map(|s| (s["seat"].as_str().unwrap(), s["current"].as_str().unwrap())).collect();
    assert_eq!(seats, [("emmy", "Reviewing the budgets"), ("rocky", "Building the world page.")]);
    // OPEN QUESTIONS: the two unanswered decides, oldest first.
    let q: Vec<&str> = w["questions"]["items"].as_array().unwrap().iter().map(|q| q["id"].as_str().unwrap()).collect();
    let n = ids.len();
    assert_eq!(q, [ids[n - 3].as_str(), ids[n - 1].as_str()], "the answered one is gone: {}", w["questions"]);
    assert!(!w.to_string().contains("secret"), "a private room's records never reach the page");

    // Through POST /mcp: the same page, rendered, under 6,000 bytes; the brief verb is the same answer.
    let r = served.call("oracle_brief", json!({"where": "world"}));
    assert_eq!(r["isError"], false, "{r}");
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.len() < 6_000, "{} bytes:\n{text}", text.len());
    for head in ["WORLD STATE · composed", "NORTH STAR", "NOW", "ACTIVE PROJECTS", "KEY DECISIONS", "ACTIVE SEATS", "OPEN QUESTIONS"] {
        assert!(text.contains(head), "{head} missing:\n{text}");
    }
    assert!(text.contains("  + 3 more · ") && text.contains("  + 2 more · ") && text.contains("    + 2 more in the queue"), "each section counts the rest:\n{text}");
    assert!(text.trim_end().lines().last().unwrap().starts_with("composed at ") && text.contains(&format!(" · {} bytes · next: ox plan <room> <name> · ox get <id>", text.len())), "the footer says the page's own size:\n{text}");
    let code = |id: &str| id.rsplit('-').next().unwrap().to_string();
    // Every record the page shows is named on its line by its code; the superseded identity head and the answered ask are not.
    for (i, id) in ids.iter().enumerate() {
        let shown = text.contains(&code(id));
        if i == 1 || i == n - 2 {
            assert!(!shown, "{id} should not be on the page:\n{text}");
        } else {
            assert!(shown, "{id} is not on the page:\n{text}");
        }
    }
    let v = served.call("brief", json!({"where": "world"}));
    assert_eq!(v["content"][0]["text"], r["content"][0]["text"]);
    assert_eq!(r["structuredContent"]["schema"], "world/1");
    // A where the brief does not know is refused, naming the forms.
    let bad = served.call("oracle_brief", json!({"where": "moon"}));
    assert!(bad["isError"] == true && bad["content"][0]["text"].as_str().unwrap().contains("me (the default) or world"), "{bad}");
}
