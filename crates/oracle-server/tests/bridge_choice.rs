//! A message to the owner that offers a choice in prose must be a decide (Blaze, 31 Oct 2026: "Oracle should tell them, send this
//! in this schema if it contains an option or a recommendation, so that Blaze can choose from the Bridge"). Refused with the schema;
//! everything else passes. Real signed writes on the real store.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};
use serde_json::json;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn store() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), 1).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), 2), 2).unwrap();
    s
}
fn say(s: &Store, who: &str, text: &str, tags: &[&str], cargo: Option<serde_json::Value>, at: i64) -> Result<String, String> {
    let key = if who == "blaze" { 1 } else { 2 };
    let env = Envelope::sign(&sk(key), "rocky", who, Kind::Say, text, cargo.map(|c| c.to_string()), tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).map(|r| r.id).map_err(|e| e.to_string())
}

#[test]
fn a_choice_in_prose_to_the_owner_is_refused_with_the_schema() {
    let s = store();
    let prose = "How should Pro charge? 1) pay per use 2) $19 a month 3) both. I recommend 1.";
    for tags in [&["to:blaze"][..], &[][..]] {
        let text = if tags.is_empty() { format!("@blaze {prose}") } else { prose.to_string() };
        let e = say(&s, "rocky", &text, tags, None, 10).unwrap_err();
        assert!(e.contains("must be a decide") && e.contains("--ask decide") && e.contains("recommendation") && e.contains("oracle_say"), "{e}");
    }
    let e = say(&s, "rocky", "@blaze Option A is faster, Option B is cheaper: which do you want?", &[], None, 11).unwrap_err();
    assert!(e.contains("must be a decide"), "{e}");
    let e = say(&s, "rocky", "@blaze naïve café ü: (a) the bridge first (b) the search first; your call", &[], None, 12).unwrap_err();
    assert!(e.contains("must be a decide"), "non-ASCII text is read, never a panic: {e}");
}

#[test]
fn everything_that_is_not_a_choice_for_the_owner_passes() {
    let s = store();
    // The same choice as a decide.
    let cargo = json!({"options": ["pay per use", "$19 a month", "both"], "recommendation": {"option": 1, "why": "cheapest and no cliff"}});
    say(&s, "rocky", "How should Pro charge?", &["to:blaze", "ask:decide"], Some(cargo), 20).unwrap();
    // To someone else; numbers that are not options; a list with no ask; the owner's own words; an answer that picks.
    say(&s, "rocky", "@emmy 1) the ledger 2) the guard. I recommend 1.", &[], None, 21).unwrap();
    say(&s, "rocky", "@blaze hit@5 0.966, v1.2 shipped; I recommend nothing new today.", &[], None, 22).unwrap();
    say(&s, "rocky", "@blaze done today: 1) codes live 2) PageRank measured 3) trail next.", &[], None, 23).unwrap();
    say(&s, "blaze", "@rocky 1) do it 2) then that. I recommend speed.", &["to:rocky"], None, 24).unwrap();
    let ask = say(&s, "rocky", "Pick?", &["to:blaze", "ask:decide"], Some(json!({"options": ["a", "b"], "recommendation": {"option": 1, "why": "w"}})), 25).unwrap();
    say(&s, "blaze", "@rocky 1) a 2) b, I choose 1", &[&format!("reply:{ask}"), "choice:1"], None, 26).unwrap();
}
