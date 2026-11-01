//! The trust and data rules, as executable claims. Each negative here is a rule in
//! docs/DESIGN.md; if one starts passing a bad write, the design is broken.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store, StoreError};
use std::collections::BTreeMap;

const NOW: i64 = 1_800_000_000_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

/// An Oracle with owner `blaze` and seats `rocky` and `kin`.
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW).unwrap();
    for (h, k) in [("rocky", 2), ("kin", 3)] {
        let ev = KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW);
        s.apply_key_event(&ev, NOW).unwrap();
    }
    s
}

fn who(h: &str) -> Principal {
    Principal {
        handle: h.into(),
        scope: Scope::Write,
    }
}

fn say(s: &Store, key: u8, author: &str, channel: &str, text: &str, at: i64) -> String {
    let env = Envelope::sign(&sk(key), channel, author, Kind::Say, text, None, vec![], at);
    s.write(&who(author), &env, at).unwrap().id
}

#[test]
fn a_signed_say_is_stored_and_readable() {
    let s = oracle();
    let id = say(&s, 2, "rocky", "room", "hello\nworld 🐝", NOW);
    let got = s.get(&id).unwrap();
    assert_eq!(
        (got.author.as_str(), got.content.as_str()),
        ("rocky", "hello\nworld 🐝")
    );
    assert_eq!(s.recent(Some("room"), 10, None).unwrap().len(), 1);
}

#[test]
fn a_bad_signature_is_rejected_not_downgraded() {
    let s = oracle();
    let mut env = Envelope::sign(&sk(2), "room", "rocky", Kind::Say, "hi", None, vec![], NOW);
    env.content = "tampered".into();
    assert!(matches!(
        s.write(&who("rocky"), &env, NOW),
        Err(StoreError::Unauthorized(_))
    ));
    assert!(
        s.recent(None, 10, None).unwrap().is_empty(),
        "nothing may land"
    );
}

#[test]
fn signing_with_someone_elses_name_is_rejected() {
    let s = oracle();
    // kin's key, claiming to be rocky, with rocky's token
    let env = Envelope::sign(
        &sk(3),
        "room",
        "rocky",
        Kind::Say,
        "i am rocky",
        None,
        vec![],
        NOW,
    );
    assert!(matches!(
        s.write(&who("rocky"), &env, NOW),
        Err(StoreError::Unauthorized(_))
    ));
}

#[test]
fn a_token_cannot_post_as_another_seat() {
    let s = oracle();
    let env = Envelope::sign(&sk(2), "room", "rocky", Kind::Say, "hi", None, vec![], NOW);
    assert!(matches!(
        s.write(&who("kin"), &env, NOW),
        Err(StoreError::Forbidden(_))
    ));
}

#[test]
fn a_read_token_cannot_write() {
    let s = oracle();
    let env = Envelope::sign(&sk(2), "room", "rocky", Kind::Say, "hi", None, vec![], NOW);
    let reader = Principal {
        handle: "rocky".into(),
        scope: Scope::Read,
    };
    assert!(matches!(
        s.write(&reader, &env, NOW),
        Err(StoreError::Forbidden(_))
    ));
}

#[test]
fn a_captured_envelope_cannot_be_replayed() {
    let s = oracle();
    let env = Envelope::sign(
        &sk(2),
        "room",
        "rocky",
        Kind::Say,
        "pay 100",
        None,
        vec![],
        NOW,
    );
    s.write(&who("rocky"), &env, NOW).unwrap();
    // identical bytes again → same client_op → idempotent, exactly one row
    assert!(s.write(&who("rocky"), &env, NOW).unwrap().duplicate);
    // attacker changes client_op to dodge idempotency → signature breaks
    let mut forged = env.clone();
    forged.client_op = "other".into();
    assert!(matches!(
        s.write(&who("rocky"), &forged, NOW),
        Err(StoreError::Unauthorized(_))
    ));
    // and much later the original is stale
    let later = NOW + 6 * 60 * 1000;
    let s2 = oracle();
    assert!(matches!(
        s2.write(&who("rocky"), &env, later),
        Err(StoreError::Unauthorized(_))
    ));
    assert_eq!(s.recent(None, 10, None).unwrap().len(), 1);
}

#[test]
fn a_reused_nonce_is_rejected() {
    let s = oracle();
    let a = Envelope::sign(&sk(2), "room", "rocky", Kind::Say, "one", None, vec![], NOW);
    s.write(&who("rocky"), &a, NOW).unwrap();
    // a second, validly signed envelope that reuses the nonce
    let mut b = Envelope::sign(&sk(2), "room", "rocky", Kind::Say, "two", None, vec![], NOW);
    b.nonce = a.nonce.clone();
    use ed25519_dalek::Signer;
    b.sig = hex::encode(sk(2).sign(&b.signing_bytes()).to_bytes());
    assert!(matches!(
        s.write(&who("rocky"), &b, NOW),
        Err(StoreError::Unauthorized(_))
    ));
}

#[test]
fn retry_is_idempotent_even_after_thousands_of_later_posts() {
    let s = oracle();
    let env = Envelope::sign(
        &sk(2),
        "room",
        "rocky",
        Kind::Say,
        "first",
        None,
        vec![],
        NOW,
    );
    let first = s.write(&who("rocky"), &env, NOW).unwrap();
    for i in 0..1500 {
        say(&s, 3, "kin", "room", &format!("noise {i}"), NOW + 1);
    }
    let again = s.write(&who("rocky"), &env, NOW + 2).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.id, first.id);
}

#[test]
fn an_unsigned_or_wrongly_signed_key_change_is_rejected() {
    let s = oracle();
    let attacker = sk(9);
    // kin tries to rotate rocky's key
    let ev = KeyEvent::sign(
        &sk(3),
        "kin",
        KeyAction::Rotate,
        "rocky",
        &pubkey_hex(&attacker),
        NOW,
    );
    assert!(matches!(
        s.apply_key_event(&ev, NOW),
        Err(StoreError::Forbidden(_))
    ));
    // attacker signs as "blaze" with their own key
    let ev = KeyEvent::sign(
        &attacker,
        "blaze",
        KeyAction::Rotate,
        "rocky",
        &pubkey_hex(&attacker),
        NOW,
    );
    assert!(matches!(
        s.apply_key_event(&ev, NOW),
        Err(StoreError::Unauthorized(_))
    ));
    // a seat cannot add seats
    let ev = KeyEvent::sign(
        &sk(2),
        "rocky",
        KeyAction::Add,
        "mallory",
        &pubkey_hex(&attacker),
        NOW,
    );
    assert!(matches!(
        s.apply_key_event(&ev, NOW),
        Err(StoreError::Forbidden(_))
    ));
    assert_eq!(
        s.active_key("rocky").unwrap().unwrap().0,
        pubkey_hex(&sk(2)),
        "key unchanged"
    );
}

#[test]
fn a_seat_can_rotate_its_own_key_and_the_old_key_stops_working() {
    let s = oracle();
    let ev = KeyEvent::sign(
        &sk(2),
        "rocky",
        KeyAction::Rotate,
        "rocky",
        &pubkey_hex(&sk(22)),
        NOW,
    );
    s.apply_key_event(&ev, NOW).unwrap();
    let old = Envelope::sign(
        &sk(2),
        "room",
        "rocky",
        Kind::Say,
        "old key",
        None,
        vec![],
        NOW,
    );
    assert!(matches!(
        s.write(&who("rocky"), &old, NOW),
        Err(StoreError::Unauthorized(_))
    ));
    let new = Envelope::sign(
        &sk(22),
        "room",
        "rocky",
        Kind::Say,
        "new key",
        None,
        vec![],
        NOW,
    );
    s.write(&who("rocky"), &new, NOW).unwrap();
    assert_eq!(
        s.key_events().unwrap().len(),
        3,
        "history is append-only: 2 adds + 1 rotate"
    );
}

#[test]
fn a_key_event_cannot_be_replayed_and_revocation_kills_tokens() {
    let s = oracle();
    let token = s.mint_token("kin", Scope::Write, "test", NOW).unwrap();
    assert_eq!(s.authenticate(&token).unwrap().handle, "kin");
    let ev = KeyEvent::sign(&sk(1), "blaze", KeyAction::Revoke, "kin", "", NOW);
    s.apply_key_event(&ev, NOW).unwrap();
    assert!(s.authenticate(&token).is_err());
    assert!(matches!(
        s.apply_key_event(&ev, NOW),
        Err(StoreError::NotFound) | Err(StoreError::Unauthorized(_))
    ));
    let env = Envelope::sign(
        &sk(3),
        "room",
        "kin",
        Kind::Say,
        "still here?",
        None,
        vec![],
        NOW,
    );
    assert!(s.write(&who("kin"), &env, NOW).is_err());
}

#[test]
fn the_registry_can_only_be_initialised_once() {
    let s = oracle();
    assert!(matches!(
        s.init_owner("mallory", &pubkey_hex(&sk(9)), NOW),
        Err(StoreError::Forbidden(_))
    ));
}

#[test]
fn fold_returns_what_is_new_for_me_and_ack_advances() {
    let s = oracle();
    say(&s, 3, "kin", "room", "hello @rocky, a question", NOW);
    say(&s, 3, "kin", "room", "general chatter", NOW);
    say(&s, 2, "rocky", "room", "my own post", NOW);
    say(&s, 3, "kin", "other", "elsewhere", NOW);

    let f = s.fold("rocky", 50, NOW).unwrap();
    assert_eq!(f.returned, 3, "my own post is not news to me");
    assert_eq!(f.pending.get("room"), Some(&2));
    assert_eq!(f.items.iter().filter(|i| i.for_me).count(), 1);
    assert!(!f.has_more);

    // peeking twice changes nothing
    assert_eq!(s.fold("rocky", 50, NOW).unwrap().returned, 3);
    s.ack("rocky", &f.next_marks, false).unwrap();
    assert_eq!(s.fold("rocky", 50, NOW).unwrap().returned, 0);

    say(&s, 3, "kin", "room", "one more", NOW);
    assert_eq!(s.fold("rocky", 50, NOW).unwrap().returned, 1);

    // cursors never move backwards by accident
    let stale: BTreeMap<String, i64> = [("room".to_string(), 0)].into();
    s.ack("rocky", &stale, false).unwrap();
    assert_eq!(s.fold("rocky", 50, NOW).unwrap().returned, 1);
}

#[test]
fn a_reply_to_my_message_is_for_me_even_without_my_name() {
    let s = oracle();
    let mine = say(&s, 2, "rocky", "room", "shipped the release", NOW);
    let env = Envelope::sign(&sk(3), "room", "kin", Kind::Say, "verified, all good", None, vec![format!("reply:{mine}")], NOW + 1);
    s.write(&who("kin"), &env, NOW + 1).unwrap();
    say(&s, 3, "kin", "room", "unrelated chatter", NOW + 2);
    let f = s.fold("rocky", 50, NOW + 3).unwrap();
    let for_me: Vec<&str> = f.items.iter().filter(|i| i.for_me).map(|i| i.obs.content.as_str()).collect();
    assert_eq!(for_me, vec!["verified, all good"], "the answer is addressed by its target; chatter is not");
}

/// for_me is decided from the item's own tags by an exact relation prefix and the target's id (21 Oct: the
/// former join walked the seat's whole authored history per item). The same answers as before on every edge
/// Emmy's fixture named: own, other and missing targets; mixed-case and
/// malformed prefixes; no colon; an empty suffix. A tag the write path refuses is reported, not asserted.
#[test]
fn for_me_resolves_a_relation_from_the_items_own_tags_exactly() {
    let s = oracle();
    let mine = say(&s, 2, "rocky", "room", "shipped the release", NOW);
    let other = say(&s, 3, "kin", "room", "a note of kin's own", NOW);
    let cases: Vec<(String, bool)> = vec![
        (format!("reply:{mine}"), true),
        (format!("re:{mine}"), true),
        (format!("in_reply_to:{mine}"), true),
        (format!("closes:{mine}"), true),
        (format!("supersedes:{mine}"), true),
        ("to:rocky".into(), true),
        ("mention:rocky".into(), true),
        ("to:all".into(), true),
        (format!("reply:{other}"), false),
        (format!("closes:{other}"), false),
        (format!("REPLY:{mine}"), false),
        (format!("Reply:{mine}"), false),
        (format!("RE:{mine}"), false),
        (format!("in-reply-to:{mine}"), false),
        (format!("inXreplyYto:{mine}"), false),
        (format!("xreply:{mine}"), false),
        (format!("reply:{mine}:colon"), false),
        ("reply:OBS-room-20000101-000000000000".into(), false),
        ("reply:".into(), false),
        ("replymine".into(), false),
        (mine.clone(), false),
        ("to:kin".into(), false),
    ];
    let mut at = NOW + 1;
    let mut refused = Vec::new();
    for (tag, expected) in &cases {
        let text = format!("tagged {tag}");
        let env = Envelope::sign(&sk(3), "room", "kin", Kind::Say, &text, None, vec![tag.clone()], at);
        match s.write(&who("kin"), &env, at) {
            Ok(_) => {}
            Err(e) => {
                refused.push(format!("{tag}: {e}"));
                continue;
            }
        }
        at += 1;
        let f = s.fold("rocky", 200, at).unwrap();
        let item = f.items.iter().find(|i| i.obs.content == text).unwrap_or_else(|| panic!("{text} folded"));
        assert_eq!(item.for_me, *expected, "tag {tag:?}");
    }
    assert!(cases.iter().filter(|(t, _)| t.starts_with("reply:") && t.ends_with(&mine)).count() == 1);
    eprintln!("refused at the write door (not storable, so never for anyone): {refused:?}");
    // NEGATIVE CONTROL: another seat's fold sees none of the relation cases as its own
    let f = s.fold("kin", 200, at + 1).unwrap();
    assert!(f.items.iter().all(|i| !i.for_me || i.obs.content == "shipped the release" || i.obs.content.contains("to:all")), "kin is addressed by none of rocky's relations");
}

#[test]
fn fold_shares_the_limit_fairly_and_reports_the_denominator() {
    let s = oracle();
    for i in 0..40 {
        say(&s, 3, "kin", "loud", &format!("loud {i}"), NOW);
    }
    say(&s, 3, "kin", "quiet", "the one that matters", NOW);
    let f = s.fold("rocky", 10, NOW).unwrap();
    assert_eq!(f.returned, 10);
    assert!(f.has_more);
    assert_eq!(f.pending["loud"], 40);
    assert!(
        f.items.iter().any(|i| i.obs.channel == "quiet"),
        "a loud room cannot starve a quiet one"
    );
}

#[test]
fn what_is_addressed_to_me_is_listed_newest_first_even_past_the_page() {
    // Jobs' audit, 2026-10-10: 1,364 new paged oldest-first, so what was written to him today never showed.
    let s = oracle();
    let to_me = |text: &str, at: i64| {
        let env = Envelope::sign(&sk(3), "loud", "kin", Kind::Say, text, None, vec!["to:rocky".into()], at);
        s.write(&who("kin"), &env, at).unwrap();
    };
    to_me("early ask", NOW);
    for i in 0..40 {
        say(&s, 3, "kin", "loud", &format!("loud {i}"), NOW + 1);
    }
    to_me("today's ask", NOW + 2);
    say(&s, 2, "rocky", "loud", "my own, never for me", NOW + 3);
    let f = s.fold("rocky", 5, NOW + 4).unwrap();
    assert!(!f.items.iter().any(|i| i.obs.content == "today's ask"), "the page, oldest first, does not reach it");
    let unread: Vec<&str> = f.unread_for_me.iter().map(|o| o.content.as_str()).collect();
    assert_eq!(unread, vec!["today's ask", "early ask"], "newest first, whatever the page reached");
    // Read past them, and they are no longer listed.
    let all: BTreeMap<String, i64> = [("loud".to_string(), i64::MAX / 4)].into();
    s.ack("rocky", &all, false).unwrap();
    assert!(s.fold("rocky", 5, NOW + 5).unwrap().unread_for_me.is_empty());
}

#[test]
fn a_superseded_record_ranks_below_the_record_that_replaces_it_and_names_it() {
    let s = oracle();
    // The old fact is the better keyword match (it repeats the words); it was replaced.
    let old = say(&s, 3, "kin", "room", "launch date launch date: the launch date is Sunday 18 October", NOW);
    let env = Envelope::sign(&sk(3), "room", "kin", Kind::Say, "corrected: the launch date is Sunday 25 October", None,
                             vec![format!("supersedes:{old}")], NOW + 1);
    let new = s.write(&who("kin"), &env, NOW + 1).unwrap().id;
    let (hits, _) = s.search_with("launch date", None, None, 10, None).unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.obs.id.as_str()).collect();
    assert_eq!(ids, vec![new.as_str(), old.as_str()], "the successor first");
    assert_eq!(hits[1].superseded_by.as_deref(), Some(new.as_str()), "the old record says what replaced it");
    assert!(hits[0].superseded_by.is_none());
    // Control: the same two records without the supersedes link keep keyword order (the repeated words first).
    let t = oracle();
    let a = say(&t, 3, "kin", "room", "launch date launch date: the launch date is Sunday 18 October", NOW);
    say(&t, 3, "kin", "room", "corrected: the launch date is Sunday 25 October", NOW + 1);
    assert_eq!(t.search_with("launch date", None, None, 10, None).unwrap().0[0].obs.id, a);
}

#[test]
fn a_never_folded_agent_sees_the_last_day_not_all_of_history() {
    let s = oracle();
    say(&s, 3, "kin", "room", "ancient", NOW - 3 * 86_400_000);
    say(&s, 3, "kin", "room", "today", NOW);
    let f = s.fold("rocky", 50, NOW).unwrap();
    assert_eq!(
        f.items
            .iter()
            .map(|i| i.obs.content.as_str())
            .collect::<Vec<_>>(),
        vec!["today"]
    );
}

#[test]
fn recent_paginates_by_keyset() {
    let s = oracle();
    for i in 0..25 {
        say(&s, 3, "kin", "room", &format!("m{i}"), NOW);
    }
    let p1 = s.recent(Some("room"), 10, None).unwrap();
    let p2 = s
        .recent(Some("room"), 10, Some(p1.last().unwrap().seq))
        .unwrap();
    assert_eq!(p1[0].content, "m24");
    assert_eq!(p2[0].content, "m14");
    assert!(p1.iter().all(|a| p2.iter().all(|b| a.seq != b.seq)));
}

#[test]
fn search_ranks_by_relevance_and_cannot_be_injected() {
    let s = oracle();
    say(
        &s,
        3,
        "kin",
        "room",
        "the notifier was OOM killed, memory cap too low",
        NOW,
    );
    say(&s, 3, "kin", "room", "lunch was good", NOW);
    say(
        &s,
        3,
        "kin",
        "other",
        "notifier notifier notifier memory",
        NOW,
    );
    let hits = s.search("notifier memory", None, None, 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits[0].score >= hits[1].score);
    // precise first: a doc with ALL terms outranks docs with only some…
    say(&s, 3, "kin", "room", "memory is a fine thing", NOW);
    let hits = s.search("OOM memory", None, None, 10).unwrap();
    assert!(hits[0].obs.content.contains("OOM killed"));
    // …and when nothing has every term, it widens instead of returning nothing
    assert!(!s.search("lunch zebra", None, None, 10).unwrap().is_empty());
    assert_eq!(
        s.search("notifier", Some("room"), None, 10).unwrap().len(),
        1
    );
    for evil in ["\" OR 1=1 --", "NEAR(", "content:*", "((("] {
        s.search(evil, None, None, 10).unwrap();
    }
}

#[test]
fn tags_are_an_index_and_mentions_are_derived() {
    let s = oracle();
    let env = Envelope::sign(
        &sk(2),
        "room",
        "rocky",
        Kind::Say,
        "ping @kin",
        None,
        vec!["decision".into()],
        NOW,
    );
    s.write(&who("rocky"), &env, NOW).unwrap();
    assert_eq!(s.by_tag("decision", 10).unwrap().len(), 1);
    assert_eq!(s.by_tag("mention:kin", 10).unwrap().len(), 1);
    assert_eq!(s.by_tag("from:rocky", 10).unwrap().len(), 1);
    // the stored, signed tags are exactly what was sent — derived tags never alter them
    assert_eq!(
        s.by_tag("decision", 10).unwrap()[0].tags,
        vec!["decision".to_string()]
    );
}

#[test]
fn state_heads_supersede_and_open_work_needs_a_clock() {
    let s = oracle();
    let cargo = |state: &str, next: Option<&str>| {
        let mut v = serde_json::json!({"subject": "ship-oracle", "state": state});
        if let Some(n) = next {
            v["next_touch"] = n.into();
        }
        Some(v.to_string())
    };
    let bad = Envelope::sign(
        &sk(2),
        "board",
        "rocky",
        Kind::State,
        "start",
        cargo("running", None),
        vec![],
        NOW,
    );
    assert!(matches!(
        s.write(&who("rocky"), &bad, NOW),
        Err(StoreError::Rejected(_))
    ));

    let a = Envelope::sign(
        &sk(2),
        "board",
        "rocky",
        Kind::State,
        "start",
        cargo("running", Some("2026-09-20")),
        vec![],
        NOW,
    );
    s.write(&who("rocky"), &a, NOW).unwrap();
    let b = Envelope::sign(
        &sk(2),
        "board",
        "rocky",
        Kind::State,
        "shipped",
        cargo("done", None),
        vec![],
        NOW,
    );
    s.write(&who("rocky"), &b, NOW).unwrap();

    let head = s.state_head("ship-oracle").unwrap();
    assert_eq!(
        (
            head.state.as_str(),
            head.owner.as_str(),
            head.observation.content.as_str()
        ),
        ("done", "rocky", "shipped")
    );
    assert_eq!(s.state_heads(Some("rocky")).unwrap().len(), 1);
    // A head knows its room, the observation that set it, its first line and when — a room's board
    // reads /states?channel= and shows them (2026-09-28: ox task heads were invisible to a person).
    let rows = s.state_heads_in(None, Some("board")).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].channel.as_str(), rows[0].id.as_str(), rows[0].note.as_str(), rows[0].at_ms), ("board", head.observation.id.as_str(), "shipped", NOW));
    assert!(s.state_heads_in(None, Some("astra")).unwrap().is_empty(), "another room's board is empty");
    assert!(s.state_heads_in(Some("kin"), Some("board")).unwrap().is_empty(), "another owner's heads are not mine");
    assert_eq!(
        s.recent(Some("board"), 10, None).unwrap().len(),
        2,
        "history is kept"
    );
}

#[test]
fn a_huge_backlog_is_reported_as_capped_not_counted() {
    let s = oracle();
    for i in 0..1200 {
        say(&s, 3, "kin", "firehose", &format!("m{i}"), NOW);
    }
    let f = s.fold("rocky", 10, NOW).unwrap();
    assert_eq!(f.pending["firehose"], oracle_server::store::PENDING_CAP);
    assert!(f.pending_capped && f.has_more);
    assert_eq!(f.returned, 10);
    assert_eq!(
        f.items[0].obs.content, "m0",
        "oldest first, so nothing is skipped"
    );
}

#[test]
fn legacy_import_is_idempotent_searchable_and_never_passes_as_verified() {
    use oracle_server::store::LegacyRow;
    let s = oracle();
    let rows: Vec<LegacyRow> = (0..3)
        .map(|i| {
            serde_json::from_value(serde_json::json!({
                "id": format!("OBS-old-{i}"), "channel": "archive", "author": "kin",
                "content": format!("an old decision about rollback number {i} @rocky"),
                "tags": ["decision"], "created_ms": NOW - 5 * 86_400_000 + i,
            }))
            .unwrap()
        })
        .collect();
    assert_eq!(s.import_legacy(&rows).unwrap(), 3);
    assert_eq!(s.import_legacy(&rows).unwrap(), 0, "re-import is a no-op");
    let got = s.get("OBS-old-1").unwrap();
    assert_eq!(
        (got.envelope, got.created_ms),
        (1, NOW - 5 * 86_400_000 + 1)
    );
    assert_eq!(s.search("rollback", None, None, 10).unwrap().len(), 3);
    assert_eq!(s.by_tag("envelope:1", 10).unwrap().len(), 3);
    assert_eq!(s.by_tag("mention:rocky", 10).unwrap().len(), 3);
    // imported history is never "news", even when imported into a LIVE oracle after
    // real posts (old timestamps at high sequence numbers must not hide or flood anything)
    assert_eq!(s.fold("rocky", 50, NOW).unwrap().returned, 0);
    say(
        &s,
        3,
        "kin",
        "room",
        "live post before a second import",
        NOW,
    );
    let more: Vec<LegacyRow> = vec![serde_json::from_value(serde_json::json!({
        "id": "OBS-old-late", "channel": "room", "author": "kin", "content": "imported late", "created_ms": NOW - 9 * 86_400_000,
    })).unwrap()];
    s.import_legacy(&more).unwrap();
    say(&s, 3, "kin", "room", "live post after it", NOW);
    let f = s.fold("rocky", 50, NOW).unwrap();
    assert_eq!(
        f.items
            .iter()
            .map(|i| i.obs.content.as_str())
            .collect::<Vec<_>>(),
        vec!["live post before a second import", "live post after it"]
    );
}

#[test]
fn mirrored_live_messages_are_news_but_history_is_not() {
    use oracle_server::store::LegacyRow;
    let s = oracle();
    let row = |id: &str, news: bool| -> LegacyRow {
        serde_json::from_value(serde_json::json!({
        "id": id, "channel": "room", "author": "kin", "content": format!("{id} @rocky"), "created_ms": NOW, "news": news,
    })).unwrap()
    };
    s.import_legacy(&[row("OBS-history", false), row("OBS-mirrored-live", true)])
        .unwrap();
    let f = s.fold("rocky", 50, NOW).unwrap();
    assert_eq!(
        f.items
            .iter()
            .map(|i| i.obs.id.as_str())
            .collect::<Vec<_>>(),
        vec!["OBS-mirrored-live"]
    );
    assert!(f.items[0].for_me);
    assert_eq!(
        f.items[0].obs.envelope, 1,
        "mirrored is still not presented as v2-verified"
    );
}

#[test]
fn import_orders_history_by_time_whatever_the_file_order() {
    use oracle_server::store::LegacyRow;
    let s = oracle();
    let row = |id: &str, at: i64| -> LegacyRow {
        serde_json::from_value(serde_json::json!({
        "id": id, "channel": "archive", "author": "kin", "content": id, "created_ms": at }))
        .unwrap()
    };
    // file order is alphabetical by id, the opposite of time order
    s.import_legacy(&[
        row("OBS-a-newest", NOW - 1_000),
        row("OBS-b-middle", NOW - 2_000),
        row("OBS-c-oldest", NOW - 3_000),
    ])
    .unwrap();
    let got: Vec<String> = s
        .recent(Some("archive"), 10, None)
        .unwrap()
        .into_iter()
        .map(|o| o.id)
        .collect();
    assert_eq!(
        got,
        vec!["OBS-a-newest", "OBS-b-middle", "OBS-c-oldest"],
        "recent is newest first"
    );
}

#[test]
fn ack_all_marks_every_room_read_and_never_rewinds() {
    let s = oracle();
    for ch in ["a", "b", "c"] {
        say(&s, 3, "kin", ch, "hello @rocky", NOW);
    }
    assert_eq!(s.fold("rocky", 50, NOW).unwrap().returned, 3);
    s.ack_all("rocky").unwrap();
    assert_eq!(s.fold("rocky", 50, NOW).unwrap().returned, 0);
    say(&s, 3, "kin", "a", "after", NOW);
    assert_eq!(s.fold("rocky", 50, NOW).unwrap().returned, 1);
    assert_eq!(
        s.fold("kin", 50, NOW).unwrap().returned,
        0,
        "one agent acking does not touch another"
    );
}

#[test]
fn import_rebuilds_state_heads_and_never_clobbers_a_newer_native_head() {
    use oracle_server::store::LegacyRow;
    let s = oracle();
    let row = |id: &str, at: i64, tags: &[&str]| -> LegacyRow {
        serde_json::from_value(serde_json::json!({
        "id": id, "channel": "board", "author": "kin", "content": id, "created_ms": at, "tags": tags })).unwrap()
    };
    let rows = vec![
        row(
            "OBS-2-ruled",
            NOW - 1_000,
            &[
                "live-state",
                "subject:old-task",
                "state:ruled",
                "owner:rocky",
            ],
        ),
        row(
            "OBS-1-running",
            NOW - 2_000,
            &[
                "live-state",
                "subject:old-task",
                "state:running",
                "owner:rocky",
                "clock:2026-09-01",
            ],
        ),
        row(
            "OBS-3-open",
            NOW - 500,
            &[
                "live-state",
                "subject:other",
                "state:open",
                "clock:2026-10-01",
            ],
        ),
        row(
            "OBS-4-chat",
            NOW - 400,
            &["subject:not-state", "state:open"],
        ),
        row(
            "OBS-5-junk",
            NOW - 300,
            &["live-state", "subject:junk", "state:rotted"],
        ),
    ];
    s.import_legacy(&rows).unwrap();
    let head = s.state_head("old-task").unwrap();
    assert_eq!(
        (
            head.state.as_str(),
            head.owner.as_str(),
            head.observation.id.as_str()
        ),
        ("done", "rocky", "OBS-2-ruled"),
        "latest wins, ruled reads as done"
    );
    let other = s.state_head("other").unwrap();
    assert_eq!(
        (other.owner.as_str(), other.next_touch.as_deref()),
        ("kin", Some("2026-10-01")),
        "owner falls back to the author"
    );
    assert!(s.state_head("not-state").is_err() && s.state_head("junk").is_err());

    // a native write moves the head; re-importing the history must not take it back
    let cargo = serde_json::json!({"subject": "other", "state": "done"}).to_string();
    let e = Envelope::sign(
        &sk(2),
        "board",
        "rocky",
        Kind::State,
        "closed",
        Some(cargo),
        vec![],
        NOW,
    );
    s.write(&who("rocky"), &e, NOW).unwrap();
    assert_eq!(s.import_legacy(&rows).unwrap(), 0);
    assert_eq!(s.state_head("other").unwrap().state, "done");
    assert_eq!(s.state_head("old-task").unwrap().state, "done");
}

#[test]
fn recent_narrows_by_tag_and_by_how_the_content_starts() {
    let s = oracle();
    let tagged = |text: &str, tag: &str, ch: &str| {
        let env = Envelope::sign(
            &sk(2),
            ch,
            "rocky",
            Kind::Say,
            text,
            None,
            vec![tag.into()],
            NOW,
        );
        s.write(&who("rocky"), &env, NOW).unwrap().id
    };
    let a = tagged("[Task: ship] é first", "board", "room");
    let _b = tagged("chatter", "noise", "room");
    let c = tagged("[Task: ship] second", "board", "room");
    let _d = tagged("[Task: ship] elsewhere", "board", "other");
    let ids =
        |v: Vec<oracle_server::store::Observation>| v.into_iter().map(|o| o.id).collect::<Vec<_>>();

    assert_eq!(
        ids(s
            .recent_filtered(Some("room"), 10, None, Some("board"), None)
            .unwrap()),
        vec![c.clone(), a.clone()]
    );
    assert_eq!(
        ids(s
            .recent_filtered(Some("room"), 10, None, None, Some("[Task: ship] é"))
            .unwrap()),
        vec![a.clone()],
        "prefix is counted in characters"
    );
    assert_eq!(
        s.recent_filtered(None, 10, None, Some("board"), None)
            .unwrap()
            .len(),
        3,
        "no room means every room"
    );
    assert_eq!(
        ids(s
            .recent_filtered(Some("room"), 1, None, Some("board"), Some("[Task:"))
            .unwrap()),
        vec![c.clone()]
    );
    let page2 = s
        .recent_filtered(
            Some("room"),
            1,
            Some(s.get(&c).unwrap().seq),
            Some("board"),
            Some("[Task:"),
        )
        .unwrap();
    assert_eq!(ids(page2), vec![a], "keyset paging holds under a filter");
}

#[test]
fn read_marks_show_where_each_seat_stands_in_a_room() {
    let s = oracle();
    say(&s, 3, "kin", "room", "one", NOW);
    say(&s, 3, "kin", "room", "two", NOW + 5);
    let f = s.fold("rocky", 50, NOW + 10).unwrap();
    s.ack("rocky", &f.next_marks, false).unwrap();
    say(&s, 3, "kin", "room", "three", NOW + 20);
    say(
        &s,
        2,
        "rocky",
        "room",
        "my own words are not news to me",
        NOW + 30,
    );

    let marks = s.read_marks("room").unwrap();
    assert_eq!(marks.len(), 1, "a seat that never acknowledged has no mark");
    assert_eq!(
        (
            marks[0].agent.as_str(),
            marks[0].through_ms,
            marks[0].pending
        ),
        ("rocky", Some(NOW + 5), 1)
    );
    assert!(s.read_marks("empty").unwrap().is_empty());
}

/// The spec is only worth having if it cannot drift: every route the server mounts must be
/// in docs/openapi.yaml, and every path in the spec must be a route.
#[test]
fn the_openapi_spec_lists_exactly_the_routes_the_server_mounts() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let http = std::fs::read_to_string(root.join("src/http.rs")).unwrap();
    let spec = std::fs::read_to_string(root.join("../../docs/openapi.yaml")).unwrap();
    let mounted: std::collections::BTreeSet<String> = http
        .lines()
        .filter_map(|l| l.trim().strip_prefix(".route(\""))
        .filter_map(|l| l.split('"').next())
        .map(|p| {
            p.split('/')
                .map(|s| {
                    s.strip_prefix(':')
                        .map(|n| format!("{{{n}}}"))
                        .unwrap_or_else(|| s.to_string())
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    let documented: std::collections::BTreeSet<String> = spec
        .lines()
        .filter(|l| l.starts_with("  /") && l.trim_end().ends_with(':'))
        .map(|l| l.trim().trim_end_matches(':').to_string())
        .collect();
    assert!(
        mounted.len() >= 10,
        "route scan found too little: {mounted:?}"
    );
    assert_eq!(
        mounted, documented,
        "docs/openapi.yaml and src/http.rs disagree"
    );
}

// ── a seat with several devices ─────────────────────────────────────────────────

fn key_ev(
    signer_key: u8,
    signer: &str,
    action: KeyAction,
    handle: &str,
    new_key: Option<u8>,
) -> KeyEvent {
    KeyEvent::sign(
        &sk(signer_key),
        signer,
        action,
        handle,
        &new_key.map(|k| pubkey_hex(&sk(k))).unwrap_or_default(),
        NOW,
    )
}

#[test]
fn a_seat_can_speak_from_several_devices_and_each_row_records_which() {
    let s = oracle();
    // the owner gives rocky a second device; rocky then adds a third itself
    s.apply_key_event(&key_ev(1, "blaze", KeyAction::Add, "rocky", Some(20)), NOW)
        .unwrap();
    s.apply_key_event(&key_ev(20, "rocky", KeyAction::Add, "rocky", Some(21)), NOW)
        .unwrap();
    for k in [2, 20, 21] {
        let env = Envelope::sign(
            &sk(k),
            "room",
            "rocky",
            Kind::Say,
            &format!("from device {k}"),
            None,
            vec![],
            NOW,
        );
        let id = s.write(&who("rocky"), &env, NOW).unwrap().id;
        assert_eq!(
            s.get(&id).unwrap().pubkey,
            pubkey_hex(&sk(k)),
            "the verifying key is on the record"
        );
    }
    // a key nobody registered is still nobody
    let forged = Envelope::sign(
        &sk(99),
        "room",
        "rocky",
        Kind::Say,
        "forged",
        None,
        vec![],
        NOW,
    );
    assert!(matches!(
        s.write(&who("rocky"), &forged, NOW),
        Err(StoreError::Unauthorized(_))
    ));
    // kin cannot add a device to rocky, and one key cannot be two identities
    assert!(matches!(
        s.apply_key_event(&key_ev(3, "kin", KeyAction::Add, "rocky", Some(30)), NOW),
        Err(StoreError::Forbidden(_))
    ));
    assert!(matches!(
        s.apply_key_event(&key_ev(1, "blaze", KeyAction::Add, "kin", Some(20)), NOW),
        Err(StoreError::Rejected(_))
    ));
}

#[test]
fn a_device_key_under_the_owners_handle_can_speak_but_never_administer() {
    let s = oracle();
    // blaze's web gateway key: added by the owner key, lives on a server
    s.apply_key_event(&key_ev(1, "blaze", KeyAction::Add, "blaze", Some(40)), NOW)
        .unwrap();
    let env = Envelope::sign(
        &sk(40),
        "room",
        "blaze",
        Kind::Say,
        "posted from the web",
        None,
        vec![],
        NOW,
    );
    s.write(&who("blaze"), &env, NOW).unwrap();

    // a stolen web key must not be able to mint seats, reset seats, or evict the owner key
    assert!(matches!(
        s.apply_key_event(
            &key_ev(40, "blaze", KeyAction::Add, "mallory", Some(41)),
            NOW
        ),
        Err(StoreError::Forbidden(_))
    ));
    assert!(matches!(
        s.apply_key_event(
            &key_ev(40, "blaze", KeyAction::Rotate, "rocky", Some(42)),
            NOW
        ),
        Err(StoreError::Forbidden(_))
    ));
    let evict = KeyEvent::sign(
        &sk(40),
        "blaze",
        KeyAction::Revoke,
        "blaze",
        &pubkey_hex(&sk(1)),
        NOW,
    );
    assert!(matches!(
        s.apply_key_event(&evict, NOW),
        Err(StoreError::Forbidden(_))
    ));
    assert!(matches!(
        s.apply_key_event(&key_ev(40, "blaze", KeyAction::Revoke, "blaze", None), NOW),
        Err(StoreError::Forbidden(_))
    ));
    // rotating itself replaces only itself, and it stays a seat key
    s.apply_key_event(
        &key_ev(40, "blaze", KeyAction::Rotate, "blaze", Some(43)),
        NOW,
    )
    .unwrap();
    let keys = s.active_keys("blaze").unwrap();
    assert_eq!(
        keys,
        vec![
            (pubkey_hex(&sk(1)), "owner".to_string()),
            (pubkey_hex(&sk(43)), "seat".to_string())
        ]
    );
    assert!(matches!(
        s.apply_key_event(
            &key_ev(43, "blaze", KeyAction::Add, "mallory", Some(44)),
            NOW
        ),
        Err(StoreError::Forbidden(_))
    ));
    // the owner key still can
    s.apply_key_event(
        &key_ev(1, "blaze", KeyAction::Add, "newseat", Some(45)),
        NOW,
    )
    .unwrap();
}

#[test]
fn revoking_one_device_leaves_the_seat_and_its_tokens_alive() {
    let s = oracle();
    s.apply_key_event(&key_ev(1, "blaze", KeyAction::Add, "rocky", Some(20)), NOW)
        .unwrap();
    let token = s.mint_token("rocky", Scope::Write, "t", NOW).unwrap();
    let lost = KeyEvent::sign(
        &sk(1),
        "blaze",
        KeyAction::Revoke,
        "rocky",
        &pubkey_hex(&sk(20)),
        NOW,
    );
    s.apply_key_event(&lost, NOW).unwrap();
    assert!(
        s.authenticate(&token).is_ok(),
        "one lost laptop does not log the seat out"
    );
    let from_lost = Envelope::sign(
        &sk(20),
        "room",
        "rocky",
        Kind::Say,
        "from the lost laptop",
        None,
        vec![],
        NOW,
    );
    assert!(matches!(
        s.write(&who("rocky"), &from_lost, NOW),
        Err(StoreError::Unauthorized(_))
    ));
    say(&s, 2, "rocky", "room", "still here", NOW);
    // revoking the seat (no key named) ends it, tokens and all
    s.apply_key_event(&key_ev(1, "blaze", KeyAction::Revoke, "rocky", None), NOW)
        .unwrap();
    assert!(s.authenticate(&token).is_err());
    // an owner reset of a seat leaves exactly the new key
    s.apply_key_event(&key_ev(1, "blaze", KeyAction::Add, "kin", Some(50)), NOW)
        .unwrap();
    s.apply_key_event(&key_ev(1, "blaze", KeyAction::Rotate, "kin", Some(51)), NOW)
        .unwrap();
    assert_eq!(
        s.active_keys("kin").unwrap(),
        vec![(pubkey_hex(&sk(51)), "seat".to_string())]
    );
}

#[test]
fn a_registry_from_before_multi_key_seats_is_upgraded_in_place() {
    let path = std::env::temp_dir().join(format!("oracle-mig-{}.db", oracle_proto::random_hex(6)));
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch("CREATE TABLE keys (handle TEXT PRIMARY KEY, pubkey TEXT NOT NULL, role TEXT NOT NULL, created_ms INTEGER NOT NULL, revoked_ms INTEGER);").unwrap();
        c.execute(
            "INSERT INTO keys VALUES('blaze',?1,'owner',1,NULL),('rocky',?2,'seat',2,NULL)",
            rusqlite::params![pubkey_hex(&sk(1)), pubkey_hex(&sk(2))],
        )
        .unwrap();
    }
    let s = Store::open(path.to_str().unwrap()).unwrap();
    assert_eq!(s.active_keys("rocky").unwrap().len(), 1);
    s.apply_key_event(&key_ev(1, "blaze", KeyAction::Add, "rocky", Some(20)), NOW)
        .unwrap();
    assert_eq!(
        s.active_keys("rocky").unwrap().len(),
        2,
        "the old primary key would have refused this row"
    );
    drop(s);
    assert_eq!(
        Store::open(path.to_str().unwrap())
            .unwrap()
            .active_keys("blaze")
            .unwrap()[0]
            .1,
        "owner",
        "opening twice is harmless"
    );
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
}

#[test]
fn a_stored_observation_can_be_re_verified_by_anyone_from_its_envelope() {
    let s = oracle();
    let sent = Envelope::sign(
        &sk(2),
        "room",
        "rocky",
        Kind::Say,
        "prove it",
        Some("{\"k\":1}".into()),
        vec!["b".into(), "a".into()],
        NOW,
    );
    let id = s.write(&who("rocky"), &sent, NOW).unwrap().id;
    let back = s.envelope_of(&id).unwrap();
    assert_eq!(back, sent, "byte for byte what was signed");
    back.verify(&s.get(&id).unwrap().pubkey, NOW)
        .expect("verifies offline against the recorded key");

    use oracle_server::store::LegacyRow;
    let old: LegacyRow = serde_json::from_value(serde_json::json!({"id": "OBS-old", "channel": "room", "author": "kin", "content": "history", "created_ms": NOW})).unwrap();
    s.import_legacy(&[old]).unwrap();
    assert!(
        matches!(s.envelope_of("OBS-old"), Err(StoreError::NotFound)),
        "imported history has no envelope to show"
    );
}

#[test]
fn legacy_import_conflicts_are_atomic_and_repairs_preserve_both_versions() {
    use oracle_server::store::LegacyRow;
    let s = oracle();
    let value = serde_json::json!({"id":"OBS-lossless","channel":"room","author":"kin","content":"original","created_ms":NOW,"cargo":"short"});
    let row = |v| serde_json::from_value::<LegacyRow>(v).unwrap();
    s.import_legacy(&[row(value.clone())]).unwrap();
    let mut changed = value.clone();
    changed["cargo"] = serde_json::json!("complete evidence");
    let mut innocent = value.clone();
    innocent["id"] = serde_json::json!("OBS-aaa-rollback");
    assert!(s
        .import_legacy(&[row(innocent), row(changed.clone())])
        .is_err());
    assert!(s.get("OBS-aaa-rollback").is_err());
    changed["repair_legacy"] = serde_json::json!(true);
    assert!(
        s.import_legacy(&[row(changed.clone())]).is_err(),
        "repair requires source"
    );
    changed["source"] = serde_json::json!({"evidence":"complete evidence","confidence":"speculative","custom":{"keep":"everything"}});
    s.import_legacy(&[row(changed.clone())]).unwrap();
    assert_eq!(
        s.get("OBS-lossless").unwrap().cargo.as_deref(),
        Some("complete evidence")
    );
    assert_eq!(s.legacy_sources("OBS-lossless").unwrap().len(), 2);
    s.import_legacy(&[row(changed)]).unwrap();
    assert_eq!(
        s.legacy_sources("OBS-lossless").unwrap().len(),
        2,
        "retry does not duplicate source"
    );
}

#[test]
fn late_historical_state_cannot_replace_a_newer_head() {
    use oracle_server::store::LegacyRow;
    let s = oracle();
    let row = |id: &str, at: i64, state: &str| {
        serde_json::from_value::<LegacyRow>(serde_json::json!({
        "id":id,"channel":"board","author":"rocky","content":id,"created_ms":at,
        "tags":["live-state","subject:late-history",format!("state:{state}"),"owner:rocky"]}))
        .unwrap()
    };
    s.import_legacy(&[row("OBS-new", NOW, "done")]).unwrap();
    s.import_legacy(&[row("OBS-arrived-late", NOW - 10000, "open")])
        .unwrap();
    assert_eq!(
        s.state_head("late-history").unwrap().observation.id,
        "OBS-new"
    );
}

#[test]
fn graph_synthesis_ranks_below_other_rooms_for_the_same_words() {
    // Blaze, 2026-10-08: "weight graph-synthesis down everywhere". Without the weight the echo (both words twice)
    // outranks the room message (each once).
    let s = oracle();
    say(&s, 3, "kin", "graph-synthesis", "observer protocol observer protocol", NOW);
    say(&s, 3, "kin", "room", "the observer protocol was signed", NOW);
    let hits = s.search("observer protocol", None, None, 10).unwrap();
    assert_eq!(hits.iter().map(|h| h.obs.channel.as_str()).collect::<Vec<_>>(), ["room", "graph-synthesis"]);
    assert!(hits[0].score >= hits[1].score);
}
