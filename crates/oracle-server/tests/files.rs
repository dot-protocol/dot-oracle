//! Files through the store: bytes in, the sha256 is the id, the same bytes are stored once (Round 68).
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::files;
use oracle_server::store::{Principal, Scope, Store, StoreError};
use sha2::{Digest, Sha256};

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

#[test]
fn bytes_in_their_sha256_out_and_back_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let png: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    let want = hex::encode(Sha256::digest(&png));
    let p = files::put(&s, "rocky", "dot", Some("face.png"), "image/png", &png, NOW).unwrap();
    assert_eq!(p.id, format!("sha256:{want}"));
    assert!(!p.duplicate);
    let (back, ctype) = files::bytes(&s, &p.id).unwrap();
    assert_eq!(back, png);
    assert_eq!(ctype, "image/png");
    // bare hex works too, and case does not matter
    assert_eq!(files::bytes(&s, &want.to_uppercase()).unwrap().0, png);
}

#[test]
fn the_same_bytes_are_stored_once_and_remember_every_room() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let a = files::put(&s, "rocky", "dot", None, "text/markdown; charset=utf-8", b"# page\n", NOW).unwrap();
    let b = files::put(&s, "rocky", "oracle", Some("page.md"), "text/markdown", b"# page\n", NOW + 1).unwrap();
    let c = files::put(&s, "rocky", "dot", None, "text/markdown", b"# page\n", NOW + 2).unwrap();
    assert_eq!(a.id, b.id);
    assert!(b.duplicate && c.duplicate);
    assert_eq!(a.content_type, "text/markdown");
    let m = files::meta(&s, &a.id).unwrap();
    assert_eq!(m.size, 7);
    assert_eq!(m.rooms.iter().map(|p| p.room.as_str()).collect::<Vec<_>>(), vec!["dot", "oracle"]);
    assert_eq!(files::list(&s, "oracle", 10).unwrap().len(), 1);
    assert!(files::list(&s, "hustler", 10).unwrap().is_empty());
}

#[test]
fn a_message_that_cites_the_file_is_listed_on_it() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let p = files::put(&s, "rocky", "dot", None, "image/png", b"\x89PNG fake", NOW).unwrap();
    let env = Envelope::sign(&sk(2), "dot", "rocky", Kind::Say, "the face", None, vec![format!("file:{}", p.sha256)], NOW + 10);
    let id = s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, NOW + 10).unwrap().id;
    assert_eq!(files::meta(&s, &p.id).unwrap().cited_by, vec![id]);
}

#[test]
fn refusals_are_named() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let rejected = |r: Result<files::Put, StoreError>| matches!(r, Err(StoreError::Rejected(_)));
    assert!(rejected(files::put(&s, "rocky", "Dot!", None, "text/plain", b"x", NOW)));
    assert!(rejected(files::put(&s, "rocky", "dot", None, "text/plain", b"", NOW)));
    assert!(rejected(files::put(&s, "rocky", "dot", Some("../etc/passwd"), "text/plain", b"x", NOW)));
    assert!(rejected(files::put(&s, "rocky", "dot", None, "nonsense", b"x", NOW)));
    let big = vec![0u8; files::MAX_BYTES + 1];
    assert!(rejected(files::put(&s, "rocky", "dot", None, "application/octet-stream", &big, NOW)));
    assert!(matches!(files::bytes(&s, "sha256:abc"), Err(StoreError::Rejected(_))));
    assert!(matches!(files::bytes(&s, &"0".repeat(64)), Err(StoreError::NotFound)));
}
