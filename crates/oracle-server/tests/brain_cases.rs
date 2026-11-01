//! Brain cases through the cortex door (oracle-mcp cases.rs; Blaze, 28 Oct 2026: "give Noor the tools to read brain cases and draft
//! marks"; "approval will be by me"): only the listed seats read Blaze's chats, and a draft is always written as the caller.
use axum::{extract::State as AxState, routing::{get, post}, Json, Router};
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, KeyAction, KeyEvent};
use oracle_server::http::{router, App};
use oracle_server::store::{Scope, Store};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

/// A stand-in for the Time Machine's cases store: answers the two reads and records every draft it is sent.
fn fake_cases() -> (String, Arc<Mutex<Vec<Value>>>) {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let app = Router::new()
        .route("/api/sessions", get(|| async { Json(json!({"count": 1, "sessions": [{"n": 146, "state": "drafted", "marks": 117, "reviewed": 0, "prompts": 24, "first_at": "2026-02-24T16:39:59Z", "conversation": "Anime-style GUI"}]})) }))
        .route("/api/sessions/:n", get(|| async { Json(json!({"n": 146, "conversation": "Anime-style GUI", "prompts": 1, "state": "drafted", "messages": [{"sender": "human", "uuid": "p1", "case_n": 1066, "text": "So I'm thinking Monitors"}], "marks": []})) }))
        .route("/api/seat-marks", post(|AxState(seen): AxState<Arc<Mutex<Vec<Value>>>>, Json(b): Json<Value>| async move {
            seen.lock().unwrap().push(b.clone());
            Json(json!({"ok": true, "mark": 2000, "case": b["case"], "ordinal": 146}))
        }))
        .with_state(seen.clone());
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
            axum::serve(tokio::net::TcpListener::from_std(l).unwrap(), app).await.unwrap();
        })
    });
    (base, seen)
}

fn call(base: &str, token: &str, method: &str, path: &str, body: Option<Value>) -> u16 {
    let req = ureq::request(method, &format!("{base}{path}")).set("Authorization", &format!("Bearer {token}"));
    let sent = if let Some(b) = body { req.send_json(b) } else { req.call() };
    match sent {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(c, _)) => c,
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn only_listed_seats_read_brain_cases_and_a_draft_is_always_written_as_the_caller() {
    let store = Store::memory().unwrap();
    store.init_owner("blaze", &pubkey_hex(&sk(1)), 1).unwrap();
    for (seat, n) in [("rocky", 2), ("noor", 3), ("dinesh", 4)] {
        store.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, seat, &pubkey_hex(&sk(n)), 2), 2).unwrap();
    }
    let noor = store.mint_token("noor", Scope::Write, "noor", 3).unwrap();
    let dinesh = store.mint_token("dinesh", Scope::Write, "dinesh", 3).unwrap();
    let dinesh_admin = store.mint_token("dinesh", Scope::Admin, "dinesh admin", 3).unwrap();
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let mut app = App::new(store);
    app.self_url = base.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
            axum::serve(tokio::net::TcpListener::from_std(l).unwrap(), router(app)).await.unwrap();
        })
    });
    let (cases, seen) = fake_cases();
    std::env::set_var("ORACLE_CASES_URL", &cases);

    // Unset: no seat reads Blaze's chats (fail closed).
    std::env::remove_var("ORACLE_CASES_SEATS");
    assert_eq!(call(&base, &noor, "GET", "/cortex/cases", None), 403);

    std::env::set_var("ORACLE_CASES_SEATS", "rocky, Noor");
    assert_eq!(call(&base, &noor, "GET", "/cortex/cases?state=drafted", None), 200);
    assert_eq!(call(&base, &noor, "GET", "/cortex/case/146", None), 200);
    assert_eq!(call(&base, &dinesh, "GET", "/cortex/case/146", None), 403, "a seat not listed reads nothing");
    assert_eq!(call(&base, &dinesh, "POST", "/cortex/drafts", Some(json!({"case": "p1"}))), 403, "and drafts nothing");
    assert!(call(&base, &noor, "GET", "/cortex/case/..%2Fapi%2Fmarks", None) >= 400, "a chat is a number or an id, never a path");
    // Emmy's review of 9a9c31b: an unlisted admin viewing as a listed seat read case 146. View-as is refused on these routes.
    let viewed = ureq::get(&format!("{base}/cortex/case/146")).set("Authorization", &format!("Bearer {dinesh_admin}")).set("x-oracle-view-as", "noor").call();
    assert!(matches!(viewed, Err(ureq::Error::Status(403, _))), "view-as must not pass the list: {:?}", viewed.map(|r| r.status()));
    let own = ureq::get(&format!("{base}/cortex/case/146")).set("Authorization", &format!("Bearer {noor}")).set("x-oracle-view-as", "noor").call();
    assert!(matches!(own, Err(ureq::Error::Status(403, _))), "not even as oneself");

    // The body may claim any seat; the draft is written as the authenticated caller.
    assert_eq!(call(&base, &noor, "POST", "/cortex/drafts", Some(json!({"seat": "blaze", "case": "p1", "quote": "So I'm thinking"}))), 200);
    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["seat"], "noor");
}
