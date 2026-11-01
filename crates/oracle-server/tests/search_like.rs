//! A machine's templated repeats take one place in recall (Blaze, 31 Oct 2026: Hypatia's heartbeat receipts took four
//! places of ten). The first-ranked of them stands for the rest, which it names (`like`); a person's words never fold.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{template_key, Principal, Ranker, Scope, Store};

const NOW: i64 = 1_790_000_000_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn put(s: &Store, who: &str, key: u8, room: &str, text: &str, at: i64) -> String {
    let e = Envelope::sign(&sk(key), room, who, Kind::Say, text, None, vec![], at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &e, at).unwrap().id
}

#[test]
fn templated_receipts_fold_into_one_hit_and_a_persons_words_do_not() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("o.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "hypatia", &pubkey_hex(&sk(2)), NOW - 900), NOW - 900).unwrap();
    let mut beats = vec![];
    for (i, t) in ["14:20", "15:20", "15:50", "16:50"].iter().enumerate() {
        beats.push(put(&s, "hypatia", 2, "coordination", &format!("Hypatia heartbeat receipt 2026-06-20T{t}Z: Checked live Oracle: healthy. Read the inbox, routed review asks for the lantern queue, step {i}."), NOW + i as i64));
    }
    let other = put(&s, "hypatia", 2, "coordination", "Hypatia receipt: completed commit afdfa90 on vps-hypatia, the lantern queue guard with an explicit budget and a harness timeout.", NOW + 10);
    let mut said = vec![];
    for i in 0..2 {
        said.push(put(&s, "blaze", 1, "coordination", &format!("The lantern queue: I want every heartbeat receipt to say what changed, not that it is healthy, take {i}."), NOW + 20 + i));
    }

    let (hits, _) = s.search_in("lantern queue", None, None, 10, None, Ranker::Rrf, false).unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.obs.id.as_str()).collect();
    let beat = hits.iter().find(|h| beats.contains(&h.obs.id)).expect("a heartbeat stands for them");
    assert_eq!(beat.like.len(), 3, "the other three heartbeats are named on it");
    assert_eq!(hits.iter().filter(|h| beats.contains(&h.obs.id)).count(), 1, "one place for four heartbeats");
    assert!(ids.contains(&other.as_str()), "a different receipt by the same seat keeps its place");
    assert!(said.iter().all(|id| ids.contains(&id.as_str())), "a person's words never fold: {ids:?}");

    // The key: times, dates, counts and hex ids do not make a new template; other words, another author or room do.
    let k = |a: &str, r: &str, t: &str| template_key(a, r, t);
    let base = "Hypatia heartbeat receipt 2026-06-20T14:20Z: Checked live Oracle: healthy. Read the inbox and the review queue.";
    assert_eq!(k("hypatia", "c", base), k("hypatia", "c", &base.replace("14:20", "09:05").replace("20T", "21T")));
    assert_eq!(k("h", "c", "Hypatia receipt: commit afdfa90 pushed to vps-hypatia with the guard, lint passed, tests passed, done."),
               k("h", "c", "Hypatia receipt: commit 84f2ed9 pushed to vps-hypatia with the guard, lint passed, tests passed, done."));
    assert_ne!(k("hypatia", "c", base), k("emmy", "c", base));
    assert_ne!(k("hypatia", "c", base), k("hypatia", "d", base));
    assert_ne!(k("hypatia", "c", base), k("hypatia", "c", &base.replace("Checked live", "Restarted the")));
    assert_eq!(k("hypatia", "c", "short receipt 12:00"), None, "a short text has no template");
}
