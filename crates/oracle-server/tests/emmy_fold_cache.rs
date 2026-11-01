//! View as another seat (docs/VIEW-AS.md): the owner, or an admin token, reads as any seat through the header
//! `x-oracle-view-as`, with read scope only; a seat that is not the owner may not; every use is audited; the MCP door
//! passes it on, so a tool answers as that seat. A real server on a loopback port.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::http::{now_ms, router, App};
use oracle_server::store::{Scope, Store};
use serde_json::{json, Value};

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

struct Srv {
    base: String,
    app: App,
    blaze: String,
    rocky: String,
}

fn start() -> Srv {
    let store = Store::memory().unwrap();
    store.init_owner("blaze", &pubkey_hex(&sk(1)), 1).unwrap();
    store.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), 2), 2).unwrap();
    // The AXXIS web door holds a read token for Blaze: the owner's seat, read scope.
    let blaze = store.mint_token("blaze", Scope::Read, "web door", 3).unwrap();
    let rocky = store.mint_token("rocky", Scope::Write, "rocky", 3).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut app = App::new(store);
    app.self_url = base.clone();
    let served = app.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
            let l = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(l, router(served)).await.unwrap();
        })
    });
    Srv { base, app, blaze, rocky }
}

impl Srv {
    fn get(&self, token: &str, path: &str, view: Option<&str>) -> (u16, Value) {
        let mut req = ureq::get(&format!("{}{path}", self.base)).set("Authorization", &format!("Bearer {token}"));
        if let Some(v) = view {
            req = req.set("x-oracle-view-as", v);
        }
        match req.call() {
            Ok(r) => (r.status(), r.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(c, r)) => (c, r.into_json().unwrap_or(Value::Null)),
            Err(e) => panic!("{e}"),
        }
    }
    fn post(&self, token: &str, path: &str, body: &Value, view: Option<&str>) -> (u16, Value) {
        let mut req = ureq::post(&format!("{}{path}", self.base)).set("Authorization", &format!("Bearer {token}")).set("Accept", "application/json, text/event-stream");
        if let Some(v) = view {
            req = req.set("x-oracle-view-as", v);
        }
        match req.send_json(body.clone()) {
            Ok(r) => (r.status(), r.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(c, r)) => (c, r.into_json().unwrap_or(Value::Null)),
            Err(e) => panic!("{e}"),
        }
    }
}


fn put(s: &Srv, seat: &str, key: u8, text: &str) {
    let at=now_ms();
    let env=Envelope::sign(&sk(key), "core", seat, Kind::Say, text, None, vec![], at);
    s.app.store.write(&oracle_server::store::Principal{handle:seat.into(),scope:Scope::Write},&env,at).unwrap();
}
#[test]
fn cached_fold_http_wires_cursors_identity_writes_and_metrics() {
    let s=start();
    put(&s,"blaze",1,"for rocky"); put(&s,"rocky",2,"for blaze");
    let (code,a)=s.get(&s.rocky,"/fold",None);assert_eq!(code,200);assert_eq!(a["agent"],"rocky");assert_eq!(a["returned"],1);
    let hits=s.app.folds.hits();
    assert_eq!(s.get(&s.rocky,"/fold",None).1,a);assert_eq!(s.app.folds.hits(),hits+1);
    let (code,m)=s.get(&s.rocky,"/metrics/routes",None);assert_eq!(code,200);assert_eq!(m["fold_cache_hits"].as_u64(),Some(hits+1));
    assert_eq!(s.get(&s.blaze,"/fold",None).1["agent"],"blaze");
    assert_eq!(s.get(&s.blaze,"/fold",Some("rocky")).1,a);
    assert_eq!(s.get("not-a-token","/fold",None).0,401);
    assert_eq!(s.post(&s.rocky,"/fold",&json!({"all":true}),None).0,200);
    assert_eq!(s.get(&s.rocky,"/fold",None).1["returned"],0);
    assert_eq!(s.get(&s.blaze,"/fold",None).1["returned"],1);
    assert_eq!(s.post(&s.rocky,"/fold",&json!({"marks":{"core":0},"rewind":true}),None).0,200);
    let r=s.get(&s.rocky,"/fold",None).1;assert_eq!(r["returned"],1);
    assert_eq!(s.post(&s.rocky,"/fold",&json!({"marks":r["next_marks"]}),None).0,200);
    assert_eq!(s.get(&s.rocky,"/fold",None).1["returned"],0);
    let hits=s.app.folds.hits(); put(&s,"blaze",1,"new write");
    assert_eq!(s.get(&s.rocky,"/fold",None).1["returned"],1);assert_eq!(s.app.folds.hits(),hits);
}
