//! The HTTP layer: thin, stateless, and never on the hot path of the async executor —
//! every store call runs in `spawn_blocking`, at most ORACLE_STORE_LANES (3) at a time: the store is one SQLite under
//! one lock, and calls beyond that only fight for it (19 Oct 2026, measured on a copy: six /asks in turn cost 4.8 CPU-s,
//! six at once 18.3, twelve at once 50.6, while the live process sat at 5.7 cores on one request a second).

use crate::projection;
use crate::store::{Principal, Scope, Store, StoreError};
use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use futures_util::stream::Stream;
use oracle_proto::{Envelope, KeyEvent};
use serde::Deserialize;
use serde_json::json;
use std::{collections::BTreeMap, convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

/// Re-exported so routes and tests keep `http::now_ms`; the clock itself is `oracle_proto::now_ms`.
pub use oracle_proto::now_ms;

#[derive(Clone)]
pub struct App {
    pub store: Arc<Store>,
    /// Every stored observation, for `GET /stream` (`App::publish`).
    pub events: broadcast::Sender<Arc<Live>>,
    /// Vouches for bearer tokens issued elsewhere. `None` unless configured.
    pub introspect: Option<Arc<crate::introspect::Introspector>>,
    /// Verifies the old Oracle's `ocg_` tokens here, with its secret. `None` unless configured.
    pub legacy: Option<Arc<crate::legacy::Legacy>>,
    /// The connectors' OAuth (`/oauth/*`, `/.well-known/*`), ported from the old Oracle; see `oauth.rs`.
    pub oauth: Arc<crate::oauth::OAuth>,
    /// Per-route calls, latency and callers since start (`GET /metrics/routes`).
    pub metrics: Arc<crate::metrics::Metrics>,
    /// `GET /health` row counts, with when they were taken.
    pub stats: Arc<std::sync::Mutex<Option<(i64, serde_json::Value)>>>,
    /// The world page (world.rs) and when it was composed: kept 30 s, so a room of agents waking at once costs one compose.
    pub world: Arc<std::sync::Mutex<Option<(i64, serde_json::Value)>>>,
    /// Where this server reaches itself: `POST /mcp` runs its tools through its own API.
    pub self_url: String,
    /// Keys this server holds for seats (`<dir>/<handle>.key`), so `POST /mcp` can sign for them.
    pub hosted_keys: Option<std::path::PathBuf>,
    /// Each room's folded task board, kept between `GET /cards` requests.
    pub cards: Arc<crate::cards::BoardCache>,
    /// `GET /fold` answers, one per (seat, limit, minute), kept while the store's newest sequence is unchanged: the
    /// Bridge folds every second and the ledger changes every few seconds, so most folds are the last one again
    /// (21 Oct 2026: 22,000 folds in four hours, p95 15 s, every write waiting behind them). An ack drops the seat's
    /// entries; a write moves the sequence and misses every entry.
    pub folds: Arc<crate::fold_cache::FoldCache>,
    /// `GET /asks` answers, kept per question until the log changes (asks.rs `AsksCache`).
    pub asks: Arc<crate::asks::AsksCache>,
    /// `ORACLE_CROSS_SEAT=enforce`: another seat's asks, asked and unread are refused unless the caller is a registered gateway. Anything else (the
    /// default) is audit mode: every cross-seat read is allowed and recorded with what enforcing would have done, so the flip can be checked first.
    pub cross_seat_enforce: bool,
    /// Meaning search (`ORACLE_VEC_DB`): fused with keyword search for best-match queries. `None` = keyword only.
    pub semantic: Option<Arc<crate::semantic::Semantic>>,
    /// Rooms a stranger may read on the stranger page (ORACLE_PUBLIC_ROOMS); empty means nothing is public.
    pub public_rooms: Arc<Vec<crate::public::PublicRoom>>,
    /// Public searches run a few at a time.
    pub public_gate: Arc<tokio::sync::Semaphore>,
    /// How many store calls run at once (`blocking`): ORACLE_STORE_LANES, default 3. See the header.
    pub lanes: Arc<tokio::sync::Semaphore>,
    /// How many of those lanes the heavy views (`/asks`) may hold at once: one fewer than the lanes, at least one, so a
    /// short read always finds one free (`heavy`). 27 Oct 2026, live 904b963: an /asks view cost 0.85-1.3 CPU-s on an idle
    /// copy (asks-phases), was asked ~35 times a minute, and its waiters held lanes too, so /health's p95 reached 1.3 s.
    pub heavy: Arc<tokio::sync::Semaphore>,
    /// The last searches: (milliseconds, whether meaning search answered), newest last, for `GET /health`.
    pub search_log: Arc<std::sync::Mutex<std::collections::VecDeque<(u64, bool)>>>,
}

/// Searches `GET /health` summarises.
const SEARCH_LOG: usize = 200;

/// Recent search latency and how many fell back to keyword (2026-10-10: seats saw timeouts before anyone saw a
/// number): p50, p95 and max over the last SEARCH_LOG searches, with the count where meaning search did not answer.
pub fn search_health(log: &std::collections::VecDeque<(u64, bool)>) -> serde_json::Value {
    if log.is_empty() {
        return json!({"searches": 0});
    }
    let mut ms: Vec<u64> = log.iter().map(|(m, _)| *m).collect();
    ms.sort_unstable();
    let at = |q: f64| ms[((ms.len() as f64 - 1.0) * q).round() as usize];
    json!({"searches": ms.len(), "p50_ms": at(0.5), "p95_ms": at(0.95), "max_ms": ms[ms.len() - 1],
           "keyword_fallbacks": log.iter().filter(|(_, meaning)| !meaning).count()})
}

/// One stored observation as `GET /stream` sends it: its seq (the event id) and room (the filter) beside its JSON, so a
/// subscriber filters and resumes without parsing (docs/STREAM.md).
#[derive(Debug)]
pub struct Live {
    pub seq: i64,
    pub channel: String,
    pub json: String,
}

impl App {
    /// Tell every `GET /stream` subscriber about an observation just stored.
    pub fn publish(&self, o: &crate::store::Observation) {
        let _ = self.events.send(Arc::new(Live { seq: o.seq, channel: o.channel.clone(), json: serde_json::to_string(o).unwrap_or_default() }));
    }

    pub fn new(store: Store) -> App {
        App {
            store: Arc::new(store),
            events: broadcast::channel(1024).0,
            introspect: crate::introspect::Introspector::from_env().map(Arc::new),
            legacy: crate::legacy::Legacy::from_env().map(Arc::new),
            oauth: Arc::new(crate::oauth::OAuth::from_env()),
            metrics: Arc::new(crate::metrics::Metrics::new(now_ms())),
            stats: Arc::new(std::sync::Mutex::new(None)),
            world: Arc::new(std::sync::Mutex::new(None)),
            self_url: "http://127.0.0.1:8900".into(),
            hosted_keys: None,
            cards: Arc::new(crate::cards::BoardCache::default()),
            folds: Arc::new(crate::fold_cache::FoldCache::default()),
            asks: Arc::new(crate::asks::AsksCache::default()),
            cross_seat_enforce: std::env::var("ORACLE_CROSS_SEAT").is_ok_and(|v| v == "enforce"),
            semantic: crate::semantic::Semantic::from_env(),
            public_rooms: Arc::new(crate::public::rooms_from_env()),
            public_gate: Arc::new(tokio::sync::Semaphore::new(4)),
            lanes: Arc::new(tokio::sync::Semaphore::new(store_lanes())),
            heavy: Arc::new(tokio::sync::Semaphore::new(store_lanes().saturating_sub(1).max(1))),
            search_log: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        }
    }
}

pub struct ApiError(StoreError);

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, kind) = match &self.0 {
            StoreError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized"),
            StoreError::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            StoreError::Rejected(_) => (StatusCode::UNPROCESSABLE_ENTITY, "rejected"),
            StoreError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            StoreError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            StoreError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            StoreError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
            StoreError::Limited { .. } => (StatusCode::TOO_MANY_REQUESTS, "limited"),
        };
        let mut res = (
            code,
            Json(json!({"ok": false, "error": kind, "detail": self.0.to_string()})),
        )
            .into_response();
        // A full limit says when it frees (publicseats.rs), in whole seconds.
        if let StoreError::Limited { retry_after_ms, .. } = &self.0 {
            if let Ok(v) = axum::http::HeaderValue::from_str(&((retry_after_ms + 999) / 1000).max(1).to_string()) {
                res.headers_mut().insert(axum::http::header::RETRY_AFTER, v);
            }
        }
        res
    }
}

type Api<T> = Result<T, ApiError>;

/// Bodies are parsed only AFTER the caller is authenticated, so an anonymous request
/// learns nothing about the API beyond "401".
fn parse<T: serde::de::DeserializeOwned>(body: &axum::body::Bytes) -> Api<T> {
    serde_json::from_slice(body)
        .map_err(|e| ApiError(StoreError::Rejected(format!("invalid JSON body: {e}"))))
}

/// Run a store call off the async executor.
fn store_lanes() -> usize {
    std::env::var("ORACLE_STORE_LANES").ok().and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0).unwrap_or(3)
}

/// `blocking` for a heavy view: it first waits (async, holding no lane) for one of `App::heavy`'s permits, so heavy
/// views never take every lane, and a request waiting for the same view's walk waits here, not inside a lane.
/// The permit rides into the blocking closure beside the lane (Emmy's review of 6b4f1c7): a request dropped by a
/// timeout or a closed client would otherwise free its heavy permit while its walk still runs in a lane, and repeated
/// timeouts could again fill every lane. With ORACLE_STORE_LANES=1 there is no lane to reserve; heavy views share it.
async fn heavy<T, F>(app: &App, f: F) -> Api<T>
where
    T: Send + 'static,
    F: FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
{
    let permit = app.heavy.clone().acquire_owned().await.map_err(|e| ApiError(StoreError::Internal(e.to_string())))?;
    blocking(app, move |s| {
        let _heavy = permit;
        f(s)
    })
    .await
}

async fn blocking<T, F>(app: &App, f: F) -> Api<T>
where
    T: Send + 'static,
    F: FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
{
    let store = app.store.clone();
    // The permit rides into the blocking thread and is released when the call returns.
    let permit = app
        .lanes
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| ApiError(StoreError::Internal(e.to_string())))?;
    tokio::task::spawn_blocking(move || {
        let _lane = permit;
        f(&store)
    })
    .await
    .map_err(|e| ApiError(StoreError::Internal(e.to_string())))?
    .map_err(ApiError)
}

pub(crate) async fn principal(app: &App, headers: &HeaderMap, need: Scope) -> Api<Principal> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        .ok_or(ApiError(StoreError::Unauthorized("missing bearer token")))?;
    let foreign = app.introspect.clone();
    let legacy = app.legacy.clone();
    let seen = token.clone();
    // A token this Oracle minted, else one the old Oracle minted (verified here, with its secret),
    // else whatever an introspection endpoint vouches for. Each step only for its own kind.
    let who = blocking(app, move |s| match s.authenticate(&token) {
        Err(StoreError::Unauthorized(_)) if legacy.is_some() && token.starts_with(crate::legacy::PREFIX) => legacy
            .and_then(|l| l.resolve(&token, s, now_ms()))
            .ok_or(StoreError::Unauthorized("unknown token")),
        Err(StoreError::Unauthorized(_)) if foreign.is_some() => foreign
            .and_then(|i| i.resolve(&token, s, now_ms()))
            .ok_or(StoreError::Unauthorized("unknown token")),
        other => other,
    })
    .await?;
    app.metrics.remember_handle(&seen, &who.handle);
    // View as another seat (docs/VIEW-AS.md): the owner, or an admin token, may read as any seat (that seat's brief,
    // inbox, asks and fold), never write as it. Every such request is a row in the access audit.
    let who = match view_as(headers) {
        Some(seat) => {
            let caller = who.handle.clone();
            let admin = who.scope >= Scope::Admin;
            blocking(app, move |s| s.view_as(&caller, admin, &seat, now_ms())).await?
        }
        None => who,
    };
    if who.scope < need {
        return Err(ApiError(StoreError::Forbidden(format!(
            "needs {} scope",
            need.as_str()
        ))));
    }
    Ok(who)
}

/// The header that asks to read as another seat (docs/VIEW-AS.md).
pub const VIEW_AS: &str = "x-oracle-view-as";

pub(crate) fn view_as(headers: &HeaderMap) -> Option<String> {
    headers.get(VIEW_AS).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Every (method, path) the router serves. A test fails if a route is added without listing it here,
/// so `/metrics/routes` can show endpoints nobody calls.
pub const ROUTES: &[(&str, &str)] = &[
    ("GET", "/health"), ("GET", "/whoami"), ("GET", "/terms"), ("GET", "/experiments"), ("POST", "/observations"), ("GET", "/chain/:author"), ("GET", "/chain/:author/head"), ("GET", "/search"), ("GET", "/types"), ("GET", "/threads"),
    ("GET", "/asks"), ("GET", "/delta"), ("GET", "/world"),
    ("GET", "/claims"), ("GET", "/cortex"), ("GET", "/cortex/node/:nid"), ("GET", "/cortex/entity/:name"), ("GET", "/cortex/law"), ("GET", "/minds"), ("GET", "/minds/:handle"), ("GET", "/answerable"), ("POST", "/extract"), ("POST", "/reflect"), ("GET", "/cortex/marks"), ("GET", "/cortex/cases"), ("GET", "/cortex/case/:n"), ("POST", "/cortex/drafts"), ("GET", "/graph"), ("GET", "/files"), ("PUT", "/files"), ("GET", "/files/:id"), ("GET", "/files/:id/meta"), ("GET", "/hallway"), ("GET", "/ask"), ("GET", "/public/search"), ("GET", "/public/obs/:id"), ("GET", "/public/obs/:id/envelope"), ("GET", "/public/register/challenge"),
    ("POST", "/public/register"),
    ("GET", "/public/rooms"), ("GET", "/public/room/:room"), ("GET", "/tasks"), ("GET", "/cards"), ("GET", "/rooms/:ch/messages"), ("GET", "/rooms/:ch/view"), ("GET", "/rooms/mine"), ("GET", "/rooms/:ch/state"), ("GET", "/rooms/:ch/page"), ("GET", "/rooms/:ch/map"), ("GET", "/maps"), ("GET", "/rooms/:ch/unread"), ("GET", "/asked"), ("GET", "/rooms/:ch/threads"), ("GET", "/threads/:root"), ("GET", "/observations/:id/content"), ("GET", "/observations/:id/cargo"), ("GET", "/presence"), ("GET", "/states"), ("GET", "/channels"), ("GET", "/fold"), ("POST", "/fold"), ("GET", "/keys"), ("POST", "/keys"),
    ("GET", "/stream"), ("POST", "/admin/import"), ("GET", "/sync/log"), ("GET", "/sync/keys"), ("POST", "/sync/push"), ("GET", "/sync/status"), ("POST", "/admin/rooms"), ("GET", "/admin/rooms"), ("GET", "/metrics/routes"),
    ("POST", "/mcp"), ("GET", "/mcp"), ("DELETE", "/mcp"), ("POST", "/mcp/"), ("GET", "/mcp/"), ("DELETE", "/mcp/"),
    ("GET", "/.well-known/oauth-authorization-server"), ("GET", "/.well-known/oauth-protected-resource"), ("GET", "/.well-known/oauth-protected-resource/*path"),
    ("POST", "/oauth/register"), ("GET", "/oauth/authorize"), ("POST", "/oauth/authorize"), ("POST", "/oauth/token"), ("POST", "/oauth/revoke"),
];

async fn route_metrics(State(app): State<App>, headers: HeaderMap) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let mut v = app.metrics.snapshot(ROUTES, now_ms());
    v["fold_cache_hits"] = json!(app.folds.hits());
    v["fold_cache_bytes"] = json!(app.folds.bytes());
    Ok(Json(v))
}

// ── The API: one door per job (2026-09-22) ───────────────────────────────────────────────────────
// Fifteen endpoints. Anything specific is a parameter, never another path: one write
// (`POST /observations`), one read of messages (`GET /search`), one read of threads, of state, of
// keys, of the fold. The handlers these dispatch to hold the logic; the doors below only choose.

#[derive(Deserialize)]
struct SearchQ {
    /// One observation by id (add `include=envelope,source` for the signed envelope / legacy sources).
    id: Option<String>,
    include: Option<String>,
    /// Ranked full-text search.
    q: Option<String>,
    /// A worldline: comma-separated names of one thing, followed through `channel`.
    about: Option<String>,
    channel: Option<String>,
    kind: Option<String>,
    ranker: Option<String>,
    approved: Option<String>,
    recency: Option<f64>,
    rerank: Option<String>,
    as_of: Option<String>,
    tag: Option<String>,
    limit: Option<i64>,
    before: Option<i64>,
    starts: Option<String>,
    state: Option<String>,
    keep: Option<String>,
    /// With `q=`: any of these (author, class, before, order, match, include_cargo, format=page) asks for the paged item shape instead of the legacy `hits`.
    author: Option<String>,
    class: Option<String>,
    order: Option<String>,
    #[serde(rename = "match")]
    match_: Option<String>,
    include_cargo: Option<bool>,
    format: Option<String>,
    /// With `q=`: `in=text` matches the words in a message's text only, not in its tags (a record whose links name an
    /// #openfab message is not a match for "openfab"). Default `all`, as before.
    #[serde(rename = "in")]
    in_: Option<String>,
    /// With `q=`: search the archived rooms and expired records too (docs/ARCHIVE.md).
    include_archived: Option<bool>,
    /// With `order=rank` or `order=date`: skip this many matches (paging past the first page).
    offset: Option<i64>,
    /// With `q=`: also `rooms`, every room with a match, its count and first/last date (`roomread::rooms_of`).
    by_room: Option<bool>,
    /// Reading #cortex: list its links, verdicts and concept nodes too (folded by default).
    scaffold: Option<bool>,
}

fn missing(what: &str) -> ApiError {
    ApiError(StoreError::Rejected(what.to_string()))
}

#[derive(Deserialize)]
struct TypesQ {
    /// A type address or its prefix (`oracle.decision`); empty lists every type.
    prefix: Option<String>,
    limit: Option<usize>,
}

/// `GET /types?prefix=` — the records under a type address, newest first, retypes applied, with the count per type
/// (plan.oracle.search T7). A private room the caller is not a member of is left out, never named.
async fn types_any(State(app): State<App>, headers: HeaderMap, Query(q): Query<TypesQ>) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Read).await?;
    let prefix = q.prefix.unwrap_or_default().trim().to_string();
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    Ok(Json(blocking(&app, move |s| {
        let hidden = crate::privaterooms::hidden_for(&s.private_rooms_table()?, &who.handle);
        crate::typeaddr::typed(s, &prefix, &hidden, limit)
    })
    .await?))
}

/// `GET /search` — every read of messages. `id=` one observation; `q=` ranked full text; `about=`
/// one thing through a room; otherwise newest first, narrowed by channel, tag, kind of state, prefix.
async fn search_any(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<SearchQ>,
) -> Api<Json<serde_json::Value>> {
    if let Some(id) = q.id {
        // A record's code is resolved once, so every part (envelope, source) is read for the same full id.
        let id = blocking(&app, move |s| s.resolve_id(&id)).await?;
        let Json(mut out) = observation(State(app.clone()), headers.clone(), Path(id.clone())).await?;
        for part in q.include.unwrap_or_default().split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let Json(extra) = match part {
                "envelope" => envelope(State(app.clone()), headers.clone(), Path(id.clone())).await?,
                "source" => legacy_source(State(app.clone()), headers.clone(), Path(id.clone())).await?,
                other => return Err(missing(&format!("include={other}: use envelope or source"))),
            };
            if let (Some(o), Some(e)) = (out.as_object_mut(), extra.as_object()) {
                for (k, v) in e {
                    o.insert(k.clone(), v.clone());
                }
            }
        }
        return Ok(Json(out));
    }
    if let Some(query) = q.q {
        let paged = q.author.is_some() || q.class.is_some() || q.order.is_some() || q.match_.is_some() || q.include_cargo.is_some() || q.before.is_some() || q.in_.is_some() || q.offset.is_some() || q.by_room.is_some() || q.format.as_deref() == Some("page");
        if paged {
            principal(&app, &headers, Scope::Read).await?;
            let sq = crate::roomread::SearchQ {
                q: query, room: q.channel, author: q.author.filter(|a| !a.is_empty()), before: q.before, limit: q.limit, order: q.order,
                class: q.class.map(|s| s.split(',').map(|x| x.trim().to_lowercase()).filter(|x| !x.is_empty()).collect()).unwrap_or_default(),
                any: match q.match_.as_deref() { None | Some("all") => false, Some("any") => true, Some(m) => return Err(missing(&format!("match must be all or any, not {m}"))) },
                include_cargo: q.include_cargo.unwrap_or(false),
                text_only: match q.in_.as_deref() { None | Some("all") => false, Some("text") => true, Some(m) => return Err(missing(&format!("in must be text or all, not {m}"))) },
                offset: q.offset,
                by_room: q.by_room.unwrap_or(false),
            };
            let sem = app.semantic.clone();
            return Ok(Json(blocking(&app, move |s| crate::roomread::search_with(s, &sq, sem.as_deref())).await?));
        }
        let body = json!({"query": query, "channel": q.channel, "kind": q.kind, "limit": q.limit, "ranker": q.ranker, "approved": q.approved, "recency": q.recency, "rerank": q.rerank, "as_of": q.as_of, "include_archived": q.include_archived});
        return search(State(app), headers, axum::body::Bytes::from(body.to_string())).await;
    }
    if let Some(terms) = q.about {
        let channel = q.channel.ok_or_else(|| missing("about= needs channel="))?;
        return about(State(app), headers, Query(AboutQ { channel, terms, limit: q.limit })).await;
    }
    recent(
        State(app),
        headers,
        Query(RecentQ { channel: q.channel, limit: q.limit, before: q.before, tag: q.tag, starts: q.starts, state: q.state, keep: q.keep, scaffold: q.scaffold }),
    )
    .await
}

#[derive(Deserialize)]
struct ThreadsAnyQ {
    /// One whole thread, found from any message in it.
    id: Option<String>,
    /// `digest`: the room in a few hundred tokens.
    view: Option<String>,
    channel: Option<String>,
    roots: Option<String>,
    limit: Option<i64>,
    before: Option<i64>,
    expand: Option<bool>,
    scope: Option<String>,
    filter: Option<String>,
    status: Option<String>,
    participant: Option<String>,
    days: Option<i64>,
    offset: Option<i64>,
}

/// `GET /threads` — a room as threads (`channel`, `roots`, filters), its digest (`view=digest`),
/// or one thread (`id=`).
async fn threads_any(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<ThreadsAnyQ>,
) -> Api<Json<serde_json::Value>> {
    if let Some(id) = q.id {
        return thread(State(app), headers, Query(ThreadQ { id })).await;
    }
    let channel = q.channel.ok_or_else(|| missing("threads need channel= (or id=)"))?;
    let roots = q.roots.ok_or_else(|| missing("threads need roots= (or id=)"))?;
    if q.view.as_deref() == Some("digest") {
        return digest(State(app), headers, Query(DigestQ { channel, roots })).await;
    }
    threads(
        State(app),
        headers,
        Query(ThreadsQ {
            channel, roots, limit: q.limit, before: q.before, expand: q.expand, scope: q.scope,
            filter: q.filter, status: q.status, participant: q.participant, days: q.days, offset: q.offset,
        }),
    )
    .await
}

#[derive(Deserialize)]
struct AsksQ {
    seat: Option<String>,
    state: Option<String>,
    room: Option<String>,
    /// `count=1`: the lists' lengths only, no rows (`asks::counts`), for the footer every ox command ends with.
    count: Option<String>,
}

#[derive(Deserialize)]
struct HallwayQ {
    days: Option<i64>,
}

/// `GET /hallway?days=` — every room active in the last `days` (default 14): its State's current and next lines and its oldest open ask.
async fn hallway_read(State(app): State<App>, headers: HeaderMap, Query(q): Query<HallwayQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let days = q.days.unwrap_or(14);
    Ok(Json(blocking(&app, move |s| crate::hallway::read(s, days, now_ms())).await?))
}

#[derive(Deserialize)]
struct ClaimsQ {
    room: Option<String>,
    status: Option<String>,
    cid: Option<String>,
}

/// `GET /claims?room=&status=&cid=` — the cortex: every claim folded from its signed mint, edge and status messages.
async fn claims_read(State(app): State<App>, headers: HeaderMap, Query(q): Query<ClaimsQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let out = blocking(&app, move |s| crate::claims::read(s, q.room.as_deref(), q.status.as_deref(), q.cid.as_deref())).await?;
    let mut v = serde_json::to_value(&out).map_err(|e| ApiError(StoreError::Internal(e.to_string())))?;
    v["count"] = json!(out.claims.len());
    Ok(Json(v))
}

#[derive(Deserialize)]
struct CortexQ {
    room: Option<String>,
    kind: Option<String>,
    q: Option<String>,
    source: Option<String>,
    /// marks: only those with no verdict yet.
    pending: Option<String>,
    /// nodes: every node, drafts included, each with its approval class (the default is the Cortex: what Blaze approved).
    all: Option<String>,
    /// nodes: at most this many, in the list's own order (oracle_mcp::cortex_keep_top).
    top: Option<usize>,
    /// nodes: only the counts (schema cortex-count/1, cortex::counts), no rows.
    count: Option<String>,
}

/// `GET /cortex?room=&kind=&q=&all=&top=` — the Cortex's nodes (what Blaze approved; `all=1` every node with its approval class)
/// with their live marks counted (docs/CORTEX.md); `top=n` keeps the first n of the ranked list and says how many it cut;
/// `count=1` answers only the counts the brief prints (cortex::counts).
async fn cortex_nodes(State(app): State<App>, headers: HeaderMap, Query(q): Query<CortexQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    if q.count.as_deref().is_some_and(|p| p == "1" || p == "true") {
        return Ok(Json(blocking(&app, crate::cortex::counts).await?));
    }
    let top = q.top;
    let mut v = blocking(&app, move |s| crate::cortex::nodes(s, q.room.as_deref(), q.kind.as_deref(), q.q.as_deref(), q.all.as_deref().is_some_and(|p| p == "1" || p == "true"))).await?;
    if let Some(n) = top {
        oracle_mcp::cortex_keep_top(&mut v, n);
    }
    Ok(Json(v))
}

/// `GET /minds` — every mind ranked by how much it has said (minds::list).
async fn minds_list(State(app): State<App>, headers: HeaderMap) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    Ok(Json(blocking(&app, crate::minds::list).await?))
}

#[derive(Deserialize)]
struct MindQ {
    before: Option<i64>,
    limit: Option<usize>,
}

/// `GET /minds/:handle?before=&limit=` — one mind's statements, newest first, under the reader's visibility (minds::statements).
async fn mind_statements(State(app): State<App>, headers: HeaderMap, Path(handle): Path<String>, Query(q): Query<MindQ>) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Read).await?;
    Ok(Json(blocking(&app, move |s| crate::minds::statements(s, &who.handle, &handle, q.before, q.limit)).await?))
}

#[derive(Deserialize)]
struct AnswerableQ {
    q: String,
    room: Option<String>,
}

/// `GET /answerable?q=&room=` — whether Oracle can answer a question before anyone recalls it (answerable::check): yes, partial
/// or no, the words on record and not, and what the owner approved. The caller's hidden rooms are left out of every count.
async fn answerable(State(app): State<App>, headers: HeaderMap, Query(q): Query<AnswerableQ>) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Read).await?;
    Ok(Json(blocking(&app, move |s| {
        let hidden = crate::privaterooms::hidden_for(&s.private_rooms_table()?, &who.handle);
        crate::answerable::check(s, &q.q, q.room.as_deref(), &hidden)
    }).await?))
}

#[derive(Deserialize)]
struct ExtractBody {
    text: String,
}

/// `POST /extract` — the decisions, lessons, preferences and commitments in a session's text (extract.rs, docs/CAPTURE.md).
/// It stores nothing: the seat signs what it keeps. A seat that can write may ask; 503 when no model is configured.
async fn extract(State(app): State<App>, headers: HeaderMap, body: axum::body::Bytes) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Write).await?;
    let b: ExtractBody = parse(&body)?;
    let ex = crate::extract::configured()
        .ok_or_else(|| ApiError(StoreError::Unavailable("no extraction model is configured here (ORACLE_EXTRACT_URL)".into())))?;
    let out = tokio::task::spawn_blocking(move || ex.extract(&b.text))
        .await
        .map_err(|e| ApiError(StoreError::Internal(e.to_string())))?
        .map_err(|e| ApiError(StoreError::Unavailable(e)))?;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct ReflectBody {
    memories: Vec<serde_json::Value>,
    #[serde(default)]
    previous: Vec<serde_json::Value>,
}

/// `POST /reflect` — a mind's recent memories and current observations in, refined observations out (extract.rs reflect,
/// docs/REFLECT.md). Stores nothing: the mind signs what it keeps (`ox reflect`). Write scope; 503 with no model.
async fn reflect(State(app): State<App>, headers: HeaderMap, body: axum::body::Bytes) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Write).await?;
    let b: ReflectBody = parse(&body)?;
    let ex = crate::extract::configured()
        .ok_or_else(|| ApiError(StoreError::Unavailable("no model is configured here (ORACLE_EXTRACT_URL)".into())))?;
    let out = tokio::task::spawn_blocking(move || ex.reflect(&b.memories, &b.previous))
        .await
        .map_err(|e| ApiError(StoreError::Internal(e.to_string())))?
        .map_err(|e| ApiError(StoreError::Unavailable(e)))?;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct LawQ {
    top: Option<usize>,
}

/// `GET /cortex/law?top=` — what Blaze approved as Rule, Profile or Preference, one line each, most linked first (cortex::law);
/// every seat's brief reads it.
async fn cortex_law(State(app): State<App>, headers: HeaderMap, Query(q): Query<LawQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    Ok(Json(blocking(&app, move |s| crate::cortex::law(s, q.top)).await?))
}

/// `GET /cortex/entity/:name` — a sourced, deterministic primer under the caller's visibility.
async fn entity_brief(State(app): State<App>, headers: HeaderMap, Path(name): Path<String>) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Read).await?;
    Ok(Json(heavy(&app, move |s| crate::entity::brief(s, &name, &who.handle)).await?))
}

async fn cortex_node(State(app): State<App>, headers: HeaderMap, Path(nid): Path<String>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    match blocking(&app, move |s| crate::cortex::node(s, &nid)).await? {
        Some(v) => Ok(Json(v)),
        None => Err(ApiError(StoreError::NotFound)),
    }
}

/// `GET /cortex/marks?room=&source=` — the marks on one source message.
async fn cortex_marks(State(app): State<App>, headers: HeaderMap, Query(q): Query<CortexQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let pending = q.pending.as_deref().is_some_and(|p| p == "1" || p == "true");
    Ok(Json(blocking(&app, move |s| crate::cortex::marks(s, q.room.as_deref(), q.source.as_deref(), pending)).await?))
}

/// Brain cases through the cortex door (Blaze, 28 Oct 2026: "give Noor the tools to read brain cases and draft marks"; "approval will
/// be by me"; oracle-mcp cases.rs). The chats live in the Time Machine's cases store on this host (`ORACLE_CASES_URL`, default
/// http://127.0.0.1:8901); they are Blaze's own words, so only the seats in `ORACLE_CASES_SEATS` (comma separated; unset = none) read
/// them or draft. A draft is written as the authenticated seat; nothing here approves.
fn cases_seats() -> Vec<String> {
    std::env::var("ORACLE_CASES_SEATS").unwrap_or_default().split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect()
}

async fn cases_gate(app: &App, headers: &HeaderMap, need: Scope) -> Api<Principal> {
    // Brain cases are read and drafted as yourself: principal() applies x-oracle-view-as, so an unlisted admin viewing as a listed
    // seat would pass the list (Emmy's review of 9a9c31b). View-as is refused here, before anyone is resolved.
    if headers.contains_key("x-oracle-view-as") {
        return Err(ApiError(StoreError::Forbidden("brain cases are read as yourself; view-as is refused here".into())));
    }
    let who = principal(app, headers, need).await?;
    let seats = cases_seats();
    if !seats.contains(&who.handle.to_lowercase()) {
        return Err(ApiError(StoreError::Forbidden(if seats.is_empty() { "brain cases are open to no seat here (ORACLE_CASES_SEATS)".into() } else { format!("brain cases are open to {}", seats.join(", ")) })));
    }
    Ok(who)
}

async fn cases_call(path: String, body: Option<serde_json::Value>) -> Api<serde_json::Value> {
    let base = std::env::var("ORACLE_CASES_URL").unwrap_or_else(|_| "http://127.0.0.1:8901".into());
    let r = tokio::task::spawn_blocking(move || {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).build();
        let url = format!("{base}{path}");
        let sent = match body {
            Some(b) => agent.post(&url).send_json(b),
            None => agent.get(&url).call(),
        };
        match sent {
            Ok(resp) => resp.into_json::<serde_json::Value>().map_err(|e| StoreError::Unavailable(format!("brain cases: {e}"))),
            Err(ureq::Error::Status(code, resp)) => {
                let why = resp.into_json::<serde_json::Value>().ok().and_then(|v| v["error"].as_str().map(str::to_owned)).unwrap_or_else(|| format!("HTTP {code}"));
                Err(match code {
                    404 => StoreError::NotFound,
                    403 => StoreError::Forbidden(why),
                    _ => StoreError::Rejected(why),
                })
            }
            Err(e) => Err(StoreError::Unavailable(format!("brain cases: {e}"))),
        }
    })
    .await
    .map_err(|e| ApiError(StoreError::Internal(e.to_string())))?;
    r.map_err(ApiError)
}

#[derive(Deserialize)]
struct CasesQ {
    state: Option<String>,
    offset: Option<u64>,
    limit: Option<u64>,
}

/// `GET /cortex/cases?state=&offset=&limit=` — the chats of Brain cases, as the cases page lists them.
async fn cortex_cases(State(app): State<App>, headers: HeaderMap, Query(q): Query<CasesQ>) -> Api<Json<serde_json::Value>> {
    cases_gate(&app, &headers, Scope::Read).await?;
    let mut path = format!("/api/sessions?limit={}&offset={}", q.limit.unwrap_or(50).clamp(1, 500), q.offset.unwrap_or(0));
    if let Some(s) = q.state.filter(|s| !s.is_empty() && s.len() < 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-' || c == '_')) {
        path.push_str(&format!("&state={}", s.replace(' ', "%20")));
    }
    Ok(Json(cases_call(path, None).await?))
}

/// `GET /cortex/case/<n>` — one chat whole: every prompt and reply, and every live mark with its review, drafts included.
async fn cortex_case(State(app): State<App>, headers: HeaderMap, Path(n): Path<String>) -> Api<Json<serde_json::Value>> {
    cases_gate(&app, &headers, Scope::Read).await?;
    if n.is_empty() || n.len() > 64 || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(ApiError(StoreError::Rejected("a chat is its number or its conversation id".into())));
    }
    Ok(Json(cases_call(format!("/api/sessions/{n}"), None).await?))
}

/// `POST /cortex/drafts` — a draft mark written as the calling seat (the cases store checks the five fields, the links, and that a
/// replaced mark is an unreviewed draft). It waits for Blaze's verdict in his page.
async fn cortex_draft(State(app): State<App>, headers: HeaderMap, Json(mut body): Json<serde_json::Value>) -> Api<Json<serde_json::Value>> {
    let who = cases_gate(&app, &headers, Scope::Write).await?;
    if !body.is_object() {
        return Err(ApiError(StoreError::Rejected("a draft is a JSON object".into())));
    }
    body["seat"] = json!(who.handle);
    Ok(Json(cases_call("/api/seat-marks".into(), Some(body)).await?))
}

#[derive(Deserialize)]
struct GraphQ {
    view: Option<String>,
    min: Option<i64>,
    marks: Option<String>,
    top: Option<i64>,
}

/// `GET /graph?view=core|cortex|seo&min=&marks=approved|all&top=` — the graph of Oracle (docs/GRAPH.md): rooms and seats, or the
/// cortex; `marks` (default approved) says whether drafts, marks without Blaze's verdict, count and join.
async fn graph_read(State(app): State<App>, headers: HeaderMap, Query(q): Query<GraphQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let view = q.view.unwrap_or_else(|| "cortex".into());
    if !crate::graph::VIEWS.contains(&view.as_str()) {
        return Err(ApiError(StoreError::Rejected("view is core, cortex or seo".into())));
    }
    let min = q.min.unwrap_or(3).clamp(1, 1_000_000);
    let marks = q.marks.unwrap_or_else(|| "approved".into());
    if marks != "approved" && marks != "all" {
        return Err(ApiError(StoreError::Rejected("marks is approved or all".into())));
    }
    let approved_only = marks == "approved";
    if view == "seo" {
        // The words come from the hourly terms file, not the store (docs/TERMS.md).
        let path = std::env::var("ORACLE_TERMS_FILE").unwrap_or_else(|_| "/var/lib/oracle-terms/terms.json".into());
        let top = q.top.unwrap_or(120).clamp(1, 500) as usize;
        let text = tokio::task::spawn_blocking(move || std::fs::read_to_string(&path)).await.map_err(|e| ApiError(StoreError::Internal(e.to_string())))?.map_err(|e| ApiError(StoreError::Internal(format!("the word ranking is not there yet: {e}"))))?;
        let terms: serde_json::Value = serde_json::from_str(&text).map_err(|e| ApiError(StoreError::Internal(format!("terms file: {e}"))))?;
        return Ok(Json(crate::graph::seo(&terms, top)));
    }
    Ok(Json(blocking(&app, move |s| if view == "core" { crate::graph::core(s, min) } else { crate::graph::cortex(s, approved_only) }).await?))
}

#[derive(Deserialize)]
struct FilePutQ {
    room: String,
    name: Option<String>,
}

/// `PUT /files?room=&name=` — bytes in, `sha256:<hex>` out (docs/FILES.md). The body is the file; `Content-Type` is kept.
async fn file_put(State(app): State<App>, headers: HeaderMap, Query(q): Query<FilePutQ>, body: axum::body::Bytes) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Write).await?;
    let ctype = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
    let now = now_ms();
    let out = blocking(&app, move |s| crate::files::put(s, &who.handle, &q.room, q.name.as_deref(), &ctype, &body, now)).await?;
    Ok(Json(serde_json::to_value(&out).map_err(|e| ApiError(StoreError::Internal(e.to_string())))?))
}

/// `GET /files/{id}` — the bytes, under the same grant as a room read. Served sandboxed: a file is never a page of this origin.
async fn file_get(State(app): State<App>, headers: HeaderMap, Path(id): Path<String>) -> Api<Response> {
    principal(&app, &headers, Scope::Read).await?;
    let (bytes, ctype) = blocking(&app, move |s| crate::files::bytes(s, &id)).await?;
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes));
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, ctype),
            (header::HeaderName::from_static("x-content-sha256"), sha),
            (header::CACHE_CONTROL, "private, max-age=31536000, immutable".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_SECURITY_POLICY, "sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'".to_string()),
        ],
        bytes,
    )
        .into_response())
}

/// `GET /files/{id}/meta` — size, type, the rooms it is in, the messages that cite it (`file:<hex>`).
async fn file_meta(State(app): State<App>, headers: HeaderMap, Path(id): Path<String>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let out = blocking(&app, move |s| crate::files::meta(s, &id)).await?;
    Ok(Json(serde_json::to_value(&out).map_err(|e| ApiError(StoreError::Internal(e.to_string())))?))
}

#[derive(Deserialize)]
struct FilesQ {
    room: String,
    limit: Option<i64>,
}

/// `GET /files?room=&limit=` — a room's files, newest first.
async fn files_list(State(app): State<App>, headers: HeaderMap, Query(q): Query<FilesQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let room = q.room.clone();
    let out = blocking(&app, move |s| crate::files::list(s, &q.room, q.limit.unwrap_or(50))).await?;
    Ok(Json(json!({"room": room, "count": out.len(), "files": out})))
}

/// `GET /asks` — a seat's three lists (waiting on me, I'm waiting on, done today), read out of the
/// messages: `seat` (default: the token's), `room` (default: every room), `state=open|done|all`; `count=1` answers the
/// lists' lengths and no rows (`asks::counts`).
/// Which seats are running (docs/DESIGN.md, "Presence"): every registered seat, live · idle · gone · retired, from
/// each seat's last authenticated request to this server. Nothing is written; a restart starts from nobody seen.
async fn presence(State(app): State<App>, headers: HeaderMap) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let keys = blocking(&app, |s| s.keys()).await?;
    let seats = crate::metrics::seat_rows(&keys);
    Ok(Json(app.metrics.presence(&seats, now_ms())))
}

async fn asks(State(app): State<App>, headers: HeaderMap, Query(q): Query<AsksQ>) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Read).await?;
    let filter = match q.state.as_deref() {
        None => crate::asks::StateFilter::All,
        Some(s) => crate::asks::StateFilter::parse(s).ok_or_else(|| missing("state must be open, done or all"))?,
    };
    let seat = q.seat.unwrap_or_else(|| who.handle.clone());
    let (room, now) = (q.room, now_ms());
    let (caller, enforce, target) = (who.handle.clone(), app.cross_seat_enforce, seat.clone());
    blocking(&app, move |s| s.cross_seat(&caller, &target, "asks", enforce, now)).await?;
    let cache = app.asks.clone();
    let out = heavy(&app, move |s| cache.get(s, &seat, room.as_deref(), filter, now)).await?;
    // The count answer is read from the same kept answer, so it costs no second walk and never disagrees with the lists.
    let mut v = match q.count.as_deref() {
        None | Some("0" | "false") => (*out).clone(),
        Some("1" | "true") => crate::asks::counts(&out),
        Some(_) => return Err(missing("count must be 1 or 0")),
    };
    v["now_ms"] = json!(now);
    v["now"] = json!(oracle_proto::iso_ms(now));
    Ok(Json(v))
}

/// `GET /world` — WORLD STATE (schema world/1, world.rs): six bounded sections composed from signed heads, every line
/// with its record id. Kept 30 s; the private-scope layer removes per reader what it may not see, and a scoped seat is
/// refused by the seat-rooms guard (the route is not on its list).
async fn world(State(app): State<App>, headers: HeaderMap) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let now = now_ms();
    if let Some((_, v)) = app.world.lock().ok().and_then(|c| c.clone()).filter(|(at, _)| now - at < 30_000) {
        return Ok(Json(v));
    }
    let v = heavy(&app, move |s| crate::world::compose(s, now)).await?;
    if let Ok(mut c) = app.world.lock() {
        *c = Some((now, v.clone()));
    }
    Ok(Json(v))
}

#[derive(Deserialize)]
struct DeltaQ {
    since: Option<String>,
}

/// `GET /delta?since=` — what changed for the token's seat since a point (delta.rs; Blaze, 1 Nov 2026): `since` an OBS id
/// or code, a time (`2026-10-31T08:00`, UTC) or a span (`6h`, `2d`); absent, the seat's last RESUME. Counts over seq
/// ranges and a few head lines; the asks half is one walk of the asks view, so it runs on a heavy lane.
async fn delta(State(app): State<App>, headers: HeaderMap, Query(q): Query<DeltaQ>) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Read).await?;
    let (public, started, now) = (app.public_rooms.clone(), app.metrics.started_ms(), now_ms());
    let out = heavy(&app, move |s| {
        let since = crate::delta::resolve_since(s, &who.handle, q.since.as_deref(), now)?;
        crate::delta::delta(s, &who, &public, &since, started, now)
    })
    .await?;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct TasksQ {
    room: String,
    seat: Option<String>,
    state: Option<String>,
    /// Exclusive seq cursor, the same as `/rooms/:ch/state`; absent = now.
    as_of_seq: Option<i64>,
}

/// `GET /tasks` — a room's tasks derived from its messages (docs/ASKS-AND-EVENTS.md, "Tasks"): one row per `subject:`,
/// its obligations, revisions, checks and verdicts, each naming the messages that set it. `room` (required), `seat` (a
/// holder or asker), `state=open|done|cancelled|unrouted|all` (default open). Shadow: read-only, beside `/cards`.
async fn tasks(State(app): State<App>, headers: HeaderMap, Query(q): Query<TasksQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let state = q.state.unwrap_or_else(|| "open".into());
    if !matches!(state.as_str(), "open" | "done" | "cancelled" | "unrouted" | "all") {
        return Err(missing("state must be open, done, cancelled, unrouted or all"));
    }
    let now = now_ms();
    if matches!(q.as_of_seq, Some(n) if n < 1) {
        return Err(missing("as_of_seq must be a positive exclusive seq cursor"));
    }
    let out = blocking(&app, move |s| crate::tasks::tasks_in(s, &q.room, q.seat.as_deref(), &state, q.as_of_seq)).await?;
    let mut v = serde_json::to_value(&out).map_err(|e| ApiError(StoreError::Internal(e.to_string())))?;
    v["now_ms"] = json!(now);
    v["now"] = json!(oracle_proto::iso_ms(now));
    Ok(Json(v))
}

#[derive(Deserialize)]
struct CardsQ {
    owner: Option<String>,
    room: Option<String>,
    /// Comma list of declared statuses; `open` is everything not done.
    status: Option<String>,
    /// Cards in review or blocked whose `unblock` line names this seat.
    waiting_on: Option<String>,
    limit: Option<usize>,
}

/// `GET /cards` — the global task board: every room's `room-task/1` and `room-task-event/1` messages folded into
/// current cards by the same rules the web board applies (docs/DESIGN.md, "Cards"). Filters: `owner`, `room`,
/// `status=review,blocked|open`, `waiting_on=<seat>`, `limit`. Read-only; nothing is written.
async fn cards(State(app): State<App>, headers: HeaderMap, Query(q): Query<CardsQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let status = match q.status.as_deref() {
        None | Some("") => None,
        Some(list) => {
            let v: Vec<String> = list.split(',').map(|x| x.trim().to_lowercase()).filter(|x| !x.is_empty()).collect();
            if let Some(bad) = v.iter().find(|x| !crate::cards::STATUSES.contains(&x.as_str()) && x.as_str() != "open") {
                return Err(missing(&format!("unknown status {bad}; use {} or open", crate::cards::STATUSES.join(", "))));
            }
            Some(v)
        }
    };
    let filter = crate::cards::CardFilter { owner: q.owner, room: q.room, status, waiting_on: q.waiting_on, limit: q.limit.unwrap_or(0) };
    let now = now_ms();
    let cache = app.cards.clone();
    let out = blocking(&app, move |s| crate::cards::cards_cached(s, &filter, &cache)).await?;
    let mut v = serde_json::to_value(&out).map_err(|e| ApiError(StoreError::Internal(e.to_string())))?;
    v["now_ms"] = json!(now);
    v["now"] = json!(oracle_proto::iso_ms(now));
    Ok(Json(v))
}

#[derive(Deserialize)]
struct RoomPageQ {
    before: Option<i64>,
    after: Option<i64>,
    limit: Option<i64>,
    /// Comma list of classes (`message,card`); none means every class, history included.
    class: Option<String>,
    author: Option<String>,
    /// Comma list of thread states, for the threads list.
    state: Option<String>,
    include_cargo: Option<bool>,
    /// #cortex: list the graph's links, verdicts and concept nodes too.
    scaffold: Option<bool>,
}

impl RoomPageQ {
    fn page(self) -> crate::roomread::PageQ {
        let list = |v: Option<String>| v.map(|s| s.split(',').map(|x| x.trim().to_lowercase()).filter(|x| !x.is_empty()).collect()).unwrap_or_default();
        crate::roomread::PageQ {
            before: self.before, after: self.after, limit: self.limit, class: list(self.class), author: self.author.filter(|a| !a.is_empty()),
            state: list(self.state), include_cargo: self.include_cargo.unwrap_or(false), scaffold: self.scaffold.unwrap_or(false),
        }
    }
}

/// The canonical cursor is exclusive; `as_of` is an explicit compatibility alias.
#[derive(Deserialize)]
struct StateQ {
    as_of_seq: Option<i64>,
    as_of: Option<i64>,
}

impl StateQ {
    fn cursor(self) -> Api<Option<i64>> {
        if self.as_of_seq.is_some() && self.as_of.is_some() && self.as_of_seq != self.as_of {
            return Err(missing("as_of_seq and as_of name different cursors"));
        }
        let cursor = self.as_of_seq.or(self.as_of);
        if cursor.is_some_and(|c| c < 1) {
            return Err(missing("as_of_seq must be a positive exclusive seq cursor"));
        }
        Ok(cursor)
    }
}

/// One authoritative snapshot, resolved by the same function as the write guard.
async fn room_state(State(app): State<App>, headers: HeaderMap, Path(ch): Path<String>, Query(q): Query<StateQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let cursor = q.cursor()?;
    Ok(Json(blocking(&app, move |s| s.room_state_at(&ch, cursor)).await?))
}

#[derive(Deserialize)]
struct MapsQ {
    rooms: Option<String>,
}

/// The hourly word ranking, when it is there (docs/TERMS.md); a map without it has no words, which is not an error.
async fn terms_file() -> Option<serde_json::Value> {
    let path = std::env::var("ORACLE_TERMS_FILE").unwrap_or_else(|_| "/var/lib/oracle-terms/terms.json".into());
    let raw = tokio::task::spawn_blocking(move || std::fs::read_to_string(path)).await.ok()?.ok()?;
    serde_json::from_str(&raw).ok()
}

/// `GET /rooms/{ch}/page` — the room's page, its live state (room_page.rs, docs/ROOM-PAGE.md): composed on every read from the
/// room's signed records. Authorized like room-view/1: a room the caller may not read answers 404, exactly like one that does not
/// exist. Never cached, so a revocation holds on the next read.
async fn room_page(State(app): State<App>, headers: HeaderMap, Path(ch): Path<String>) -> Response {
    let who = match principal(&app, &headers, Scope::Read).await { Ok(w) => w, Err(e) => return e.into_response() };
    let public = app.public_rooms.clone();
    let through = app.semantic.as_ref().and_then(|s| s.indexed_through());
    let now = now_ms();
    let out = blocking(&app, move |s| {
        if crate::roomview::access(s, &public, &who, &ch)?.is_none() {
            return Ok(None);
        }
        crate::room_page::compose(s, &ch, now, through).map(Some)
    })
    .await;
    match out {
        Ok(Some(v)) => {
            let mut res = Json(v).into_response();
            res.headers_mut().insert(header::CACHE_CONTROL, axum::http::HeaderValue::from_static("private, no-store"));
            res
        }
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({"error": "no such room"}))).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `GET /rooms/{ch}/map` — the room's map (docs/ROOM-MAP.md): the written map from its State beside what the index computes, and the line the brief prints.
async fn room_map(State(app): State<App>, headers: HeaderMap, Path(ch): Path<String>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let terms = terms_file().await;
    let now = now_ms();
    Ok(Json(blocking(&app, move |s| crate::roommap::map_of(s, &ch, terms.as_ref(), now)).await?))
}

/// `GET /maps?rooms=a,b,c` — the maps of up to 20 rooms in one read: what a seat reads at wake (docs/ROOM-MAP.md).
async fn maps(State(app): State<App>, headers: HeaderMap, Query(q): Query<MapsQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let rooms: Vec<String> = q.rooms.unwrap_or_default().split(',').map(|r| r.trim().trim_start_matches('#').to_string()).filter(|r| !r.is_empty()).take(20).collect();
    if rooms.is_empty() {
        return Err(missing("rooms is a comma-separated list of room names"));
    }
    let terms = terms_file().await;
    let now = now_ms();
    Ok(Json(blocking(&app, move |s| {
        let mut out = Vec::with_capacity(rooms.len());
        for r in &rooms {
            out.push(crate::roommap::map_of(s, r, terms.as_ref(), now)?);
        }
        Ok(json!({"rooms": out}))
    }).await?))
}

/// `GET /rooms/{ch}/messages` — the whole room from the room index: every class unless `class=` narrows it, newest first
/// (`before=<seq>`) or oldest first from a seq (`after=<seq>`). `next_before: null` means nothing older.
async fn room_messages(State(app): State<App>, headers: HeaderMap, Path(ch): Path<String>, Query(q): Query<RoomPageQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let q = q.page();
    Ok(Json(blocking(&app, move |s| crate::roomread::messages(s, &ch, &q)).await?))
}

/// `GET /rooms/{ch}/threads` — thread rows by newest message, with `state=` and `before=<last_seq>`.
async fn room_threads(State(app): State<App>, headers: HeaderMap, Path(ch): Path<String>, Query(q): Query<RoomPageQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let q = q.page();
    Ok(Json(blocking(&app, move |s| crate::roomread::threads(s, &ch, &q)).await?))
}

#[derive(Deserialize)]
struct SeatQ {
    seat: Option<String>,
}

/// `GET /rooms/{ch}/unread?seat=` — what a seat has not read in a room: its read cursor, the count of other seats' speech after it (capped at 1000)
/// and where it starts. Another seat's is gated like `/asks`.
async fn room_unread(State(app): State<App>, headers: HeaderMap, Path(ch): Path<String>, Query(q): Query<SeatQ>) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Read).await?;
    let seat = q.seat.unwrap_or_else(|| who.handle.clone());
    let (caller, enforce, target) = (who.handle, app.cross_seat_enforce, seat.clone());
    blocking(&app, move |s| s.cross_seat(&caller, &target, "unread", enforce, now_ms())).await?;
    Ok(Json(blocking(&app, move |s| crate::roomread::unread(s, &ch, &seat)).await?))
}

/// `GET /asked?seat=&state=&before=&limit=` — the request ledger from the room index: every item of every message the seat wrote, newest message first,
/// with its words, state and owner. `state=` is a comma list of item states. Another seat's is gated like `/asks`.
async fn asked(State(app): State<App>, headers: HeaderMap, Query(q): Query<AskedQ>) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Read).await?;
    let seat = q.seat.clone().unwrap_or_else(|| who.handle.clone());
    let (caller, enforce, target) = (who.handle, app.cross_seat_enforce, seat.clone());
    blocking(&app, move |s| s.cross_seat(&caller, &target, "asked", enforce, now_ms())).await?;
    let page = RoomPageQ { before: q.before, after: None, limit: q.limit, class: None, author: None, state: q.state, include_cargo: None, scaffold: None }.page();
    Ok(Json(blocking(&app, move |s| crate::roomread::asked(s, &seat, &page)).await?))
}

#[derive(Deserialize)]
struct AskedQ {
    seat: Option<String>,
    state: Option<String>,
    before: Option<i64>,
    limit: Option<i64>,
}

/// `GET /threads/{root}` — one thread by root id or seq (any of its messages names it); the root first, the rest paged newest first.
async fn thread_one(State(app): State<App>, headers: HeaderMap, Path(root): Path<String>, Query(q): Query<RoomPageQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let q = q.page();
    Ok(Json(blocking(&app, move |s| crate::roomread::thread(s, &s.resolve_id(&root)?, &q)).await?))
}

/// `GET /observations/{id}/content` and `/cargo`: one body exactly as the log holds it, for an item a page omitted (`body_omitted.fetch`).
/// The same bearer auth as every read; `x-content-sha256` lets a client check it received the exact bytes.
async fn observation_body(app: App, headers: HeaderMap, id: String, field: &'static str) -> Api<Response> {
    principal(&app, &headers, Scope::Read).await?;
    let (text, sha) = blocking(&app, move |s| crate::roomread::body(s, &s.resolve_id(&id)?, field)).await?;
    let ctype = if field == "cargo" { "application/json; charset=utf-8" } else { "text/plain; charset=utf-8" };
    Ok((StatusCode::OK, [(header::CONTENT_TYPE, ctype.to_string()), (header::HeaderName::from_static("x-content-sha256"), sha)], text).into_response())
}
async fn observation_content(State(app): State<App>, headers: HeaderMap, Path(id): Path<String>) -> Api<Response> {
    observation_body(app, headers, id, "content").await
}
async fn observation_cargo(State(app): State<App>, headers: HeaderMap, Path(id): Path<String>) -> Api<Response> {
    observation_body(app, headers, id, "cargo").await
}

#[derive(Deserialize)]
struct StatesQ {
    owner: Option<String>,
    /// Only heads written in this room (a room's board).
    channel: Option<String>,
    /// One subject's head, with the observation that set it.
    subject: Option<String>,
    /// With `subject`: that head's lineage, its newest `history` revisions (1 to 200) with `current` and `superseded_by`.
    history: Option<i64>,
}

/// `GET /states` — state heads, one owner's, or one subject's.
async fn states_any(State(app): State<App>, headers: HeaderMap, Query(q): Query<StatesQ>) -> Api<Json<serde_json::Value>> {
    if let Some(subject) = q.subject {
        // `history=<n>`: the head's lineage, newest first (src/lineage.rs). Without it, the head as ever.
        if let Some(n) = q.history {
            principal(&app, &headers, Scope::Read).await?;
            return Ok(Json(blocking(&app, move |s| crate::lineage::chain(s, &subject, n)).await?));
        }
        return state_head(State(app), headers, Path(subject)).await;
    }
    if q.history.is_some() {
        return Err(missing("history needs subject="));
    }
    state_heads(State(app), headers, Query(OwnerQ { owner: q.owner, channel: q.channel })).await
}

/// `GET /health` — liveness, the shared clock, the build, and row counts. Counts are cached for
/// 30 seconds so a monitor polling every few seconds never costs a table scan.
async fn health_all(State(app): State<App>) -> Api<Json<serde_json::Value>> {
    let now = now_ms();
    // The room index: whether the room routes will answer or say 503, and how far it has read (cheap: two keys and one max).
    let index = blocking(&app, |s| {
        s.with_conn(|c| {
            let ready = crate::roomindex::ready(c)?;
            let through: Option<i64> = c.query_row("SELECT max(seq) FROM msg", [], |r| r.get(0))?;
            let latest: Option<i64> = c.query_row("SELECT max(seq) FROM observations", [], |r| r.get(0))?;
            Ok(json!({"ready": ready, "through_seq": through, "log_seq": latest}))
        })
    })
    .await?;
    let cached = app.stats.lock().ok().and_then(|c| c.clone()).filter(|(at, _)| now - at < 30_000);
    let (at, stats) = match cached {
        Some(c) => c,
        None => {
            let fresh = blocking(&app, |s| s.stats()).await?;
            let v = json!(fresh);
            if let Ok(mut c) = app.stats.lock() {
                *c = Some((now, v.clone()));
            }
            (now, v)
        }
    };
    Ok(Json(json!({
        "status": "ok", "service": "oracle", "version": env!("CARGO_PKG_VERSION"), "commit": env!("ORACLE_BUILD_COMMIT"),
        "ms": now, "iso": oracle_proto::iso_ms(now), "local": oracle_proto::local_clock_env(now), "stats": stats, "stats_ms": at,
        // When this process started: a restart since a seat's last look is something `ox delta` reports (1 Nov 2026).
        "started_ms": app.metrics.started_ms(), "started": oracle_proto::iso_ms(app.metrics.started_ms()),
        "index": index, "cross_seat": if app.cross_seat_enforce { "enforce" } else { "audit" },
        "search": app.search_log.lock().map(|l| search_health(&l)).unwrap_or(serde_json::Value::Null),
    })))
}

#[derive(Deserialize)]
struct KeysQ {
    history: Option<bool>,
}

/// `GET /keys` — the registry; `?history=true` its signed change log.
async fn keys_any(State(app): State<App>, headers: HeaderMap, Query(q): Query<KeysQ>) -> Api<Json<serde_json::Value>> {
    if q.history.unwrap_or(false) {
        return key_events(State(app), headers).await;
    }
    keys(State(app), headers).await
}

#[derive(Deserialize)]
struct FoldQ {
    limit: Option<i64>,
    /// With `channel=`: where each seat's cursor stands in that room.
    marks: Option<bool>,
    channel: Option<String>,
}

/// `GET /fold` — what is new for this seat; `?marks=true&channel=` where every seat stands in a room.
async fn fold_any(State(app): State<App>, headers: HeaderMap, Query(q): Query<FoldQ>) -> Api<Json<serde_json::Value>> {
    if q.marks.unwrap_or(false) {
        let channel = q.channel.ok_or_else(|| missing("marks=true needs channel="))?;
        return marks(State(app), headers, Query(MarksQ { channel })).await;
    }
    fold(State(app), headers, Query(LimitQ { limit: q.limit })).await
}

/// The seat-rooms guard (seatrooms.rs). A scoped seat may reach only one room's routes for its rooms, a message or thread whose room is
/// one of them, its own asks, `/health`, `/whoami`, and the write route (the store checks the room). Every answer is scrubbed of other
/// rooms; everything else is refused. Unscoped seats, and requests the route itself will refuse, pass untouched.
pub(crate) async fn seat_scope(State(app): State<App>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    // Fail closed: a policy that cannot be read is an outage, never an open door (Emmy, 22 Oct).
    let table = match blocking(&app, |s| s.seat_scope_table()).await {
        Ok(t) => t,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "seat scope unavailable"}))).into_response(),
    };
    // A request with no bearer is not a seat's: the routes judge it (public reads, or a refusal).
    if !req.headers().contains_key(header::AUTHORIZATION) {
        return next.run(req).await;
    }
    let Ok(who) = principal(&app, req.headers(), Scope::Read).await else { return next.run(req).await };
    // A public seat (publicseats.rs) is always scoped, to its public rooms, whether or not the owner's table names anyone. Before
    // 29 Oct 2026 an empty table, or a seat missing from it, let a public seat read every room (found live, contained the same hour).
    let rooms = if crate::publicseats::is_public_handle(&who.handle) {
        match blocking(&app, {
            let h = who.handle.clone();
            move |s| s.seat_rooms_for(&h)
        })
        .await
        {
            Ok(Some(r)) => r,
            _ => return (StatusCode::FORBIDDEN, Json(json!({"error": "a public seat reads only its public rooms"}))).into_response(),
        }
    } else {
        let Some(r) = table.get(&who.handle).cloned() else { return next.run(req).await };
        r
    };
    let refuse = |why: String| (StatusCode::FORBIDDEN, Json(json!({"error": why}))).into_response();
    // Everything the decision needs, copied out first: nothing borrowed from the request lives across an await.
    let path = req.extensions().get::<axum::extract::MatchedPath>().map(|p| p.as_str().to_owned()).unwrap_or_default();
    let query: BTreeMap<String, String> = Query::<BTreeMap<String, String>>::try_from_uri(req.uri()).map(|q| q.0).unwrap_or_default();
    let room = req.uri().path().split('/').nth(2).unwrap_or("").to_owned();
    let method = req.method().clone();
    let get = method == axum::http::Method::GET;
    // A scoped seat sees raw signed records only (seatrooms::raw): every derived answer (threads, thread lists, unread, the map, asks)
    // is refused until projections are scoped at their source records (docs/ROOMS.md); its room's State keeps the foreign check, since
    // State revisions are written in the room itself (Emmy, 22 Oct: a derived field carries no room label).
    enum Mode { Pass, Raw, Checked }
    let mode = match (get, path.as_str()) {
        (true, "/health" | "/whoami") => Mode::Pass,
        (false, "/observations") if method == axum::http::Method::POST => Mode::Pass,
        (true, "/rooms/:ch/messages") if rooms.may_read(&room) => Mode::Raw,
        // room-view/1 (roomview.rs) authorizes the room itself; the answer still passes the foreign check.
        (true, "/rooms/:ch/view") if rooms.may_read(&room) => Mode::Checked,
        (true, "/rooms/mine") => Mode::Pass,
        (true, "/rooms/:ch/state") if rooms.may_read(&room) => Mode::Checked,
        (true, "/search") if query.contains_key("id") || query.get("channel").is_some_and(|c| rooms.may_read(c)) => Mode::Raw,
        _ => return refuse(format!("{} is scoped to its rooms; {method} {path} is not open to it", who.handle)),
    };
    if matches!(mode, Mode::Pass) {
        return next.run(req).await;
    }
    let res = next.run(req).await;
    if !res.status().is_success() {
        return res;
    }
    let (parts, body) = res.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 32 * 1024 * 1024).await else { return refuse("answer too large to check".into()) };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return refuse("answer could not be checked".into()) };
    let shown = match mode {
        Mode::Raw => crate::seatrooms::raw(v, &rooms),
        _ => (!crate::seatrooms::foreign(&v, &rooms)).then_some(v),
    };
    match shown {
        Some(v) => {
            let mut res = Json(v).into_response();
            *res.status_mut() = parts.status;
            // Keep the route's own headers (room-view/1 answers are private, no-store), never the stale length or type.
            for (k, val) in parts.headers.iter() {
                if k != header::CONTENT_LENGTH && k != header::CONTENT_TYPE {
                    res.headers_mut().insert(k.clone(), val.clone());
                }
            }
            res
        }
        None => refuse(format!("{} reads only its rooms", who.handle)),
    }
}

/// The private-rooms guard (privaterooms.rs). For a seat that is not a member of some private room: a route of that room, a query
/// naming it, an id or file of it, is refused; every other JSON answer is scrubbed of its records, at any depth. `/stream` filters in
/// its handler; `/mcp` calls back into these routes with the caller's own token, so it is guarded call by call. Replication between
/// nodes is a full copy that carries the table with it; every node enforces it. Members, and requests without a seat, pass untouched.
/// Six to sixty-four hex digits: a record's code, not an id with a room in it (see `Store::resolve_id`).
fn is_code(s: &str) -> bool {
    (6..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) async fn private_scope(State(app): State<App>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    // Fail closed: a table that cannot be read is an outage, never an open door.
    let table = match blocking(&app, |s| s.private_rooms_table()).await {
        Ok(t) if !t.is_empty() => t,
        Ok(_) => return next.run(req).await,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "private rooms unavailable"}))).into_response(),
    };
    let Ok(who) = principal(&app, req.headers(), Scope::Read).await else { return next.run(req).await };
    let hidden = crate::privaterooms::hidden_for(&table, &who.handle);
    if hidden.is_empty() {
        return next.run(req).await;
    }
    let refuse = |what: &str| (StatusCode::FORBIDDEN, Json(json!({"error": format!("#{what} is private to its members")}))).into_response();
    let path = req.extensions().get::<axum::extract::MatchedPath>().map(|p| p.as_str().to_owned()).unwrap_or_default();
    let query: BTreeMap<String, String> = Query::<BTreeMap<String, String>>::try_from_uri(req.uri()).map(|q| q.0).unwrap_or_default();
    let segs: Vec<String> = req.uri().path().split('/').map(str::to_owned).collect();
    if path.starts_with("/sync/") || path == "/stream" || path.starts_with("/mcp") || path.starts_with("/oauth") || path.starts_with("/.well-known") {
        return next.run(req).await;
    }
    if path.starts_with("/rooms/:ch") {
        if let Some(r) = segs.get(2).filter(|r| hidden.contains(r.as_str())) {
            // room-view/1: a room the caller may not read is answered as one that does not exist, never named as private.
            if path == "/rooms/:ch/view" {
                let mut res = (StatusCode::NOT_FOUND, Json(json!({"error": "no such room"}))).into_response();
                res.headers_mut().insert(header::CACHE_CONTROL, axum::http::HeaderValue::from_static("private, no-store"));
                return res;
            }
            return refuse(r);
        }
    }
    for key in ["channel", "room", "rooms"] {
        if let Some(r) = query.get(key).and_then(|v| v.split(',').map(str::trim).find(|r| hidden.contains(*r))) {
            return refuse(r);
        }
    }
    // A record's code (31 Oct 2026) names no room: it is resolved to the record's own room here, so the refusal is the same
    // as for its full id, and a code that cannot be resolved for the check is refused, never let through.
    let mut named: Vec<String> = Vec::new();
    if path.starts_with("/observations/:id") || path.starts_with("/threads/:root") {
        named.extend(segs.get(2).cloned());
    }
    named.extend(query.iter().filter(|(k, _)| matches!(k.as_str(), "id" | "root" | "ref")).map(|(_, v)| v.clone()));
    for id in named {
        let room = match crate::privaterooms::room_of_id(&id) {
            Some(r) => Some(r.to_string()),
            None if is_code(&id) => match blocking(&app, move |s| Ok(s.resolve_id(&id).and_then(|full| s.get(&full)).map(|o| o.channel).ok())).await {
                Ok(r) => r,
                Err(_) => return refuse("that room"),
            },
            None => None,
        };
        if let Some(r) = room.filter(|r| hidden.contains(r)) {
            return refuse(&r);
        }
    }
    if path == "/files/:id" || path == "/files/:id/meta" {
        let id = segs.get(2).cloned().unwrap_or_default();
        let rooms: Vec<String> = match blocking(&app, move |s| crate::files::meta(s, &id)).await {
            Ok(m) => serde_json::to_value(&m.rooms).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default()
                .iter().filter_map(|p| p.get("room").and_then(|r| r.as_str()).map(str::to_owned)).collect(),
            Err(_) => Vec::new(),
        };
        if !rooms.is_empty() && rooms.iter().all(|r| hidden.contains(r)) {
            return refuse(&rooms[0]);
        }
        if path == "/files/:id" {
            return next.run(req).await;
        }
    }
    let res = next.run(req).await;
    let is_json = res.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("application/json"));
    if !res.status().is_success() || !is_json {
        return res;
    }
    let (parts, body) = res.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024 * 1024).await else {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "answer too large to check"}))).into_response();
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "answer could not be checked"}))).into_response();
    };
    if crate::privaterooms::is_hidden(&v, &hidden) {
        return refuse("that room");
    }
    let mut out = Json(crate::privaterooms::scrub(v, &hidden)).into_response();
    *out.status_mut() = parts.status;
    // Keep the route's own headers (room-view/1 answers are private, no-store), never the stale length or type.
    for (k, val) in parts.headers.iter() {
        if k != header::CONTENT_LENGTH && k != header::CONTENT_TYPE {
            out.headers_mut().insert(k.clone(), val.clone());
        }
    }
    out
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/health", get(health_all))
        .route("/whoami", get(whoami))
        .route("/terms", get(terms))
        .route("/experiments", get(experiments))
        .route("/observations", post(write))
        .route("/chain/:author", get(chain_read))
        .route("/chain/:author/head", get(chain_head))
        .route("/search", get(search_any))
        .route("/sync/log", get(sync_log))
        .route("/sync/keys", get(sync_keys))
        .route("/sync/push", post(sync_push))
        .route("/sync/status", get(sync_status))
        .route("/threads", get(threads_any))
        .route("/types", get(types_any))
        .route("/asks", get(asks))
        .route("/delta", get(delta))
        .route("/world", get(world))
        .route("/claims", get(claims_read))
        .route("/cortex", get(cortex_nodes))
        .route("/cortex/node/:nid", get(cortex_node))
        .route("/cortex/entity/:name", get(entity_brief))
        .route("/cortex/law", get(cortex_law))
        .route("/minds", get(minds_list))
        .route("/minds/:handle", get(mind_statements))
        .route("/answerable", get(answerable))
        .route("/extract", post(extract))
        .route("/reflect", post(reflect))
        .route("/cortex/marks", get(cortex_marks))
        .route("/cortex/cases", get(cortex_cases))
        .route("/cortex/case/:n", get(cortex_case))
        .route("/cortex/drafts", post(cortex_draft))
        .route("/graph", get(graph_read))
        .route("/files", get(files_list).put(file_put))
        .route("/files/:id", get(file_get))
        .route("/files/:id/meta", get(file_meta))
        .route("/hallway", get(hallway_read))
        .route("/ask", get(crate::public::ask_page))
        .route("/public/search", get(crate::public::search))
        .route("/public/obs/:id", get(crate::public::obs_page))
        .route("/public/obs/:id/envelope", get(crate::public::obs_envelope))
        .route("/public/rooms", get(crate::public::rooms))
        .route("/public/register/challenge", get(crate::public::register_challenge))
        .route("/public/register", post(crate::public::register))
        .route("/public/room/:room", get(crate::public::room))
        .route("/tasks", get(tasks))
        .route("/cards", get(cards))
        .route("/rooms/mine", get(crate::roomview::mine))
        .route("/rooms/:ch/view", get(crate::roomview::view))
        .route("/rooms/:ch/messages", get(room_messages))
        .route("/rooms/:ch/state", get(room_state))
        .route("/rooms/:ch/page", get(room_page))
        .route("/rooms/:ch/map", get(room_map))
        .route("/maps", get(maps))
        .route("/rooms/:ch/threads", get(room_threads))
        .route("/rooms/:ch/unread", get(room_unread))
        .route("/asked", get(asked))
        .route("/threads/:root", get(thread_one))
        .route("/observations/:id/content", get(observation_content))
        .route("/observations/:id/cargo", get(observation_cargo))
        .route("/presence", get(presence))
        .route("/states", get(states_any))
        .route("/channels", get(channels))
        .route("/fold", get(fold_any).post(ack))
        .route("/keys", get(keys_any).post(apply_key_event))
        .route("/stream", get(stream))
        .route("/admin/import", post(admin_import))
        .route("/admin/rooms", post(admin_rooms).get(admin_room_ops))
        .route("/metrics/routes", get(route_metrics))
        .route("/.well-known/oauth-authorization-server", get(crate::oauth::authorization_server))
        .route("/.well-known/oauth-protected-resource", get(crate::oauth::protected_resource))
        .route("/.well-known/oauth-protected-resource/*path", get(crate::oauth::protected_resource))
        .route("/oauth/register", post(crate::oauth::register))
        .route("/oauth/authorize", get(crate::oauth::authorize).post(crate::oauth::authorize))
        .route("/oauth/token", post(crate::oauth::token))
        .route("/oauth/revoke", post(crate::oauth::revoke))
        .route("/mcp", post(crate::mcp::post).get(crate::mcp::get).delete(crate::mcp::delete))
        .route("/mcp/", post(crate::mcp::post).get(crate::mcp::get).delete(crate::mcp::delete))
        .route_layer(axum::middleware::from_fn_with_state(app.clone(), seat_scope))
        .route_layer(axum::middleware::from_fn_with_state(app.clone(), private_scope))
        .route_layer(axum::middleware::from_fn_with_state(app.clone(), crate::metrics::track))
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .with_state(app)
}



#[derive(Deserialize)]
struct ExperimentsQ {
    track: Option<String>,
    stage: Option<String>,
    /// words that must all appear in the subject, title, hypothesis, result or note (any case)
    q: Option<String>,
    limit: Option<i64>,
}

/// `GET /experiments` — the experiment registry: every `exp.*` head in every room with its data, newest first,
/// narrowed by track, stage or words. `total` counts the registry before narrowing.
async fn experiments(State(app): State<App>, headers: HeaderMap, Query(q): Query<ExperimentsQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let all = blocking(&app, |s| s.experiments()).await?;
    let total = all.len();
    let words: Vec<String> = q.q.as_deref().unwrap_or("").split_whitespace().map(|w| w.to_lowercase()).collect();
    let field = |e: &serde_json::Value, k: &str| e["data"][k].as_str().unwrap_or("").to_lowercase();
    let limit = q.limit.unwrap_or(200).clamp(1, 1000) as usize;
    let items: Vec<serde_json::Value> = all
        .into_iter()
        .filter(|e| q.track.as_deref().is_none_or(|t| field(e, "track") == t.to_lowercase()))
        .filter(|e| q.stage.as_deref().is_none_or(|t| field(e, "stage") == t.to_lowercase()))
        .filter(|e| {
            let hay = format!("{} {} {} {} {}", e["subject"].as_str().unwrap_or("").to_lowercase(), field(e, "title"), field(e, "hypothesis"), field(e, "result"), e["note"].as_str().unwrap_or("").to_lowercase());
            words.iter().all(|w| hay.contains(w.as_str()))
        })
        .take(limit)
        .collect();
    Ok(Json(json!({"total": total, "count": items.len(), "items": items})))
}

#[derive(Deserialize)]
struct TermsQ {
    limit: Option<i64>,
}

/// `GET /terms` — the words that matter in Oracle and the Time Machine, best first: `[word, weight, messages]`.
/// `oracle terms` (docs/TERMS.md) writes `ORACLE_TERMS_FILE` hourly (times said x rarity; Blaze 2026-10-08: "make it available to
/// the room whenever they search Oracle"). Read on each call, so the answer is never older than the last run.
async fn terms(State(app): State<App>, headers: HeaderMap, Query(q): Query<TermsQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let path = std::env::var("ORACLE_TERMS_FILE").unwrap_or_else(|_| "/var/lib/oracle-terms/terms.json".into());
    let limit = q.limit.unwrap_or(100).clamp(1, 500) as usize;
    let file = path.clone();
    let raw = match tokio::task::spawn_blocking(move || std::fs::read_to_string(file)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Ok(Json(json!({"error": format!("no word ranking at {path}: {e}"), "top": []}))),
        Err(e) => return Ok(Json(json!({"error": format!("reading {path}: {e}"), "top": []}))),
    };
    let d: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    let top: Vec<serde_json::Value> = d["terms"]
        .as_array()
        .map(|a| a.iter().take(limit).map(|t| json!([t["term"], t["weight"], t["documents"]])).collect())
        .unwrap_or_default();
    Ok(Json(json!({"generated_at": d["generated_at"], "documents": d["documents"], "how": d["how"], "top": top})))
}

async fn whoami(State(app): State<App>, headers: HeaderMap) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Read).await?;
    let handle = who.handle.clone();
    let keys = blocking(&app, move |s| s.active_keys(&handle)).await?;
    Ok(Json(json!({
        "handle": who.handle, "scope": who.scope.as_str(),
        // `pubkey` is the seat's first key; `pubkeys` is every device it may sign from.
        "pubkey": keys.first().map(|k| k.0.clone()), "role": keys.first().map(|k| k.1.clone()),
        "pubkeys": keys.iter().map(|k| k.0.clone()).collect::<Vec<_>>(),
        "server_ms": now_ms(), "local": oracle_proto::local_clock_env(now_ms()),
        // What this server understands, so a client never sends what it would refuse.
        // cortex_count: GET /cortex?count=1 (1 Nov 2026), so a brief never fetches the whole node list from an older server.
        // cortex_law: GET /cortex/law (3 Nov 2026), the LAW lines of every brief.
        // extract: POST /extract (4 Nov 2026), present only when a model is configured (ORACLE_EXTRACT_URL).
        "features": (["authored_ms", "prev", "cortex_count", "cortex_law", "minds", "answerable"].into_iter()
            .chain(crate::extract::configured().into_iter().flat_map(|_| ["extract", "reflect"])).collect::<Vec<_>>()),
    })))
}

/// A chain spans every room its author wrote in, so only the author (or an admin) reads it (docs/CHAIN.md).
async fn chain_reader(app: &App, headers: &HeaderMap, author: &str) -> Api<Principal> {
    let who = principal(app, headers, Scope::Read).await?;
    if who.handle != author && who.scope < Scope::Admin {
        return Err(ApiError(StoreError::Forbidden(format!("a chain is read by its author or an admin, not by '{}'", who.handle))));
    }
    Ok(who)
}

/// `GET /chain/{author}/head` — the link the author's next write names as `prev`.
async fn chain_head(State(app): State<App>, headers: HeaderMap, Path(author): Path<String>) -> Api<Json<serde_json::Value>> {
    chain_reader(&app, &headers, &author).await?;
    let a = author.clone();
    let (head, at) = blocking(&app, move |s| s.chain_head(&a)).await?;
    Ok(Json(json!({"author": author, "head": head, "seq": at.as_ref().map(|x| x.0), "id": at.map(|x| x.1)})))
}

#[derive(Deserialize)]
struct ChainQ {
    after: Option<i64>,
    limit: Option<usize>,
}

/// `GET /chain/{author}?after=&limit=` — the author's native observations in order, each as signed (docs/CHAIN.md).
async fn chain_read(State(app): State<App>, headers: HeaderMap, Path(author): Path<String>, Query(q): Query<ChainQ>) -> Api<Json<serde_json::Value>> {
    let who = chain_reader(&app, &headers, &author).await?;
    let limit = q.limit.unwrap_or(500).clamp(1, 2000);
    let a = author.clone();
    let records = blocking(&app, move |s| {
        let records = s.chain(&a, q.after.unwrap_or(0), limit)?;
        // A seat that wrote in a room it can no longer read (the room went private without it) gets those records as
        // withheld links, never dropped: the private-rooms guard would drop them whole, and the chain would show a break
        // that is not a fork (docs/CHAIN.md "Withheld links"). The link keeps no room, words or id.
        let hidden = crate::privaterooms::hidden_for(&s.private_rooms_table()?, &who.handle);
        Ok(records.into_iter().map(|r| if hidden.contains(&r.envelope.channel) { r.withhold() } else { r }).collect::<Vec<_>>())
    })
    .await?;
    let next = (records.len() == limit).then(|| records.last().map(|r| r.seq)).flatten();
    Ok(Json(json!({"author": author, "records": records, "next": next})))
}

/// The one write door, `POST /observations`. `kind` (say / state / memory) is inside the
/// signed bytes, so a say can never be replayed as a state write.
async fn write(
    State(app): State<App>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Write).await?;
    let env: Envelope = parse(&body)?;
    let receipt = blocking(&app, move |s| {
        let r = s.write(&who, &env, now_ms())?;
        let obs = s.get(&r.id)?;
        Ok((r, obs))
    })
    .await?;
    let (receipt, obs) = receipt;
    if !receipt.duplicate {
        app.publish(&obs);
    }
    Ok(Json(
        json!({"ok": true, "id": receipt.id, "seq": receipt.seq,
                   "duplicate": receipt.duplicate, "signed": receipt.signed}),
    ))
}

async fn observation(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let now = now_ms();
    let out = blocking(&app, move |s| {
        let obs = s.get(&s.resolve_id(&id)?)?;
        // Judge it against what points at it (replies, closes) so one observation carries the
        // same derived fields as a page: state, intent, ask, answered_by, overdue.
        let mut around = s.resolvers_of(std::slice::from_ref(&obs.id))?;
        around.extend(s.thread_members(&obs.id, 300)?);
        let items: Vec<projection::Item> = std::iter::once(&obs)
            .chain(around.iter())
            .map(|o| projection::Item { id: &o.id, author: &o.author, created_ms: o.created_ms, tags: &o.tags })
            .collect();
        let verdicts = projection::judge_in(&items, now, &s.room_roots(Some(obs.channel.as_str())));
        let mut row = json!(obs);
        if let (Some(v), Some(map)) = (verdicts.get(&obs.id), row.as_object_mut()) {
            map.insert("state".into(), json!(v.state.as_str()));
            map.insert("intent".into(), json!(v.intent.as_str()));
            map.insert("ask".into(), json!(v.ask.as_str()));
            if let Some(by) = &v.by { map.insert("resolved_by".into(), json!(by)); }
            if v.replies > 0 { map.insert("replies".into(), json!(v.replies)); }
            if let Some(b) = v.by_ms { map.insert("by_ms".into(), json!(b)); }
            if let Some(a) = &v.answered_by { map.insert("answered_by".into(), json!(a)); }
            if v.ask.as_str() == "decide" { if let Some(d) = crate::asks::decide_view(obs.cargo.as_deref(), v.choice) { map.insert("decide".into(), d); } }
            if v.overdue { map.insert("overdue".into(), json!(true)); }
            if let Some(c) = &v.classified_by { map.insert("classified_by".into(), json!(c)); }
            if v.unclassified { map.insert("unclassified".into(), json!(true)); }
            if !v.items.is_empty() { map.insert("items".into(), json!(v.items)); }
            if let Some(f) = v.fold { map.insert("fold".into(), json!(f)); }
            if !v.marks.is_empty() { map.insert("marks".into(), json!(v.marks)); }
            if let Some(w) = v.settled { map.insert("settled".into(), json!(w)); map.insert("settled_by".into(), json!(v.settled_by)); map.insert("settled_at".into(), json!(v.settled_at)); }
        }
        // Every state update that named this message, oldest first: who set what, and why.
        let mut history: Vec<&crate::store::Observation> = around
            .iter()
            .filter(|o| o.tags.iter().any(|t| t.strip_prefix("annotates:") == Some(obs.id.as_str())))
            .collect();
        history.sort_by_key(|o| o.seq);
        history.dedup_by_key(|o| o.seq);
        if let (false, Some(map)) = (history.is_empty(), row.as_object_mut()) {
            map.insert("history".into(), json!(history.iter().map(|o| json!({
                "id": o.id, "by": o.author, "created_ms": o.created_ms,
                "set": o.tags.iter().filter_map(|t| t.strip_prefix("set:")).collect::<Vec<_>>(),
                "why": o.content,
            })).collect::<Vec<_>>()));
        }
        Ok(row)
    })
    .await?;
    Ok(Json(out))
}

async fn legacy_source(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let rows = blocking(&app, move |s| s.legacy_sources(&id)).await?;
    Ok(Json(json!({"versions":rows})))
}

async fn envelope(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let (env, pubkey) =
        blocking(&app, move |s| {
            let id = s.resolve_id(&id)?;
            Ok((s.envelope_of(&id)?, s.get(&id)?.pubkey))
        })
        .await?;
    Ok(Json(json!({"envelope": env, "pubkey": pubkey})))
}

#[derive(Deserialize)]
struct RecentQ {
    channel: Option<String>,
    limit: Option<i64>,
    before: Option<i64>,
    /// Only observations carrying this tag.
    tag: Option<String>,
    /// Only observations whose content starts with this.
    starts: Option<String>,
    /// Derived states to show, comma separated, e.g. `open` or `open,closed`. `all` or absent
    /// shows everything. A name that is not a state matches nothing, so a typo shows an empty
    /// room rather than silently turning the filter off.
    state: Option<String>,
    /// Authors that are never filtered out, comma separated. A room where the owner's own words
    /// age out of view is a room nobody trusts.
    keep: Option<String>,
    /// #cortex only: list the graph's links, verdicts and concept nodes too (folded by default).
    scaffold: Option<bool>,
}

/// One page of a room's recent rows with their derived state, and the cursor for the next page: the seq
/// of the last row returned (exclusive), so following `next_before` never skips a row (pub for tests).
#[allow(clippy::too_many_arguments)]
pub fn recent_rows(
    s: &Store,
    channel: Option<&str>,
    limit: i64,
    before0: Option<i64>,
    tag: Option<&str>,
    starts: Option<&str>,
    view: &projection::View,
    now: i64,
) -> Result<(Vec<serde_json::Value>, i64, Option<i64>), StoreError> {
    recent_rows_folding(s, channel, limit, before0, tag, starts, view, now, None).map(|(rows, scanned, next, _)| (rows, scanned, next))
}

/// [`recent_rows`], folding #cortex's graph scaffolding ([`crate::store::is_scaffold`]) when `approved` is given: those
/// rows are counted, not listed, and each reading the owner approved is marked `approved`. Returns the count folded.
#[allow(clippy::too_many_arguments)]
pub fn recent_rows_folding(
    s: &Store,
    channel: Option<&str>,
    limit: i64,
    before0: Option<i64>,
    tag: Option<&str>,
    starts: Option<&str>,
    view: &projection::View,
    now: i64,
    approved: Option<&crate::cortex::ConceptIndex>,
) -> Result<(Vec<serde_json::Value>, i64, Option<i64>, i64), StoreError> {
        let (mut shown, mut scanned, mut before, mut folded) = (Vec::new(), 0i64, before0, 0i64);
        // A room's deciders are one lookup per request, not one per page.
        let deciders = s.room_roots(channel);
        // A bounded walk: at most ten pages, so a room that is entirely noise cannot hold a
        // request open while it reads the whole channel.
        // Always a walk, not one page: updates (annotations, marks) are judged but never listed, so a
        // page of them must not read as an empty room. The first cut walked only with state=, and
        // #hustler read "0 recent" the moment the classifier's updates filled the newest page.
        for _ in 0..10 {
            let page = s.recent_filtered(
                channel,
                limit,
                before,
                tag,
                starts,
            )?;
            if page.is_empty() {
                before = None;
                break;
            }
            scanned += page.len() as i64;
            before = Some(page[page.len() - 1].seq);
            let ids: Vec<String> = page.iter().map(|o| o.id.clone()).collect();
            let resolvers = s.resolvers_of(&ids)?;
            let mut judged: Vec<projection::Item> = page
                .iter()
                .chain(resolvers.iter())
                .map(|o| projection::Item {
                    id: &o.id,
                    author: &o.author,
                    created_ms: o.created_ms,
                    tags: &o.tags,
                })
                .collect();
            judged.sort_by_key(|i| i.created_ms);
            let verdicts = projection::judge_in(&judged, now, &deciders);
            for o in &page {
                let Some(v) = verdicts.get(&o.id) else {
                    continue;
                };
                if !view.shows(&o.author, v.state) {
                    continue;
                }
                // A state update is judged (its set: values fill its target) but never listed.
                if o.kind == "annotate" || projection::is_state_update(&o.tags) {
                    continue;
                }
                if approved.is_some() && crate::store::is_scaffold(o) {
                    folded += 1;
                    continue;
                }
                // An observation always serialises; if it somehow did not, drop the row rather than
                // failing the whole read.
                let Ok(mut row) = serde_json::to_value(o) else {
                    continue;
                };
                if let Some(map) = row.as_object_mut() {
                    map.insert("state".into(), json!(v.state.as_str()));
                    map.insert("intent".into(), json!(v.intent.as_str()));
                    if let Some(by) = &v.by {
                        map.insert("resolved_by".into(), json!(by));
                    }
                    if v.replies > 0 {
                        map.insert("replies".into(), json!(v.replies));
                    }
                    // The shape Emmy approved (2026-09-22): what was asked, by when, and the
                    // claimed answer. Derived here, never trusted from the author.
                    map.insert("ask".into(), json!(v.ask.as_str()));
                    if let Some(b) = v.by_ms {
                        map.insert("by_ms".into(), json!(b));
                    }
                    if let Some(a) = &v.answered_by {
                        map.insert("answered_by".into(), json!(a));
                    }
                    if v.ask.as_str() == "decide" {
                        if let Some(d) = crate::asks::decide_view(o.cargo.as_deref(), v.choice) {
                            map.insert("decide".into(), d);
                        }
                    }
                    if v.overdue {
                        map.insert("overdue".into(), json!(true));
                    }
                    if let Some(c) = &v.classified_by {
                        map.insert("classified_by".into(), json!(c));
                    }
                    if v.unclassified {
                        map.insert("unclassified".into(), json!(true));
                    }
                    if !v.items.is_empty() {
                        map.insert("items".into(), json!(v.items));
                    }
                    if let Some(f) = v.fold {
                        map.insert("fold".into(), json!(f));
                    }
                    if !v.marks.is_empty() {
                        map.insert("marks".into(), json!(v.marks));
                    }
                    if let Some(w) = v.settled {
                        map.insert("settled".into(), json!(w));
                        map.insert("settled_by".into(), json!(v.settled_by));
                        map.insert("settled_at".into(), json!(v.settled_at));
                    }
                    if approved.is_some_and(|a| a.approved.contains(&o.id)) {
                        map.insert("approved".into(), json!(true));
                    }
                }
                shown.push(row);
                if shown.len() as i64 >= limit {
                    // The cursor is the last row RETURNED, not the last row scanned: rows of this page
                    // after it were never shown, and `before` is exclusive (Emmy, 2026-10-03: a 500-row
                    // page scanned 1334 and handed back a cursor 327 rows past what it returned).
                    return Ok((shown, scanned, Some(o.seq), folded));
                }
            }
            if (page.len() as i64) < limit {
                before = None;
                break;
            }
        }
        Ok((shown, scanned, before, folded))
}

/// Every observation carries its derived state (see `projection`). With `state=`, pages are
/// walked until `limit` of them are worth showing, so a filtered room still fills a screen instead
/// of returning two rows out of twenty.
async fn recent(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<RecentQ>,
) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let limit = q.limit.unwrap_or(20).clamp(1, 500);
    let view = projection::View::parse(q.state.as_deref(), q.keep.as_deref());
    let now = now_ms();
    let (q_channel, q_tag, q_starts, q_before) = (q.channel.clone(), q.tag.clone(), q.starts.clone(), q.before);
    // #cortex read whole is its readings: the graph's links and verdicts are folded unless asked for by tag or scaffold=1.
    let fold = q.channel.as_deref() == Some("cortex") && q.tag.is_none() && !q.scaffold.unwrap_or(false);
    let (shown, scanned, next, folded) = blocking(&app, move |s| {
        // When the cortex views cannot be read nothing is folded: the room reads as it is stored.
        let approved = if fold { s.approved_index().ok() } else { None };
        recent_rows_folding(s, q_channel.as_deref(), limit, q_before, q_tag.as_deref(), q_starts.as_deref(), &view, now, approved.as_deref())
    })
    .await?;
    // A message naming a subject whose state moved after it says so (roomread::mark_stale).
    let mut shown = shown;
    let shown = blocking(&app, move |s| crate::roomread::mark_stale(s, &mut shown).map(|_| shown)).await?;
    let mut out = json!({
        "count": shown.len(), "items": shown, "scanned": scanned,
        "next_before": next.filter(|_| !shown.is_empty() || scanned > 0),
    });
    if fold {
        out["folded"] = json!({"scaffold": folded, "show": "scaffold=true"});
    }
    Ok(Json(out))
}

/// The reply a card points at (its outcome or its summary), as text a reader can use at once.
fn excerpt(id: &Option<String>, members: &[crate::store::Observation]) -> serde_json::Value {
    id.as_ref()
        .and_then(|i| members.iter().find(|m| &m.id == i))
        .map(|m| {
            json!({"id": m.id, "author": m.author, "created_ms": m.created_ms,
            "text": m.content.chars().take(700).collect::<String>()})
        })
        .unwrap_or(serde_json::Value::Null)
}

#[derive(Deserialize)]
struct ThreadsQ {
    channel: String,
    /// Whose messages start a thread, comma separated.
    roots: String,
    limit: Option<i64>,
    before: Option<i64>,
    /// Include each thread's replies, not just its card.
    expand: Option<bool>,
    /// `active`: every conversation that is alive, from ANY seat, newest activity first — the root
    /// voices' messages, plus anyone's top-level message that is still open or was replied to
    /// lately. A reply revives a thread, the way a Discord thread un-archives when someone speaks.
    scope: Option<String>,
    /// `scope=room` only: which threads — all (default: everything not archived), people, agents,
    /// automated, needs_answer, in_progress, answered, quiet, closed, archived.
    filter: Option<String>,
    /// `scope=room` only: combine with a kind filter, e.g. filter=people&status=needs_answer.
    status: Option<String>,
    /// `scope=room` only: only threads this seat took part in.
    participant: Option<String>,
    /// `scope=room` only: how far back a thread's last activity may be (default 7, max 60).
    days: Option<i64>,
    /// `scope=room` only: skip this many threads of the filtered, newest-first list.
    offset: Option<i64>,
}

/// One answer for every view of a room (`/threads?scope=room`): each thread classified once
/// (kind, bucket), counts over ALL of them before any filter, and what each seat has said.
fn room_view(
    s: &Store,
    q: &ThreadsQ,
    people: &[String],
    limit: i64,
    now: i64,
) -> Result<serde_json::Value, StoreError> {
    let days = q.days.unwrap_or(7).clamp(1, 60);
    let since = now - days * 24 * projection::HOUR_MS;
    // Every top-level message in the window, from anyone; older roots come back when a reply revives
    // them (their members are read below). The walk is bounded.
    let mut roots: Vec<crate::store::Observation> = Vec::new();
    // Per seat: sent, asks made (reply/act), answers given (a reply or close), asks answered.
    // The last two over the first is the seat's signal; a seat that only sends has none.
    let mut spoke: std::collections::BTreeMap<String, [u64; 4]> = std::collections::BTreeMap::new();
    let mut total = 0u64;
    let mut revived: Vec<String> = Vec::new();
    let mut before = None;
    'scan: for _ in 0..40 {
        let page = s.recent(Some(&q.channel), 500, before)?;
        let Some(last) = page.last() else { break };
        before = Some(last.seq);
        for o in page {
            if o.created_ms < since {
                break 'scan;
            }
            if o.kind != "say" {
                continue;
            }
            // Not speech: a state update or a reaction is neither a message, an answer, nor a
            // reason to revive a thread.
            if !projection::is_speech(&o.tags) {
                continue;
            }
            let bookkeeping = projection::kind_of(&o.author, &o.tags, &o.content, &[], people)
                == projection::Kind::Bookkeeping;
            if !bookkeeping {
                total += 1;
                let e = spoke.entry(o.author.clone()).or_default();
                e[0] += 1;
                let ask = projection::ask_of(&o.tags, projection::intent_of(&o.tags));
                if matches!(ask, projection::Ask::Reply | projection::Ask::Act | projection::Ask::Decide | projection::Ask::Review) {
                    e[1] += 1;
                }
                if o.tags.iter().any(|t| ["re:", "reply:", "in_reply_to:", "closes:"].iter().any(|p| t.starts_with(p))) {
                    e[2] += 1;
                }
            }
            if crate::store::starts_thread(&o) {
                if !bookkeeping {
                    roots.push(o);
                }
            } else if !revived.contains(&o.id) {
                revived.push(o.id.clone()); // a reply in the window: its thread is alive, however old
            }
        }
    }
    for id in revived {
        if let Ok(root) = s.thread_root(&id) {
            if !roots.iter().any(|r| r.id == root.id)
                && projection::kind_of(&root.author, &root.tags, &root.content, &[], people)
                    != projection::Kind::Bookkeeping
            {
                roots.push(root);
            }
        }
    }
    let keys = [
        "all",
        "people",
        "agents",
        "automated",
        "needs_answer",
        "in_progress",
        "answered",
        "quiet",
        "closed",
        "archived",
    ];
    let mut counts: BTreeMap<&str, u64> = keys.iter().map(|k| (*k, 0)).collect();
    // Status counts WITHIN each kind, so "People · Needs an answer" is a real number, not a guess.
    let mut by_kind: BTreeMap<&str, BTreeMap<&str, u64>> = BTreeMap::new();
    let want = q.filter.as_deref().unwrap_or("all");
    let status = q.status.as_deref().unwrap_or("any");
    let mut rows = Vec::new();
    let deciders = s.room_roots(Some(q.channel.as_str()));
    for root in &roots {
        let all = s.thread_members(&root.id, 300)?;
        let members: Vec<crate::store::Observation> = all.iter().filter(|o| projection::is_speech(&o.tags)).cloned().collect();
        let items: Vec<projection::Item> = std::iter::once(root)
            .chain(all.iter())
            .map(|o| projection::Item {
                id: &o.id,
                author: &o.author,
                created_ms: o.created_ms,
                tags: &o.tags,
            })
            .collect();
        let verdicts = projection::judge_in(&items, now, &deciders);
        for (id, v) in &verdicts {
            if v.answered_by.is_some() && matches!(v.ask, projection::Ask::Reply | projection::Ask::Act | projection::Ask::Decide | projection::Ask::Review) {
                if let Some(it) = items.iter().find(|i| i.id == id) {
                    if let Some(e) = spoke.get_mut(it.author) {
                        e[3] += 1;
                    }
                }
            }
        }
        let state_of = |id: &str| {
            verdicts
                .get(id)
                .map(|v| v.state)
                .unwrap_or(projection::State::Open)
        };
        let judged: Vec<(projection::Item, projection::State)> = items[1..]
            .iter()
            .map(|i| (i.clone(), state_of(i.id)))
            .collect();
        let root_state = state_of(&root.id);
        let card = projection::card(&items[0], root_state, &judged);
        // Bookkeeping that only ever sat alone is ambient, not a thread.
        if root_state == projection::State::Ambient && members.is_empty() {
            continue;
        }
        let kind = projection::kind_of(
            &root.author,
            &root.tags,
            &root.content,
            &card.participants,
            people,
        );
        let bucket = projection::bucket_of(root_state, &card, verdicts.get(&root.id));
        let kind_word = match kind {
            projection::Kind::People => "people",
            projection::Kind::Agents => "agents",
            projection::Kind::Automated => "automated",
            projection::Kind::Bookkeeping => "bookkeeping",
        };
        if bucket != "archived" {
            *counts.get_mut("all").unwrap() += 1;
            if let Some(c) = counts.get_mut(kind_word) {
                *c += 1;
            }
        }
        if let Some(c) = counts.get_mut(bucket) {
            *c += 1;
        }
        let per = by_kind.entry(kind_word).or_default();
        *per.entry(bucket).or_default() += 1;
        if bucket != "archived" {
            *per.entry("all").or_default() += 1;
        }
        let last = members
            .iter()
            .map(|m| m.created_ms)
            .max()
            .unwrap_or(root.created_ms)
            .max(root.created_ms);
        let takes_part =
            |seat: &str| root.author == seat || card.participants.iter().any(|p| p == seat);
        let chosen = match want {
            "all" => bucket != "archived",
            "people" | "agents" | "automated" => kind_word == want && bucket != "archived",
            other => bucket == other,
        } && (status == "any" || status == bucket)
            && q.participant.as_deref().is_none_or(takes_part);
        if chosen {
            let latest = members.iter().max_by_key(|m| m.created_ms);
            let (outcome, summary) = (
                excerpt(&card.recommendation, &members),
                excerpt(&card.summary, &members),
            );
            rows.push(json!({"root": root, "state": root_state.as_str(), "card": card, "kind": kind_word, "bucket": bucket,
                "outcome": outcome, "summary": summary,
                "last_activity_ms": last, "last": latest.map(|m| json!({"author": m.author, "created_ms": m.created_ms}))}));
        }
    }
    rows.sort_by_key(|t| std::cmp::Reverse(t["last_activity_ms"].as_i64().unwrap_or(0)));
    let matched = rows.len();
    let offset = q.offset.unwrap_or(0).max(0) as usize;
    let page: Vec<_> = rows.into_iter().skip(offset).take(limit as usize).collect();
    Ok(
        json!({"scope": "room", "filter": want, "status": status, "participant": q.participant, "window_days": days,
        "counts": counts, "by_kind": by_kind, "matched": matched, "count": page.len(), "offset": offset, "threads": page,
        // by_author stays a plain count per seat: a page loaded before 2026-09-22 renders it as a number and
        // cannot be reloaded from here. The richer signal is additive, under its own key.
        "messages": {"total": total,
            "by_author": spoke.iter().map(|(a, n)| (a.clone(), json!(n[0]))).collect::<serde_json::Map<_, _>>(),
            "by_author_signals": spoke.iter().map(|(a, n)| (a.clone(), json!({"sent": n[0], "asks": n[1], "answers": n[2], "answered": n[3]}))).collect::<serde_json::Map<_, _>>()}}),
    )
}

/// A room as threads: every message from a root voice, with a card rolled up from what happened
/// under it — research, actions, what is pending, ideas, the latest recommendation. The card is
/// derived on every read; nobody maintains it.
async fn threads(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<ThreadsQ>,
) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let authors: Vec<String> = q
        .roots
        .split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .take(16)
        .map(str::to_owned)
        .collect();
    let (expand, now) = (q.expand.unwrap_or(false), now_ms());
    if q.scope.as_deref() == Some("room") {
        let out = blocking(&app, move |s| room_view(s, &q, &authors, limit, now)).await?;
        return Ok(Json(out));
    }
    let active = q.scope.as_deref() == Some("active");
    let out = blocking(&app, move |s| {
        let mut candidates = s.by_authors(&q.channel, &authors, if active { 60 } else { limit }, q.before)?;
        if active {
            // Anyone's top-level message from the last week. Older ones from non-root seats have
            // burned every fuse there is, so they cannot be alive; the walk is bounded either way.
            let (mut before, week) = (None, now - 7 * 24 * projection::HOUR_MS);
            'scan: for _ in 0..4 {
                let page = s.recent(Some(&q.channel), 500, before)?;
                let Some(last) = page.last() else { break };
                before = Some(last.seq);
                for o in page {
                    if o.created_ms < week {
                        break 'scan;
                    }
                    if o.kind == "say" && !authors.contains(&o.author) && crate::store::starts_thread(&o) {
                        candidates.push(o);
                    }
                }
            }
        }
        let next = if !active && candidates.len() as i64 == limit {
            candidates.last().map(|o| o.seq)
        } else {
            None
        };
        let mut threads = Vec::new();
        // Roots arrive newest first, so the root AFTER this one in time is the previous element.
        let mut newer_root_seq: Option<i64> = None;
        let deciders = s.room_roots(Some(q.channel.as_str()));
        for root in &candidates {
            // A root voice replying inside someone's thread is a member of that thread, not a new one.
            // The link may be a tag or a `replyTo` in the cargo (the web composer writes the latter).
            if !crate::store::starts_thread(root) {
                continue;
            }
            let all = s.thread_members(&root.id, 300)?;
            let members: Vec<crate::store::Observation> = all.iter().filter(|o| projection::is_speech(&o.tags)).cloned().collect();
            let items: Vec<projection::Item> = std::iter::once(root)
                .chain(all.iter())
                .map(|o| projection::Item {
                    id: &o.id,
                    author: &o.author,
                    created_ms: o.created_ms,
                    tags: &o.tags,
                })
                .collect();
            let verdicts = projection::judge_in(&items, now, &deciders);
            let state_of = |id: &str| {
                verdicts
                    .get(id)
                    .map(|v| v.state)
                    .unwrap_or(projection::State::Open)
            };
            let judged: Vec<(projection::Item, projection::State)> = items[1..]
                .iter()
                .map(|i| (i.clone(), state_of(i.id)))
                .collect();
            let card = projection::card(&items[0], state_of(&root.id), &judged);
            // History has no links: agents answered in the room without pointing at what they
            // answered. Say what FOLLOWED the root, separately from replies, and call it unlinked.
            let after = if active {
                Vec::new() // every top-level message is its own conversation here; nothing is "loose"
            } else {
                s.followed_by(&q.channel, root.seq, root.created_ms, newer_root_seq, 6 * projection::HOUR_MS, 40)?
            };
            newer_root_seq = Some(root.seq);
            let mut who: Vec<&str> = Vec::new();
            for o in after.iter().filter(|o| !members.iter().any(|m| m.id == o.id)) {
                if !who.contains(&o.author.as_str()) {
                    who.push(&o.author);
                }
            }
            let unlinked = after.iter().filter(|o| !members.iter().any(|m| m.id == o.id)).count();
            let (outcome, summary) = (excerpt(&card.recommendation, &members), excerpt(&card.summary, &members));
            // The root's items (declared by its author or any seat's annotation, moved by replies): what an agent
            // answering the thread must see, or its answer moves nothing (Blaze, 2026-10-01: 23 replies, no row moved).
            let root_items = verdicts.get(root.id.as_str()).map(|v| v.items.clone()).unwrap_or_default();
            let mut row = json!({"root": root, "state": state_of(&root.id).as_str(), "card": card, "outcome": outcome, "summary": summary, "items": root_items,
                "floor": projection::floor_of(&root.tags, &root.content),
                "followed_by": {"count": unlinked, "authors": who, "linked": false,
                    "note": "said in the room within 6h, before the next root; nothing links these to it"}});
            if expand {
                let replies: Vec<serde_json::Value> = members
                    .iter()
                    .filter_map(|o| {
                        let mut v = serde_json::to_value(o).ok()?;
                        let st = state_of(&o.id);
                        v["state"] = json!(st.as_str());
                        v["facet"] = json!(projection::facet_of(&o.tags, st));
                        Some(v)
                    })
                    .collect();
                row["replies"] = json!(replies);
                row["followed_by"]["items"] = json!(after.iter().filter(|o| !members.iter().any(|m| m.id == o.id)).collect::<Vec<_>>());
            }
            if active {
                let last = members.iter().map(|m| m.created_ms).max().unwrap_or(root.created_ms).max(root.created_ms);
                let st = state_of(&root.id);
                let alive = authors.contains(&root.author)
                    || st == projection::State::Open
                    || (!members.is_empty() && now - last < 72 * projection::HOUR_MS);
                // Bookkeeping (status lines, receipts, task cards, snapshots) is ambient: not a conversation.
                if !alive || (st == projection::State::Ambient && members.is_empty()) {
                    continue;
                }
                row["last_activity_ms"] = json!(last);
                if let Some(m) = members.iter().max_by_key(|m| m.created_ms) {
                    row["last"] = json!({"author": m.author, "created_ms": m.created_ms});
                }
            }
            threads.push(row);
        }
        if active {
            threads.sort_by_key(|t| std::cmp::Reverse(t["last_activity_ms"].as_i64().unwrap_or(0)));
            threads.truncate(limit as usize);
        }
        Ok((threads, next))
    })
    .await?;
    Ok(Json(
        json!({"count": out.0.len(), "threads": out.0, "next_before": out.1}),
    ))
}

#[derive(Deserialize)]
struct AboutQ {
    channel: String,
    /// Whole-token names for the thing, comma separated: `AAVE,AAVEUSDT`.
    terms: String,
    limit: Option<i64>,
}

/// A worldline: what a room has said about one thing, newest first. Each entry carries the lines
/// that name it, what kind of statement it was, the thread it belongs to, and whether it was later
/// corrected (`superseded_by`) or itself corrects something (`supersedes`). Bookkeeping faces
/// (task cards, snapshots, configs — content starting with `[`) are not statements and are left out.
async fn about(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<AboutQ>,
) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let now = now_ms();
    let terms: Vec<String> = q
        .terms
        .split(',')
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .collect();
    let out = blocking(&app, move |s| {
        let hits: Vec<_> = s.about(&q.channel, &terms, q.limit.unwrap_or(120))?.into_iter().filter(|o| !o.content.trim_start().starts_with('[')).collect();
        let ids: Vec<String> = hits.iter().map(|o| o.id.clone()).collect();
        let resolvers = s.resolvers_of(&ids)?;
        let items: Vec<projection::Item> = hits.iter().chain(resolvers.iter())
            .map(|o| projection::Item { id: &o.id, author: &o.author, created_ms: o.created_ms, tags: &o.tags }).collect();
        let verdicts = projection::judge_in(&items, now, &s.room_roots(Some(q.channel.as_str())));
        let lower: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
        let mut roots: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
        let mut entries = Vec::with_capacity(hits.len());
        for o in &hits {
            let v = verdicts.get(&o.id);
            // Only the lines that actually name the thing, each with the line after it for context.
            let lines: Vec<&str> = o.content.lines().collect();
            let mut keep = vec![false; lines.len()];
            for (i, l) in lines.iter().enumerate() {
                let ll = l.to_lowercase();
                if lower.iter().any(|t| ll.contains(t.as_str())) {
                    keep[i] = true;
                    if i + 1 < lines.len() { keep[i + 1] = true; }
                }
            }
            let excerpt: String = lines.iter().zip(&keep).filter(|(l, k)| **k && !l.trim().is_empty()).map(|(l, _)| l.trim()).collect::<Vec<_>>().join("\n").chars().take(900).collect();
            let root = s.thread_root(&o.id)?;
            let root_json = roots.entry(root.id.clone()).or_insert_with(|| json!({"id": root.id, "author": root.author, "created_ms": root.created_ms,
                "text": root.content.chars().take(160).collect::<String>()})).clone();
            let supersedes: Vec<&str> = o.tags.iter().filter_map(|t| t.strip_prefix("supersedes:")).collect();
            entries.push(json!({"id": o.id, "author": o.author, "created_ms": o.created_ms, "excerpt": excerpt,
                "intent": v.map(|v| v.intent.as_str()), "facet": projection::facet_of(&o.tags, v.map(|v| v.state).unwrap_or(projection::State::Open)),
                "state": v.map(|v| v.state.as_str()), "superseded_by": v.and_then(|v| if v.state == projection::State::Superseded { v.by.clone() } else { None }),
                "supersedes": supersedes, "is_root": root.id == o.id, "root": root_json}));
        }
        Ok(json!({"channel": q.channel, "terms": terms, "count": entries.len(), "threads": roots.len(), "entries": entries}))
    }).await?;
    let mut out = out;
    out["now_ms"] = json!(now);
    Ok(Json(out))
}


#[derive(Deserialize)]
struct ThreadQ {
    id: String,
}

/// One whole thread, found from ANY message in it: the root, its card, and every linked reply
/// with its state and facet. What an agent needs before it answers, in one read.
async fn thread(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<ThreadQ>,
) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let now = now_ms();
    let out = blocking(&app, move |s| {
        let (root, all) = s.thread_of(&s.resolve_id(&q.id)?, 300)?;
        let items: Vec<projection::Item> = std::iter::once(&root)
            .chain(all.iter())
            .map(|o| projection::Item { id: &o.id, author: &o.author, created_ms: o.created_ms, tags: &o.tags })
            .collect();
        let verdicts = projection::judge_in(&items, now, &s.room_roots(Some(root.channel.as_str())));
        let state_of = |id: &str| verdicts.get(id).map(|v| v.state).unwrap_or(projection::State::Open);
        let judged: Vec<(projection::Item, projection::State)> =
            items[1..].iter().map(|i| (i.clone(), state_of(i.id))).collect();
        let card = projection::card(&items[0], state_of(&root.id), &judged);
        // State updates are the root's history, not its replies; reactions stay (the surface folds them).
        let members: Vec<crate::store::Observation> = all.into_iter().filter(|o| !projection::is_state_update(&o.tags)).collect();
        let replies: Vec<serde_json::Value> = members
            .iter()
            .filter_map(|o| {
                let mut v = serde_json::to_value(o).ok()?;
                let st = state_of(&o.id);
                v["state"] = json!(st.as_str());
                v["facet"] = json!(projection::facet_of(&o.tags, st));
                Some(v)
            })
            .collect();
        let (outcome, summary) = (excerpt(&card.recommendation, &members), excerpt(&card.summary, &members));
        let root_items = verdicts.get(root.id.as_str()).map(|v| v.items.clone()).unwrap_or_default();
        Ok(json!({"asked": q.id, "state": state_of(&root.id).as_str(), "card": card, "outcome": outcome, "summary": summary,
            "floor": projection::floor_of(&root.tags, &root.content), "root": root, "items": root_items, "replies": replies}))
    })
    .await?;
    let mut out = out;
    out["now_ms"] = json!(now);
    out["now"] = json!(oracle_proto::iso_ms(now));
    Ok(Json(out))
}

#[derive(Deserialize)]
struct DigestQ {
    channel: String,
    roots: String,
}

/// A room in a few hundred tokens, for an agent that must know where things stand without
/// reading the room: per floor how many threads are waiting / active / answered / quiet, and the
/// handful that are waiting, oldest first. Short keys on purpose; every count ships with its
/// denominator (`of`), so an empty list can be told apart from a room nobody looked at.
async fn digest(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<DigestQ>,
) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let authors: Vec<String> = q
        .roots
        .split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .take(16)
        .map(str::to_owned)
        .collect();
    let now = now_ms();
    let out = blocking(&app, move |s| {
        let roots = s.by_authors(&q.channel, &authors, 200, None)?;
        let mut floors: std::collections::BTreeMap<String, [u32; 4]> = Default::default();
        let (mut waiting, mut considered) = (Vec::new(), 0u32);
        let deciders = s.room_roots(Some(q.channel.as_str()));
        for root in &roots {
            if !crate::store::starts_thread(root) {
                continue;
            }
            considered += 1;
            let members = s.thread_members(&root.id, 300)?;
            let items: Vec<projection::Item> = std::iter::once(root)
                .chain(members.iter())
                .map(|o| projection::Item { id: &o.id, author: &o.author, created_ms: o.created_ms, tags: &o.tags })
                .collect();
            let verdicts = projection::judge_in(&items, now, &deciders);
            let state_of = |id: &str| verdicts.get(id).map(|v| v.state).unwrap_or(projection::State::Open);
            let judged: Vec<(projection::Item, projection::State)> = items[1..].iter().map(|i| (i.clone(), state_of(i.id))).collect();
            let card = projection::card(&items[0], state_of(&root.id), &judged);
            let floor = projection::floor_of(&root.tags, &root.content);
            let slot = match card.status { "waiting" => 0, "active" => 1, "answered" => 2, _ => 3 };
            floors.entry(floor.name.clone()).or_default()[slot] += 1;
            // "Needs someone": an open thread that is waiting, or fresh and unanswered.
            if card.status == "waiting" || (card.status == "quiet" && state_of(&root.id) == projection::State::Open) {
                let title: String = root.content.lines().next().unwrap_or("").chars().take(80).collect();
                waiting.push(json!({"id": root.id, "by": root.author, "fl": floor.name, "h": (now - root.created_ms) / projection::HOUR_MS,
                    "re": card.replies, "t": title}));
            }
        }
        waiting.reverse(); // oldest first: the one that has waited longest leads
        let total_waiting = waiting.len();
        waiting.truncate(8);
        let floors: Vec<serde_json::Value> = floors
            .into_iter()
            .map(|(f, c)| json!({"fl": f, "wait": c[0], "act": c[1], "ans": c[2], "quiet": c[3]}))
            .collect();
        Ok(json!({"room": q.channel, "as_of_ms": now, "threads": considered, "of": roots.len(),
            "floors": floors, "needs": waiting, "needs_of": total_waiting}))
    })
    .await?;
    Ok(Json(out))
}

async fn channels(State(app): State<App>, headers: HeaderMap) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let (rows, archived) = blocking(&app, |s| Ok((s.channels()?, s.archived_rooms()?))).await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|(c, n, last, seq)| {
            let mut v = json!({"channel": c, "count": n, "last_ms": last, "last_seq": seq});
            if let Some(why) = archived.get(&c) {
                v["archived"] = json!(true);
                v["archived_why"] = json!(why);
            }
            v
        })
        .collect();
    Ok(Json(json!({"channels": list, "archived": archived.len()})))
}

#[derive(Deserialize)]
struct MarksQ {
    channel: String,
}

async fn marks(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<MarksQ>,
) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let channel = q.channel.clone();
    let rows = blocking(&app, move |s| s.read_marks(&channel)).await?;
    Ok(Json(json!({"channel": q.channel, "marks": rows})))
}

#[derive(Deserialize)]
struct LimitQ {
    limit: Option<i64>,
}


/// The fold is always *your* fold: the agent is the token's handle, never a parameter.
async fn fold(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<LimitQ>,
) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Read).await?; // the effective identity, before any lookup
    let limit = q.limit.unwrap_or(50);
    let now = now_ms();
    let key = crate::fold_cache::FoldCache::key(&who.handle, limit, now);
    let _lane = app.folds.lane(&who.handle).lock_owned().await; // same-seat folds one at a time
    let generation = app.folds.generation(&who.handle);
    let newest = blocking(&app, |s| s.newest_seq()).await?;
    if let Some(v) = app.folds.lookup(&who.handle, &key, newest) {
        return Ok(Json(v));
    }
    let handle = who.handle.clone();
    let f = blocking(&app, move |s| s.fold(&handle, limit, now)).await?;
    let v = json!(f);
    // fingerprinted by the sequence read BEFORE the fold: a write during the fold leaves the entry stale-by-sequence
    app.folds.publish(&who.handle, key, newest, generation, v.clone());
    Ok(Json(v))
}

#[derive(Deserialize)]
struct AckBody {
    #[serde(default)]
    marks: BTreeMap<String, i64>,
    /// Mark every room read, up to its current head.
    #[serde(default)]
    all: bool,
    #[serde(default)]
    rewind: bool,
}

async fn ack(
    State(app): State<App>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Read).await?;
    let b: AckBody = parse(&body)?;
    let handle = who.handle.clone();
    let n = if b.all {
        blocking(&app, move |s| s.ack_all(&handle)).await?
    } else {
        let n = b.marks.len();
        blocking(&app, move |s| s.ack(&handle, &b.marks, b.rewind)).await?;
        n
    };
    app.folds.invalidate(&who.handle); // after the commit: a fill that began before it is refused at publication
    Ok(Json(json!({"ok": true, "channels": n})))
}

#[derive(Deserialize)]
struct SearchBody {
    query: String,
    channel: Option<String>,
    kind: Option<String>,
    limit: Option<i64>,
    /// Ranking experiment (store::Ranker): rrf (default), and-first, and3. For measured A/B with `ox eval --param`.
    ranker: Option<String>,
    /// How approved records place (store::ApprovedOrder): lead (default), boost, off. For measured A/B with `--param`.
    approved: Option<String>,
    /// How much a recent record gains over an old one, 0 (default) to 1 (store::Tuning).
    recency: Option<f64>,
    /// The archived rooms and expired records too (docs/ARCHIVE.md); off, the answer says what was not considered.
    include_archived: Option<bool>,
    /// `off` keeps the fused order instead of the cross-encoder's (store::Tuning::rerank), for measured A/B.
    rerank: Option<String>,
    /// As of a moment (oracle_proto::parse_when): what was on record then, and what held then (docs/SEARCH.md "Valid in time").
    as_of: Option<String>,
}

async fn search(
    State(app): State<App>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let b: SearchBody = parse(&body)?;
    let sem = app.semantic.clone();
    let day_ago = now_ms() - 86_400_000;
    let started = std::time::Instant::now();
    let meaning_on = app.semantic.is_some();
    let include_archived = b.include_archived.unwrap_or(false);
    let (hits, engine, strength, not_considered, timing, cited) = blocking(&app, move |s| {
        // strength (how much the record holds on these words) does not depend on the hits: it runs beside the search.
        let (found, strength, strength_ms) = std::thread::scope(|sc| {
            let st = sc.spawn(|| {
                let t = std::time::Instant::now();
                let v = s.strength(&b.query, b.channel.as_deref(), day_ago, sem.as_deref());
                (v, t.elapsed().as_millis() as u64)
            });
            crate::store::SEARCH_TUNING.with(|t| *t.borrow_mut() = crate::store::Tuning::parse(b.approved.as_deref(), b.recency).with_rerank(b.rerank.as_deref()).with_as_of(b.as_of.as_deref().and_then(oracle_proto::parse_when)));
            let found = s.search_in(
                &b.query,
                b.channel.as_deref(),
                b.kind.as_deref(),
                b.limit.unwrap_or(10),
                sem.as_deref(),
                crate::store::Ranker::parse(b.ranker.as_deref()),
                include_archived,
            );
            let (strength, ms) = st.join().expect("strength thread");
            (found, strength, ms)
        });
        let (mut hits, engine) = found?;
        let strength = strength?;
        let mut timing: Vec<(&'static str, u64)> = crate::store::SEARCH_LAPS.with(|t| std::mem::take(&mut *t.borrow_mut()));
        timing.push(("strength (beside)", strength_ms));
        let t = std::time::Instant::now();
        let not_considered = if include_archived { None } else { Some(s.not_considered()?).filter(|(rooms, _)| *rooms > 0) };
        timing.push(("not-considered", t.elapsed().as_millis() as u64));
        // Cited by (docs/CORTEX.md): one read for every returned id; a failure leaves recall whole and says so.
        let t = std::time::Instant::now();
        let ids: Vec<String> = hits.iter().map(|h| h.obs.id.clone()).collect();
        let cited = crate::store::enrich_cited_by(&mut hits, s.citations(&ids));
        timing.push(("cited-by", t.elapsed().as_millis() as u64));
        Ok((hits, engine, strength, not_considered, timing, cited))
    })
    .await?;
    // A search with meaning search configured that did not get it is a fallback; one without it configured is not.
    let answered = !meaning_on || engine.contains("hybrid") || engine.starts_with("id alias");
    if let Ok(mut log) = app.search_log.lock() {
        if log.len() >= SEARCH_LOG {
            log.pop_front();
        }
        log.push_back((started.elapsed().as_millis() as u64, answered));
    }
    let prior = crate::store::Store::prior(&strength);
    // The floor, said plainly for every reader (Round 50 ask 6; `ox eval` counts it): the nearest hits still follow.
    let no_prior = prior["verdict"] == "none";
    // Stage by stage, where this search spent its time (docs/SEARCH.md "Measured in use").
    let mut timing_ms = serde_json::Map::new();
    for (stage, ms) in &timing {
        timing_ms.insert((*stage).to_string(), json!(ms));
    }
    timing_ms.insert("total".into(), json!(started.elapsed().as_millis() as u64));
    let mut out = json!({"count": hits.len(), "hits": hits, "engine": engine, "strength": strength, "prior": prior, "no_prior": no_prior, "timing_ms": timing_ms, "cited_by": cited});
    // The archive, named (docs/ARCHIVE.md): what this answer did not consider, and how to.
    if let Some((rooms, records)) = not_considered {
        out["not_considered"] = json!({"rooms": rooms, "records": records, "include": "include_archived=1"});
    }
    Ok(Json(out))
}

// ── oracle-sync/1 (docs/SYNC.md): node to node, admin scope only ──────────────────────────────────────────────────────
// A node's sync token is an admin-scope token of the peer: a full replica is the whole authorized log, so a lesser token
// (or none) gets nothing, not a filtered copy.

#[derive(Deserialize)]
struct SyncLogQ {
    #[serde(default)]
    after: i64,
    limit: Option<i64>,
    skip_origin: Option<String>,
}

async fn sync_log(State(app): State<App>, headers: HeaderMap, Query(q): Query<SyncLogQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Admin).await?;
    let page = blocking(&app, move |s| s.sync_log(q.after, q.limit.unwrap_or(500), q.skip_origin.as_deref())).await?;
    Ok(Json(json!(page)))
}

#[derive(Deserialize)]
struct SyncKeysQ {
    #[serde(default)]
    after: i64,
}

async fn sync_keys(State(app): State<App>, headers: HeaderMap, Query(q): Query<SyncKeysQ>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Admin).await?;
    let page = blocking(&app, move |s| s.sync_keys(q.after)).await?;
    Ok(Json(json!(page)))
}

async fn sync_push(State(app): State<App>, headers: HeaderMap, body: axum::body::Bytes) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Admin).await?;
    #[derive(Deserialize)]
    struct Body {
        origin: String,
        rows: Vec<crate::sync::SyncRow>,
    }
    let b: Body = parse(&body)?;
    if b.origin.trim().is_empty() || b.rows.len() as i64 > crate::sync::PAGE_MAX {
        return Err(missing("push needs an origin and at most 1000 rows"));
    }
    let report = blocking(&app, move |s| s.replicate(&b.rows, &b.origin, now_ms())).await?;
    Ok(Json(json!(report)))
}

async fn sync_status(State(app): State<App>, headers: HeaderMap) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Admin).await?;
    let peer = std::env::var("ORACLE_SYNC_PEER_NAME").unwrap_or_default();
    let (cursor, quarantine) = blocking(&app, move |s| Ok((s.sync_cursor(&peer)?, s.sync_quarantine(20)?))).await?;
    Ok(Json(json!({"schema": crate::sync::SCHEMA, "node": std::env::var("ORACLE_NODE").ok(), "cursor": cursor, "quarantine": quarantine})))
}

async fn state_head(
    State(app): State<App>,
    headers: HeaderMap,
    Path(subject): Path<String>,
) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let head = blocking(&app, move |s| s.state_head(&subject)).await?;
    Ok(Json(json!(head)))
}

#[derive(Deserialize)]
struct OwnerQ {
    owner: Option<String>,
    /// Only heads written in this room.
    channel: Option<String>,
}

async fn state_heads(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<OwnerQ>,
) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    let rows = blocking(&app, move |s| s.state_heads_in(q.owner.as_deref(), q.channel.as_deref())).await?;
    Ok(Json(json!({"heads": rows})))
}

/// Bulk import (admin): history, or live messages mirrored from another Oracle. Rows with
/// `news: true` are folded and streamed like any new message.
/// Delete a room, or move its messages into another (`rooms.rs`). Owner-level: admin scope only. A dry run unless the body
/// says `"dry_run": false`, so a mistyped call can only ever report.
async fn admin_rooms(
    State(app): State<App>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Api<Json<serde_json::Value>> {
    let who = principal(&app, &headers, Scope::Admin).await?;
    #[derive(Deserialize)]
    struct Body {
        op: String,
        room: String,
        #[serde(default)]
        to: Option<String>,
        #[serde(default = "yes")]
        dry_run: bool,
        #[serde(default)]
        force: bool,
    }
    fn yes() -> bool {
        true
    }
    let b: Body = parse(&body)?;
    let actor = who.handle.clone();
    let (op, room, to, dry, force) = (b.op.clone(), b.room.clone(), b.to.clone(), b.dry_run, b.force);
    let report = blocking(&app, move |s| match op.as_str() {
        "delete" => s.delete_room(&room, force, dry, &actor, now_ms()),
        "move" => {
            let to = to.ok_or_else(|| StoreError::Rejected("move needs \"to\": the room the messages go into".into()))?;
            s.move_room(&room, &to, dry, &actor, now_ms())
        }
        other => Err(StoreError::Rejected(format!("op must be delete or move, not {other}"))),
    })
    .await?;
    if !report.dry_run && (report.outcome == "deleted" || report.outcome == "moved") {
        app.cards.forget(&report.room);
        if let Some(to) = &report.to {
            app.cards.forget(to);
        }
    }
    Ok(Json(json!({"ok": true, "report": report})))
}

/// The record of room operations, newest first.
async fn admin_room_ops(State(app): State<App>, headers: HeaderMap, Query(q): Query<std::collections::HashMap<String, String>>) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Admin).await?;
    let limit = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(50);
    let ops = blocking(&app, move |s| s.room_ops(limit)).await?;
    Ok(Json(json!({"ops": ops})))
}

async fn admin_import(
    State(app): State<App>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Admin).await?;
    #[derive(Deserialize)]
    struct Body {
        rows: Vec<crate::store::LegacyRow>,
    }
    let b: Body = parse(&body)?;
    let ids: Vec<(String, bool)> = b.rows.iter().map(|r| (r.id.clone(), r.news)).collect();
    let (added, fresh) = blocking(&app, move |s| {
        let before: Vec<bool> = ids.iter().map(|(id, _)| s.get(id).is_ok()).collect();
        let added = s.import_legacy(&b.rows)?;
        let mut fresh = Vec::new();
        for ((id, news), existed) in ids.iter().zip(before) {
            if *news && !existed {
                if let Ok(o) = s.get(id) {
                    fresh.push(o);
                }
            }
        }
        Ok((added, fresh))
    })
    .await?;
    for o in &fresh {
        app.publish(o);
    }
    Ok(Json(json!({"ok": true, "imported": added})))
}

async fn keys(State(app): State<App>, headers: HeaderMap) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    Ok(Json(json!({"keys": blocking(&app, |s| s.keys()).await?})))
}

async fn key_events(State(app): State<App>, headers: HeaderMap) -> Api<Json<serde_json::Value>> {
    principal(&app, &headers, Scope::Read).await?;
    Ok(Json(
        json!({"events": blocking(&app, |s| s.key_events()).await?}),
    ))
}

/// Registry changes authenticate by SIGNATURE, not by token: the event carries its own
/// proof, so it can be applied from anywhere the owner key lives.
async fn apply_key_event(
    State(app): State<App>,
    body: axum::body::Bytes,
) -> Api<Json<serde_json::Value>> {
    let ev: KeyEvent = parse(&body)?;
    blocking(&app, move |s| s.apply_key_event(&ev, now_ms())).await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct StreamQ {
    room: Option<String>,
    after: Option<i64>,
}

/// At most this many stored observations are replayed to a resuming reader; past that it gets `reset` (docs/STREAM.md).
pub const STREAM_REPLAY_MAX: i64 = 1000;

/// What the live part of `GET /stream` sends, before it is an SSE event (so it can be tested without HTTP).
#[derive(Debug, PartialEq)]
pub enum Followed {
    Observation { seq: i64, json: String },
    /// The reader fell behind the broadcast buffer: `missed` writes were dropped; `last` is the seq it holds. The stream ends.
    Lagged { missed: u64, last: i64 },
}

/// The live writes after `last`, in `rooms` when given: each once (a write already replayed is skipped by seq), and on a
/// broadcast lag one `Lagged` and the end, so the reader resumes from what it holds instead of silently missing a gap.
pub fn follow(rx: broadcast::Receiver<Arc<Live>>, rooms: Option<Vec<String>>, mut last: i64) -> impl Stream<Item = Followed> {
    use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
    let mut ended = false;
    BroadcastStream::new(rx)
        .map_while(move |m| {
            if ended {
                return None;
            }
            Some(match m {
                Ok(l) if l.seq <= last || rooms.as_ref().is_some_and(|r| !r.iter().any(|c| *c == l.channel)) => None,
                Ok(l) => {
                    last = l.seq;
                    Some(Followed::Observation { seq: l.seq, json: l.json.clone() })
                }
                Err(BroadcastStreamRecvError::Lagged(missed)) => {
                    ended = true;
                    Some(Followed::Lagged { missed, last })
                }
            })
        })
        .filter_map(|f| f)
}

fn observation_event(seq: i64, json: &str) -> Event {
    Event::default().event("observation").id(seq.to_string()).data(json)
}

type EventStream = std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

/// `GET /stream[?room=a,b][&after=<seq>]`, `Last-Event-ID: <seq>` (docs/STREAM.md): every stored observation as it is
/// stored, in the rooms asked for; a resuming reader first gets what it missed, oldest first, never one twice.
async fn stream(State(app): State<App>, headers: HeaderMap, Query(q): Query<StreamQ>) -> Api<Sse<EventStream>> {
    let who = principal(&app, &headers, Scope::Read).await?;
    // Private rooms (privaterooms.rs): a seat outside a private room never gets its records, replayed or live.
    let table = blocking(&app, |s| s.private_rooms_table()).await?;
    let hidden = crate::privaterooms::hidden_for(&table, &who.handle);
    let rooms: Option<Vec<String>> = q
        .room
        .as_deref()
        .map(|r| r.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect::<Vec<_>>())
        .filter(|v| !v.is_empty());
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok())
        .or(q.after);
    // Subscribe before reading the store: a write stored in between is then in both the replay and the live feed, and
    // `follow` sends it once by seq. Subscribing after the read would lose it.
    let rx = app.events.subscribe();
    let wanted = rooms.clone();
    let (head, replay) = blocking(&app, move |s| {
        let head = s.recent(None, 1, None)?.first().map_or(0, |o| o.seq);
        let replay = match resume {
            Some(after) => s.after_seq(wanted.as_deref(), after, STREAM_REPLAY_MAX + 1)?,
            None => Vec::new(),
        };
        Ok((head, replay))
    })
    .await?;
    let hello = Event::default().event("connected").data(json!({"rooms": rooms, "after": resume, "head": head}).to_string());
    let sse = |s: EventStream| Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)));
    if replay.len() as i64 > STREAM_REPLAY_MAX {
        let reset = Event::default().event("reset").data(json!({"replay_max": STREAM_REPLAY_MAX, "last": resume}).to_string());
        return Ok(sse(Box::pin(tokio_stream::iter([Ok(hello), Ok(reset)]))));
    }
    let last = replay.last().map(|o| o.seq).or(resume).unwrap_or(0);
    let replayed: Vec<Result<Event, Infallible>> = replay
        .iter()
        .filter(|o| !hidden.contains(&o.channel))
        .map(|o| Ok(observation_event(o.seq, &serde_json::to_string(o).unwrap_or_default())))
        .collect();
    // The policy is read again for every live record (Emmy's review of 3c3bdae): a subscriber already open when a room turns private,
    // or when its seat leaves the members, stops receiving it at once. An unreadable table drops the record (fail closed).
    let live_store = app.store.clone();
    let live_who = who.handle.clone();
    let live = follow(rx, rooms, last).filter(move |f| match f {
        Followed::Observation { json, .. } => {
            let Some(room) = serde_json::from_str::<serde_json::Value>(json).ok().and_then(|v| v.get("channel").and_then(|c| c.as_str()).map(str::to_owned)) else {
                return false;
            };
            match live_store.private_rooms_table() {
                Ok(t) => t.get(&room).is_none_or(|m| m.contains(&live_who)),
                Err(_) => false,
            }
        }
        _ => true,
    }).map(|f| {
        Ok(match f {
            Followed::Observation { seq, json } => observation_event(seq, &json),
            Followed::Lagged { missed, last } => Event::default().event("lagged").data(json!({"missed": missed, "last": last}).to_string()),
        })
    });
    Ok(sse(Box::pin(tokio_stream::once(Ok(hello)).chain(tokio_stream::iter(replayed)).chain(live))))
}

#[cfg(test)]
mod heavy_lane_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Three slow calls start, then a short one. Through `heavy` the short read finds a free lane at once; the same
    /// three through plain `blocking` take every lane and the short read waits for one to finish (the control).
    async fn short_read_wait(through_heavy: bool) -> Duration {
        let app = App::new(Store::memory().unwrap());
        assert_eq!((app.lanes.available_permits(), app.heavy.available_permits()), (3, 2));
        let slow = |s: &Store| -> Result<(), StoreError> { let _ = s; std::thread::sleep(Duration::from_millis(600)); Ok(()) };
        let mut held = Vec::new();
        for _ in 0..3 {
            let app = app.clone();
            held.push(tokio::spawn(async move { if through_heavy { heavy(&app, slow).await } else { blocking(&app, slow).await }.map(|_| ()).map_err(|_| ()) }));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let t = Instant::now();
        blocking(&app, |_| Ok(())).await.map_err(|_| ()).unwrap();
        let waited = t.elapsed();
        for h in held { h.await.unwrap().unwrap(); }
        waited
    }

    /// A heavy call dropped while its walk runs keeps its heavy permit until the walk returns, as the lane always has
    /// (the control): both stay held while the walk runs, then both come back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_dropped_heavy_call_keeps_its_permit_until_the_walk_ends() {
        let app = App::new(Store::memory().unwrap());
        let call = {
            let app = app.clone();
            tokio::spawn(async move { heavy(&app, |_| { std::thread::sleep(Duration::from_millis(500)); Ok(()) }).await.map_err(|_| ()) })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        call.abort();
        let _ = call.await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!((app.heavy.available_permits(), app.lanes.available_permits()), (1, 2), "the walk still runs, so both stay held");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!((app.heavy.available_permits(), app.lanes.available_permits()), (2, 3), "the walk ended, so both come back");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_short_read_never_waits_behind_heavy_views() {
        let with = short_read_wait(true).await;
        assert!(with < Duration::from_millis(200), "through heavy the short read waited {with:?}");
        let without = short_read_wait(false).await;
        assert!(without >= Duration::from_millis(350), "the control should wait for a lane, waited {without:?}");
    }
}

#[cfg(test)]
mod search_health_tests {
    use super::*;

    #[test]
    fn health_reports_search_percentiles_and_keyword_fallbacks() {
        let mut log = std::collections::VecDeque::new();
        assert_eq!(search_health(&log)["searches"], 0);
        for ms in 1..=100u64 {
            log.push_back((ms * 10, ms % 10 != 0));
        }
        let h = search_health(&log);
        assert_eq!((h["searches"].clone(), h["p50_ms"].clone(), h["p95_ms"].clone(), h["max_ms"].clone()), (json!(100), json!(510), json!(950), json!(1000)));
        assert_eq!(h["keyword_fallbacks"], 10);
    }
}
