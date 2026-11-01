//! Text for the reading commands that have no MCP twin (docs/ox.md, the parity rule): `ox rooms`, `threads`, `health`,
//! `metrics`, `presence`, `seats` and the lines `ox watch` prints. Each takes the JSON the server answered and returns
//! what a person reads; `--json` prints that JSON unchanged instead.
use anyhow::{bail, Result};
use oracle_mcp::{ago, one_line};
use serde_json::Value;

/// `7d`, `24h`, `30m` or a number of seconds, as milliseconds.
pub fn span_ms(s: &str) -> Result<i64> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = n.parse().map_err(|_| anyhow::anyhow!("a span is a number with d, h, m or s: 7d, 24h, 30m"))?;
    Ok(n * match unit {
        "d" => 86_400_000,
        "h" => 3_600_000,
        "m" => 60_000,
        "s" | "" => 1_000,
        _ => bail!("a span is a number with d, h, m or s: 7d, 24h, 30m"),
    })
}

fn when(now: i64, ms: &Value) -> String {
    ms.as_i64().map(|m| ago(now, m)).unwrap_or_else(|| "never".into())
}

/// `GET /channels`: every room, most recently active first; `since` keeps the rooms that spoke in that window.
pub fn rooms_text(v: &Value, since: Option<i64>, now: i64) -> String {
    let mut rows: Vec<&Value> = v["channels"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    rows.retain(|r| since.is_none_or(|s| r["last_ms"].as_i64().is_some_and(|l| now - l <= s)));
    rows.sort_by_key(|r| std::cmp::Reverse(r["last_ms"].as_i64().unwrap_or(0)));
    let mut out = format!("{} rooms{}\n", rows.len(), if since.is_some() { " active in the window" } else { "" });
    for r in rows {
        out += &format!("  #{:<28} {:>7} messages   last {}\n", r["channel"].as_str().unwrap_or("?"), r["count"].as_i64().unwrap_or(0), when(now, &r["last_ms"]));
    }
    out
}

/// `GET /rooms/{room}/threads`: one line per thread, newest activity first, then how to page.
pub fn threads_text(v: &Value, now: i64) -> String {
    let room = v["room"].as_str().unwrap_or("?");
    let threads = v["threads"].as_array().cloned().unwrap_or_default();
    let mut out = format!("#{room} — {} threads\n", threads.len());
    for t in &threads {
        let root = &t["root"];
        let who: Vec<String> = t["participants"].as_array().into_iter().flatten().filter_map(|p| p.as_str().or_else(|| p["handle"].as_str()).map(str::to_owned)).collect();
        let settled = match (t["settled_by"].as_str(), t["settled_word"].as_str()) {
            (Some(by), Some(w)) => format!("  · {w} by {by}"),
            _ => t["answered_by"].as_str().map(|a| format!("  · answered by {a}")).unwrap_or_default(),
        };
        out += &format!(
            "  {}  {:<9} {:>3} replies  last {}  {}{settled}\n      {}\n",
            root["id"].as_str().or_else(|| root.as_str()).unwrap_or("?"),
            t["state"].as_str().unwrap_or("?"),
            t["replies"].as_i64().unwrap_or(0),
            when(now, &t["last_ms"]),
            if who.is_empty() { String::new() } else { who.join(", ") },
            one_line(root["content"].as_str().or_else(|| root["text"].as_str()).unwrap_or(""), 110),
        );
    }
    if let Some(nb) = v["next_before"].as_i64() {
        out += &format!("older: ox threads {room} --before {nb}\n");
    }
    out
}

/// `GET /health`: the server, its build, its sizes, the room index and how search is doing.
pub fn health_text(v: &Value) -> String {
    let mut out = format!(
        "{} {} · {} · commit {}\n",
        v["service"].as_str().unwrap_or("oracle"),
        v["version"].as_str().unwrap_or("?"),
        v["status"].as_str().unwrap_or("?"),
        v["commit"].as_str().map(|c| &c[..c.len().min(12)]).unwrap_or("?"),
    );
    out += &format!("clock     {}\n", v["iso"].as_str().unwrap_or("?"));
    if let Some(stats) = v["stats"].as_object() {
        let parts: Vec<String> = stats.iter().filter(|(_, x)| x.is_number() || x.is_string()).map(|(k, x)| format!("{k} {}", x.as_str().map(str::to_owned).unwrap_or_else(|| x.to_string()))).collect();
        out += &format!("stats     {}\n", parts.join(" · "));
    }
    let ix = &v["index"];
    if !ix.is_null() {
        out += &format!(
            "index     {} · through seq {} of {}\n",
            if ix["ready"] == true { "ready" } else { "NOT READY (room routes answer 503)" },
            ix["through_seq"],
            ix["log_seq"]
        );
    }
    let s = &v["search"];
    if s.is_object() {
        out += &format!(
            "search    {} searches · p50 {} ms · p95 {} ms · max {} ms · keyword fallbacks {}\n",
            s["searches"], s["p50_ms"], s["p95_ms"], s["max_ms"], s["keyword_fallbacks"]
        );
    }
    if let Some(c) = v["cross_seat"].as_str() {
        out += &format!("cross-seat reads  {c}\n");
    }
    out
}

/// `GET /metrics/routes`: one line per route. `route` keeps the routes whose path contains it; `sort` is calls, p95,
/// max or errors; `all` keeps the routes nobody called.
pub fn metrics_text(v: &Value, route: Option<&str>, sort: &str, all: bool, now: i64) -> Result<String> {
    let key = |r: &Value| -> i64 {
        match sort {
            "calls" => r["calls"].as_i64().unwrap_or(0),
            "p95" => r["p95_ms"].as_i64().unwrap_or(0),
            "max" => r["max_ms"].as_i64().unwrap_or(0),
            "errors" => r["client_errors"].as_i64().unwrap_or(0) + r["server_errors"].as_i64().unwrap_or(0),
            _ => -1,
        }
    };
    if !["calls", "p95", "max", "errors"].contains(&sort) {
        bail!("--sort is calls, p95, max or errors");
    }
    let mut rows: Vec<&Value> = v["routes"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    rows.retain(|r| (all || r["calls"].as_i64().unwrap_or(0) > 0) && route.is_none_or(|p| r["path"].as_str().unwrap_or("").contains(p)));
    rows.sort_by_key(|r| std::cmp::Reverse(key(r)));
    let since = v["since_ms"].as_i64().map(|s| ago(now, s)).unwrap_or_else(|| "?".into());
    let mut out = format!("routes since the server started ({since}); latency over the last {} calls of each\n", v["window"]);
    out += &format!("  {:<7} {:<34} {:>8} {:>6} {:>6} {:>7} {:>7} {:>7}  top callers\n", "method", "path", "calls", "4xx", "5xx", "p50 ms", "p95 ms", "max ms");
    for r in rows {
        let callers: Vec<String> = r["callers"].as_array().into_iter().flatten().take(3).map(|c| format!("{} {}", c["handle"].as_str().unwrap_or("?"), c["calls"])).collect();
        let n = |k: &str| r[k].as_i64().map(|x| x.to_string()).unwrap_or_else(|| "-".into());
        out += &format!(
            "  {:<7} {:<34} {:>8} {:>6} {:>6} {:>7} {:>7} {:>7}  {}\n",
            r["method"].as_str().unwrap_or("?"),
            r["path"].as_str().unwrap_or("?"),
            n("calls"),
            n("client_errors"),
            n("server_errors"),
            n("p50_ms"),
            n("p95_ms"),
            n("max_ms"),
            callers.join(", ")
        );
    }
    Ok(out)
}

/// `GET /presence`: every seat's liveness as this server has seen it since it started.
pub fn presence_text(v: &Value) -> String {
    let now = v["now_ms"].as_i64().unwrap_or(0);
    let rows = v["seats"].as_array().cloned().unwrap_or_default();
    let mut out = format!(
        "seats as seen since the server started ({}); live = a call in the last {} s, idle = in the last {} s\n",
        when(now, &v["since_ms"]),
        v["live_s"],
        v["idle_s"]
    );
    for r in rows {
        out += &format!(
            "  {:<14} {:<8} last {:<14} {:>7} calls  {}\n",
            r["handle"].as_str().unwrap_or("?"),
            r["state"].as_str().unwrap_or("?"),
            when(now, &r["last_seen_ms"]),
            r["calls"].as_i64().unwrap_or(0),
            r["last_route"].as_str().unwrap_or("")
        );
    }
    out
}

/// `GET /keys`: the registry, one line per key; a revoked key says when.
pub fn seats_text(v: &Value, now: i64) -> String {
    let rows = v["keys"].as_array().cloned().unwrap_or_default();
    let mut out = format!("{} keys\n", rows.len());
    for r in rows {
        let key = r["pubkey"].as_str().unwrap_or("?");
        out += &format!(
            "  {:<14} {:<8} {}…  added {}{}\n",
            r["handle"].as_str().unwrap_or("?"),
            r["role"].as_str().unwrap_or("?"),
            &key[..key.len().min(16)],
            when(now, &r["created_ms"]),
            r["revoked_ms"].as_i64().map(|m| format!("  REVOKED {}", ago(now, m))).unwrap_or_default()
        );
    }
    out
}

/// `GET /keys?history=true`: the signed key events, oldest first.
pub fn key_events_text(v: &Value, now: i64) -> String {
    let rows = v["events"].as_array().cloned().unwrap_or_default();
    let mut out = format!("{} key events\n", rows.len());
    for e in rows {
        let ev = if e["event_json"].is_string() { serde_json::from_str::<Value>(e["event_json"].as_str().unwrap_or("{}")).unwrap_or(Value::Null) } else { e.clone() };
        let key = ev["pubkey"].as_str().unwrap_or("");
        out += &format!(
            "  {}  {:<7} {:<14} {}…  signed by {}\n",
            when(now, &ev["ts_ms"].clone()),
            ev["action"].as_str().unwrap_or("?"),
            ev["handle"].as_str().unwrap_or("?"),
            &key[..key.len().min(16)],
            ev["signer"].as_str().unwrap_or("?")
        );
    }
    out
}

/// One `observation` event of `GET /stream` as one line, or None when `rooms` is given and it is another room's.
pub fn watch_line(data: &str, rooms: &[String]) -> Option<String> {
    let o: Value = serde_json::from_str(data).ok()?;
    let room = o["channel"].as_str().unwrap_or("?");
    if !rooms.is_empty() && !rooms.iter().any(|r| r == room) {
        return None;
    }
    let at = o["created_ms"].as_i64().or_else(|| o["ts_ms"].as_i64()).map(|m| oracle_proto::iso_ms(m)[11..19].to_string()).unwrap_or_default();
    Some(format!(
        "{at}  #{room:<14} {:<12} {}  {}",
        o["author"].as_str().unwrap_or("?"),
        o["id"].as_str().unwrap_or(""),
        one_line(o["content"].as_str().unwrap_or(""), 140)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn spans_read_as_milliseconds_and_nonsense_is_refused() {
        assert_eq!((span_ms("7d").unwrap(), span_ms("24h").unwrap(), span_ms("30m").unwrap(), span_ms("90").unwrap()), (604_800_000, 86_400_000, 1_800_000, 90_000));
        assert!(span_ms("soon").is_err() && span_ms("3w").is_err());
    }

    #[test]
    fn rooms_are_newest_first_and_the_window_drops_quiet_rooms() {
        let v = json!({"channels": [
            {"channel": "old", "count": 5, "last_ms": NOW - 30 * 86_400_000_i64},
            {"channel": "busy", "count": 900, "last_ms": NOW - 60_000},
        ]});
        let all = rooms_text(&v, None, NOW);
        assert!(all.starts_with("2 rooms") && all.find("#busy").unwrap() < all.find("#old").unwrap(), "{all}");
        let week = rooms_text(&v, Some(span_ms("7d").unwrap()), NOW);
        assert!(week.contains("#busy") && !week.contains("#old"), "{week}");
    }

    #[test]
    fn metrics_sort_filter_and_hide_unused_routes() {
        let v = json!({"since_ms": NOW - 3_600_000, "window": 512, "routes": [
            {"method": "GET", "path": "/search", "calls": 900, "client_errors": 0, "server_errors": 0, "p50_ms": 4, "p95_ms": 30, "max_ms": 90, "callers": [{"handle": "rocky", "calls": 800}]},
            {"method": "GET", "path": "/asks", "calls": 72, "client_errors": 1, "server_errors": 0, "p50_ms": 900, "p95_ms": 3100, "max_ms": 9000, "callers": [{"handle": "blaze", "calls": 70}]},
            {"method": "GET", "path": "/terms", "calls": 0},
        ]});
        let by_p95 = metrics_text(&v, None, "p95", false, NOW).unwrap();
        assert!(by_p95.find("/asks").unwrap() < by_p95.find("/search").unwrap(), "{by_p95}");
        assert!(!by_p95.contains("/terms"), "a route nobody called is hidden by default");
        assert!(metrics_text(&v, None, "calls", true, NOW).unwrap().contains("/terms"), "--all shows it");
        let one = metrics_text(&v, Some("/asks"), "calls", false, NOW).unwrap();
        assert!(one.contains("/asks") && !one.contains("/search") && one.contains("blaze 70"), "{one}");
        assert!(metrics_text(&v, None, "speed", false, NOW).is_err());
    }

    #[test]
    fn presence_and_seats_say_state_and_revocation() {
        let p = json!({"now_ms": NOW, "since_ms": NOW - 600_000, "live_s": 120, "idle_s": 900, "seats": [
            {"handle": "rocky", "state": "live", "last_seen_ms": NOW - 5_000, "calls": 42, "last_route": "GET /search"},
            {"handle": "nexus", "state": "retired", "last_seen_ms": null, "calls": 0, "last_route": null},
        ]});
        let t = presence_text(&p);
        assert!(t.contains("rocky") && t.contains("live") && t.contains("GET /search") && t.contains("retired") && t.contains("never"), "{t}");
        let k = json!({"keys": [
            {"handle": "rocky", "pubkey": "abababababababababababababababababababababababababababababababab", "role": "seat", "created_ms": NOW - 86_400_000, "revoked_ms": null},
            {"handle": "kin", "pubkey": "aa", "role": "seat", "created_ms": NOW - 86_400_000, "revoked_ms": NOW - 3_600_000},
        ]});
        let s = seats_text(&k, NOW);
        assert!(s.starts_with("2 keys") && s.contains("abababababababab…") && s.contains("REVOKED"), "{s}");
    }

    #[test]
    fn watch_keeps_only_the_named_rooms_and_skips_what_is_not_an_observation() {
        let o = json!({"id": "OBS-room-1", "channel": "room", "author": "kin", "content": "hello\nthere", "created_ms": NOW}).to_string();
        let line = watch_line(&o, &[]).unwrap();
        assert!(line.contains("#room") && line.contains("kin") && line.contains("hello there"), "{line}");
        assert!(watch_line(&o, &["other".into()]).is_none());
        assert!(watch_line(&o, &["room".into()]).is_some());
        assert!(watch_line("{}", &[]).is_some() && watch_line("not json", &[]).is_none());
    }

    #[test]
    fn health_names_an_unready_index() {
        let v = json!({"status": "ok", "service": "oracle", "version": "0.1.0", "commit": "801259a731f40c19", "iso": "2026-10-17T10:00:00Z",
            "stats": {"observations": 375000, "db_bytes": 1}, "index": {"ready": false, "through_seq": 10, "log_seq": 20},
            "search": {"searches": 3, "p50_ms": 4, "p95_ms": 9, "max_ms": 12, "keyword_fallbacks": 0}, "cross_seat": "audit"});
        let t = health_text(&v);
        assert!(t.contains("commit 801259a731f4") && t.contains("NOT READY") && t.contains("observations 375000") && t.contains("cross-seat reads  audit"), "{t}");
    }

    #[test]
    fn threads_page_says_how_to_go_older() {
        let v = json!({"room": "oracle", "next_before": 77, "threads": [
            {"root": {"id": "OBS-oracle-1", "content": "launch gates"}, "state": "open", "replies": 4, "last_ms": NOW - 60_000, "participants": ["grok", "rocky"], "settled_by": null, "settled_word": null, "answered_by": "rocky"}]});
        let t = threads_text(&v, NOW);
        assert!(t.contains("OBS-oracle-1") && t.contains("4 replies") && t.contains("grok, rocky") && t.contains("answered by rocky") && t.contains("ox threads oracle --before 77"), "{t}");
    }
}
