//! Step 3c of docs/ROOM-INDEX.md: the request ledger (`/asked`), what a seat has not read (`unread`), and the rule for reading another seat's lists
//! (a registered gateway, audited, off until it is switched on). Real signed writes on the real store.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::roomread::{asked, messages, unread, PageQ};
use oracle_server::store::{Principal, Scope, Store, StoreError};
use serde_json::{json, Value};

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
        "kepler" => 4,
        _ => 5,
    }
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    for (h, k) in [("rocky", 2), ("dinesh", 3), ("kepler", 4), ("axxis-door", 5)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    }
    s
}
#[allow(clippy::too_many_arguments)]
fn put(s: &Store, room: &str, who: &str, kind: Kind, text: &str, cargo: Option<&str>, tags: &[&str], mins: i64) -> String {
    let at = NOW + mins * MIN;
    let env = Envelope::sign(&sk(key_of(who)), room, who, kind, text, cargo.map(str::to_owned), tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn say(s: &Store, room: &str, who: &str, text: &str, tags: &[&str], mins: i64) -> String {
    put(s, room, who, Kind::Say, text, None, tags, mins)
}
fn q() -> PageQ {
    PageQ::default()
}

const ITEMS: &str = r#"{"items":[{"k":"R1","kind":"requirement","text":"seven read tools, one per route"},{"k":"R2","kind":"requirement","text":"every list pages by seq"}]}"#;

#[test]
fn the_request_ledger_lists_every_item_a_seat_asked_with_its_words_and_state() {
    let s = oracle();
    let m = put(&s, "core", "blaze", Kind::Say, "we need two things", Some(ITEMS), &["item:R1=requirement", "item:R2=requirement", "to:dinesh"], 0);
    put(&s, "core", "dinesh", Kind::Say, "on it", None, &[&format!("re:{m}"), "item:R1=building", "owner:R1=dinesh"], 1);
    // a message of someone else's, and a message of blaze's with no items: neither is in blaze's ledger
    put(&s, "core", "rocky", Kind::Say, "rocky asks too", Some(r#"{"items":[{"k":"Q1","kind":"question","text":"why?"}]}"#), &["item:Q1=question"], 2);
    say(&s, "core", "blaze", "just talking", &[], 3);
    let v = asked(&s, "blaze", &q()).unwrap();
    assert_eq!((v["count"].as_i64(), v["item_count"].as_i64()), (Some(1), Some(2)));
    let entry = &v["asked"][0];
    assert_eq!(entry["message"]["id"], m.as_str());
    assert_eq!(entry["message"]["card"], "we need two things");
    assert_eq!(entry["items"][0], json!({"k": "R1", "kind": "requirement", "text": "seven read tools, one per route", "state": "building", "owner": "dinesh",
        "moved_by": entry["items"][0]["moved_by"], "moved_ms": entry["items"][0]["moved_ms"], "by_model": false}));
    assert!(entry["items"][0]["moved_by"].is_string() && entry["items"][0]["moved_ms"].is_i64());
    assert_eq!((entry["items"][1]["k"].as_str(), entry["items"][1]["text"].as_str(), entry["items"][1]["state"].as_str()), (Some("R2"), Some("every list pages by seq"), Some("open")));
    assert_eq!(asked(&s, "rocky", &q()).unwrap()["item_count"], 1);
    assert_eq!(asked(&s, "kepler", &q()).unwrap()["asked"], json!([]), "a seat that asked for nothing has an empty ledger");
}

#[test]
fn an_annotation_can_declare_items_and_its_words_are_the_items_words() {
    let s = oracle();
    let m = say(&s, "core", "blaze", "a long message nobody classified", &[], 0);
    put(&s, "core", "kepler", Kind::Annotate, "the reader's items", Some(ITEMS), &[&format!("annotates:{m}"), "item:R1=requirement", "item:R2=requirement", "set:by=model"], 1);
    let v = asked(&s, "blaze", &q()).unwrap();
    assert_eq!(v["asked"][0]["items"][0]["text"], "seven read tools, one per route");
    assert_eq!(v["asked"][0]["message"]["id"], m.as_str());
}

#[test]
fn state_narrows_the_ledger_and_the_pages_chain_by_message() {
    let s = oracle();
    let mut all = vec![];
    for i in 0..7 {
        let m = put(&s, "core", "blaze", Kind::Say, &format!("ask {i}"), Some(r#"{"items":[{"k":"R1","kind":"requirement","text":"one"}]}"#), &["item:R1=requirement", "to:dinesh"], i * 2);
        if i % 2 == 0 {
            say(&s, "core", "dinesh", "built", &[&format!("re:{m}"), "item:R1=built"], i * 2 + 1);
        }
        all.push(m);
    }
    let built = asked(&s, "blaze", &PageQ { state: vec!["built".into()], ..q() }).unwrap();
    assert_eq!(built["count"], 4, "only the messages with an item in that state");
    assert!(built["asked"].as_array().unwrap().iter().all(|e| e["items"].as_array().unwrap().iter().all(|i| i["state"] == "built")));
    let open = asked(&s, "blaze", &PageQ { state: vec!["open".into(), "building".into()], ..q() }).unwrap();
    assert_eq!(open["count"], 3);
    // pages of 3 messages chain to the first, none twice
    let (mut got, mut before) = (vec![], None);
    loop {
        let p = asked(&s, "blaze", &PageQ { before, limit: Some(3), ..q() }).unwrap();
        got.extend(p["asked"].as_array().unwrap().iter().map(|e| e["message"]["id"].as_str().unwrap().to_owned()));
        match p["next_before"].as_i64() {
            Some(n) => before = Some(n),
            None => break,
        }
    }
    assert_eq!(got, all.iter().rev().cloned().collect::<Vec<_>>());
    for bad in [PageQ { state: vec!["finished".into()], ..q() }, PageQ { limit: Some(0), ..q() }, PageQ { after: Some(1), ..q() }] {
        assert!(matches!(asked(&s, "blaze", &bad), Err(StoreError::Rejected(_))));
    }
    assert!(matches!(asked(&s, "", &q()), Err(StoreError::Rejected(_))));
}

#[test]
fn unread_counts_other_seats_speech_after_the_read_mark() {
    let s = oracle();
    let one = say(&s, "core", "rocky", "one", &[], 0);
    let two = say(&s, "core", "rocky", "two", &[], 1);
    say(&s, "core", "dinesh", "my own words never count as unread", &[], 2);
    let like = say(&s, "core", "kepler", "✓", &[&format!("re:{two}"), "react:like"], 3);
    say(&s, "ops", "rocky", "another room", &[], 4);
    let v = unread(&s, "core", "dinesh").unwrap();
    assert_eq!((v["cursor"].as_i64(), v["unread"].as_i64()), (Some(0), Some(2)), "two messages of rocky's; a mark and my own are not unread");
    assert_eq!(v["first_unread_seq"].as_i64(), Some(s.get(&one).unwrap().seq), "and it starts at the first of them");
    assert_eq!(v["latest_seq"].as_i64(), Some(s.get(&like).unwrap().seq), "the room's newest seq, marks included, and not another room's");
    say(&s, "core", "rocky", "three", &[], 5);
    // reading moves the mark: a fold acknowledgement, exactly as the CLI does it
    let seq_two = s.get(&two).unwrap().seq;
    s.with_conn(|c| Ok(c.execute("INSERT OR REPLACE INTO cursors(agent,channel,mark_seq) VALUES('dinesh','core',?1)", [seq_two]).unwrap())).unwrap();
    let v = unread(&s, "core", "dinesh").unwrap();
    assert_eq!((v["cursor"].as_i64(), v["unread"].as_i64()), (Some(seq_two), Some(1)), "only 'three' is after the mark");
    let page = messages(&s, "core", &PageQ { after: v["cursor"].as_i64(), ..q() }).unwrap();
    assert_eq!(page["items"].as_array().unwrap().iter().filter(|i| i["author"] != "dinesh").count(), 2, "the messages after the cursor are one call away (a mark among them)");
    assert_eq!(unread(&s, "nowhere", "dinesh").unwrap()["unread"], 0);
    assert!(matches!(unread(&s, "Not A Room", "dinesh"), Err(StoreError::Rejected(_))));
}

#[test]
fn a_seat_reads_its_own_lists_and_only_a_registered_gateway_reads_anyone_elses() {
    let s = oracle();
    let ask = |caller: &str, target: &str, route: &str, enforce: bool| s.cross_seat(caller, target, route, enforce, NOW);
    // your own, always, and no audit row for it
    ask("dinesh", "dinesh", "asks", true).unwrap();
    assert!(s.cross_seat_audit(10).unwrap().is_empty());
    // an ordinary agent token, enforcing: refused, and audited as refused
    let e = ask("dinesh", "blaze", "asks", true).unwrap_err();
    assert!(matches!(&e, StoreError::Forbidden(m) if m.contains("dinesh may not read blaze's asks")), "{e:?}");
    // audit mode lets it through and records what enforcing would have done
    ask("dinesh", "blaze", "asked", false).unwrap();
    let rows = s.cross_seat_audit(10).unwrap();
    assert_eq!((rows[0].caller.as_str(), rows[0].route.as_str(), rows[0].target.as_str(), rows[0].permitted, rows[0].enforced), ("dinesh", "asked", "blaze", false, false), "let through in audit mode, recorded as not permitted");
    assert_eq!((rows[1].permitted, rows[1].enforced), (false, true), "the enforced refusal is recorded as refused");
    // nobody becomes a gateway by asking: only the registry does, and only for a seat with a key and known routes
    assert!(matches!(s.gateway_add("ghost", &["asks".into()], NOW), Err(StoreError::Rejected(m)) if m.contains("not a seat")));
    assert!(matches!(s.gateway_add("axxis-door", &["everything".into()], NOW), Err(StoreError::Rejected(m)) if m.contains("unknown route")));
    assert!(matches!(s.gateway_add("axxis-door", &[], NOW), Err(StoreError::Rejected(_))));
    s.gateway_add("axxis-door", &["asks".into(), "unread".into()], NOW).unwrap();
    ask("axxis-door", "blaze", "asks", true).unwrap();
    ask("axxis-door", "blaze", "unread", true).unwrap();
    assert!(matches!(ask("axxis-door", "blaze", "asked", true), Err(StoreError::Forbidden(_))), "a gateway is scoped to its routes");
    assert!(matches!(ask("dinesh", "blaze", "asks", true), Err(StoreError::Forbidden(_))), "and a registration for one seat is not one for another");
    assert_eq!(s.gateways().unwrap(), vec![("axxis-door".to_string(), vec!["asks".to_string(), "unread".to_string()])]);
    let rows = s.cross_seat_audit(20).unwrap();
    assert!(rows.iter().filter(|r| r.caller == "axxis-door" && r.permitted).count() == 2, "the gateway's allowed reads are audited too");
    // a registration can be taken away, and then it is refused again
    assert!(s.gateway_remove("axxis-door").unwrap() && !s.gateway_remove("axxis-door").unwrap());
    assert!(matches!(ask("axxis-door", "blaze", "asks", true), Err(StoreError::Forbidden(_))));
}

#[test]
fn nothing_in_a_request_can_make_a_token_a_gateway() {
    // The policy reads one thing, the registry, keyed by the token's own handle (which the server took from the token). Nothing else is an input:
    // `cross_seat(caller, target, route, enforce, now)` has no field for a claim, a header or a query parameter, and the registry has one writer, the CLI.
    let s = oracle();
    for claim in ["axxis-door", "gateway", "blaze", "*"] {
        // whatever string a caller offers as the TARGET, it is judged as the token's own handle: dinesh, who is not registered
        assert!(matches!(s.cross_seat("dinesh", claim, "asks", true, NOW), Err(StoreError::Forbidden(_))), "{claim}");
    }
    s.gateway_add("axxis-door", &["asks".into()], NOW).unwrap();
    assert!(matches!(s.cross_seat("dinesh", "axxis-door", "asks", true, NOW), Err(StoreError::Forbidden(_))), "reading a gateway's lists is not being one");
    let listed: Value = json!(s.gateways().unwrap().iter().map(|g| g.0.clone()).collect::<Vec<_>>());
    assert_eq!(listed, json!(["axxis-door"]), "dinesh's attempts registered nothing");
}
