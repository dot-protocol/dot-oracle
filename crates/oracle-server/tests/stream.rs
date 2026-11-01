//! `GET /stream` (docs/STREAM.md): `id: <seq>` on every observation, `room=` filtering on the server, a resume by
//! `Last-Event-ID` or `after=` that replays what was missed oldest first and never sends one twice, `reset` past the
//! replay bound, and `lagged` (then the end) for a reader that falls behind. A real server on a loopback port, a real
//! SSE reader.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::http::{follow, now_ms, router, App, Followed, Live, STREAM_REPLAY_MAX};
use oracle_server::store::{Principal, Scope, Store};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;
use std::time::Duration;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

struct Srv {
    base: String,
    app: App,
    token: String,
}

fn start() -> Srv {
    let store = Store::memory().unwrap();
    store.init_owner("blaze", &pubkey_hex(&sk(1)), 1).unwrap();
    store.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), 2), 2).unwrap();
    let token = store.mint_token("rocky", Scope::Read, "stream test", 3).unwrap();
    let app = App::new(store);
    let served = app.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
            let l = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(l, router(served)).await.unwrap();
        })
    });
    Srv { base, app, token }
}

impl Srv {
    /// Store one message in `room` as rocky and tell the stream, as `POST /observations` does. Returns its seq.
    fn say(&self, room: &str, text: &str) -> i64 {
        let at = now_ms();
        let env = Envelope::sign(&sk(2), room, "rocky", Kind::Say, text, None, vec![], at);
        let r = self.app.store.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, at).unwrap();
        self.app.publish(&self.app.store.get(&r.id).unwrap());
        r.seq
    }
    fn open(&self, query: &str, last_event_id: Option<i64>) -> Reader {
        let agent = ureq::AgentBuilder::new().timeout_read(Duration::from_secs(5)).build();
        let mut req = agent.get(&format!("{}/stream{query}", self.base)).set("Authorization", &format!("Bearer {}", self.token));
        if let Some(id) = last_event_id {
            req = req.set("Last-Event-ID", &id.to_string());
        }
        let r = req.call().unwrap();
        Reader { lines: BufReader::new(r.into_reader()) }
    }
}

struct Reader {
    lines: BufReader<Box<dyn Read + Send + Sync>>,
}

#[derive(Debug)]
struct Ev {
    event: String,
    id: Option<String>,
    data: Value,
}

impl Reader {
    /// The next event, or None when the server ended the stream (or nothing came for 5 s).
    fn next(&mut self) -> Option<Ev> {
        let (mut event, mut id, mut data) = (String::new(), None, String::new());
        loop {
            let mut line = String::new();
            match self.lines.read_line(&mut line) {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if event.is_empty() && data.is_empty() {
                    continue; // a keep-alive comment's end
                }
                return Some(Ev { event, id, data: serde_json::from_str(&data).unwrap_or(Value::Null) });
            }
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().into();
            } else if let Some(v) = line.strip_prefix("id:") {
                id = Some(v.trim().into());
            } else if let Some(v) = line.strip_prefix("data:") {
                data.push_str(v.strip_prefix(' ').unwrap_or(v));
            }
        }
    }
    fn observation(&mut self) -> Ev {
        let e = self.next().expect("an event");
        assert_eq!(e.event, "observation", "{e:?}");
        e
    }
}

#[test]
fn every_observation_carries_its_seq_and_a_room_filter_is_applied_by_the_server() {
    let srv = start();
    let mut a = srv.open("?room=alpha", None);
    let hello = a.next().unwrap();
    assert_eq!((hello.event.as_str(), &hello.data["rooms"]), ("connected", &serde_json::json!(["alpha"])));
    let mut all = srv.open("", None);
    assert_eq!(all.next().unwrap().event, "connected");

    let b = srv.say("beta", "not for alpha");
    let s = srv.say("alpha", "for alpha");
    let got = a.observation();
    assert_eq!(got.id.as_deref(), Some(s.to_string().as_str()), "id is the seq");
    assert_eq!((got.data["channel"].as_str(), got.data["content"].as_str()), (Some("alpha"), Some("for alpha")), "beta's write was not sent");
    // The negative control: without room= the same beta write does arrive, so the filter is what kept it out.
    assert_eq!(all.observation().id, Some(b.to_string()));
    assert_eq!(all.observation().id, Some(s.to_string()));
}

#[test]
fn a_resuming_reader_gets_what_it_missed_oldest_first_and_never_one_twice() {
    let srv = start();
    let s1 = srv.say("alpha", "one");
    let s2 = srv.say("alpha", "two");
    srv.say("beta", "elsewhere");
    let s3 = srv.say("alpha", "three");

    let mut r = srv.open("?room=alpha", Some(s1));
    let hello = r.next().unwrap();
    assert_eq!(hello.data["after"], s1);
    assert_eq!(r.observation().id, Some(s2.to_string()));
    assert_eq!(r.observation().id, Some(s3.to_string()), "replayed in seq order, the other room's skipped");

    // A write already replayed arriving on the live feed is not sent again; the next new one is.
    srv.app.publish(&srv.app.store.get(&srv.app.store.recent(Some("alpha"), 1, None).unwrap()[0].id).unwrap());
    let s4 = srv.say("alpha", "four");
    assert_eq!(r.observation().id, Some(s4.to_string()));

    // `after=` is the same resume for a client that cannot set the header.
    let mut q = srv.open(&format!("?room=alpha&after={s3}"), None);
    q.next().unwrap();
    assert_eq!(q.observation().id, Some(s4.to_string()));
}

#[test]
fn a_gap_too_large_to_replay_is_a_reset_and_the_end() {
    let srv = start();
    for i in 0..=STREAM_REPLAY_MAX {
        srv.say("alpha", &format!("m{i}"));
    }
    let mut r = srv.open("?room=alpha&after=0", None);
    assert_eq!(r.next().unwrap().event, "connected");
    let reset = r.next().unwrap();
    assert_eq!((reset.event.as_str(), &reset.data["replay_max"]), ("reset", &serde_json::json!(STREAM_REPLAY_MAX)));
    assert!(r.next().is_none(), "the stream ends after reset");
}

#[tokio::test]
async fn a_reader_that_falls_behind_is_told_and_the_stream_ends() {
    use tokio_stream::StreamExt;
    let (tx, rx) = tokio::sync::broadcast::channel::<Arc<Live>>(2);
    let live = |seq: i64, channel: &str| Arc::new(Live { seq, channel: channel.into(), json: format!("{{\"seq\":{seq}}}") });
    let mut f = Box::pin(follow(rx, Some(vec!["alpha".into()]), 10));
    tx.send(live(9, "alpha")).unwrap(); // already held (seq <= 10): skipped
    tx.send(live(11, "beta")).unwrap(); // another room: skipped
    assert!(tokio::time::timeout(Duration::from_millis(50), f.next()).await.is_err(), "nothing to send yet");
    tx.send(live(12, "alpha")).unwrap();
    assert_eq!(f.next().await, Some(Followed::Observation { seq: 12, json: "{\"seq\":12}".into() }));
    for seq in 13..20 {
        tx.send(live(seq, "alpha")).unwrap(); // 7 writes into a buffer of 2
    }
    assert_eq!(f.next().await, Some(Followed::Lagged { missed: 5, last: 12 }), "the gap is named, from the last seq sent");
    assert_eq!(f.next().await, None, "and the stream ends, so the reader resumes with Last-Event-ID");
}

#[test]
fn an_open_stream_stops_carrying_a_room_the_moment_it_turns_private() {
    // Emmy's review of 3c3bdae: the policy is read again for every live record, not once when the stream opens.
    let srv = start();
    let mut r = srv.open("", None);
    assert_eq!(r.next().unwrap().event, "connected");
    srv.say("imran", "before it was private");
    assert_eq!(r.observation().data["content"], "before it was private");
    let blaze = |room: &str, text: &str, cargo: Option<String>| {
        let at = now_ms();
        let env = Envelope::sign(&sk(1), room, "blaze", Kind::Say, text, cargo, vec![], at);
        let w = srv.app.store.write(&Principal { handle: "blaze".into(), scope: Scope::Write }, &env, at).unwrap();
        srv.app.publish(&srv.app.store.get(&w.id).unwrap());
    };
    blaze("oracle", "[Private rooms] test", Some(r#"{"schema":"private-rooms/1","rooms":{"imran":["blaze"]}}"#.into()));
    assert_eq!(r.observation().data["channel"], "oracle");
    blaze("imran", "PRIVATE after the policy", None);
    srv.say("rocky", "still mine");
    let next = r.observation();
    assert_eq!(next.data["content"], "still mine", "the private record must not reach a non-member: {next:?}");
}
