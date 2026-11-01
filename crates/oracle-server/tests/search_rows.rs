//! Search S1 (docs/SEARCH.md): every hit is a row (class, title, a clean snippet, thread, author kind), and a synthetic
//! record ranks under every first-hand one.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{clean_text, Principal, Ranker, Scope, Store};

const NOW: i64 = 1_790_000_000_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn oracle(dir: &std::path::Path) -> Store {
    let s = Store::open(dir.join("oracle.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 1000), NOW - 1000).unwrap();
    s
}

#[allow(clippy::too_many_arguments)]
fn put(s: &Store, key: &SigningKey, who: &str, room: &str, kind: Kind, text: &str, cargo: Option<String>, tags: Vec<String>, at: i64) -> String {
    let env = Envelope::sign(key, room, who, kind, text, cargo, tags, at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}

#[test]
fn a_synthetic_record_ranks_under_the_first_hand_one_it_paraphrases_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let source = put(&s, &sk(2), "rocky", "dot", Kind::Say, "The channel capacity of the DOT mesh is bounded by the lantern code.", None, vec![], NOW);
    let loud = "channel capacity lantern code ".repeat(12);
    // The negative control: the same loud text, word for word, in a first-hand room (notes) and in the synthesis room.
    // Only the room differs, so where they land relative to each other is the rule under test, not the words.
    let control = put(&s, &sk(2), "rocky", "notes", Kind::Say, &loud, None, vec![], NOW + 1);
    let synth = put(&s, &sk(2), "rocky", "graph-synthesis", Kind::Say, &format!("{loud} (paraphrase)"), None, vec![], NOW + 2);
    let (hits, _) = s.search_ranked("channel capacity lantern", None, None, 5, None, Ranker::Keyword).unwrap();
    let order: Vec<(&str, bool)> = hits.iter().map(|h| (h.obs.id.as_str(), h.synthetic)).collect();
    let pos = |id: &str| hits.iter().position(|h| h.obs.id == id).unwrap_or(99);
    assert!(pos(&control) < pos(&synth), "the first-hand copy of the same words outranks the synthetic one: {order:?}");
    assert!(pos(&source) < pos(&synth), "the source outranks the synthetic paraphrase: {order:?}");
    let last_first_hand = hits.iter().rposition(|h| !h.synthetic).unwrap();
    let first_synthetic = hits.iter().position(|h| h.synthetic).unwrap_or_else(|| panic!("no synthetic hit in {order:?}"));
    assert!(last_first_hand < first_synthetic, "every first-hand hit before every synthetic one: {order:?}");
    let sh = &hits[pos(&synth)];
    assert!(sh.synthetic && sh.class == "synthetic");
    assert!(!hits[pos(&control)].synthetic && hits[pos(&control)].class == "message");
    // A tag marks a synthetic record in any room.
    // (Its own words: the same body would fold into the control as a copy, which is the dedupe doing its job.)
    let tagged = put(&s, &sk(2), "rocky", "dot", Kind::Say, "channel capacity and the lantern, restated by a model with no source", None, vec!["synthetic".into()], NOW + 3);
    let (hits, _) = s.search_ranked("channel capacity lantern", None, None, 6, None, Ranker::Keyword).unwrap();
    let t = hits.iter().find(|h| h.obs.id == tagged).unwrap();
    assert!(t.synthetic);
    assert!(hits.iter().position(|h| h.obs.id == source).unwrap() < hits.iter().position(|h| h.obs.id == tagged).unwrap());
}

#[test]
fn the_row_has_a_clean_snippet_a_title_a_class_a_thread_and_the_author_kind() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let import = put(&s, &sk(1), "blaze", "illuminati", Kind::Say,
        "[CLAUDE-EXPORT · blaze · 2026-03-26 · room:6fa5 'The Room' · turn 7]\nThis block is not supported on your current device yet.\nThe quokka ledger holds every turn of the room, signed.\nA second line.",
        None, vec![], NOW);
    let (hits, _) = s.search_ranked("quokka ledger", None, None, 5, None, Ranker::Keyword).unwrap();
    let h = hits.iter().find(|h| h.obs.id == import).expect("the import is a hit");
    assert_eq!(h.class, "import");
    assert_eq!(h.title, "The quokka ledger holds every turn of the room, signed.");
    assert!(h.snippet.contains("quokka ledger") && !h.snippet.contains("CLAUDE-EXPORT") && !h.snippet.contains("not supported"), "{}", h.snippet);
    assert!(h.obs.content.contains("CLAUDE-EXPORT"), "the signed content is untouched");
    assert_eq!(h.author_kind, "person", "blaze holds the owner key");
    assert_eq!(clean_text("  a  b \n\n[CLAUDE-EXPORT x]\nc"), "a b\nc");

    let head = put(&s, &sk(2), "rocky", "board", Kind::State, "Cutting the quokka release", Some(r#"{"subject":"ship-quokka","state":"running","next_touch":"2026-10-22"}"#.into()), vec!["subject:ship-quokka".into()], NOW + 1);
    let page = put(&s, &sk(2), "rocky", "dot", Kind::Say, "A page about quokka ledgers", Some(r##"{"schema":"doc/1","title":"Quokka ledgers, a page","markdown":"# Quokka\n\nwords"}"##.into()), vec![], NOW + 2);
    let q = put(&s, &sk(2), "rocky", "dot", Kind::Say, "Is the quokka ledger signed?", None, vec![], NOW + 3);
    put(&s, &sk(2), "rocky", "dot", Kind::Say, "a reply", None, vec![format!("reply:{q}")], NOW + 4);
    put(&s, &sk(1), "blaze", "dot", Kind::Say, "yes, signed", None, vec![format!("reply:{q}"), format!("closes:{q}")], NOW + 5);
    let (hits, _) = s.search_ranked("quokka", None, None, 10, None, Ranker::Keyword).unwrap();
    let find = |id: &str| hits.iter().find(|h| h.obs.id == id).unwrap_or_else(|| panic!("{id} is a hit"));
    let hh = find(&head);
    assert_eq!(hh.class, "head");
    assert_eq!(hh.title, "ship-quokka · Cutting the quokka release");
    assert_eq!(hh.author_kind, "model");
    let ph = find(&page);
    assert_eq!((ph.class, ph.title.as_str()), ("page", "Quokka ledgers, a page"));
    let qh = find(&q);
    assert_eq!(qh.class, "message");
    assert_eq!((qh.thread.replies, qh.thread.answered, qh.thread.reply_to.as_deref()), (1, true, None), "one plain reply, answered by the close");
}

/// The in-memory store has one connection. A search that took a second one from the pool for the author kind
/// waited out r2d2's 30 s timeout per author (the scoreboard suite ran for hours on the host, 2026-10-21).
/// Negative control: the same search on the file store, which has eight connections, was always fast.
#[test]
fn a_search_takes_no_second_connection() {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 100_000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 100_000), NOW - 100_000).unwrap();
    put(&s, &sk(1), "blaze", "notes", Kind::Say, "the lantern and channel capacity, first hand", None, vec![], NOW);
    put(&s, &sk(2), "rocky", "notes", Kind::Say, "the lantern and channel capacity, a reply", None, vec![], NOW + 1);
    let t0 = std::time::Instant::now();
    let hits = s.search("lantern capacity", None, None, 10).unwrap();
    assert!(t0.elapsed() < std::time::Duration::from_secs(5), "search took {:?}", t0.elapsed());
    assert_eq!(hits.len(), 2);
    let kinds: Vec<&str> = hits.iter().map(|h| h.author_kind).collect();
    assert!(kinds.contains(&"person") && kinds.contains(&"model"), "{kinds:?}");
}

/// A graph node (#graph-records, the import's node table) holds the words but is never the answer: class node, under
/// every first-hand hit. Negative control: the same text with a graph: tag in an ordinary room stays a message and
/// keeps its rank.
#[test]
fn a_graph_node_ranks_under_first_hand_hits() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let node = put(&s, &sk(2), "rocky", "graph-records", Kind::Say, "rocky", None, vec!["graph:Handle".into()], NOW);
    let said = put(&s, &sk(1), "blaze", "notes", Kind::Say, "rocky, the lantern and the channel capacity", None, vec![], NOW + 1);
    let tagged = put(&s, &sk(1), "blaze", "notes", Kind::Say, "rocky", None, vec!["graph:Handle".into()], NOW + 2);
    let hits = s.search("rocky", None, None, 10).unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.obs.id.as_str()).collect();
    let pos = |id: &str| ids.iter().position(|x| *x == id).unwrap_or_else(|| panic!("{id} missing from {ids:?}"));
    assert_eq!(hits[pos(&node)].class, "node");
    assert_eq!(hits[pos(&tagged)].class, "message", "a graph: tag outside #graph-records is not a node");
    assert!(pos(&node) > pos(&said) && pos(&node) > pos(&tagged), "node must come last: {ids:?}");
    assert!(!hits[pos(&node)].synthetic);
}
