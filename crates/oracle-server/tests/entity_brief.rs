use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, Kind};
use oracle_server::{
    cortex::{nid_of, observation_nid},
    entity,
    store::{Principal, Scope, Store},
};
use serde_json::{json, Value};

struct Fixture {
    s: Store,
    at: i64,
}
impl Fixture {
    fn new() -> Self {
        let s = Store::memory().unwrap();
        s.init_owner("blaze", &pubkey_hex(&SigningKey::from_bytes(&[1; 32])), 1)
            .unwrap();
        Self { s, at: 100 }
    }
    fn put(&mut self, room: &str, text: &str, cargo: Value, tags: &[&str]) -> String {
        self.at += 1;
        let e = Envelope::sign(
            &SigningKey::from_bytes(&[1; 32]),
            room,
            "blaze",
            Kind::Say,
            text,
            Some(cargo.to_string()),
            tags.iter().map(|s| s.to_string()).collect(),
            self.at,
        );
        self.s
            .write(
                &Principal {
                    handle: "blaze".into(),
                    scope: Scope::Write,
                },
                &e,
                self.at,
            )
            .unwrap()
            .id
    }
    fn node(&mut self, kind: &str, title: &str) -> String {
        self.put(
            "cortex",
            title,
            json!({"kind":kind,"title":title}),
            &["cortex:node"],
        );
        nid_of(kind, title)
    }
    fn reading(
        &mut self,
        text: &str,
        label: &str,
        room: &str,
        approve: bool,
        targets: &[&str],
    ) -> String {
        let source = self.put(room, text, json!({}), &[]);
        let nid = observation_nid(&source, 0, text.len() as u64);
        self.put("cortex",text,json!({"kind":"observation","title":text,"quote":text,"labels":[label],"source":{"message":source,"start":0,"end":text.len()}}),&["cortex:node"]);
        if approve {
            self.put(
                "cortex",
                "approve",
                json!({"target":nid,"verdict":"approve","by":"blaze"}),
                &["cortex:verdict"],
            );
        }
        for target in targets {
            self.put(
                "cortex",
                "about",
                json!({"from":nid,"to":target,"kind":"about"}),
                &["cortex:link"],
            );
        }
        nid
    }
}

#[test]
fn primer_uses_approved_evidence_and_never_invents_missing_fields() {
    let mut f = Fixture::new();
    let oracle = f.node("entity", "Oracle");
    let dot = f.node("entity", "DOT");
    f.reading(
        "Oracle is shared memory.",
        "Profile",
        "notes",
        true,
        &[&oracle],
    );
    f.reading(
        "Oracle keeps signed records.",
        "Decision",
        "notes",
        true,
        &[&oracle, &dot],
    );
    f.reading(
        "UNAPPROVED fiction.",
        "Decision",
        "notes",
        false,
        &[&oracle],
    );
    let q = f.node("question", "How does recovery work?");
    f.put(
        "cortex",
        "about",
        json!({"from":q,"to":oracle,"kind":"about"}),
        &["cortex:link"],
    );
    let b = entity::brief(&f.s, "oracle", "blaze").unwrap();
    assert_eq!(b["charter"]["text"], "Oracle is shared memory.");
    assert_eq!(b["positions"].as_array().unwrap().len(), 1);
    assert_eq!(b["connections"][0]["title"], "DOT");
    assert_eq!(b["open_questions"][0]["text"], "How does recovery work?");
    assert!(!b.to_string().contains("UNAPPROVED"));
    assert!(b["primer"].as_str().unwrap().len() <= 500);
    let empty = entity::brief(&f.s, "DOT", "blaze").unwrap();
    assert!(empty["charter"].is_null());
    assert!(matches!(
        entity::brief(&f.s, "missing", "blaze"),
        Err(oracle_server::store::StoreError::NotFound)
    ));
    f.put(
        "cortex",
        "answered",
        json!({"target":q,"verdict":"answered","by":"blaze"}),
        &["cortex:verdict"],
    );
    assert!(
        entity::brief(&f.s, "Oracle", "blaze").unwrap()["open_questions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn private_source_and_private_links_do_not_influence_a_primer() {
    let mut f = Fixture::new();
    let id = f.node("entity", "Oracle");
    f.reading("Oracle PRIVATE-PROFILE.", "Profile", "secret", true, &[&id]);
    f.put(
        "oracle",
        "[Private rooms]",
        json!({"schema":"private-rooms/1","rooms":{"secret":["blaze"]}}),
        &["schema:private-rooms/1"],
    );
    let outsider = entity::brief(&f.s, "Oracle", "rocky").unwrap();
    assert!(outsider["charter"].is_null());
    assert!(!outsider.to_string().contains("PRIVATE-PROFILE"));
    assert!(entity::brief(&f.s, "Oracle", "blaze").unwrap()["charter"].is_object());
}

#[test]
fn unicode_primer_stays_inside_the_byte_budget() {
    let mut f = Fixture::new();
    let id = f.node("entity", "Oracle");
    f.reading(&"🦉智慧".repeat(100), "Profile", "notes", true, &[&id]);
    let b = entity::brief(&f.s, "Oracle", "blaze").unwrap();
    assert!(b["primer"].as_str().unwrap().len() <= 500);
}

#[test]
fn existing_title_variants_resolve_without_fuzzy_person_merging() {
    let mut f = Fixture::new();
    f.node("entity", "Emmy Noether");
    f.node("entity", "Emmy-Noether");
    f.node("entity", "Emmy");
    assert_eq!(entity::brief(&f.s,"emmy noether","blaze").unwrap()["entity"]["title"], "Emmy Noether");
    assert_eq!(entity::brief(&f.s,"Emmy","blaze").unwrap()["entity"]["title"], "Emmy");
}

#[test]
fn an_approved_said_by_link_is_attributed_to_the_entity() {
    let mut f = Fixture::new();
    let oracle = f.node("entity", "Oracle");
    let reading = f.reading("Keep evidence.", "Rule", "notes", true, &[]);
    f.put(
        "cortex",
        "said-by",
        json!({"from":reading,"to":oracle,"kind":"said-by"}),
        &["cortex:link"],
    );
    assert_eq!(
        entity::brief(&f.s, "Oracle", "blaze").unwrap()["positions"][0]["relation"],
        "by"
    );
}

#[test]
fn entity_primer_is_available_through_http_and_mcp_without_a_hosted_key() {
    let mut f = Fixture::new();
    let id = f.node("entity", "Oracle");
    f.reading(
        "Oracle keeps signed memory.",
        "Profile",
        "notes",
        true,
        &[&id],
    );
    let token =
        f.s.mint_token("blaze", Scope::Read, "entity-test", f.at)
            .unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let mut app = oracle_server::http::App::new(f.s);
    app.self_url = base.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let server = runtime.spawn(async move {
        let listener = tokio::net::TcpListener::from_std(socket).unwrap();
        axum::serve(listener, oracle_server::http::router(app))
            .await
            .unwrap();
    });
    let url = format!("{base}/cortex/entity/Oracle");
    assert!(matches!(
        ureq::get(&url).call(),
        Err(ureq::Error::Status(401, _))
    ));
    let auth = format!("Bearer {token}");
    let body: Value = ureq::get(&url)
        .set("Authorization", &auth)
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(body["charter"]["text"], "Oracle keeps signed memory.");
    let rpc: Value = ureq::post(&format!("{base}/mcp")).set("Authorization", &auth).send_json(json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"oracle_cortex","arguments":{"op":"entity","q":"Oracle"}}
    })).unwrap().into_json().unwrap();
    assert_ne!(rpc["result"]["isError"], true, "{rpc}");
    assert!(
        rpc.to_string().contains("Oracle keeps signed memory"),
        "{rpc}"
    );
    server.abort();
}

#[test]
fn addressed_asks_require_the_same_seat_or_an_authorized_gateway() {
    let mut f = Fixture::new();
    f.node("entity", "Blaze");
    let ask = f.put("notes", "Please check the deployment.", json!({}), &["ask:act", "to:blaze"]);
    let own = entity::brief(&f.s, "Blaze", "blaze").unwrap();
    assert_eq!(own["open_questions"][0]["id"], ask);
    let other = entity::brief(&f.s, "Blaze", "emmy").unwrap();
    assert_eq!(other["addressed_asks"], "not authorized for this seat");
    assert!(other["open_questions"].as_array().unwrap().is_empty());
    f.put("oracle", "[Private rooms]", json!({"schema":"private-rooms/1","rooms":{"secret":["blaze"]}}), &["schema:private-rooms/1"]);
    f.put("secret", "PRIVATE-ASK", json!({}), &["ask:act", "to:blaze"]);
    let event = oracle_proto::KeyEvent::sign(&SigningKey::from_bytes(&[1; 32]), "blaze", oracle_proto::KeyAction::Add, "emmy", &pubkey_hex(&SigningKey::from_bytes(&[2; 32])), f.at);
    f.s.apply_key_event(&event, f.at).unwrap();
    f.s.gateway_add("emmy", &["asks".to_string()], f.at).unwrap();
    assert!(!entity::brief(&f.s,"Blaze","emmy").unwrap().to_string().contains("PRIVATE-ASK"));
    assert_eq!(entity::brief(&f.s,"Blaze","emmy").unwrap()["open_questions"][0]["id"], ask);
    f.put("notes", "Completed.", json!({}), &["intent:result", &format!("reply:{ask}")]);
    assert!(entity::brief(&f.s,"Blaze","emmy").unwrap()["open_questions"].as_array().unwrap().is_empty());
}
