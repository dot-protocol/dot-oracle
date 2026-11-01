//! Which endpoints are used, by whom, and how fast — kept in memory since the process started.
//! No request body, query or token is kept: the caller is the handle a token authenticated as,
//! found again by the first 8 bytes of the token's SHA-256.

use axum::{
    extract::{MatchedPath, Request, State},
    http::header,
    middleware::Next,
    response::Response,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Mutex,
    time::Instant,
};

/// Latency samples kept per route; percentiles are over the most recent WINDOW calls.
const WINDOW: usize = 512;

#[derive(Default)]
struct Stat {
    calls: u64,
    client_errors: u64,
    server_errors: u64,
    recent: VecDeque<u32>,
    last_ms: i64,
    callers: BTreeMap<String, u64>,
}

pub struct Metrics {
    started_ms: i64,
    routes: Mutex<HashMap<(String, String), Stat>>,
    handles: Mutex<HashMap<[u8; 8], String>>,
    /// Each seat's last authenticated request: (when, calls since start, which route). The heartbeat behind
    /// `GET /presence` (Blaze, 2026-09-29: "which agent is working, which is not … who takes on their responsibility").
    seen: Mutex<HashMap<String, (i64, u64, String)>>,
}

/// One row per registered seat: (handle, retired). A seat is retired only when every key it has ever had is revoked; a
/// seat with two devices that lost one is still a seat.
pub fn seat_rows(keys: &[crate::store::KeyRow]) -> Vec<(String, bool)> {
    let mut seats: Vec<(String, bool)> = Vec::new();
    for k in keys {
        let live_key = k.revoked_ms.is_none();
        match seats.iter_mut().find(|(h, _)| *h == k.handle) {
            Some(e) => e.1 = e.1 && !live_key,
            None => seats.push((k.handle.clone(), !live_key)),
        }
    }
    seats
}

/// Seen within this many seconds: live. Within IDLE_S: idle. Longer, or never since start: gone.
pub const LIVE_S: i64 = 180;
pub const IDLE_S: i64 = 1800;

fn token_key(token: &str) -> [u8; 8] {
    let digest = Sha256::digest(token.as_bytes());
    let mut key = [0u8; 8];
    key.copy_from_slice(&digest[..8]);
    key
}

fn percentile(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)]
}

impl Metrics {
    pub fn new(now_ms: i64) -> Metrics {
        Metrics { started_ms: now_ms, routes: Mutex::new(HashMap::new()), handles: Mutex::new(HashMap::new()), seen: Mutex::new(HashMap::new()) }
    }

    /// Called once a token has authenticated, so later calls with it are attributed to the seat.
    pub fn remember_handle(&self, token: &str, handle: &str) {
        if let Ok(mut map) = self.handles.lock() {
            if map.len() > 4096 {
                map.clear();
            }
            map.insert(token_key(token), handle.to_string());
        }
    }

    fn handle_for(&self, key: [u8; 8]) -> Option<String> {
        self.handles.lock().ok()?.get(&key).cloned()
    }

    pub fn record(&self, method: &str, path: &str, status: u16, ms: u32, caller: Option<String>, now_ms: i64) {
        let Ok(mut routes) = self.routes.lock() else { return };
        let stat = routes.entry((method.to_string(), path.to_string())).or_default();
        stat.calls += 1;
        match status {
            400..=499 => stat.client_errors += 1,
            500..=599 => stat.server_errors += 1,
            _ => {}
        }
        if stat.recent.len() == WINDOW {
            stat.recent.pop_front();
        }
        stat.recent.push_back(ms);
        stat.last_ms = now_ms;
        // 401 means it did not authenticate: the caller is only the handle this token had before it stopped working.
        if let (Some(h), true) = (caller.as_deref(), status != 401) {
            if let Ok(mut seen) = self.seen.lock() {
                let e = seen.entry(h.to_string()).or_insert((0, 0, String::new()));
                *e = (now_ms, e.1 + 1, format!("{method} {path}"));
            }
        }
        *stat.callers.entry(caller.unwrap_or_else(|| "anonymous".into())).or_default() += 1;
    }

    /// Every registered seat with its liveness, then any other handle that called. `seats` is (handle, retired).
    /// Nothing is written anywhere: it is what this process has seen since it started (`since_ms`).
    /// When this process started (epoch ms): on /health, and in `delta` as the last restart.
    pub fn started_ms(&self) -> i64 {
        self.started_ms
    }

    pub fn presence(&self, seats: &[(String, bool)], now_ms: i64) -> Value {
        let seen = self.seen.lock().map(|s| s.clone()).unwrap_or_default();
        let row = |h: &str, retired: bool| {
            let got = seen.get(h);
            let state = if retired {
                "retired"
            } else {
                match got {
                    Some((at, _, _)) if now_ms - at <= LIVE_S * 1000 => "live",
                    Some((at, _, _)) if now_ms - at <= IDLE_S * 1000 => "idle",
                    _ => "gone",
                }
            };
            json!({"handle": h, "state": state, "last_seen_ms": got.map(|g| g.0), "calls": got.map(|g| g.1).unwrap_or(0), "last_route": got.map(|g| g.2.clone())})
        };
        let mut rows: Vec<Value> = seats.iter().map(|(h, r)| row(h, *r)).collect();
        let mut others: Vec<&String> = seen.keys().filter(|h| !seats.iter().any(|(s, _)| s == *h)).collect();
        others.sort();
        rows.extend(others.into_iter().map(|h| row(h, false)));
        json!({"ok": true, "since_ms": self.started_ms, "now_ms": now_ms, "live_s": LIVE_S, "idle_s": IDLE_S, "seats": rows})
    }

    /// Every known route appears, used or not, so an unused endpoint is visible as zero calls.
    pub fn snapshot(&self, known: &[(&str, &str)], now_ms: i64) -> Value {
        let routes = match self.routes.lock() {
            Ok(r) => r,
            Err(_) => return json!({"ok": false, "error": "metrics unavailable"}),
        };
        let mut keys: Vec<(String, String)> = known.iter().map(|(m, p)| (m.to_string(), p.to_string())).collect();
        for k in routes.keys() {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
        let rows: Vec<Value> = keys
            .into_iter()
            .map(|(method, path)| match routes.get(&(method.clone(), path.clone())) {
                Some(s) => {
                    let mut sorted: Vec<u32> = s.recent.iter().copied().collect();
                    sorted.sort_unstable();
                    let mut callers: Vec<(&String, &u64)> = s.callers.iter().collect();
                    callers.sort_by(|a, b| b.1.cmp(a.1));
                    json!({
                        "method": method, "path": path, "calls": s.calls,
                        "client_errors": s.client_errors, "server_errors": s.server_errors,
                        "p50_ms": percentile(&sorted, 0.5), "p95_ms": percentile(&sorted, 0.95),
                        "max_ms": sorted.last().copied().unwrap_or(0), "last_ms": s.last_ms,
                        "callers": callers.into_iter().map(|(h, n)| json!({"handle": h, "calls": n})).collect::<Vec<_>>(),
                    })
                }
                None => json!({"method": method, "path": path, "calls": 0}),
            })
            .collect();
        json!({"ok": true, "since_ms": self.started_ms, "now_ms": now_ms, "window": WINDOW, "routes": rows})
    }
}

/// Route layer: runs only for matched routes, so `path` is the route pattern (`/state/:subject`),
/// never a concrete id. For `/stream` the time is time-to-headers.
pub async fn track(State(app): State<crate::http::App>, req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_string();
    let path = req.extensions().get::<MatchedPath>().map(|p| p.as_str().to_string()).unwrap_or_default();
    let key = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(token_key);
    let started = Instant::now();
    let response = next.run(req).await;
    let ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
    let caller = key.and_then(|k| app.metrics.handle_for(k));
    app.metrics.record(&method, &path, response.status().as_u16(), ms, caller, crate::http::now_ms());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_errors_percentiles_and_callers() {
        let m = Metrics::new(0);
        for ms in 1..=100 {
            m.record("GET", "/search", 200, ms, Some("rocky".into()), 5);
        }
        m.record("GET", "/search", 404, 7, None, 6);
        m.record("POST", "/observations", 500, 3, Some("bobby".into()), 7);
        let snap = m.snapshot(&[("GET", "/search"), ("GET", "/health")], 9);
        let rows = snap["routes"].as_array().unwrap();
        let recent = &rows[0];
        assert_eq!(recent["calls"], 101);
        assert_eq!(recent["client_errors"], 1);
        assert_eq!(recent["p50_ms"], 50);
        assert_eq!(recent["callers"][0]["handle"], "rocky");
        assert_eq!(recent["callers"][1]["handle"], "anonymous");
        assert_eq!(rows[1]["path"], "/health");
        assert_eq!(rows[1]["calls"], 0, "a known route that was never called still shows");
        assert_eq!(rows[2]["server_errors"], 1, "a route outside the known list is still reported");
    }

    #[test]
    fn a_token_is_attributed_without_being_kept() {
        let m = Metrics::new(0);
        m.remember_handle("secret-token", "taylor-core");
        assert_eq!(m.handle_for(token_key("secret-token")).as_deref(), Some("taylor-core"));
        assert_eq!(m.handle_for(token_key("other")), None);
        let kept = format!("{:?}", m.handles.lock().unwrap().keys().collect::<Vec<_>>());
        assert!(!kept.contains("secret"), "only a hash prefix is stored");
    }

    #[test]
    fn every_route_in_the_router_is_listed() {
        let source = include_str!("http.rs");
        let router = &source[source.find("pub fn router").unwrap()..];
        for line in router.lines().take_while(|l| !l.contains(".with_state")) {
            if let Some(rest) = line.trim().strip_prefix(".route(\"") {
                let path = &rest[..rest.find('"').unwrap()];
                for (verb, method) in [("get(", "GET"), ("post(", "POST")] {
                    if rest.contains(verb) {
                        assert!(
                            crate::http::ROUTES.contains(&(method, path)),
                            "{method} {path} is served but missing from ROUTES"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn presence_is_live_idle_gone_or_retired_from_requests_alone() {
        let m = Metrics::new(0);
        let min = 60_000;
        m.record("GET", "/fold", 200, 3, Some("rocky".into()), 100 * min);
        m.record("GET", "/search", 200, 3, Some("dinesh".into()), 80 * min);
        m.record("GET", "/search", 200, 3, Some("legacy-seat".into()), 10 * min);
        let seats = vec![("rocky".to_string(), false), ("dinesh".to_string(), false), ("taylor-core".to_string(), false), ("rocky-core".to_string(), true)];
        let p = m.presence(&seats, 101 * min);
        let st = |h: &str| p["seats"].as_array().unwrap().iter().find(|r| r["handle"] == h).unwrap()["state"].as_str().unwrap().to_string();
        assert_eq!(st("rocky"), "live", "seen a minute ago");
        assert_eq!(st("dinesh"), "idle", "seen 21 minutes ago");
        assert_eq!(st("taylor-core"), "gone", "never seen since start");
        assert_eq!(st("rocky-core"), "retired", "a revoked seat is retired, whatever it did");
        assert_eq!(st("legacy-seat"), "gone", "a caller outside the registry still shows, 91 minutes ago");
        assert_eq!(p["seats"][0]["last_route"], "GET /fold");
    }

    #[test]
    fn a_rejected_request_is_not_a_sign_of_life() {
        // A revoked or rotated-out token still maps to its old handle in the cache, and the server answers it 401.
        // A stale process retrying it is not the seat running.
        let m = Metrics::new(0);
        let min = 60_000;
        m.record("GET", "/fold", 200, 3, Some("rocky".into()), 10 * min);
        m.record("GET", "/fold", 401, 1, Some("rocky".into()), 100 * min);
        m.record("GET", "/fold", 401, 1, Some("dinesh".into()), 100 * min);
        m.record("POST", "/observations", 403, 1, Some("kin".into()), 100 * min); // authenticated, then refused: it is running
        let seats = vec![("rocky".to_string(), false), ("dinesh".to_string(), false), ("kin".to_string(), false)];
        let p = m.presence(&seats, 101 * min);
        let get = |h: &str| p["seats"].as_array().unwrap().iter().find(|r| r["handle"] == h).unwrap().clone();
        assert_eq!(get("rocky")["state"], "gone", "its last authenticated request was 91 minutes ago");
        assert_eq!(get("rocky")["last_seen_ms"], 10 * min);
        assert_eq!(get("dinesh")["state"], "gone", "a seat that has only ever been refused was never seen");
        assert_eq!(get("dinesh")["calls"], 0);
        assert_eq!(get("kin")["state"], "live", "a 403 comes after authentication");
    }

    #[test]
    fn a_seat_is_retired_only_when_every_key_it_ever_had_is_revoked() {
        let key = |h: &str, revoked: Option<i64>| crate::store::KeyRow { handle: h.into(), pubkey: "k".into(), role: "seat".into(), created_ms: 0, revoked_ms: revoked };
        let keys = vec![
            key("rocky", None), key("rocky", Some(5)), // two devices, one lost: still a seat
            key("old", Some(5)), key("old", Some(6)),  // every key revoked: retired
            key("laptop", Some(5)), key("laptop", None), // the revoked key comes first: still a seat
            key("solo", None),
        ];
        let rows = seat_rows(&keys);
        let get = |h: &str| rows.iter().find(|(n, _)| n == h).unwrap().1;
        assert!(!get("rocky") && !get("laptop") && !get("solo"));
        assert!(get("old"));
        assert_eq!(rows.len(), 4, "one row per seat");
    }
}
