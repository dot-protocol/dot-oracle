//! Paging is lossless (Bobby's property for Emmy's defect 7556c094fab3, 2026-10-03): for a fixed upper bound U and every page size L, following `next_before`
//! until it is null returns every listed row below U exactly once, `next_before` never lies above what the page returned (`<= min(returned seq)`), and no
//! listed row sits strictly between the two. The fixture mixes imported rows (envelope 1) with native ones, and puts runs of state updates (judged, never
//! listed) between them that are longer than the ten-page walk budget, so a page can come back EMPTY with a cursor and the chain must still reach the rest.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::http::recent_rows;
use oracle_server::projection::View;
use oracle_server::store::{LegacyRow, Principal, Scope, Store};
use std::collections::{BTreeMap, HashSet};

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;
const ROOM: &str = "core";

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 5000 * MIN).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 5000 * MIN), NOW - 5000 * MIN).unwrap();
    s
}
struct Fixture {
    s: Store,
    /// every row that must be listed, by id
    listed: HashSet<String>,
}
impl Fixture {
    fn say(&mut self, text: &str, mins: i64) -> String {
        let at = NOW + mins * MIN;
        let env = Envelope::sign(&sk(2), ROOM, "rocky", Kind::Say, text, None, vec![], at);
        let id = self.s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, at).unwrap().id;
        self.listed.insert(id.clone());
        id
    }
    /// A state update: judged, never listed.
    fn noise(&mut self, target: &str, n: usize, mins: i64) {
        for i in 0..n {
            let at = NOW + (mins + i as i64) * MIN / 100;
            let env = Envelope::sign(&sk(2), ROOM, "rocky", Kind::Annotate, "noted", None, vec![format!("annotates:{target}"), "set:intent=result".to_string(), format!("n:{mins}-{i}")], at);
            self.s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, at).unwrap();
        }
    }
    fn imported(&mut self, tag: &str, count: usize, base_ms: i64) {
        let rows: Vec<LegacyRow> = (0..count)
            .map(|i| {
                let id = format!("OBS-{ROOM}-20260101-{tag}{i:04}");
                self.listed.insert(id.clone());
                LegacyRow {
                    id, channel: ROOM.into(), author: "unknown".into(), kind: "say".into(), content: format!("imported {tag} {i}"), cargo: None, tags: vec![],
                    created_ms: base_ms + i as i64, pubkey: String::new(), sig: String::new(), news: false, source: None, repair_legacy: false,
                }
            })
            .collect();
        self.s.import_legacy(&rows).unwrap();
    }
    fn seq_of(&self, id: &str) -> i64 {
        self.s.get(id).unwrap().seq
    }
}

/// Imported, then native rows with noise runs of 0, 1, 12 and 130 updates between them (130 > ten pages of the smallest page sizes and of 3, 40 and 100... 
/// the last run is longer than 10 pages for every L up to 13).
fn fixture() -> Fixture {
    let mut f = Fixture { s: oracle(), listed: HashSet::new() };
    f.imported("a", 30, NOW - 4000 * MIN);
    let mut prev = f.say("first native", 0);
    for (i, run) in [0usize, 1, 12, 130, 3, 0, 55, 1, 0, 0, 8, 300, 2].into_iter().enumerate() {
        f.noise(&prev, run, 10 + i as i64 * 10);
        for j in 0..(2 + i % 5) {
            prev = f.say(&format!("native {i}.{j}"), 10 + i as i64 * 10 + j as i64 + 1);
        }
    }
    f.imported("b", 25, NOW - 3000 * MIN); // imported AFTER the natives: higher seq, older clock
    f.noise(&prev, 40, 500);
    f
}

/// Follow the cursor from `upper` (exclusive) with page size `limit` until it is null. Returns every returned id in order.
fn walk(f: &Fixture, limit: i64, upper: Option<i64>) -> (Vec<String>, usize) {
    let view = View::parse(None, None);
    let (mut seen, mut before, mut pages, mut empties) = (Vec::new(), upper, 0, 0);
    loop {
        let (rows, _scanned, next) = recent_rows(&f.s, Some(ROOM), limit, before, None, None, &view, NOW + 100_000 * MIN).unwrap();
        pages += 1;
        assert!(pages < 5000, "L={limit}: the cursor chain must end");
        // a page with no visible row but a continuation cursor: the reader must go on, not stop
        if rows.is_empty() && next.is_some() {
            empties += 1;
        }
        let ids: Vec<String> = rows.iter().map(|r| r["id"].as_str().unwrap().to_string()).collect();
        let seqs: Vec<i64> = ids.iter().map(|i| f.seq_of(i)).collect();
        if let Some(b) = next {
            // never above what this page returned, and (checked below) nothing listed in between
            if let Some(min) = seqs.iter().min() {
                assert!(b <= *min, "L={limit}: next_before {b} is above the lowest returned seq {min}");
                let between: Vec<_> = f.listed.iter().filter(|id| { let q = f.seq_of(id); q > b && q < *min }).collect();
                assert!(between.is_empty(), "L={limit}: listed rows lost between the cursor {b} and the page's lowest seq {min}: {between:?}");
            }
            if let Some(prev) = before {
                assert!(b < prev, "L={limit}: the cursor must move down ({prev} -> {b})");
            }
        }
        seen.extend(ids);
        match next {
            Some(b) => before = Some(b),
            None => break,
        }
    }
    (seen, empties)
}

#[test]
fn following_next_before_returns_every_listed_row_below_the_bound_once_for_every_page_size() {
    let f = fixture();
    let max_seq = f.listed.iter().map(|i| f.seq_of(i)).max().unwrap();
    for upper in [None, Some(max_seq + 1000), Some(max_seq - 40), Some(max_seq / 2)] {
        let want: BTreeMap<i64, String> = f.listed.iter().map(|i| (f.seq_of(i), i.clone())).filter(|(q, _)| upper.is_none_or(|u| *q < u)).collect();
        for limit in [1i64, 3, 7, 40, 100, 500] {
            let (got, empties) = walk(&f, limit, upper);
            if upper.is_none() && limit <= 7 {
                assert!(empties > 0, "L={limit}: the fixture must exercise the empty-page-with-cursor path (got {empties})");
            }
            let unique: HashSet<_> = got.iter().collect();
            assert_eq!(unique.len(), got.len(), "L={limit} U={upper:?}: a row came back twice");
            let got_seqs: Vec<i64> = got.iter().map(|i| f.seq_of(i)).collect();
            assert!(got_seqs.windows(2).all(|w| w[0] > w[1]), "L={limit} U={upper:?}: newest first, strictly");
            let missing: Vec<_> = want.iter().filter(|(_, id)| !unique.contains(id)).map(|(q, _)| *q).collect();
            assert!(missing.is_empty(), "L={limit} U={upper:?}: {} listed rows never returned, e.g. seq {:?}", missing.len(), &missing[..missing.len().min(5)]);
            assert_eq!(got.len(), want.len(), "L={limit} U={upper:?}: exactly the listed rows, nothing else");
        }
    }
}

/// A page that fills is exactly the page size and its cursor is its last row. The newest rows of the fixture are state updates, which are never listed, so the
/// first page can legitimately be EMPTY with a cursor (the walk spends its ten-page budget on them): start inside the native rows.
#[test]
fn a_page_that_fills_is_exactly_the_page_size_and_its_cursor_is_its_last_row() {
    let f = fixture();
    let view = View::parse(None, None);
    let start = f.listed.iter().map(|i| f.seq_of(i)).filter(|q| *q > 100).max().unwrap() + 1;
    for limit in [1i64, 3, 40, 100] {
        let (rows, _scanned, next) = recent_rows(&f.s, Some(ROOM), limit, Some(start), None, None, &view, NOW + 100_000 * MIN).unwrap();
        assert_eq!(rows.len() as i64, limit, "L={limit}");
        let last = f.seq_of(rows.last().unwrap()["id"].as_str().unwrap());
        assert_eq!(next, Some(last), "L={limit}: the cursor is the last row returned, not the last row scanned");
    }
}

/// An empty first page is legal (all ten budgeted pages were state updates) but the cursor must still move the reader on, never read as "nothing here".
#[test]
fn an_empty_page_over_nothing_but_updates_still_hands_back_a_cursor_that_moves_down() {
    let f = fixture();
    let view = View::parse(None, None);
    let (rows, scanned, next) = recent_rows(&f.s, Some(ROOM), 1, None, None, None, &view, NOW + 10_000 * MIN).unwrap();
    assert!(rows.is_empty() && scanned > 0, "the head of the fixture is updates only: a page of one row finds nothing in ten pages");
    let max_seq = f.listed.iter().map(|i| f.seq_of(i)).max().unwrap();
    assert!(next.is_some_and(|b| b > 0), "an empty page keeps a cursor so the reader continues: {next:?} (newest listed seq {max_seq})");
}

/// NEGATIVE CONTROL: the same walk with the cursor taken from the last row SCANNED (what the live server did before #74) loses listed rows on this fixture,
/// so the property above is not vacuous. Raw order (every row, listed or not) is read straight from the store.
#[test]
fn the_property_catches_a_cursor_taken_from_the_last_row_scanned() {
    let f = fixture();
    let view = View::parse(None, None);
    let limit = 3;
    let (mut seen, mut before) = (HashSet::new(), None);
    for _ in 0..5000 {
        let (rows, scanned, _next) = recent_rows(&f.s, Some(ROOM), limit, before, None, None, &view, NOW + 10_000 * MIN).unwrap();
        seen.extend(rows.iter().map(|r| r["id"].as_str().unwrap().to_string()));
        let raw = f.s.recent_filtered(Some(ROOM), 100_000, before, None, None).unwrap();
        // the OLD cursor: the seq of the last row the walk scanned
        match raw.get((scanned as usize).saturating_sub(1)) {
            Some(o) if scanned > 0 && raw.len() > scanned as usize => before = Some(o.seq),
            _ => break,
        }
    }
    let lost = f.listed.iter().filter(|id| !seen.contains(*id)).count();
    assert!(lost > 0, "a scanned-row cursor must lose listed rows on this fixture (lost {lost}); otherwise the fixture proves nothing");
}

/// More than ten storage pages of updates for the LARGE page sizes too (10 x 500 rows): a page of 100 or 500 can come back empty with a cursor as well.
/// The updates are spread over 40 different messages: a run of thousands on ONE message would make every write re-judge its whole thread.
#[test]
fn a_run_of_updates_longer_than_the_walk_budget_at_large_page_sizes_is_crossed_without_loss() {
    let mut f = Fixture { s: oracle(), listed: HashSet::new() };
    f.imported("a", 10, NOW - 4000 * MIN);
    let early: Vec<String> = (0..46).map(|i| f.say(&format!("early {i}"), i)).collect();
    // the NEWEST rows are updates only: the first page of any size finds nothing in its ten storage pages
    for k in 0..5200 {
        f.noise(&early[k % 40], 1, 100 + k as i64);
    }
    let want = f.listed.clone();
    for limit in [100i64, 500] {
        let (got, empties) = walk(&f, limit, None);
        let unique: HashSet<&String> = got.iter().collect();
        assert_eq!(unique.len(), got.len(), "L={limit}: a row came back twice");
        assert!(empties > 0, "L={limit}: 5200 updates must produce an empty page with a cursor (got {empties})");
        assert_eq!(got.len(), want.len(), "L={limit}: every listed row, once");
        assert!(want.iter().all(|id| unique.contains(id)), "L={limit}: a listed row was lost across the run");
    }
}

// ── the room routes and the keyword search page by the same rule ─────────────────────────────────────────────────────────────────────────────────────────

use oracle_server::roomread::{messages, search, PageQ, SearchQ};

/// Follow `next_before` of a list route until it is null; every item id in order. `page` returns the whole `{items, next_before}` answer.
fn follow(limit: i64, mut page: impl FnMut(Option<i64>) -> serde_json::Value) -> Vec<(i64, String)> {
    let (mut out, mut before, mut pages) = (Vec::new(), None, 0);
    loop {
        let v = page(before);
        pages += 1;
        assert!(pages < 5000, "L={limit}: the cursor chain must end");
        let items = v["items"].as_array().cloned().unwrap_or_default();
        let rows: Vec<(i64, String)> = items.iter().map(|i| (i["seq"].as_i64().unwrap(), i["id"].as_str().unwrap().to_string())).collect();
        if let (Some(b), Some(min)) = (v["next_before"].as_i64(), rows.iter().map(|r| r.0).min()) {
            assert!(b <= min, "L={limit}: next_before {b} above the lowest returned seq {min}");
        }
        out.extend(rows);
        match v["next_before"].as_i64() {
            Some(b) => before = Some(b),
            None => break,
        }
    }
    out
}

#[test]
fn the_room_messages_route_pages_without_loss_or_repeats() {
    let f = fixture();
    // every message of the room, from the store itself, not from the route under test
    let want: HashSet<String> = f.s.recent_filtered(Some(ROOM), 1_000_000, None, None, None).unwrap().into_iter().map(|o| o.id).collect();
    for limit in [1i64, 3, 40, 100] {
        let got = follow(limit, |before| messages(&f.s, ROOM, &PageQ { limit: Some(limit), before, ..PageQ::default() }).unwrap());
        let ids: Vec<&String> = got.iter().map(|r| &r.1).collect();
        let unique: HashSet<_> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "L={limit}: a message came back twice");
        assert!(got.windows(2).all(|w| w[0].0 > w[1].0), "L={limit}: newest first, strictly");
        let missing = want.iter().filter(|id| !unique.contains(id)).count();
        assert_eq!(missing, 0, "L={limit}: {missing} of the room's {} messages never came back", want.len());
    }
}

#[test]
fn the_keyword_search_in_newest_order_pages_without_loss_or_repeats() {
    let f = fixture();
    let want: HashSet<String> = f.listed.iter().filter(|id| f.s.get(id).unwrap().content.contains("native")).cloned().collect();
    assert!(want.len() > 20);
    for limit in [1i64, 3, 7, 40] {
        let got = follow(limit, |before| search(&f.s, &SearchQ { q: "native".into(), room: Some(ROOM.into()), limit: Some(limit), before, order: Some("newest".into()), ..SearchQ::default() }).unwrap());
        let unique: HashSet<&String> = got.iter().map(|r| &r.1).collect();
        assert_eq!(unique.len(), got.len(), "L={limit}: a match came back twice");
        let missing: Vec<_> = want.iter().filter(|id| !unique.contains(id)).collect();
        assert!(missing.is_empty(), "L={limit}: {} matches never came back", missing.len());
        assert_eq!(unique.len(), want.len(), "L={limit}: only the matches");
    }
}
