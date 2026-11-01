//! A room as its reader sees it (room-view/1; Emmy's contract, 29 Oct 2026).
//!
//! `GET /rooms/:room/view` is the public room projection (public.rs: State, claims, messages) for any room the caller's own
//! seat may read, private rooms included, with the room's visibility and what the caller may do there. `GET /rooms/mine` lists
//! exactly the rooms the caller may read now. Authorization comes first and fails closed: a room the caller may not read is
//! answered exactly like a room that does not exist (404), so neither its name nor its size leaks. Every claim that
//! names a room the caller may not read is left out. State and messages are the room's own records only: what they reference
//! elsewhere (a reply:, closes: or file: tag, an id in their text) stays an opaque id, and no content of another room is ever
//! projected here; opening it is a separate request the caller's own scope decides. Answers are never cached (`Cache-Control: private, no-store`), so a
//! revocation holds on the next request. AXXIS reads with the signed-in member's own scoped credential, never an owner token.

use crate::http::{principal, App};
use crate::store::{Principal, Scope, Store, StoreError};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// What a seat may do in a room it may read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    /// `private` (a private room's member), `public` (listed public), or `studio` (a registry room).
    pub visibility: &'static str,
    pub write: bool,
}

/// May `who` read `room`, and how? None when it may not, or when the room is not known. A seat's own scope (seat-rooms/1, and
/// every public seat, seatrooms.rs and publicseats.rs) bounds everything, private rooms included; within it a private room is
/// closed to every seat that is not its member, studio seats and the owner included (privaterooms.rs); a registry seat with no
/// scope reads every room that exists. Write needs a write token, write scope where there is one, and membership where private.
/// Emmy's review of 97627eb: membership is intersected with the scope, never a way around it.
pub fn access(s: &Store, public: &[crate::public::PublicRoom], who: &Principal, room: &str) -> Result<Option<Access>, StoreError> {
    let scope = s.seat_rooms_for(&who.handle)?;
    if crate::publicseats::is_public_handle(&who.handle) && scope.is_none() {
        // Never reached while seat_rooms_for scopes every public seat; kept so a change there cannot open everything.
        return Ok(None);
    }
    let (in_scope, scope_write) = match &scope {
        Some(r) => (r.may_read(room), r.may_write(room)),
        None => (true, true),
    };
    if !in_scope {
        return Ok(None);
    }
    let write = who.scope == Scope::Write && scope_write;
    if let Some(members) = s.private_rooms_table()?.get(room) {
        return Ok(members.contains(&who.handle).then_some(Access { visibility: "private", write }));
    }
    let visibility = if public.iter().any(|r| r.name == room) { "public" } else { "studio" };
    if scope.is_some() {
        return Ok(Some(Access { visibility, write }));
    }
    let exists = s.channels()?.iter().any(|(name, ..)| name == room);
    Ok(exists.then_some(Access { visibility, write }))
}

fn private_answer(status: StatusCode, body: Value) -> Response {
    let mut res = (status, Json(body)).into_response();
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    h.insert(header::VARY, HeaderValue::from_static("Authorization"));
    res
}
fn no_such_room() -> Response {
    private_answer(StatusCode::NOT_FOUND, json!({"error": "no such room"}))
}

#[derive(Deserialize)]
pub struct ViewQ {
    limit: Option<i64>,
    before: Option<i64>,
}

/// Rooms a value names under "room" or "channel", at any depth.
fn rooms_named(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if let (true, Some(r)) = (k == "room" || k == "channel", x.as_str()) {
                    out.insert(r.to_string());
                }
                rooms_named(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| rooms_named(x, out)),
        _ => {}
    }
}

/// `GET /rooms/:room/view?limit=&before=`
pub async fn view(State(app): State<App>, headers: HeaderMap, Path(room): Path<String>, Query(q): Query<ViewQ>) -> Response {
    let Ok(who) = principal(&app, &headers, Scope::Read).await else {
        return private_answer(StatusCode::UNAUTHORIZED, json!({"error": "a bearer token is required"}));
    };
    let name = room.trim_start_matches('#').to_lowercase();
    let public = app.public_rooms.clone();
    let store = app.store.clone();
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let before = q.before;
    let out = tokio::task::spawn_blocking(move || -> Result<Option<Value>, StoreError> {
        let Some(acc) = access(&store, &public, &who, &name)? else { return Ok(None) };
        let st = store.room_state_at(&name, None)?;
        let heads: Vec<Value> = st["heads"].as_array().cloned().unwrap_or_default();
        let state = if heads.is_empty() { Value::Null } else { json!({"state": st["state"], "heads": heads, "fields": st["fields"]}) };
        let rows = store.recent(Some(&name), limit, before)?;
        let messages: Vec<Value> = rows.iter().filter(|o| o.channel == name).map(crate::public::message_value).collect();
        let next = rows.last().filter(|_| rows.len() as i64 == limit).map(|o| o.seq);
        // A claim that names another room is shown only when the caller may read that room too.
        let mut readable: std::collections::BTreeMap<String, bool> = [(name.clone(), true)].into();
        let mut claims = Vec::new();
        for c in crate::claims::read(&store, Some(&name), None, None)?.claims {
            if c.room != name {
                continue;
            }
            let Ok(v) = serde_json::to_value(&c) else { continue };
            let mut named = BTreeSet::new();
            rooms_named(&v, &mut named);
            let mut ok = true;
            for r in named {
                let may = match readable.get(&r) {
                    Some(m) => *m,
                    None => {
                        let m = access(&store, &public, &who, &r)?.is_some();
                        readable.insert(r.clone(), m);
                        m
                    }
                };
                ok &= may;
            }
            if ok {
                claims.push(v);
            }
        }
        Ok(Some(json!({
            "schema": "room-view/1", "room": name, "visibility": acc.visibility,
            "capabilities": {"read": true, "write": acc.write},
            "state": state, "claims": claims, "messages": messages, "before": next,
        })))
    })
    .await;
    match out {
        Ok(Ok(Some(v))) => private_answer(StatusCode::OK, v),
        Ok(Ok(None)) => no_such_room(),
        _ => private_answer(StatusCode::SERVICE_UNAVAILABLE, json!({"error": "room view unavailable"})),
    }
}

/// `GET /rooms/mine` — the rooms this seat may read now, each with its visibility and whether it may write there. Nothing about
/// any other room: no name, no count.
pub async fn mine(State(app): State<App>, headers: HeaderMap) -> Response {
    let Ok(who) = principal(&app, &headers, Scope::Read).await else {
        return private_answer(StatusCode::UNAUTHORIZED, json!({"error": "a bearer token is required"}));
    };
    let public = app.public_rooms.clone();
    let store = app.store.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<Value, StoreError> {
        let mut rooms = Vec::new();
        for (name, ..) in store.channels()? {
            if let Some(a) = access(&store, &public, &who, &name)? {
                rooms.push(json!({"room": name, "visibility": a.visibility, "write": a.write}));
            }
        }
        Ok(json!({"schema": "rooms-mine/1", "seat": who.handle, "rooms": rooms}))
    })
    .await;
    match out {
        Ok(Ok(v)) => private_answer(StatusCode::OK, v),
        _ => private_answer(StatusCode::SERVICE_UNAVAILABLE, json!({"error": "rooms unavailable"})),
    }
}
