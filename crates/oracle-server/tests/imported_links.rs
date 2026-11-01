//! An imported answer answers. Rows imported from the graph Oracle carry their reply link in the legacy cargo
//! (`replyTo`), not in their tags; the import indexed it, but the thread judgement reads tags, so an ask answered
//! by an imported message stayed open forever (Emmy).

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::asks::{asks_for, StateFilter};
use oracle_server::store::{LegacyRow, Principal, Scope, Store};

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

#[test]
fn an_ask_answered_by_an_imported_message_is_answered() {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    let env = Envelope::sign(&sk(2), "axxis", "rocky", Kind::Say, "@emmy please review the /opt plan", None,
        vec!["to:emmy".into(), "intent:proposal".into(), "ask:reply".into()], NOW);
    let ask = s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, NOW).unwrap().id;
    let open = |s: &Store| asks_for(s, "emmy", None, StateFilter::Open, NOW + 60 * MIN).unwrap().waiting_on_me.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
    assert_eq!(open(&s), vec![ask.clone()], "open until answered");
    // Emmy's answer, imported: the link is only in the legacy cargo.
    s.import_legacy(&[LegacyRow {
        id: "OBS-axxis-20000926-819752aee0a5021123".into(), channel: "axxis".into(), author: "emmy".into(), kind: "say".into(),
        content: format!("@rocky Re {ask}: HOLD the wholesale move; peripheral moves one at a time."),
        cargo: Some(serde_json::json!({"replyTo": ask}).to_string()), tags: vec!["from:emmy".into(), "kind:say".into()],
        created_ms: NOW + 10 * MIN, pubkey: String::new(), sig: String::new(), news: true, source: None, repair_legacy: false,
    }]).unwrap();
    assert!(open(&s).is_empty(), "the imported answer answers it: {:?}", open(&s));
    // The materialized room/thread view must agree, both incrementally and after replay.
    let check_index = || {
        let (state, answered): (String, Option<i64>) = s.with_conn(|c| Ok(c.query_row(
            "SELECT t.state,t.answered_by FROM thread t JOIN observations o ON o.seq=t.root_seq WHERE o.id=?1",
            [&ask], |r| Ok((r.get(0)?, r.get(1)?)))?)).unwrap();
        assert_eq!(state, "answered");
        assert!(answered.is_some());
    };
    check_index();
    s.reindex().unwrap();
    check_index();
    // Native rows are untouched: a signed message's tags are exactly what was signed.
    let o = s.get(&ask).unwrap();
    assert!(!o.tags.iter().any(|t| t.starts_with("re:")));
}
