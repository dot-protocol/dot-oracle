//! oracle-sync/1 (docs/SYNC.md): two nodes, one log. Emmy's W-OR-6 counterexamples are
//! the acceptance gate; each test names the case it covers.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Result, Scope, Store};
use oracle_server::sync::{round, KeyPage, Page, Peer, Report, SyncRow};

const NOW: i64 = 1_790_000_000_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

/// Node zero: the owner and one seat, as today's core.
fn core_node(dir: &std::path::Path) -> Store {
    let s = Store::open(dir.join("core.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 10_000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 9_000), NOW - 9_000).unwrap();
    s
}

/// A second node with the same owner root and nothing else: the Mac before its first round.
fn mac_node(dir: &std::path::Path) -> Store {
    let s = Store::open(dir.join("mac.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 10_000).unwrap();
    s
}

fn rocky() -> Principal {
    Principal { handle: "rocky".into(), scope: Scope::Write }
}

fn say(s: &Store, room: &str, text: &str, at: i64) -> String {
    s.write(&rocky(), &Envelope::sign(&sk(2), room, "rocky", Kind::Say, text, None, vec![], at), at).unwrap().id
}

fn state(s: &Store, subject: &str, word: &str, at: i64) -> String {
    let cargo = serde_json::json!({"subject": subject, "state": word, "owner": "rocky", "next_touch": "2026-11-01"}).to_string();
    s.write(&rocky(), &Envelope::sign(&sk(2), "oracle", "rocky", Kind::State, word, Some(cargo), vec![], at), at).unwrap().id
}

/// The other node, in process: the same three calls the HTTP peer makes.
struct Local<'a>(&'a Store);
impl Peer for Local<'_> {
    fn keys(&self, after: i64) -> Result<KeyPage> {
        self.0.sync_keys(after)
    }
    fn log(&self, after: i64, limit: i64, skip_origin: &str) -> Result<Page> {
        self.0.sync_log(after, limit, Some(skip_origin))
    }
    fn push(&self, origin: &str, rows: &[SyncRow]) -> Result<Report> {
        self.0.replicate(rows, origin, NOW)
    }
}

fn ids(s: &Store) -> Vec<String> {
    let mut v: Vec<String> = s.sync_log(0, 1000, None).unwrap().rows.into_iter().map(|r| r.id).collect();
    v.sort();
    v
}

#[test]
fn a_new_node_catches_up_with_keys_and_every_entry_verified_and_native() {
    let dir = tempfile::tempdir().unwrap();
    let (core, mac) = (core_node(dir.path()), mac_node(dir.path()));
    let a = say(&core, "room", "first", NOW);
    let b = say(&core, "oracle", "second", NOW + 1);
    let r = round(&mac, "mac", "core", &Local(&core), NOW).unwrap();
    assert_eq!((r.keys, r.pulled.added), (1, 2), "{r:?}");
    assert_eq!(ids(&mac), ids(&core));
    // Stored under the origin id, native: the stranger's verification of the row is unchanged on the copy.
    assert_eq!(mac.envelope_of(&a).unwrap(), core.envelope_of(&a).unwrap());
    assert_eq!(mac.envelope_of(&b).unwrap(), core.envelope_of(&b).unwrap());
}

#[test]
fn concurrent_valid_appends_in_a_partition_are_both_kept() {
    // W-OR-6 concurrent-valid-appends: both retained, no fork for independent entries.
    let dir = tempfile::tempdir().unwrap();
    let (core, mac) = (core_node(dir.path()), mac_node(dir.path()));
    round(&mac, "mac", "core", &Local(&core), NOW).unwrap();
    let on_core = say(&core, "room", "written on core while apart", NOW + 10);
    let on_mac = mac.write(&rocky(), &Envelope::sign(&sk(2), "room", "rocky", Kind::Say, "written on the mac while apart", None, vec![], NOW + 11), NOW + 11).unwrap().id;
    let r = round(&mac, "mac", "core", &Local(&core), NOW + 20).unwrap();
    assert_eq!((r.pulled.added, r.pushed.added), (1, 1), "{r:?}");
    assert_eq!(ids(&mac), ids(&core));
    assert!(ids(&core).contains(&on_mac) && ids(&mac).contains(&on_core));
}

#[test]
fn a_second_round_moves_nothing_and_nothing_travels_back() {
    // Exact duplicates are idempotent; an entry never returns to the node it came from.
    let dir = tempfile::tempdir().unwrap();
    let (core, mac) = (core_node(dir.path()), mac_node(dir.path()));
    say(&core, "room", "one", NOW);
    let r1 = round(&mac, "mac", "core", &Local(&core), NOW).unwrap();
    assert_eq!(r1.pushed.added + r1.pushed.duplicate, 0, "what came from core is not pushed back: {r1:?}");
    let r2 = round(&mac, "mac", "core", &Local(&core), NOW + 1).unwrap();
    assert_eq!((r2.keys, r2.pulled.added, r2.pushed.added), (0, 0, 0), "{r2:?}");
    // Delivered again on purpose: a duplicate, not a second row.
    let again = core.sync_log(0, 1000, None).unwrap().rows;
    let rep = mac.replicate(&again, "core", NOW).unwrap();
    assert_eq!((rep.added, rep.duplicate), (0, again.len()));
}

#[test]
fn concurrent_state_revisions_settle_on_the_same_head_on_both_nodes() {
    // W-OR-6 conflicting-heads: both revisions kept; every node reaches the same head whatever the delivery order.
    let dir = tempfile::tempdir().unwrap();
    let (core, mac) = (core_node(dir.path()), mac_node(dir.path()));
    let base = state(&core, "oracle.sync", "open", NOW);
    round(&mac, "mac", "core", &Local(&core), NOW).unwrap();
    let later = state(&core, "oracle.sync", "running", NOW + 50);
    let mac_cargo = serde_json::json!({"subject": "oracle.sync", "state": "blocked", "owner": "rocky", "next_touch": "2026-11-01"}).to_string();
    let earlier = mac
        .write(&rocky(), &Envelope::sign(&sk(2), "oracle", "rocky", Kind::State, "blocked", Some(mac_cargo), vec![], NOW + 40), NOW + 40)
        .unwrap()
        .id;
    round(&mac, "mac", "core", &Local(&core), NOW + 60).unwrap();
    for s in [&core, &mac] {
        assert!(s.get(&base).is_ok() && s.get(&earlier).is_ok() && s.get(&later).is_ok(), "every revision kept");
        assert_eq!(s.state_head("oracle.sync").unwrap().observation.id, later, "the newest signed revision is the head on both");
    }
}

#[test]
fn a_tampered_entry_is_refused_and_set_aside_never_stored() {
    // W-OR-6 hostile-carry: content changed in transit no longer verifies; it is kept aside, not applied.
    let dir = tempfile::tempdir().unwrap();
    let (core, mac) = (core_node(dir.path()), mac_node(dir.path()));
    round(&mac, "mac", "core", &Local(&core), NOW).unwrap();
    say(&core, "room", "the real words", NOW + 1);
    let mut rows = core.sync_log(0, 1000, None).unwrap().rows;
    rows.last_mut().unwrap().content = "words a carrier changed".into();
    let rep = mac.replicate(&rows, "phone", NOW + 2).unwrap();
    assert_eq!(rep.added, 0);
    assert_eq!(rep.refused.len(), 1, "{rep:?}");
    assert!(mac.get(&rows.last().unwrap().id).is_err(), "never stored");
    assert_eq!(mac.sync_quarantine(10).unwrap().len(), 1);
}

#[test]
fn one_operation_signed_twice_is_quarantined_not_overwritten() {
    // W-OR-6 equivocation-replay: the same client_op with divergent bytes is reported; the first stays.
    let dir = tempfile::tempdir().unwrap();
    let (core, mac) = (core_node(dir.path()), mac_node(dir.path()));
    round(&mac, "mac", "core", &Local(&core), NOW).unwrap();
    let mut first = Envelope::sign(&sk(2), "room", "rocky", Kind::Say, "version one", None, vec![], NOW + 1);
    let mut second = Envelope::sign(&sk(2), "room", "rocky", Kind::Say, "version two", None, vec![], NOW + 2);
    second.client_op = first.client_op.clone();
    second = second.resign(&sk(2), NOW + 2);
    first = first.resign(&sk(2), NOW + 1);
    let kept = core.write(&rocky(), &first, NOW + 1).unwrap().id;
    mac.write(&rocky(), &second, NOW + 2).unwrap();
    let r = round(&mac, "mac", "core", &Local(&core), NOW + 3).unwrap();
    assert_eq!(r.pulled.quarantined.len() + r.pushed.quarantined.len(), 2, "each side reports the other's copy: {r:?}");
    assert_eq!(core.get(&kept).unwrap().content, "version one", "never overwritten");
}

#[test]
fn an_entry_from_an_unknown_key_waits_until_the_key_arrives() {
    // A seat's key not yet replayed here: the entry is refused, kept aside, and lands once the registry has caught up.
    let dir = tempfile::tempdir().unwrap();
    let (core, mac) = (core_node(dir.path()), mac_node(dir.path()));
    say(&core, "room", "from rocky", NOW);
    let rows = core.sync_log(0, 1000, None).unwrap().rows;
    let rep = mac.replicate(&rows, "core", NOW).unwrap();
    assert_eq!((rep.added, rep.refused.len()), (0, 1));
    let r = round(&mac, "mac", "core", &Local(&core), NOW + 1).unwrap();
    assert_eq!(r.keys, 1);
    // The round's pull starts from its own cursor (0), so the entry arrives with the key that verifies it.
    assert_eq!(ids(&mac), ids(&core));
}
