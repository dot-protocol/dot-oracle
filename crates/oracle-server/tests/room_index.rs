//! The room index (docs/ROOM-INDEX.md), step 1: the tables, filled in the transaction of each write, rebuilt by `reindex`.
//! Real signed writes on the real store. Three proofs: what a row says, that `reindex` leaves exactly what the write path
//! left, and that the index agrees with the read-time fold (`judge_in`) about every message of every thread.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::projection::{self, Item, State};
use oracle_server::store::{LegacyRow, Principal, Scope, Store};
use rusqlite::params;

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn key_of(who: &str) -> u8 {
    match who {
        "blaze" => 1,
        "rocky" => 2,
        "dinesh" => 3,
        _ => 4,
    }
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    for (h, k) in [("rocky", 2), ("dinesh", 3), ("kepler", 4)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    }
    s
}
/// One write; `mins` is how many minutes after the scenario began it happens.
#[allow(clippy::too_many_arguments)]
fn put(s: &Store, room: &str, who: &str, kind: Kind, text: &str, cargo: Option<&str>, tags: &[&str], mins: i64) -> String {
    let at = NOW + mins * MIN;
    let env = Envelope::sign(&sk(key_of(who)), room, who, kind, text, cargo.map(str::to_owned), tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn say(s: &Store, room: &str, who: &str, text: &str, tags: &[&str], mins: i64) -> String {
    put(s, room, who, Kind::Say, text, None, tags, mins)
}
fn seq(s: &Store, id: &str) -> i64 {
    s.get(id).unwrap().seq
}
fn q<T: rusqlite::types::FromSql>(s: &Store, sql: &str, p: &[&dyn rusqlite::ToSql]) -> T {
    s.with_conn(|c| Ok(c.query_row(sql, p, |r| r.get(0)).unwrap())).unwrap()
}
fn msg_row(s: &Store, id: &str) -> (String, i64, Option<i64>, i64) {
    let n = seq(s, id);
    s.with_conn(|c| Ok(c.query_row("SELECT class, root_seq, parent_seq, depth FROM msg WHERE seq=?1", params![n], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap())).unwrap()
}

#[test]
fn a_message_knows_its_thread_its_parent_and_its_depth() {
    let s = oracle();
    let root = say(&s, "core", "rocky", "should we ship?", &["intent:question", "to:dinesh"], 0);
    let r1 = say(&s, "core", "dinesh", "yes", &[&format!("re:{root}")], 1);
    let r2 = say(&s, "core", "rocky", "when?", &[&format!("reply:{r1}")], 2);
    let alone = say(&s, "core", "rocky", "unrelated", &[], 3);
    let dangling = say(&s, "core", "rocky", "answers a ghost", &["re:OBS-core-19700101-000000000000"], 4);
    assert_eq!(msg_row(&s, &root), ("message".into(), seq(&s, &root), None, 0));
    assert_eq!(msg_row(&s, &r1), ("message".into(), seq(&s, &root), Some(seq(&s, &root)), 1));
    assert_eq!(msg_row(&s, &r2), ("message".into(), seq(&s, &root), Some(seq(&s, &r1)), 2), "a reply to a reply stays in the thread");
    assert_eq!(msg_row(&s, &alone).1, seq(&s, &alone));
    assert_eq!(msg_row(&s, &dangling), ("message".into(), seq(&s, &dangling), None, 0), "a link that points nowhere ends where it stands");
    let n: i64 = q(&s, "SELECT count(*) FROM thread", &[]);
    assert_eq!(n, 3, "one thread row per root");
    let root_n = seq(&s, &root);
    let t: (i64, i64, String) = s.with_conn(|c| Ok(c.query_row("SELECT replies, last_seq, participants FROM thread WHERE root_seq=?1", params![root_n], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap())).unwrap();
    assert_eq!((t.0, t.1, t.2.as_str()), (2, seq(&s, &r2), "[\"rocky\",\"dinesh\"]"), "replies counted, the newest, and who spoke in the order they first did");
}

#[test]
fn each_kind_of_write_has_its_class() {
    let s = oracle();
    let m = say(&s, "core", "rocky", "hello", &[], 0);
    let mark = say(&s, "core", "dinesh", "✓", &[&format!("re:{m}"), "react:go"], 1);
    let note = put(&s, "core", "dinesh", Kind::Annotate, "why", None, &[&format!("annotates:{m}"), "set:intent=proposal"], 2);
    let mark2 = put(&s, "core", "kepler", Kind::Annotate, "go", None, &[&format!("annotates:{m}"), "set:mark=go"], 3);
    let card = say(&s, "core", "rocky", "[Core task:x] queue: X", &["schema:room-task/1", "intent:status"], 4);
    let status = say(&s, "core", "rocky", "shipped", &["intent:status"], 5);
    let st = put(&s, "core", "rocky", Kind::State, "head", Some(r#"{"subject":"core.x","state":"running","owner":"rocky","next_touch":"2026-10-01"}"#), &["subject:core.x", "state:running", "owner:rocky"], 6);
    for (id, want) in [(&m, "message"), (&mark, "mark"), (&note, "annotation"), (&mark2, "mark"), (&card, "card"), (&status, "status"), (&st, "state")] {
        assert_eq!(msg_row(&s, id).0, want, "{id}");
    }
    assert_eq!(msg_row(&s, &note).1, seq(&s, &m), "an annotation belongs to the thread of its message");
    assert_eq!(msg_row(&s, &mark).1, seq(&s, &m));
}

#[test]
fn a_thread_moves_from_open_to_answered_to_closed_and_a_deciders_go_settles_it() {
    let s = oracle();
    // the room's deciders: only an owner-key seat's record counts
    put(&s, "core", "blaze", Kind::Say, "[Core members] roots", Some(r#"{"schema":"room-members/1","roots":["blaze"]}"#), &[], -10);
    let word = |root: &str| -> String { q(&s, "SELECT state FROM thread WHERE root_seq=?1", &[&seq(&s, root)]) };
    let q1 = say(&s, "core", "rocky", "which port?", &["intent:question", "ask:reply", "to:dinesh"], 0);
    assert_eq!(word(&q1), "open");
    let a = say(&s, "core", "dinesh", "8900", &[&format!("re:{q1}")], 1);
    assert_eq!(word(&q1), "answered");
    let ans: Option<i64> = q(&s, "SELECT answered_by FROM thread WHERE root_seq=?1", &[&seq(&s, &q1)]);
    assert_eq!(ans, Some(seq(&s, &a)));
    say(&s, "core", "rocky", "done", &[&format!("closes:{q1}")], 2);
    assert_eq!(word(&q1), "closed", "a close from inside the thread");
    // a proposal a decider says go to
    let p = say(&s, "core", "rocky", "let us ship friday", &["intent:proposal"], 3);
    assert_eq!(word(&p), "open");
    let go = say(&s, "core", "blaze", "✓", &[&format!("re:{p}"), "react:go"], 4);
    assert_eq!(word(&p), "settled");
    let p_n = seq(&s, &p);
    let (by, w): (i64, String) = s.with_conn(|c| Ok(c.query_row("SELECT settled_by, settled_word FROM thread WHERE root_seq=?1", params![p_n], |r| Ok((r.get(0)?, r.get(1)?))).unwrap())).unwrap();
    assert_eq!((by, w.as_str()), (seq(&s, &go), "accepted"));
    // a non-decider's go is an opinion: counted as a mark, settles nothing
    let p2 = say(&s, "core", "rocky", "or saturday", &["intent:proposal"], 5);
    say(&s, "core", "kepler", "✓", &[&format!("re:{p2}"), "react:go"], 6);
    assert_eq!(word(&p2), "open");
    let marks: i64 = q(&s, "SELECT count(*) FROM mark WHERE msg_seq=?1 AND react='go' AND seat='kepler'", &[&seq(&s, &p2)]);
    assert_eq!(marks, 1);
    // a supersede from another thread reaches back into the old one
    let old = say(&s, "core", "rocky", "plan A", &["intent:proposal"], 7);
    say(&s, "core", "rocky", "plan B", &["intent:proposal", &format!("supersedes:{old}")], 8);
    assert_eq!(word(&old), "superseded");
    // withdrawing a mark removes it
    let mk = say(&s, "core", "dinesh", "✓", &[&format!("re:{p2}"), "react:like"], 9);
    let likes = |n: i64| -> i64 { q(&s, "SELECT count(*) FROM mark WHERE msg_seq=?1 AND react='like'", &[&n]) };
    assert_eq!(likes(seq(&s, &p2)), 1);
    say(&s, "core", "dinesh", "✓", &[&format!("archives:{mk}")], 10);
    assert_eq!(likes(seq(&s, &p2)), 0, "an archived mark is gone");
}

#[test]
fn items_owners_and_votes_are_rows_a_reader_can_ask_for() {
    let s = oracle();
    let m = say(&s, "core", "blaze", "we need A and B", &["item:R1=requirement", "item:R2=requirement", "to:dinesh"], 0);
    say(&s, "core", "dinesh", "on it", &[&format!("re:{m}"), "item:R1=building", "owner:R1=dinesh"], 1);
    let built = say(&s, "core", "dinesh", "R1 built", &[&format!("re:{m}"), "item:R1=built"], 2);
    say(&s, "core", "kepler", "yes to B", &[&format!("re:{m}"), "vote:R2=yes"], 3);
    say(&s, "core", "rocky", "no to B", &[&format!("re:{m}"), "vote:R2=no"], 4);
    say(&s, "core", "kepler", "actually no", &[&format!("re:{m}"), "vote:R2=no"], 5);
    let m_n = seq(&s, &m);
    let it = |k: &str| -> (String, String, Option<String>, Option<i64>, i64) {
        s.with_conn(|c| Ok(c.query_row("SELECT kind, state, owner, moved_by, by_model FROM item WHERE msg_seq=?1 AND k=?2", params![m_n, k], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap())).unwrap()
    };
    assert_eq!(it("R1"), ("requirement".into(), "built".into(), Some("dinesh".into()), Some(seq(&s, &built)), 0));
    assert_eq!(it("R2").1, "open");
    let author: String = q(&s, "SELECT author FROM item WHERE msg_seq=?1 AND k='R1'", &[&seq(&s, &m)]);
    assert_eq!(author, "blaze", "an item carries its message's author, so everything a person asked is one index");
    let votes: Vec<(String, String)> = s.with_conn(|c| {
        let mut st = c.prepare("SELECT seat, stance FROM vote WHERE msg_seq=?1 AND k='R2' ORDER BY seat").unwrap();
        Ok(st.query_map(params![m_n], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect())
    }).unwrap();
    assert_eq!(votes, [("kepler".to_string(), "no".to_string()), ("rocky".to_string(), "no".to_string())], "a seat's newest vote wins");
}

/// A busy room, written in the order things really happen, with every kind of link in it.
fn busy(s: &Store) {
    put(s, "core", "blaze", Kind::Say, "[Core members] roots", Some(r#"{"schema":"room-members/1","roots":["blaze"]}"#), &[], -10);
    let a = say(s, "core", "blaze", "R1 the index, R2 the routes", &["item:R1=requirement", "item:R2=requirement", "to:dinesh", "ask:act"], 0);
    say(s, "core", "dinesh", "taking R1", &[&format!("re:{a}"), "item:R1=building", "owner:R1=dinesh"], 1);
    let b = say(s, "core", "rocky", "which db?", &["intent:question", "ask:reply", "to:kepler"], 2);
    let c = say(s, "core", "kepler", "sqlite", &[&format!("re:{b}")], 3);
    say(s, "core", "blaze", "✓", &[&format!("re:{c}"), "react:go"], 4);
    put(s, "core", "rocky", Kind::Annotate, "it is a decision", None, &[&format!("annotates:{a}"), "set:by=model", "set:intent=decision"], 5);
    put(s, "core", "blaze", Kind::Annotate, "no, a request", None, &[&format!("annotates:{a}"), "set:intent=proposal"], 6);
    let d = say(s, "core", "rocky", "plan A", &["intent:proposal", "ttl:1"], 7);
    say(s, "core", "rocky", "plan B", &["intent:proposal", &format!("supersedes:{d}")], 8);
    let e = say(s, "core", "rocky", "please review", &["ask:review", "ref:abc1234", "to:dinesh", "by:1800003600"], 9);
    say(s, "core", "dinesh", "reviewed", &[&format!("re:{e}")], 10);
    let m = say(s, "core", "kepler", "✓", &[&format!("re:{e}"), "react:like"], 11);
    say(s, "core", "kepler", "✓", &[&format!("archives:{m}")], 12);
    say(s, "core", "rocky", "[Core task:x] queue: X", &["schema:room-task/1", "intent:status"], 13);
    say(s, "core", "rocky", "shipped", &["intent:status"], 14);
    let other = say(s, "ops", "rocky", "in another room", &["intent:question"], 15);
    say(s, "ops", "dinesh", "answered", &[&format!("re:{other}")], 16);
}

#[test]
fn reindex_leaves_exactly_what_the_write_path_left() {
    let s = oracle();
    busy(&s);
    let incremental = s.index_dump().unwrap();
    assert!(incremental.iter().all(|(t, rows)| !rows.is_empty() || t == "vote"), "every projection has rows in a busy room: {incremental:?}");
    let stats = s.reindex().unwrap();
    assert!(stats.messages >= 17 && stats.threads >= 8, "{stats:?}");
    assert_eq!(s.index_dump().unwrap(), incremental, "reindex from nothing equals the incremental projection, row for row");
    s.reindex().unwrap();
    assert_eq!(s.index_dump().unwrap(), incremental, "and again");
    // and the write path carries on from a rebuilt index
    say(&s, "core", "rocky", "more", &[], 30);
    let n: i64 = q(&s, "SELECT count(*) FROM msg", &[]);
    assert_eq!(n, stats.messages as i64 + 1);
}

#[test]
fn the_index_agrees_with_the_read_time_fold_about_every_message() {
    let s = oracle();
    busy(&s);
    let roots: Vec<(i64, String)> = s.with_conn(|c| {
        let mut st = c.prepare("SELECT t.root_seq, t.channel FROM thread t ORDER BY t.root_seq").unwrap();
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect())
    }).unwrap();
    let mut compared = 0;
    for (root_seq, channel) in roots {
        let root = s.thread_root(&s.with_conn(|c| Ok(c.query_row("SELECT id FROM observations WHERE seq=?1", params![root_seq], |r| r.get::<_, String>(0)).unwrap())).unwrap()).unwrap();
        let members = s.thread_members(&root.id, 300).unwrap();
        let all: Vec<&oracle_server::store::Observation> = std::iter::once(&root).chain(members.iter()).collect();
        let items: Vec<Item> = all.iter().map(|o| Item { id: &o.id, author: &o.author, created_ms: o.created_ms, tags: &o.tags }).collect();
        let verdicts = projection::judge_in(&items, 0, &s.room_roots(Some(&channel)));
        for o in &all {
            let v = &verdicts[&o.id];
            let row: Option<(String, String, Option<i64>)> = s.with_conn(|c| Ok(c.query_row("SELECT intent, ask, by_ms FROM msg WHERE seq=?1 AND root_seq=?2", params![o.seq, root_seq], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).ok())).unwrap();
            let Some(row) = row else { continue }; // a resolver from another thread is judged with this one, not indexed under it
            assert_eq!(row, (v.intent.as_str().to_owned(), v.ask.as_str().to_owned(), v.by_ms), "{}: intent, ask and clock", o.id);
            for it in &v.items {
                let st: String = q(&s, "SELECT state FROM item WHERE msg_seq=?1 AND k=?2", &[&o.seq, &it.k]);
                assert_eq!(st, it.state, "{} item {}", o.id, it.k);
            }
            let n: i64 = q(&s, "SELECT count(*) FROM item WHERE msg_seq=?1", &[&o.seq]);
            assert_eq!(n as usize, v.items.len(), "{}: the same items", o.id);
            let marks: i64 = q(&s, "SELECT count(*) FROM mark WHERE msg_seq=?1", &[&o.seq]);
            let want = v.marks.seen.len() + v.marks.go.len() + v.marks.like.len() + v.marks.no.len();
            assert_eq!(marks as usize, want, "{}: the same marks", o.id);
            compared += 1;
        }
        let rv = &verdicts[&root.id];
        let (state, answered, settled): (String, Option<i64>, Option<String>) = s.with_conn(|c| Ok(c.query_row("SELECT state, answered_by, settled_word FROM thread WHERE root_seq=?1", params![root_seq], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap())).unwrap();
        let want = if rv.settled.is_some() { "settled" } else { match rv.state {
            State::Closed => "closed", State::Superseded => "superseded", State::Archived => "archived", State::Expired => "expired", State::Ambient => "ambient",
            State::Open if rv.answered_by.is_some() => "answered", State::Open => "open" } };
        assert_eq!(state, want, "thread {root_seq}");
        assert_eq!(settled.as_deref(), rv.settled, "thread {root_seq}: settled word");
        let want_ans = rv.answered_by.as_deref().map(|id| s.get(id).unwrap().seq);
        assert_eq!(answered, want_ans, "thread {root_seq}: answered_by");
    }
    assert!(compared >= 15, "the scenario is compared message by message: {compared}");
}

#[test]
fn time_is_the_readers_own_clock() {
    let s = oracle();
    let old = say(&s, "core", "rocky", "please answer by noon", &["ask:reply", "by:1800003600", "to:dinesh"], 0);
    let fuse = say(&s, "core", "rocky", "a proposal with a one hour fuse", &["intent:proposal", "ttl:1"], 1);
    let old_n = seq(&s, &old);
    let (state, by): (String, Option<i64>) = s.with_conn(|c| Ok(c.query_row("SELECT t.state, m.by_ms FROM thread t JOIN msg m ON m.seq=t.root_seq WHERE t.root_seq=?1", params![old_n], |r| Ok((r.get(0)?, r.get(1)?))).unwrap())).unwrap();
    assert_eq!((state.as_str(), by), ("open", Some(1_800_003_600_000)), "never overdue in a row: the clock is stored, the reader compares");
    let exp: Option<i64> = q(&s, "SELECT expires_ms FROM msg WHERE seq=?1", &[&seq(&s, &fuse)]);
    assert_eq!(exp, Some(NOW + MIN + 3_600_000), "a fuse is stored as the moment it burns out");
    let ts: String = q(&s, "SELECT state FROM thread WHERE root_seq=?1", &[&seq(&s, &fuse)]);
    assert_eq!(ts, "open", "and the thread is not marked expired by a write");
}

fn legacy(id: &str, channel: &str, content: &str, tags: &[&str], ago_min: i64, news: bool) -> LegacyRow {
    LegacyRow {
        id: id.into(), channel: channel.into(), author: "rocky".into(), kind: "say".into(), content: content.into(), cargo: None,
        tags: tags.iter().map(|t| t.to_string()).collect(), created_ms: NOW - ago_min * MIN, pubkey: String::new(), sig: String::new(), news,
        source: None, repair_legacy: false,
    }
}

#[test]
fn imported_history_is_in_the_room_threaded_and_never_judged() {
    let s = oracle();
    s.import_legacy(&[
        legacy("OBS-mirror-h1", "mirror", "an old question", &["intent:question", "ask:reply", "to:dinesh"], 5000, false),
        legacy("OBS-mirror-h2", "mirror", "an old answer", &["reply:OBS-mirror-h1", "item:R1=requirement"], 4990, false),
        legacy("OBS-mirror-h3", "mirror", "alone in the past", &[], 4900, false),
        legacy("OBS-mirror-live", "mirror", "a live mirrored message", &["intent:proposal"], 60, true),
    ]).unwrap();
    let live_reply = say(&s, "mirror", "dinesh", "answering the old question today", &["re:OBS-mirror-h1"], 0);
    let n: i64 = q(&s, "SELECT count(*) FROM msg", &[]);
    assert_eq!(n, 5, "the room is all its messages: history too");
    assert_eq!(msg_row(&s, "OBS-mirror-h1"), ("history".into(), seq(&s, "OBS-mirror-h1"), None, 0));
    assert_eq!(msg_row(&s, "OBS-mirror-h2"), ("history".into(), seq(&s, "OBS-mirror-h1"), Some(seq(&s, "OBS-mirror-h1")), 1), "threaded by its reply tag");
    assert_eq!(msg_row(&s, "OBS-mirror-h3").0, "history");
    assert_eq!(msg_row(&s, &live_reply), ("message".into(), seq(&s, "OBS-mirror-h1"), Some(seq(&s, "OBS-mirror-h1")), 1), "a live reply joins the old thread");
    assert_eq!(msg_row(&s, "OBS-mirror-live").0, "message", "an imported live row is folded like any");
    // never judged: no items, no marks, no intent or ask on a history row, and a history-only thread has no thread row
    let judged: i64 = q(&s, "SELECT count(*) FROM msg WHERE class='history' AND (intent IS NOT NULL OR ask IS NOT NULL)", &[]);
    let items: i64 = q(&s, "SELECT count(*) FROM item WHERE msg_seq IN (SELECT seq FROM msg WHERE class='history')", &[]);
    assert_eq!((judged, items), (0, 0));
    let t3: i64 = q(&s, "SELECT count(*) FROM thread WHERE root_seq=?1", &[&seq(&s, "OBS-mirror-h3")]);
    assert_eq!(t3, 0);
    let h1 = seq(&s, "OBS-mirror-h1");
    let t1: (String, i64) = s.with_conn(|c| Ok(c.query_row("SELECT state, replies FROM thread WHERE root_seq=?1", params![h1], |r| Ok((r.get(0)?, r.get(1)?))).unwrap())).unwrap();
    assert_eq!(t1, ("history".into(), 1), "a thread that opens in history is listed as such, with its live replies counted");
    // and reindex draws the same lines
    let before = s.index_dump().unwrap();
    let stats = s.reindex().unwrap();
    assert!(stats.messages >= 5);
    assert_eq!(s.index_dump().unwrap(), before, "reindex equals the incremental projection with history in the room");
}

#[test]
fn a_search_row_names_its_room_and_its_class() {
    let s = oracle();
    say(&s, "core", "rocky", "needle in core", &[], 0);
    say(&s, "hustler", "rocky", "needle in hustler", &["intent:status"], 1);
    s.import_legacy(&[legacy("OBS-core-old", "core", "needle from the past", &[], 9000, false)]).unwrap();
    let count = |s: &Store, query: &str| -> i64 { q(s, "SELECT count(*) FROM obs_fts WHERE obs_fts MATCH ?1", &[&query]) };
    let core = oracle_server::roomindex::fts_tokens("core", "message");
    let (ch_core, _) = core.split_once(' ').unwrap();
    let hustler = oracle_server::roomindex::fts_tokens("hustler", "status");
    let (ch_hustler, cls_status) = hustler.split_once(' ').unwrap();
    for pass in ["as written", "after reindex"] {
        assert_eq!(count(&s, "needle"), 3, "{pass}");
        assert_eq!(count(&s, &format!("needle AND tags:{ch_core}")), 2, "{pass}: a room's search sees only that room (its history too)");
        assert_eq!(count(&s, &format!("needle AND tags:{ch_hustler}")), 1, "{pass}");
        assert_eq!(count(&s, &format!("needle AND tags:{cls_status}")), 1, "{pass}: a class narrows it");
        assert_eq!(count(&s, &format!("needle AND tags:{}", oracle_server::roomindex::fts_tokens("core", "history").split_once(' ').unwrap().1)), 1, "{pass}: history is a class");
        let tags: String = q(&s, "SELECT tags FROM obs_fts WHERE rowid=?1", &[&seq(&s, "OBS-core-old")]);
        assert_eq!(tags.split_whitespace().filter(|w| w.starts_with("zzch")).count(), 1, "{pass}: the room token once, never twice");
        // a ledger that was written before the tokens existed: the rows have none, and reindex must give every row its own
        s.with_conn(|c| Ok(c.execute("UPDATE obs_fts SET tags='old'", []).unwrap())).unwrap();
        assert_eq!(count(&s, &format!("needle AND tags:{ch_core}")), 0);
        s.reindex().unwrap();
    }
}

#[test]
fn a_thread_that_opens_in_history_takes_its_state_from_its_live_asks() {
    let s = oracle();
    s.import_legacy(&[legacy("OBS-mirror-old", "mirror", "an old thread", &[], 5000, false), legacy("OBS-mirror-quiet", "mirror", "another old thread", &[], 4000, false)]).unwrap();
    let word = |id: &str| -> String { let n = seq(&s, id); q(&s, "SELECT state FROM thread WHERE root_seq=?1", &[&n]) };
    say(&s, "mirror", "dinesh", "a reply that asks nothing", &["re:OBS-mirror-quiet"], 0);
    assert_eq!(word("OBS-mirror-quiet"), "history", "no live ask: it is listed as history");
    let ask = say(&s, "mirror", "rocky", "please redo this today", &["re:OBS-mirror-old", "ask:act", "to:dinesh"], 1);
    assert_eq!(word("OBS-mirror-old"), "open", "a live ask in it is open, whatever its root is");
    say(&s, "mirror", "dinesh", "done", &[&format!("re:{ask}")], 2);
    assert_eq!(word("OBS-mirror-old"), "answered", "and moves with its replies");
    let before = s.index_dump().unwrap();
    s.reindex().unwrap();
    assert_eq!(s.index_dump().unwrap(), before, "reindex agrees");
}
