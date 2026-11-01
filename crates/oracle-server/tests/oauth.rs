//! The connectors' OAuth, ported from the old Oracle (src/oauth.rs): discovery, dynamic registration, the connect-code form, a full PKCE flow, the refusals
//! (admin never granted, wrong redirect, replayed code, wrong verifier, wrong connect code, stranger seat), refresh, and revocation into the file `legacy.rs`
//! reads. A real server on a loopback port, a real client; `legacy.rs` verifies what is minted.

use oracle_proto::{pubkey_hex, KeyAction, KeyEvent};
use oracle_server::http::{now_ms, router, App};
use oracle_server::legacy::Legacy;
use oracle_server::oauth::{b64u, OAuth};
use oracle_server::store::{Scope, Store};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

const SECRET: &str = "test-secret-oauth-1";
const CODE: &str = "connect-code-for-tests";
const ADMIN: &str = "admin-token-for-tests";
const ORIGIN: &str = "https://oracle.example";
const REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";

struct Srv {
    base: String,
    store: Arc<Store>,
    revoked: PathBuf,
    agent: ureq::Agent,
}

fn start(connect_code: Option<&str>) -> Srv {
    let store = Store::memory().unwrap();
    let sk = |n: u8| ed25519_dalek::SigningKey::from_bytes(&[n; 32]);
    store.init_owner("blaze", &pubkey_hex(&sk(1)), 1).unwrap();
    store.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "emmy", &pubkey_hex(&sk(2)), 2), 2).unwrap();
    let revoked = std::env::temp_dir().join(format!("oauth-revoked-{}-{}", std::process::id(), oracle_proto::random_hex(4)));
    let mut app = App::new(store);
    app.oauth = Arc::new(OAuth::new(SECRET, connect_code, Some(ADMIN), ORIGIN, Some(&revoked)));
    let store = app.store.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
            let l = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(l, router(app)).await.unwrap();
        })
    });
    Srv { base, store, revoked, agent: ureq::AgentBuilder::new().redirects(0).build() }
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// (status, location, body text)
type Reply = (u16, Option<String>, String);
fn reply(r: Result<ureq::Response, ureq::Error>) -> Reply {
    let r = match r {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("transport: {e}"),
    };
    (r.status(), r.header("location").map(str::to_owned), r.into_string().unwrap_or_default())
}
fn json_of(r: &Reply) -> Value {
    serde_json::from_str(&r.2).unwrap_or_else(|_| panic!("not json: {}", r.2))
}

impl Srv {
    fn get(&self, path: &str) -> Reply {
        reply(self.agent.get(&format!("{}{path}", self.base)).call())
    }
    fn post_json(&self, path: &str, body: Value) -> Reply {
        reply(self.agent.post(&format!("{}{path}", self.base)).send_json(body))
    }
    fn post_form(&self, path: &str, pairs: &[(&str, &str)], headers: &[(&str, &str)]) -> Reply {
        let body: Vec<String> = pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect();
        let mut r = self.agent.post(&format!("{}{path}", self.base)).set("content-type", "application/x-www-form-urlencoded");
        for (k, v) in headers {
            r = r.set(k, v);
        }
        reply(r.send_string(&body.join("&")))
    }
    fn register(&self) -> String {
        let r = self.post_json("/oauth/register", json!({"client_name": "Claude", "redirect_uris": [REDIRECT]}));
        assert_eq!(r.0, 201, "{}", r.2);
        json_of(&r)["client_id"].as_str().unwrap().to_string()
    }
    /// Approve on the form; returns the authorize reply.
    #[allow(clippy::too_many_arguments)]
    fn approve(&self, client: &str, challenge: &str, scope: &str, ticked: &[&str], seat: &str, code: &str, redirect: &str) -> Reply {
        let mut pairs = vec![
            ("client_id", client), ("redirect_uri", redirect), ("response_type", "code"), ("code_challenge", challenge), ("code_challenge_method", "S256"),
            ("state", "st 1&x"), ("scope", scope), ("seat_handle", seat), ("oracle_code", code), ("decision", "approve"),
        ];
        for t in ticked {
            pairs.push(("grant_scope", t));
        }
        self.post_form("/oauth/authorize", &pairs, &[])
    }
    fn exchange(&self, code: &str, verifier: &str) -> Reply {
        self.post_form("/oauth/token", &[("grant_type", "authorization_code"), ("code", code), ("code_verifier", verifier)], &[])
    }
    fn legacy(&self) -> Legacy {
        Legacy::new(SECRET, None, Some(&self.revoked), false)
    }
}

fn pkce() -> (String, String) {
    let verifier = b64u(&hex::decode(oracle_proto::random_hex(32)).unwrap());
    let challenge = b64u(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}
fn code_of(r: &Reply) -> String {
    assert_eq!(r.0, 302, "{}", r.2);
    let loc = r.1.clone().unwrap();
    assert!(loc.starts_with(&format!("{REDIRECT}?code=code_")), "{loc}");
    assert!(loc.ends_with("&state=st+1%26x"), "state is carried back, quoted like urlencode: {loc}");
    loc.split(['?', '&']).find_map(|p| p.strip_prefix("code=")).unwrap().to_string()
}

#[test]
fn discovery_documents_say_what_the_old_server_said() {
    let s = start(Some(CODE));
    let m = json_of(&s.get("/.well-known/oauth-authorization-server"));
    assert_eq!(m["issuer"], ORIGIN);
    assert_eq!(m["authorization_endpoint"], format!("{ORIGIN}/oauth/authorize"));
    assert_eq!(m["token_endpoint"], format!("{ORIGIN}/oauth/token"));
    assert_eq!(m["registration_endpoint"], format!("{ORIGIN}/oauth/register"));
    assert_eq!(m["scopes_supported"], json!(["oracle:read", "oracle:write", "oracle:admin", "mcp"]));
    assert_eq!(m["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(m["token_endpoint_auth_methods_supported"], json!(["none"]));
    assert_eq!(m["grant_types_supported"], json!(["authorization_code", "refresh_token"]));
    let root = json_of(&s.get("/.well-known/oauth-protected-resource"));
    assert_eq!(root["resource"], format!("{ORIGIN}/"));
    let mcp = json_of(&s.get("/.well-known/oauth-protected-resource/mcp/"));
    assert_eq!(mcp["resource"], format!("{ORIGIN}/mcp/"), "the path-specific document keeps the path");
    assert_eq!(json_of(&s.get("/.well-known/oauth-protected-resource/a/b"))["resource"], format!("{ORIGIN}/a/b/"));
    assert_eq!(mcp["authorization_servers"], json!([ORIGIN]));
    assert_eq!(mcp["bearer_methods_supported"], json!(["header"]));
}

#[test]
fn registration_gives_a_client_and_a_string_secret_and_keeps_them() {
    let s = start(Some(CODE));
    let r = s.post_json("/oauth/register", json!({"client_name": "Claude", "redirect_uris": [REDIRECT]}));
    assert_eq!(r.0, 201);
    let v = json_of(&r);
    assert!(v["client_id"].as_str().unwrap().starts_with("client_") && v["client_secret"].as_str().unwrap().starts_with("secret_"));
    assert_eq!((v["client_secret_expires_at"].clone(), v["token_endpoint_auth_method"].clone(), v["scope"].clone()), (json!(0), json!("none"), json!("mcp")));
    assert_eq!((v["grant_types"].clone(), v["response_types"].clone()), (json!(["authorization_code"]), json!(["code"])));
    let stored = s.store.with_conn(|c| oracle_server::oauth::client(c, v["client_id"].as_str().unwrap())).unwrap().unwrap();
    assert_eq!(stored, v, "stored durably, exactly as answered");
    // junk is accepted too, as before
    let j = s.post_form("/oauth/register", &[], &[]);
    assert_eq!(j.0, 201);
    assert_eq!(json_of(&j)["client_name"], "anonymous");
    assert_eq!(json_of(&j)["redirect_uris"], json!([]));
}

#[test]
fn a_full_pkce_flow_mints_a_token_the_legacy_verifier_accepts_and_refresh_works() {
    let s = start(Some(CODE));
    let client = s.register();
    let (verifier, challenge) = pkce();
    // the form first
    let q = format!("/oauth/authorize?client_id={client}&redirect_uri={}&response_type=code&code_challenge={challenge}&code_challenge_method=S256&state=s&scope={}", enc(REDIRECT), enc("oracle:read oracle:write oracle:admin"));
    let f = s.get(&q);
    assert_eq!(f.0, 200);
    assert!(f.2.contains("Claude</strong> is requesting access") && f.2.contains("claude.ai") && f.2.contains("value=\"oracle:admin\""), "asks for what was requested");
    assert!(!f.2.contains("name=\"oracle_code\" value"), "the code is never echoed");
    // approve with admin ticked: read and write only
    let a = s.approve(&client, &challenge, "oracle:read oracle:write oracle:admin", &["oracle:read", "oracle:write", "oracle:admin"], "Emmy", CODE, REDIRECT);
    let code = code_of(&a);
    let t = s.exchange(&code, &verifier);
    assert_eq!(t.0, 200, "{}", t.2);
    let t = json_of(&t);
    assert_eq!((t["token_type"].clone(), t["expires_in"].clone(), t["scope"].clone()), (json!("Bearer"), json!(86400 * 90), json!("oracle:read oracle:write")), "admin is never granted");
    let access = t["access_token"].as_str().unwrap();
    // byte shape: ocg_<body>.<sig>, compact JSON, keys sorted
    let body = access.strip_prefix("ocg_").unwrap().split('.').next().unwrap();
    let raw = String::from_utf8(oracle_server::oauth::b64u_decode(body).unwrap()).unwrap();
    let keys: Vec<&str> = raw.trim_matches(|c| c == '{' || c == '}').split(",\"").map(|p| p.trim_start_matches('"').split('"').next().unwrap()).collect();
    assert_eq!(keys, ["aud", "exp", "handle", "iat", "iss", "jti", "scope", "sub"], "{raw}");
    assert!(!raw.contains(": ") && !raw.contains(", "), "compact separators: {raw}");
    let p: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!((p["iss"].clone(), p["aud"].clone(), p["sub"].clone(), p["handle"].clone()), (json!("oracle.example"), json!("oracle-mcp"), json!(client), json!("emmy")));
    assert_eq!(p["exp"].as_i64().unwrap() - p["iat"].as_i64().unwrap(), 86400 * 90);
    // legacy.rs verifies it: emmy, write, never admin
    let who = s.legacy().resolve(access, &s.store, now_ms()).expect("the legacy verifier accepts what the port mints");
    assert_eq!((who.handle.as_str(), who.scope), ("emmy", Scope::Write));
    // refresh: a new token, same grant, no new refresh token
    let rt = t["refresh_token"].as_str().unwrap();
    assert!(rt.starts_with("refresh_"));
    let r = s.post_form("/oauth/token", &[("grant_type", "refresh_token"), ("refresh_token", rt)], &[]);
    assert_eq!(r.0, 200, "{}", r.2);
    let r = json_of(&r);
    assert_eq!(r["scope"], "oracle:read oracle:write");
    assert!(r.get("refresh_token").is_none());
    assert_ne!(r["access_token"], t["access_token"], "a fresh jti");
    assert!(s.legacy().resolve(r["access_token"].as_str().unwrap(), &s.store, now_ms()).is_some());
    // the JSON body works for token too
    let j = s.post_json("/oauth/token", json!({"grant_type": "refresh_token", "refresh_token": rt}));
    assert_eq!(j.0, 200);
    // the refresh token outlives the server: it is in the store
    let stored: i64 = s.store.with_conn(|c| Ok(c.query_row("SELECT count(*) FROM oauth_refresh WHERE token=?1", [rt], |r| r.get(0)).unwrap())).unwrap();
    assert_eq!(stored, 1);
}

#[test]
fn the_refusals() {
    let s = start(Some(CODE));
    let client = s.register();
    let (verifier, challenge) = pkce();
    let all = ["oracle:read", "oracle:write"];
    // wrong redirect: an error, never a redirect, even with a good code
    let bad = s.approve(&client, &challenge, "oracle:read", &all, "emmy", CODE, "https://evil.example/cb");
    assert_eq!(bad.0, 400);
    assert!(bad.1.is_none() && json_of(&bad)["error"] == "invalid_request");
    // unknown client
    let unknown = s.approve("client_nobody", &challenge, "oracle:read", &all, "emmy", CODE, REDIRECT);
    assert_eq!((unknown.0, json_of(&unknown)["error"].clone()), (400, json!("invalid_client")));
    // no challenge: PKCE is mandatory
    let none = s.approve(&client, "", "oracle:read", &all, "emmy", CODE, REDIRECT);
    assert_eq!(none.0, 400);
    assert!(json_of(&none)["detail"].as_str().unwrap().contains("PKCE"));
    // wrong connect code, a stranger seat, no seat, nothing ticked: the form again, never a code
    for (seat, code, ticked, why) in [("emmy", "wrong", &all[..], "Invalid connect code."), ("nobody", CODE, &all[..], "is not enrolled"), ("", CODE, &all[..], "Choose the enrolled"), ("emmy", CODE, &[][..], "Select at least one")] {
        let r = s.approve(&client, &challenge, "oracle:read oracle:write", ticked, seat, code, REDIRECT);
        assert_eq!(r.0, 200, "{why}: {}", r.2);
        assert!(r.1.is_none() && r.2.contains(why), "{why}");
        assert!(!r.2.contains(CODE), "the connect code is never echoed");
    }
    // only admin ticked, admin requested: nothing grantable
    let adm = s.approve(&client, &challenge, "oracle:admin", &["oracle:admin"], "emmy", CODE, REDIRECT);
    assert!(adm.1.is_none() && adm.2.contains("Select at least one"), "admin alone grants nothing");
    // deny goes back with access_denied
    let deny = s.post_form("/oauth/authorize", &[("client_id", &client), ("redirect_uri", REDIRECT), ("code_challenge", &challenge), ("state", "z"), ("decision", "deny")], &[]);
    assert_eq!((deny.0, deny.1.as_deref()), (302, Some(&*format!("{REDIRECT}?error=access_denied&state=z"))));
    // a wrong verifier burns the code
    let code = code_of(&s.approve(&client, &challenge, "oracle:read", &all, "emmy", CODE, REDIRECT));
    let wrong = s.exchange(&code, "not-the-verifier");
    assert_eq!((wrong.0, json_of(&wrong)["error"].clone()), (400, json!("invalid_grant")));
    assert_eq!(s.exchange(&code, &verifier).0, 400, "the code was consumed by the failed try");
    // a replayed code
    let code = code_of(&s.approve(&client, &challenge, "oracle:read", &all, "emmy", CODE, REDIRECT));
    assert_eq!(s.exchange(&code, &verifier).0, 200);
    let again = s.exchange(&code, &verifier);
    assert_eq!((again.0, json_of(&again)["error"].clone()), (400, json!("invalid_grant")));
    // a missing verifier; an expired code
    let code = code_of(&s.approve(&client, &challenge, "oracle:read", &all, "emmy", CODE, REDIRECT));
    let nov = s.post_form("/oauth/token", &[("grant_type", "authorization_code"), ("code", &code)], &[]);
    assert!(json_of(&nov)["detail"].as_str().unwrap().contains("code_verifier required"));
    let code = code_of(&s.approve(&client, &challenge, "oracle:read", &all, "emmy", CODE, REDIRECT));
    s.store.with_conn(|c| Ok(c.execute("UPDATE oauth_codes SET expires_ms=1", []).unwrap())).unwrap();
    let old = s.exchange(&code, &verifier);
    assert_eq!((old.0, json_of(&old)["detail"].clone()), (400, json!("code expired")));
    // unknown refresh token, unknown grant, no body
    let u = s.post_form("/oauth/token", &[("grant_type", "refresh_token"), ("refresh_token", "refresh_garbage")], &[]);
    assert_eq!((u.0, json_of(&u)["error"].clone()), (400, json!("invalid_grant")));
    let g = s.post_form("/oauth/token", &[("grant_type", "password")], &[]);
    assert_eq!(json_of(&g)["error"], "unsupported_grant_type");
}

#[test]
fn without_a_connect_code_authorize_is_503_and_a_redirect_uri_is_compared_exactly() {
    let s = start(None);
    let client = s.register();
    let (_, challenge) = pkce();
    let r = s.approve(&client, &challenge, "oracle:read", &["oracle:read"], "emmy", CODE, REDIRECT);
    assert_eq!((r.0, json_of(&r)["error"].clone()), (503, json!("server_not_configured")));
    let s = start(Some(CODE));
    let client = s.register();
    for near in [format!("{REDIRECT}/"), REDIRECT.replace("https", "http"), format!("{REDIRECT}?x=1")] {
        assert_eq!(s.approve(&client, &challenge, "oracle:read", &["oracle:read"], "emmy", CODE, &near).0, 400, "{near}");
    }
}

#[test]
fn revoke_writes_the_jti_where_legacy_reads_it_and_only_for_the_owner_or_admin() {
    let s = start(Some(CODE));
    let client = s.register();
    let (verifier, challenge) = pkce();
    let mint = |s: &Srv| {
        let code = code_of(&s.approve(&client, &challenge, "oracle:read oracle:write", &["oracle:read", "oracle:write"], "emmy", CODE, REDIRECT));
        json_of(&s.exchange(&code, &verifier))
    };
    let t = mint(&s);
    let access = t["access_token"].as_str().unwrap().to_string();
    assert!(s.legacy().resolve(&access, &s.store, now_ms()).is_some());
    // nobody, or somebody else, may not
    assert_eq!(s.post_form("/oauth/revoke", &[("token", &access)], &[]).0, 401);
    let other = mint(&s);
    let other_access = other["access_token"].as_str().unwrap();
    assert_eq!(s.post_form("/oauth/revoke", &[("token", &access)], &[("authorization", &format!("Bearer {other_access}"))]).0, 401);
    assert!(s.legacy().resolve(&access, &s.store, now_ms()).is_some(), "still alive");
    // its own bearer may
    let r = s.post_form("/oauth/revoke", &[("token", &access)], &[("authorization", &format!("Bearer {access}"))]);
    assert_eq!(r.0, 200);
    assert_eq!(json_of(&r), json!({"revoked": true, "durable": true}));
    assert!(s.legacy().resolve(&access, &s.store, now_ms()).is_none(), "dead at once, through the file legacy.rs reads");
    assert_eq!(std::fs::read_to_string(&s.revoked).unwrap().lines().count(), 1);
    // admin may revoke anyone's, by header or bearer
    let r = s.post_form("/oauth/revoke", &[("token", other_access)], &[("x-admin-token", ADMIN)]);
    assert_eq!(r.0, 200);
    assert!(s.legacy().resolve(other_access, &s.store, now_ms()).is_none());
    let third = mint(&s);
    let r = s.post_form("/oauth/revoke", &[("token", third["access_token"].as_str().unwrap())], &[("authorization", &format!("Bearer {ADMIN}"))]);
    assert_eq!(r.0, 200);
    // a wrong admin token is nobody
    assert_eq!(s.post_form("/oauth/revoke", &[("token", &access)], &[("x-admin-token", "nope")]).0, 401);
    // unknown token, empty token: 200, nothing written
    let before = std::fs::read_to_string(&s.revoked).unwrap();
    assert_eq!(json_of(&s.post_form("/oauth/revoke", &[("token", "ocg_nonsense.x")], &[("x-admin-token", ADMIN)])), json!({"revoked": true}));
    assert_eq!(s.post_form("/oauth/revoke", &[("token", "")], &[]).0, 200);
    assert_eq!(std::fs::read_to_string(&s.revoked).unwrap(), before);
    // a revoked refresh token stops refreshing
    let rt = t["refresh_token"].as_str().unwrap();
    assert_eq!(s.post_form("/oauth/revoke", &[("token", rt)], &[("authorization", &format!("Bearer {rt}"))]).0, 200);
    let u = s.post_form("/oauth/token", &[("grant_type", "refresh_token"), ("refresh_token", rt)], &[]);
    assert_eq!(u.0, 400);
    let _ = std::fs::remove_file(&s.revoked);
}
