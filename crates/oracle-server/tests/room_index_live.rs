//! Step 2 of docs/ROOM-INDEX.md, by hand, on a COPY of the live ledger (never the live file):
//!   ORACLE_INDEX_DB=/cache/live3/oracle.db cargo test -p oracle-server --release --test room_index_live -- --ignored --nocapture
//! It builds the index, holds it to today's read-time fold for every message of every thread, and times what the routes of step 3 will ask
//! the tables, and what the projection adds to a write.

use oracle_server::projection::{self, Item, State};
use oracle_server::store::Store;
use rusqlite::{params, Connection};
use std::collections::BTreeMap;
use std::time::Instant;

fn pct(mut v: Vec<f64>, p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}
fn report(what: &str, v: Vec<f64>) {
    println!("  {what:<44} n={:<5} p50 {:>7.2} ms   p99 {:>7.2} ms   max {:>7.2} ms", v.len(), pct(v.clone(), 0.5), pct(v.clone(), 0.99), pct(v.clone(), 1.0));
}
fn time<T>(mut f: impl FnMut() -> T) -> f64 {
    let t = Instant::now();
    let _ = f();
    t.elapsed().as_secs_f64() * 1000.0
}

#[test]
#[ignore]
fn the_index_on_the_live_ledger() {
    let Ok(db) = std::env::var("ORACLE_INDEX_DB") else { return };
    let s = Store::open(&db).unwrap();

    let t = Instant::now();
    let stats = s.reindex().unwrap();
    println!("reindex: {} messages, {} threads in {:.1?}", stats.messages, stats.threads, t.elapsed());

    // ---- parity: every message of every thread, the index against judge_in over the fold's own thread
    let roots: Vec<(i64, String, i64)> = s
        .with_conn(|c| {
            let mut st = c.prepare("SELECT t.root_seq, t.channel, (SELECT count(*) FROM msg m WHERE m.root_seq=t.root_seq) FROM thread t WHERE t.state<>'history' ORDER BY t.root_seq").unwrap();
            Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(|x| x.unwrap()).collect())
        })
        .unwrap();
    let (mut threads, mut messages, mut clean_threads) = (0usize, 0usize, 0usize);
    let mut differ: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut examples: Vec<String> = Vec::new();
    let mut skipped_big = 0usize;
    let mut roots_cache: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let t = Instant::now();
    for (root_seq, channel, members_in_index) in &roots {
        let id: String = s.with_conn(|c| Ok(c.query_row("SELECT id FROM observations WHERE seq=?1", params![root_seq], |r| r.get(0)).unwrap())).unwrap();
        let Ok(root) = s.get(&id) else { continue };
        let members = s.thread_members(&root.id, 300).unwrap();
        // The fold reads a thread 4 links deep and 300 messages: a thread the index holds more of than that cannot be compared whole.
        if *members_in_index > 250 {
            skipped_big += 1;
            continue;
        }
        let all: Vec<&oracle_server::store::Observation> = std::iter::once(&root).chain(members.iter()).collect();
        let items: Vec<Item> = all.iter().map(|o| Item { id: &o.id, author: &o.author, created_ms: o.created_ms, tags: &o.tags }).collect();
        let deciders = roots_cache.entry(channel.clone()).or_insert_with(|| s.room_roots(Some(channel))).clone();
        let verdicts = projection::judge_in(&items, 0, &deciders);
        threads += 1;
        let mut clean = true;
        let mut bump = |k: &'static str, ex: String, clean: &mut bool| {
            *differ.entry(k).or_default() += 1;
            *clean = false;
            if examples.len() < 12 {
                examples.push(format!("{k}: {ex}"));
            }
        };
        for o in &all {
            let row: Option<(String, String, Option<i64>, i64)> = s
                .with_conn(|c| Ok(c.query_row("SELECT intent, ask, by_ms, root_seq FROM msg WHERE seq=?1", params![o.seq], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).ok()))
                .unwrap();
            let Some((intent, ask, by_ms, r)) = row else { continue };
            if r != *root_seq {
                continue; // judged with this thread as a resolver, indexed under its own
            }
            messages += 1;
            let v = &verdicts[&o.id];
            if intent != v.intent.as_str() {
                bump("intent", format!("{} index {intent} fold {}", o.id, v.intent.as_str()), &mut clean);
            }
            if ask != v.ask.as_str() {
                bump("ask", format!("{} index {ask} fold {}", o.id, v.ask.as_str()), &mut clean);
            }
            if by_ms != v.by_ms {
                bump("by_ms", o.id.clone(), &mut clean);
            }
            let n_items: i64 = s.with_conn(|c| Ok(c.query_row("SELECT count(*) FROM item WHERE msg_seq=?1", params![o.seq], |r| r.get(0)).unwrap())).unwrap();
            if n_items as usize != v.items.len() {
                bump("items", format!("{} index {n_items} fold {}", o.id, v.items.len()), &mut clean);
            }
            let n_marks: i64 = s.with_conn(|c| Ok(c.query_row("SELECT count(*) FROM mark WHERE msg_seq=?1", params![o.seq], |r| r.get(0)).unwrap())).unwrap();
            let want = v.marks.seen.len() + v.marks.go.len() + v.marks.like.len() + v.marks.no.len();
            if n_marks as usize != want {
                bump("marks", format!("{} index {n_marks} fold {want}", o.id), &mut clean);
            }
        }
        let rv = &verdicts[&root.id];
        let (state, settled): (String, Option<String>) =
            s.with_conn(|c| Ok(c.query_row("SELECT state, settled_word FROM thread WHERE root_seq=?1", params![root_seq], |r| Ok((r.get(0)?, r.get(1)?))).unwrap())).unwrap();
        let want = if rv.settled.is_some() {
            "settled"
        } else {
            match rv.state {
                State::Closed => "closed",
                State::Superseded => "superseded",
                State::Archived => "archived",
                State::Expired => "expired",
                State::Ambient => "ambient",
                State::Open if rv.answered_by.is_some() => "answered",
                State::Open => "open",
            }
        };
        if state != want {
            bump("thread.state", format!("root {root_seq} index {state} fold {want}"), &mut clean);
        }
        if settled.as_deref() != rv.settled {
            bump("thread.settled", format!("root {root_seq}"), &mut clean);
        }
        if clean {
            clean_threads += 1;
        }
    }
    println!("parity: {threads} threads compared ({skipped_big} over 250 messages skipped: the fold reads at most 300), {messages} messages, {clean_threads} threads identical, in {:.1?}", t.elapsed());
    println!("  differences by field: {differ:?}");
    for e in &examples {
        println!("    {e}");
    }

    // ---- speed: what the routes of step 3 will ask the tables
    let conn = |f: &dyn Fn(&Connection)| s.with_conn(|c| {
        f(c);
        Ok(())
    }).unwrap();
    let rooms: Vec<(String, i64)> = s.with_conn(|c| {
        let mut st = c.prepare("SELECT channel, count(*) n FROM msg GROUP BY channel ORDER BY n DESC LIMIT 12").unwrap();
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect())
    }).unwrap();
    println!("speed, on {} rooms (biggest {} with {}):", rooms.len(), rooms[0].0, rooms[0].1);
    let mut page = vec![];
    let mut threadlist = vec![];
    let mut unread = vec![];
    for (room, _) in &rooms {
        for round in 0..20 {
            // the first page, then pages further back
            let before: i64 = s.with_conn(|c| Ok(c.query_row("SELECT max(seq) - (?2 * 137) FROM msg WHERE channel=?1", params![room, round], |r| r.get(0)).unwrap())).unwrap();
            page.push(time(|| conn(&|c| {
                let mut st = c.prepare_cached("SELECT m.seq,m.author,m.class,m.root_seq,m.intent,m.ask,o.id,o.content,o.created_ms FROM msg m JOIN observations o ON o.seq=m.seq WHERE m.channel=?1 AND m.seq<?2 ORDER BY m.seq DESC LIMIT 100").unwrap();
                let _: Vec<i64> = st.query_map(params![room, before], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
            })));
            threadlist.push(time(|| conn(&|c| {
                let mut st = c.prepare_cached("SELECT t.root_seq,t.state,t.replies,t.last_seq,t.participants FROM thread t WHERE t.channel=?1 AND t.state IN ('open','answered') ORDER BY t.last_seq DESC LIMIT 100").unwrap();
                let _: Vec<i64> = st.query_map(params![room], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
            })));
            unread.push(time(|| conn(&|c| {
                let _: i64 = c.query_row("SELECT count(*) FROM (SELECT 1 FROM msg WHERE channel=?1 AND seq>?2 AND class='message' LIMIT 1000)", params![room, before], |r| r.get(0)).unwrap();
            })));
        }
    }
    report("room page (100 messages, joined to the log)", page);
    report("room threads (open or answered, 100)", threadlist);
    report("room unread count", unread);
    let big: Vec<i64> = s.with_conn(|c| {
        let mut st = c.prepare("SELECT root_seq FROM thread ORDER BY replies DESC LIMIT 40").unwrap();
        Ok(st.query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect())
    }).unwrap();
    let mut th = vec![];
    for root in &big {
        th.push(time(|| conn(&|c| {
            let mut st = c.prepare_cached("SELECT m.seq,m.author,m.class,m.depth,o.content FROM msg m JOIN observations o ON o.seq=m.seq WHERE m.root_seq=?1 ORDER BY m.seq").unwrap();
            let _: Vec<i64> = st.query_map(params![root], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
        })));
    }
    let biggest: i64 = s.with_conn(|c| Ok(c.query_row("SELECT max(replies) FROM thread", [], |r| r.get(0)).unwrap())).unwrap();
    report(&format!("a thread, the 40 biggest (largest {biggest} replies)"), th);
    let authors: Vec<String> = s.with_conn(|c| {
        let mut st = c.prepare("SELECT author FROM item GROUP BY author ORDER BY count(*) DESC LIMIT 10").unwrap();
        Ok(st.query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect())
    }).unwrap();
    let mut asked = vec![];
    for a in authors.iter().cycle().take(100) {
        asked.push(time(|| conn(&|c| {
            let mut st = c.prepare_cached("SELECT i.msg_seq,i.k,i.kind,i.state,i.owner FROM item i WHERE i.author=?1 AND i.state NOT IN ('accepted','dropped') ORDER BY i.msg_seq DESC LIMIT 100").unwrap();
            let _: Vec<i64> = st.query_map(params![a], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
        })));
    }
    report("asked: a seat's open items (100)", asked);
    let mut owned = vec![];
    for a in authors.iter().cycle().take(100) {
        owned.push(time(|| conn(&|c| {
            let mut st = c.prepare_cached("SELECT i.msg_seq,i.k,i.state FROM item i WHERE i.owner=?1 AND i.state NOT IN ('accepted','dropped') ORDER BY i.msg_seq DESC LIMIT 100").unwrap();
            let _: Vec<i64> = st.query_map(params![a], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
        })));
    }
    report("items a seat owns (100)", owned);
    let mut search = vec![];
    for (i, w) in ["oracle", "ledger", "deploy", "card", "review", "sqlite", "board", "thread", "kernel", "index"].iter().cycle().take(60).enumerate() {
        let room = &rooms[i % rooms.len()].0;
        search.push(time(|| conn(&|c| {
            let mut st = c.prepare_cached("SELECT o.seq FROM obs_fts f JOIN observations o ON o.seq=f.rowid JOIN msg m ON m.seq=o.seq WHERE obs_fts MATCH ?1 AND m.channel=?2 ORDER BY rank LIMIT 50").unwrap();
            let _: Vec<i64> = st.query_map(params![w, room], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
        })));
    }
    report("search, before: a word, then the room joined after", search);
    // after: the room is a token of the search row, so only that room's rows are matched and ranked
    let mut search2 = vec![];
    for (i, w) in ["oracle", "ledger", "deploy", "card", "review", "sqlite", "board", "thread", "kernel", "index"].iter().cycle().take(60).enumerate() {
        let room = &rooms[i % rooms.len()].0;
        let token = oracle_server::roomindex::fts_tokens(room, "message");
        let ch = token.split_once(' ').unwrap().0.to_owned();
        let q = format!("{w} AND tags:{ch}");
        search2.push(time(|| conn(&|c| {
            let mut st = c.prepare_cached("SELECT rowid FROM obs_fts WHERE obs_fts MATCH ?1 ORDER BY rank LIMIT 50").unwrap();
            let _: Vec<i64> = st.query_map(params![q], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
        })));
    }
    report("search, after: the room as a token of the row", search2);
    let by_class: Vec<(String, i64)> = s.with_conn(|c| {
        let mut st = c.prepare("SELECT class, count(*) FROM msg GROUP BY class ORDER BY 2 DESC").unwrap();
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect())
    }).unwrap();
    println!("  msg rows by class: {by_class:?}");

    // ---- the routes themselves (step 3b): the functions GET /rooms/{ch}/messages, /rooms/{ch}/threads and /threads/{root} call, whole replies
    use oracle_server::roomread::{self as rr, PageQ};
    let (mut r_first, mut r_back, mut r_after, mut r_threads, mut r_thread, mut r_cargo) = (vec![], vec![], vec![], vec![], vec![], vec![]);
    let mut chain_ok = 0usize;
    for (room, n) in &rooms {
        r_first.push(time(|| rr::messages(&s, room, &PageQ { limit: Some(50), ..Default::default() }).unwrap()));
        r_first.push(time(|| rr::messages(&s, room, &PageQ { limit: Some(200), ..Default::default() }).unwrap()));
        r_cargo.push(time(|| rr::messages(&s, room, &PageQ { limit: Some(50), include_cargo: true, ..Default::default() }).unwrap()));
        let mid: i64 = s.with_conn(|c| Ok(c.query_row("SELECT seq FROM msg WHERE channel=?1 ORDER BY seq LIMIT 1 OFFSET ?2", params![room, n / 2], |r| r.get(0)).unwrap())).unwrap();
        r_back.push(time(|| rr::messages(&s, room, &PageQ { before: Some(mid), limit: Some(100), ..Default::default() }).unwrap()));
        r_after.push(time(|| rr::messages(&s, room, &PageQ { after: Some(mid), limit: Some(100), ..Default::default() }).unwrap()));
        r_threads.push(time(|| rr::threads(&s, room, &PageQ { limit: Some(50), ..Default::default() }).unwrap()));
        // the chain to the first message, in pages of 200: the count must equal what the index holds
        let (mut total, mut before) = (0i64, None);
        loop {
            let v = rr::messages(&s, room, &PageQ { before, limit: Some(200), ..Default::default() }).unwrap();
            total += v["count"].as_i64().unwrap();
            match v["next_before"].as_i64() { Some(b) => before = Some(b), None => break }
        }
        if total == *n { chain_ok += 1; }
    }
    for root in &big {
        r_thread.push(time(|| rr::thread(&s, &root.to_string(), &PageQ { limit: Some(200), ..Default::default() }).unwrap()));
    }
    println!("routes (whole replies, items built with their marks, votes, supersession and thread; biggest room {} rows):", rooms[0].1);
    report("GET /rooms/{ch}/messages, newest 50 and 200", r_first);
    report("  … with include_cargo, 50", r_cargo);
    report("  … 100 back from the middle of the room", r_back);
    report("  … 100 forward from the middle (after=)", r_after);
    report("GET /rooms/{ch}/threads, 50", r_threads);
    report("GET /threads/{root}, the 40 biggest", r_thread);
    // step 3c: the request ledger and unread, whole replies
    let seats: Vec<String> = s.with_conn(|c| {
        let mut st = c.prepare("SELECT author FROM item GROUP BY author ORDER BY count(*) DESC LIMIT 12").unwrap();
        Ok(st.query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect())
    }).unwrap();
    let (mut r_asked, mut r_asked_open, mut r_unread) = (vec![], vec![], vec![]);
    let mut ledger_rows = 0i64;
    for seat in &seats {
        r_asked.push(time(|| { let v = rr::asked(&s, seat, &PageQ { limit: Some(50), ..Default::default() }).unwrap(); ledger_rows += v["item_count"].as_i64().unwrap(); }));
        r_asked_open.push(time(|| rr::asked(&s, seat, &PageQ { limit: Some(50), state: vec!["open".into(), "building".into(), "built".into(), "blocked".into()], ..Default::default() }).unwrap()));
        for (room, _) in rooms.iter().take(4) {
            r_unread.push(time(|| rr::unread(&s, room, seat).unwrap()));
        }
    }
    report("GET /asked, 50 messages, the busiest seats", r_asked);
    report("GET /asked, only unsettled items", r_asked_open);
    report("GET /rooms/{ch}/unread, busiest rooms x seats", r_unread);
    println!("  (the ledgers held {ledger_rows} items in the first pages; item.text filled: {})", s.with_conn(|c| Ok(c.query_row("SELECT count(*) FROM item WHERE text IS NOT NULL", [], |r| r.get::<_, i64>(0)).unwrap())).unwrap());
    // step 3d: the search page itself (room token, items built, score), newest and ranked
    let (mut r_sn, mut r_sr, mut r_sa) = (vec![], vec![], vec![]);
    for (i, w) in ["oracle", "ledger", "deploy", "card", "review", "sqlite", "board", "thread", "kernel", "index"].iter().cycle().take(60).enumerate() {
        let room = rooms[i % rooms.len()].0.clone();
        r_sn.push(time(|| rr::search(&s, &rr::SearchQ { q: (*w).into(), room: Some(room.clone()), limit: Some(50), ..Default::default() }).unwrap()));
        r_sr.push(time(|| rr::search(&s, &rr::SearchQ { q: (*w).into(), room: Some(room.clone()), limit: Some(50), order: Some("rank".into()), ..Default::default() }).unwrap()));
        r_sa.push(time(|| rr::search(&s, &rr::SearchQ { q: (*w).into(), room: Some(room.clone()), author: Some("rocky".into()), class: vec!["message".into(), "status".into()], limit: Some(50), ..Default::default() }).unwrap()));
    }
    report("GET /search page, a word in a room, newest 50", r_sn);
    report("GET /search page, a word in a room, ranked 50", r_sr);
    report("GET /search page, room + author + classes", r_sa);
    let mut r_all = vec![];
    for w in ["oracle", "ledger", "deploy", "card", "review", "sqlite"] {
        r_all.push(time(|| rr::search(&s, &rr::SearchQ { q: w.into(), limit: Some(50), ..Default::default() }).unwrap()));
    }
    report("GET /search page, a word across every room, newest 50", r_all);
    println!("  page chains to the first message: {chain_ok} of {} rooms return exactly the rows the index holds", rooms.len());

    // ---- write: what the projection adds to a message that arrives (rows inserted as the store does, then projected, one transaction each)
    let mut plain = vec![];
    let mut projected = vec![];
    let mut next: i64 = s.with_conn(|c| Ok(c.query_row("SELECT max(seq) FROM observations", [], |r| r.get(0)).unwrap())).unwrap();
    let targets: Vec<(String, String)> = s.with_conn(|c| {
        let mut st = c.prepare("SELECT o.id, o.channel FROM msg m JOIN observations o ON o.seq=m.seq WHERE m.class='message' AND m.channel IN ('axxis','hustler','rocky','oracle') ORDER BY m.seq DESC LIMIT 400").unwrap();
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect())
    }).unwrap();
    for (i, (parent, room)) in targets.iter().enumerate() {
        for with_index in [false, true] {
            next += 1;
            let id = format!("OBS-{room}-99999999-{next:012}");
            let tags = serde_json::to_string(&[format!("re:{parent}"), "intent:note".to_string()]).unwrap();
            let ms = time(|| {
                s.with_conn(|c| {
                    c.execute_batch("BEGIN IMMEDIATE").unwrap();
                    c.execute(
                        "INSERT INTO observations(seq,id,channel,author,kind,content,cargo,tags_json,created_ms,ts_ms,nonce,client_op,pubkey,sig,envelope,news) VALUES(?1,?2,?3,'probe','say','a reply written by the timing test',NULL,?4,?5,?5,'','op-'||?2,'','',2,1)",
                        params![next, id, room, tags, 1_900_000_000_000i64 + i as i64],
                    ).unwrap();
                    c.execute("INSERT OR IGNORE INTO tags(name,obs_seq) VALUES(?1,?2)", params![format!("re:{parent}"), next]).unwrap();
                    if with_index {
                        oracle_server::roomindex::project_new(c, next).unwrap();
                    }
                    c.execute_batch("COMMIT").unwrap();
                    Ok(())
                }).unwrap()
            });
            if with_index { projected.push(ms) } else { plain.push(ms) }
        }
    }
    report("write: insert + tags, no projection", plain);
    report("write: the same with the projection", projected);
}
