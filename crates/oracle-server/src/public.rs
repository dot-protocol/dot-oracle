//! The stranger page (Jobs): "a stranger with no seat opens one page, asks one question,
//! and in 30 seconds sees one answer with the observation id, who signed it, and the date. They open the id. The signed
//! record is the page." No account, no email.
//!
//! Only rooms named in ORACLE_PUBLIC_ROOMS are ever read here; the list is empty by default, and then every route below
//! answers 404, so this can ship dark. Which rooms are public is the owner's decision, never a default. Searches are
//! capped in length and run a few at a time.
use crate::http::App;
use crate::store::StoreError;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

/// A room a stranger may read, and from when: only messages Oracle received at or after `from_ms` are public, so a
/// room with a private past can open from today (Blaze, 2026-10-10: #piper, new posts only; its 43 June-July messages
/// stay private).
#[derive(Clone, Debug, PartialEq)]
pub struct PublicRoom {
    pub name: String,
    pub from_ms: i64,
}

impl PublicRoom {
    pub fn admits(&self, channel: &str, created_ms: i64) -> bool {
        self.name == channel && created_ms >= self.from_ms
    }
}

/// ORACLE_PUBLIC_ROOMS="piper@1791640000000,dot": the rooms a stranger may read, each optionally from a moment
/// (Oracle-received ms). Empty: nothing is public. A from that does not parse makes the room not public at all.
/// The cortex of a room, the only part a stranger reads (Blaze, 2026-10-28: "what you show publicly is the cortex … only
/// the state transition"): its State revisions and its claims and their links. The
/// dialogue is the room's hippocampus and stays in Oracle, on every public surface: the room, the record page, search.
/// A claim's source checks are cortex too (Rocky, 29 Oct 2026; Blaze: "build the proper verified map and checking properly"):
/// `claim:check` records say how a claim was checked against its primary source (schema source-check/1), so a stranger
/// can see why a claim stands, not only that it does.
pub const CORTEX_TAGS: [&str; 4] = ["room-context:v1", "claim:mint", "claim:edge", "claim:check"];
pub fn is_cortex(o: &crate::store::Observation) -> bool {
    is_cortex_tags(&o.tags)
}
pub fn is_cortex_tags(tags: &[String]) -> bool {
    tags.iter().any(|t| CORTEX_TAGS.contains(&t.as_str()))
}

impl PublicRoom {
    /// A record a stranger may read: in this room's public window, and part of its cortex.
    pub fn admits_record(&self, o: &crate::store::Observation) -> bool {
        self.admits(&o.channel, o.created_ms) && is_cortex(o)
    }
}

pub fn rooms_from_env() -> Vec<PublicRoom> {
    parse_rooms(&std::env::var("ORACLE_PUBLIC_ROOMS").unwrap_or_default())
}

pub fn parse_rooms(spec: &str) -> Vec<PublicRoom> {
    spec.split(',')
        .filter_map(|r| {
            let (name, from) = r.trim().split_once('@').map_or((r.trim(), None), |(n, f)| (n, Some(f.trim())));
            let name = name.trim_start_matches('#').to_lowercase();
            let from_ms = match from {
                None => 0,
                Some(f) => f.parse::<i64>().ok()?,
            };
            (!name.is_empty()).then_some(PublicRoom { name, from_ms })
        })
        .collect()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn not_public() -> Response {
    (StatusCode::NOT_FOUND, "not public").into_response()
}

#[derive(Deserialize)]
pub struct AskQ {
    q: Option<String>,
}

const STYLE: &str = r#"<style>
:root{--bg:#f6f7f5;--panel:#fff;--ink:#141715;--ink-2:#4b524d;--ink-3:#7c847e;--line:#dfe3df;--good:#1f8a52;--bad:#b4372f;--mono:ui-monospace,Menlo,monospace;--sans:-apple-system,"Segoe UI",sans-serif}
@media (prefers-color-scheme:dark){:root{--bg:#0d0f0e;--panel:#151816;--ink:#e8ece9;--ink-2:#aab2ac;--ink-3:#737b75;--line:#262b28;--good:#5bd18e;--bad:#ef746b}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:17px/1.5 var(--sans);padding:32px 16px}
main{max-width:760px;margin:0 auto}h1{font-size:29px;margin:0 0 4px}p.sub{color:var(--ink-2);margin:0 0 24px}
form{display:flex;gap:8px}input{flex:1;font:inherit;padding:12px;border:1px solid var(--line);border-radius:3px;background:var(--panel);color:var(--ink)}
button{font:inherit;padding:12px 16px;border:1px solid var(--line);border-radius:3px;background:var(--panel);color:var(--ink);cursor:pointer}
.card{margin-top:24px;background:var(--panel);border:1px solid var(--line);border-radius:4px;padding:16px}
.meta{font:14px/1.4 var(--mono);color:var(--ink-3);display:flex;flex-wrap:wrap;gap:12px;margin-bottom:12px}.meta a{color:var(--ink-2)}
.body{white-space:pre-wrap;word-break:break-word}.ok{color:var(--good)}.no{color:var(--bad)}.k{font:14px var(--mono);word-break:break-all;color:var(--ink-2)}
</style>"#;

/// `GET /ask` — one question box; the answer is fetched from `/public/search`.
pub async fn ask_page(State(app): State<App>) -> Response {
    if app.public_rooms.is_empty() {
        return not_public();
    }
    let rooms = app.public_rooms.iter().map(|r| format!("#{}", esc(&r.name))).collect::<Vec<_>>().join(" · ");
    Html(format!(r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Ask the record</title>{STYLE}</head><body><main>
<h1>Ask the record</h1><p class="sub">One question, one answer from a signed record you can check. Rooms: {rooms}. No account.</p>
<form id="f"><input id="q" maxlength="200" placeholder="Ask one question" autofocus><button>Ask</button></form><div id="a"></div>
<script>
const f=document.getElementById('f'),a=document.getElementById('a');
f.onsubmit=async e=>{{e.preventDefault();const q=document.getElementById('q').value.trim();if(!q)return;a.innerHTML='<div class="card">Reading the record…</div>';
const r=await fetch('public/search?q='+encodeURIComponent(q));const v=await r.json().catch(()=>null);
if(!v||!v.answer){{a.innerHTML='<div class="card">Nothing in the public record answers this.</div>';return}}
const x=v.answer;const d=document.createElement('div');d.className='card';
d.innerHTML='<div class="meta"><a href="public/obs/'+encodeURIComponent(x.id)+'"></a><span></span><span></span><span></span></div><div class="body"></div>';
const m=d.querySelectorAll('.meta > *');m[0].textContent=x.id;m[1].textContent='signed by '+x.author;m[2].textContent=x.date;m[3].textContent='#'+x.room;
d.querySelector('.body').textContent=x.text;a.innerHTML='';a.appendChild(d);
history.replaceState(null,'','?q='+encodeURIComponent(q))}};
const q0=new URLSearchParams(location.search).get('q');if(q0){{document.getElementById('q').value=q0;f.requestSubmit()}}
</script></main></body></html>"#)).into_response()
}

/// `GET /public/search?q=` — the best answer from the public rooms only, or none.
#[allow(clippy::result_large_err)] // the error is the axum Response itself
pub async fn search(State(app): State<App>, Query(q): Query<AskQ>) -> Result<Json<Value>, Response> {
    if app.public_rooms.is_empty() {
        return Err(not_public());
    }
    let query: String = q.q.unwrap_or_default().chars().take(200).collect();
    if query.trim().is_empty() {
        return Ok(Json(json!({"answer": null})));
    }
    let _permit = app.public_gate.clone().try_acquire_owned().map_err(|_| (StatusCode::TOO_MANY_REQUESTS, "busy, try again").into_response())?;
    // A private room (privaterooms.rs) is never public, whatever the public list says; an unreadable table answers nothing.
    let private = app.store.private_rooms_table().map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())?;
    let rooms: Vec<PublicRoom> = app.public_rooms.iter().filter(|r| !private.contains_key(&r.name)).cloned().collect();
    let sem = app.semantic.clone();
    let store = app.store.clone();
    // Each public room is searched on its own (a small public room would never reach the top of the whole store),
    // and only what it admits is kept; the best score across rooms answers.
    let hits = tokio::task::spawn_blocking(move || {
        let mut best: Vec<crate::store::SearchHit> = vec![];
        for r in rooms.iter() {
            let (h, _) = store.search_ranked(&query, Some(&r.name), None, 100, sem.as_deref(), crate::store::Ranker::Rrf)?;
            best.extend(h.into_iter().filter(|h| r.admits_record(&h.obs)).take(1));
        }
        best.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        Ok::<_, crate::store::StoreError>(best)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
    let mut public = hits.into_iter();
    let answer = public.next().map(|h| json!({
        "id": h.obs.id, "room": h.obs.channel, "author": h.obs.author,
        "date": oracle_proto::iso_ms(h.obs.created_ms),
        // Whole, never cut (Jobs, 2026-10-10: the card stopped mid-sentence and did not say the rest was on the id).
        "text": h.obs.content,
    }));
    Ok(Json(json!({"answer": answer})))
}

fn public_obs(app: &App, id: &str) -> Result<crate::store::Observation, StatusCode> {
    let o = app.store.resolve_id(id).and_then(|full| app.store.get(&full)).map_err(|e| match e {
        StoreError::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    })?;
    if !app.public_rooms.iter().any(|r| r.admits_record(&o)) {
        return Err(StatusCode::NOT_FOUND);
    }
    // A private room is never public: its records are "not public" to a stranger, and an unreadable table shows nothing.
    match app.store.private_rooms_table() {
        Ok(t) if !t.contains_key(&o.channel) => Ok(o),
        Ok(_) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

/// `GET /public/obs/:id` — the signed record as a page: what was said, who signed it, when, and whether the signature verifies.
pub async fn obs_page(State(app): State<App>, Path(id): Path<String>) -> Response {
    let o = match public_obs(&app, &id) {
        Ok(o) => o,
        Err(code) => return (code, "not public").into_response(),
    };
    let check = match app.store.envelope_of(&o.id) {
        Ok(env) => match env.verify_stored(&o.pubkey) {
            Ok(()) => format!(r#"<span class="ok">signature verifies</span> against {}'s key"#, esc(&o.author)),
            Err(e) => format!(r#"<span class="no">signature does not verify</span>: {}"#, esc(&e.to_string())),
        },
        Err(_) => "imported history: not signed under this scheme".to_string(),
    };
    Html(format!(r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{id}</title>{STYLE}</head><body><main>
<p class="sub"><a href="../../ask">Ask the record</a></p><div class="card"><div class="meta"><span>{id}</span><span>signed by {author}</span><span>{date}</span><span>#{room}</span></div>
<div class="body">{content}</div></div>
<div class="card"><div class="meta"><span>{check}</span></div><div class="k">key {pubkey}</div><div class="k">signature {sig}</div>
<p class="sub">Check it yourself: <a href="{id}/envelope">the envelope as signed</a> (JSON), verified with Ed25519 over its canonical bytes.</p></div>
</main></body></html>"#,
        id = esc(&o.id), author = esc(&o.author), date = esc(&oracle_proto::iso_ms(o.created_ms)), room = esc(&o.channel),
        content = esc(&o.content), pubkey = esc(&o.pubkey), sig = esc(&o.sig))).into_response()
}

/// `GET /public/obs/:id/envelope` — the envelope exactly as signed, for anyone to verify without trusting this server.
pub async fn obs_envelope(State(app): State<App>, Path(id): Path<String>) -> Response {
    let o = match public_obs(&app, &id) {
        Ok(o) => o,
        Err(code) => return (code, "not public").into_response(),
    };
    match app.store.envelope_of(&o.id) {
        Ok(env) => ([(header::CONTENT_TYPE, "application/json")], serde_json::to_string_pretty(&json!({"envelope": env, "pubkey": o.pubkey})).unwrap_or_default()).into_response(),
        Err(_) => not_public(),
    }
}

/// The public rooms a stranger may read now: listed, and not private. An unreadable private-room table lists nothing.
fn open_rooms(app: &App) -> Result<Vec<PublicRoom>, StatusCode> {
    let private = app.store.private_rooms_table().map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(app.public_rooms.iter().filter(|r| !private.contains_key(&r.name)).cloned().collect())
}

/// A room's State for a stranger: its fields, only when every head revision was written after the room opened to the
/// public (a room with a private past shows no State from before its moment).
fn public_state(app: &App, r: &PublicRoom) -> Value {
    let Ok(st) = app.store.room_state_at(&r.name, None) else { return Value::Null };
    let heads: Vec<String> = st["heads"].as_array().map(|a| a.iter().filter_map(|h| h.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    if heads.is_empty() || !heads.iter().all(|id| app.store.get(id).is_ok_and(|o| r.admits(&o.channel, o.created_ms))) {
        return Value::Null;
    }
    json!({"state": st["state"], "heads": heads, "fields": st["fields"]})
}

/// One record as a room reader sees it (also the room view's messages, roomview.rs).
pub fn message_value(o: &crate::store::Observation) -> Value {
    public_message(o)
}

fn public_message(o: &crate::store::Observation) -> Value {
    json!({
        "id": o.id, "author": o.author, "kind": o.kind, "date": oracle_proto::iso_ms(o.created_ms),
        "text": o.content, "tags": o.tags,
        "cargo": o.cargo.as_deref().and_then(|c| serde_json::from_str::<Value>(c).ok()),
    })
}

/// `GET /public/rooms` — the rooms a stranger may read, each with its purpose and how much it holds since it opened.
#[allow(clippy::result_large_err)] // the error is the axum Response itself
pub async fn rooms(State(app): State<App>) -> Result<Json<Value>, Response> {
    let open = open_rooms(&app).map_err(|c| (c, "unavailable").into_response())?;
    if open.is_empty() {
        return Err(not_public());
    }
    let list: Vec<Value> = open.iter().map(|r| {
        let st = public_state(&app, r);
        let recent = app.store.recent(Some(&r.name), 500, None).unwrap_or_default();
        let admitted: Vec<&crate::store::Observation> = recent.iter().filter(|o| r.admits_record(o)).collect();
        json!({"room": r.name, "from": (r.from_ms > 0).then(|| oracle_proto::iso_ms(r.from_ms)),
               "purpose": st["fields"]["purpose"], "messages": admitted.len(), "capped": recent.len() == 500,
               "last": admitted.first().map(|o| oracle_proto::iso_ms(o.created_ms))})
    }).collect();
    Ok(Json(json!({"schema": "public-rooms/1", "rooms": list})))
}

#[derive(Deserialize)]
pub struct RoomQ {
    limit: Option<i64>,
    before: Option<i64>,
}

/// `GET /public/room/:room?limit=&before=` — one public room as a stranger sees it: its State, its claims and its latest
/// messages (newest first, whole, each with its id to open and verify). Only what was written after the room opened.
#[allow(clippy::result_large_err)] // the error is the axum Response itself
pub async fn room(State(app): State<App>, Path(room): Path<String>, Query(q): Query<RoomQ>) -> Result<Json<Value>, Response> {
    let name = room.trim_start_matches('#').to_lowercase();
    let open = open_rooms(&app).map_err(|c| (c, "unavailable").into_response())?;
    let Some(r) = open.into_iter().find(|r| r.name == name) else { return Err(not_public()) };
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let rows = app.store.recent(Some(&r.name), limit, q.before).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
    let messages: Vec<Value> = rows.iter().filter(|o| r.admits_record(o)).map(public_message).collect();
    let next = rows.last().filter(|_| rows.len() as i64 == limit).map(|o| o.seq);
    let claims: Vec<Value> = crate::claims::read(&app.store, Some(&r.name), None, None)
        .map(|c| c.claims.into_iter().filter(|c| r.admits(&c.room, c.minted_ms)).filter_map(|c| serde_json::to_value(c).ok()).collect())
        .unwrap_or_default();
    Ok(Json(json!({"schema": "public-room/1", "room": r.name, "from": (r.from_ms > 0).then(|| oracle_proto::iso_ms(r.from_ms)),
                   "state": public_state(&app, &r), "claims": claims, "messages": messages, "before": next})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_checks_are_cortex_and_talk_is_not() {
        assert!(is_cortex_tags(&["claim:check".to_string()]));
        assert!(is_cortex_tags(&["claim:edge".to_string(), "reply:OBS-x".to_string()]));
        assert!(!is_cortex_tags(&["intent:question".to_string(), "to:rocky".to_string()]));
        assert!(!is_cortex_tags(&[]));
    }
    use crate::store::{Principal, Scope, Store};
    use oracle_proto::{generate_key, pubkey_hex, Envelope, Kind};

    async fn serve(public: &[&str]) -> (String, String, String, String) {
        let dir = std::env::temp_dir().join(format!("oracle-public-test-{}", oracle_proto::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(dir.join("oracle.db").to_str().unwrap()).unwrap();
        let owner = generate_key();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        s.init_owner("blaze", &pubkey_hex(&owner), now - 1000).unwrap();
        let put = |room: &str, text: &str, tags: &[&str]| {
            let env = Envelope::sign(&owner, room, "blaze", Kind::Say, text, None, tags.iter().map(|t| t.to_string()).collect(), now);
            s.write(&Principal { handle: "blaze".into(), scope: Scope::Write }, &env, now).unwrap().id
        };
        // Longer than the old 1,200-character card: the answer must arrive whole.
        let open = put("mathematics", &format!("Channel capacity has three senses in this room: Shannon's pipe, the ledger, the loom. {} THE END", "More. ".repeat(300)), &["claim:mint"]);
        let private = put("astra", "channel capacity channel capacity: a customer's private case notes, never public.", &["claim:mint"]);
        // The room's dialogue (no cortex tag): written in a public room, never shown to a stranger.
        let talk = put("mathematics", "channel capacity channel capacity, the TALK of the room: how we argued it.", &[]);
        let mut app = App::new(s);
        app.public_rooms = std::sync::Arc::new(parse_rooms(&public.join(",")));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, crate::http::router(app)).await.unwrap() });
        (url, open, private, talk)
    }
    fn get(url: &str) -> (u16, String) {
        match ureq::get(url).call() {
            Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap_or_default()),
            Err(e) => panic!("{e}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nothing_is_public_until_the_owner_names_a_room() {
        let (url, open, _, _) = serve(&[]).await;
        for p in ["/ask".to_string(), "/public/search?q=channel".to_string(), format!("/public/obs/{open}")] {
            assert_eq!(get(&format!("{url}{p}")).0, 404, "{p}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stranger_gets_one_signed_answer_from_public_rooms_only() {
        let (url, open, private, talk) = serve(&["mathematics"]).await;
        let (c, page) = get(&format!("{url}/ask"));
        assert!(c == 200 && page.contains("get('q')"), "a link /ask?q=... asks on load");
        let (c, body) = get(&format!("{url}/public/search?q=channel%20capacity"));
        let v: Value = serde_json::from_str(&body).unwrap();
        assert!(v["answer"]["text"].as_str().unwrap().ends_with("THE END"), "whole, never cut");
        assert_eq!((c, v["answer"]["id"].as_str(), v["answer"]["room"].as_str()), (200, Some(open.as_str()), Some("mathematics")), "{v}");
        assert_eq!(v["answer"]["author"], "blaze");
        // Negative control: the private room's record exists, matches better, and is never shown or served.
        assert!(!body.contains(&private) && !body.contains("customer"));
        assert_eq!(get(&format!("{url}/public/obs/{private}")).0, 404);
        // The dialogue of a public room matches as well and is never the answer, never a page.
        assert!(!body.contains(&talk) && !body.contains("TALK"), "{body}");
        assert_eq!(get(&format!("{url}/public/obs/{talk}")).0, 404);
        let (c, page) = get(&format!("{url}/public/obs/{open}"));
        assert!(c == 200 && page.contains("signature verifies") && page.contains("signed by blaze"), "{page}");
        let (c, env) = get(&format!("{url}/public/obs/{open}/envelope"));
        assert!(c == 200 && env.contains("\"sig\""), "{env}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_public_room_reads_whole_with_its_messages_and_a_private_or_unlisted_room_does_not() {
        let (url, open, private, talk) = serve(&["mathematics"]).await;
        let (c, body) = get(&format!("{url}/public/room/mathematics"));
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((c, v["room"].as_str()), (200, Some("mathematics")), "{body}");
        assert_eq!(v["messages"][0]["id"].as_str(), Some(open.as_str()));
        assert!(v["messages"][0]["text"].as_str().unwrap().ends_with("THE END"), "whole");
        let (c, list) = get(&format!("{url}/public/rooms"));
        assert!(c == 200 && list.contains("\"mathematics\""), "{list}");
        // Controls: an unlisted room is not public, and a public room never carries another room's records.
        assert_eq!(get(&format!("{url}/public/room/blaze")).0, 404, "unlisted");
        assert!(!body.contains(&private) && !body.contains("customer"), "only its own room");
        assert!(!body.contains(&talk) && !body.contains("TALK"), "the cortex only, never the dialogue");
        assert!(list.contains("\"messages\":1"), "the count is of the cortex: {list}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_room_opened_later_shows_nothing_from_before() {
        let later = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64 + 3_600_000;
        let spec = format!("mathematics@{later}");
        let (url, open, _, _) = serve(&[spec.as_str()]).await;
        let (c, body) = get(&format!("{url}/public/room/mathematics"));
        assert_eq!(c, 200);
        assert!(!body.contains(&open) && body.contains("\"messages\":[]"), "{body}");
        assert!(body.contains("\"state\":null"));
    }

    #[test]
    fn rooms_parse_with_and_without_a_from() {
        assert_eq!(parse_rooms("#Piper@1791640000000, dot"), vec![PublicRoom { name: "piper".into(), from_ms: 1791640000000 },
                                                               PublicRoom { name: "dot".into(), from_ms: 0 }]);
        assert!(parse_rooms("piper@yesterday").is_empty(), "a from that does not parse makes nothing public");
        assert!(parse_rooms("").is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_room_with_a_private_past_is_public_only_from_its_moment() {
        // Everything in the fixture was written a moment ago; a from one hour ahead admits none of it.
        let later = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64 + 3_600_000;
        let spec = format!("mathematics@{later}");
        let (url, open, _, _) = serve(&[spec.as_str()]).await;
        assert_eq!(get(&format!("{url}/ask")).0, 200, "the room is named");
        let (_, body) = get(&format!("{url}/public/search?q=channel%20capacity"));
        assert!(!body.contains(&open) && body.contains("\"answer\":null"), "{body}");
        assert_eq!(get(&format!("{url}/public/obs/{open}")).0, 404, "a message from before the room opened is never served");
        assert_eq!(get(&format!("{url}/public/obs/{open}/envelope")).0, 404);
    }
}

/// The caller's address for the registration limits only: the first hop nginx put in X-Forwarded-For (the server listens
/// on loopback behind it), else X-Real-IP, else "unknown" (all unknown callers then share one bucket, which fails safe).
fn caller_ip(headers: &axum::http::HeaderMap) -> String {
    let pick = |h: &str| headers.get(h).and_then(|v| v.to_str().ok()).map(|s| s.split(',').next().unwrap_or("").trim().to_string()).filter(|s| !s.is_empty() && s.len() <= 64);
    pick("x-forwarded-for").or_else(|| pick("x-real-ip")).unwrap_or_else(|| "unknown".into())
}

fn store_status(e: &StoreError) -> Response {
    let (code, kind) = match e {
        StoreError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized"),
        StoreError::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
        StoreError::Rejected(_) => (StatusCode::UNPROCESSABLE_ENTITY, "rejected"),
        StoreError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
        StoreError::NotFound => return not_public(),
        StoreError::Limited { .. } => (StatusCode::TOO_MANY_REQUESTS, "limited"),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    };
    let mut res = (code, Json(json!({"ok": false, "error": kind, "detail": e.to_string()}))).into_response();
    if let StoreError::Limited { retry_after_ms, .. } = e {
        if let Ok(v) = axum::http::HeaderValue::from_str(&((retry_after_ms + 999) / 1000).max(1).to_string()) {
            res.headers_mut().insert(header::RETRY_AFTER, v);
        }
    }
    res
}

/// `GET /public/register/challenge`: step one of minting a public seat (publicseats.rs). Sign
/// `oracle-register/1\n<nonce>\n<pubkey hex>` with your ed25519 key and send it to `POST /public/register` within ten minutes.
/// 404 while no room takes public writes.
pub async fn register_challenge(State(app): State<App>) -> Response {
    let now = crate::http::now_ms();
    match app.store.public_challenge(now) {
        Ok(nonce) => Json(json!({
            "schema": "public-register-challenge/1",
            "nonce": nonce,
            "expires": oracle_proto::iso_ms(now + crate::publicseats::CHALLENGE_TTL_MS),
            "sign": "oracle-register/1\\n<nonce>\\n<pubkey hex, lowercase>",
            "write_rooms": app.store.public_config().write_rooms,
        })).into_response(),
        Err(e) => store_status(&e),
    }
}

#[derive(Deserialize)]
pub struct RegisterBody {
    pubkey: String,
    nonce: String,
    signature: String,
    name: String,
}

/// `POST /public/register` {pubkey, nonce, signature, name}: a public seat for the key's holder and a write token, shown
/// once. The seat writes only in the rooms listed; its writes pass the public limits; the same key is the same seat.
pub async fn register(State(app): State<App>, headers: axum::http::HeaderMap, body: axum::body::Bytes) -> Response {
    if body.len() > 4096 {
        return (StatusCode::PAYLOAD_TOO_LARGE, "too large").into_response();
    }
    let b: RegisterBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(_) => return store_status(&StoreError::Rejected("body is {pubkey, nonce, signature, name}".into())),
    };
    let ip = caller_ip(&headers);
    let now = crate::http::now_ms();
    match app.store.register_public(&b.pubkey, &b.nonce, &b.signature, &b.name, &ip, now) {
        Ok((seat, token, name)) => {
            let cfg = app.store.public_config();
            let l = cfg.limits;
            (StatusCode::CREATED, Json(json!({
                "schema": "public-seat/1", "seat": seat, "name": name, "token": token,
                "write_rooms": cfg.write_rooms,
                "limits": {"one_write_every_s": l.seat_spacing_ms / 1000, "writes_per_day": l.seat_per_day,
                           "bytes_per_day": l.seat_bytes_per_day, "room_public_writes_per_hour": l.room_per_hour},
                "note": "Keep the token; it is shown once. Sign each envelope with the same key, author = seat."
            }))).into_response()
        }
        Err(e) => store_status(&e),
    }
}
