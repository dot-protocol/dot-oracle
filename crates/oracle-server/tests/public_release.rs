//! Release gate: concurrent public HTTP retries, durable quotas and reopened storage.
//! Uses a disposable file and loopback server; never connects to a configured Oracle.
use ed25519_dalek::{Signer, SigningKey};
use oracle_proto::{pubkey_hex, Envelope, Kind};
use oracle_server::{
    http::{now_ms, router, App},
    publicseats::{challenge_message, Limits},
    store::{PublicConfig, Store},
};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};

struct Server {
    url: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn start(path: &Path) -> Self {
        let store = Store::open(path.to_str().unwrap()).unwrap();
        store.set_public_config(PublicConfig {
            write_rooms: vec!["eth".into(), "riemann".into()],
            limits: Limits {
                seat_spacing_ms: 1,
                seat_per_day: 1,
                register_per_ip_hour: 20,
                ..Limits::default()
            },
        });
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let url = format!("http://{}", socket.local_addr().unwrap());
        let mut app = App::new(store);
        app.self_url = url.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = tokio::net::TcpListener::from_std(socket).unwrap();
                    axum::serve(listener, router(app))
                        .with_graceful_shutdown(async {
                            let _ = rx.await;
                        })
                        .await
                        .unwrap();
                });
        });
        Self {
            url,
            stop: Some(tx),
            thread: Some(thread),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}
fn request(url: &str, token: Option<&str>, body: Option<Value>) -> (u16, Value, Option<String>) {
    let client = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    let req = if body.is_some() {
        client.post(url)
    } else {
        client.get(url)
    };
    let req = if let Some(t) = token {
        req.set("Authorization", &format!("Bearer {t}"))
    } else {
        req
    };
    let response = match body {
        Some(v) => req.send_json(v),
        None => req.call(),
    };
    let response = match response {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("HTTP transport failed: {e}"),
    };
    let retry = response.header("retry-after").map(str::to_owned);
    (response.status(), response.into_json().unwrap(), retry)
}

#[test]
fn concurrent_public_retries_and_quotas_survive_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("oracle.db");
    let owner = SigningKey::from_bytes(&[1; 32]);
    {
        let store = Store::open(path.to_str().unwrap()).unwrap();
        store
            .init_owner("owner", &pubkey_hex(&owner), now_ms())
            .unwrap();
    }
    let server = Server::start(&path);
    let mut seats = Vec::new();
    for i in 0..8u8 {
        let key = SigningKey::from_bytes(&[i + 10; 32]);
        let (status, challenge, _) = request(
            &format!("{}/public/register/challenge", server.url),
            None,
            None,
        );
        assert_eq!(status, 200);
        let nonce = challenge["nonce"].as_str().unwrap();
        let pk = pubkey_hex(&key);
        let signature = hex::encode(key.sign(&challenge_message(nonce, &pk)).to_bytes());
        let (status, registration, _) = request(
            &format!("{}/public/register", server.url),
            None,
            Some(
                json!({"nonce":nonce,"pubkey":pk,"signature":signature,"name":format!("Tester {i}")}),
            ),
        );
        assert_eq!(status, 201, "{registration}");
        let seat = registration["seat"].as_str().unwrap().to_owned();
        let token = registration["token"].as_str().unwrap().to_owned();
        let env = Envelope::sign(
            &key,
            "eth",
            &seat,
            Kind::Say,
            &format!("public release record {i}"),
            None,
            vec![],
            now_ms(),
        );
        seats.push((key, seat, token, env));
    }
    // Each seat issues four simultaneous copies, including racing first delivery.
    let barrier = Arc::new(Barrier::new(32));
    let started = Instant::now();
    let results = std::thread::scope(|scope| {
        let mut jobs = Vec::new();
        for (_, _, token, env) in &seats {
            for _ in 0..4 {
                let barrier = barrier.clone();
                let url = format!("{}/observations", server.url);
                jobs.push(scope.spawn(move || {
                    barrier.wait();
                    let start = Instant::now();
                    let (code, value, _) =
                        request(&url, Some(token), Some(serde_json::to_value(env).unwrap()));
                    assert_eq!(code, 200, "{value}");
                    assert_eq!(value["signed"], true);
                    let id = value["id"].as_str().unwrap();
                    let code = id.rsplit('-').next().unwrap();
                    let base = url.strip_suffix("/observations").unwrap();
                    let (status, fetched, _) =
                        request(&format!("{base}/search?id={code}"), Some(token), None);
                    assert_eq!(status, 200, "{fetched}");
                    assert_eq!(fetched["id"], value["id"]);

                    (value, start.elapsed().as_secs_f64() * 1000.0)
                }));
            }
        }
        jobs.into_iter()
            .map(|j| j.join().unwrap())
            .collect::<Vec<_>>()
    });
    let elapsed = started.elapsed();
    for group in results.chunks(4) {
        assert_eq!(
            group
                .iter()
                .filter(|(v, _)| v["duplicate"] == false)
                .count(),
            1
        );
        assert!(group.iter().all(|(v, _)| v["id"] == group[0].0["id"]));
    }
    drop(server); // Release the server and every Store connection before reopening.
    let server = Server::start(&path);
    // A different public seat must not obtain another author's successful receipt
    // by reusing a known operation ID, even with its own valid signature.
    let (key, seat, token, _) = &seats[1];
    let mut collision = Envelope::sign(
        key,
        "eth",
        seat,
        Kind::Say,
        "different author",
        None,
        vec![],
        now_ms(),
    );
    collision.client_op = seats[0].3.client_op.clone();
    let collision = collision.resign(key, now_ms());
    let (code, body, _) = request(
        &format!("{}/observations", server.url),
        Some(token),
        Some(serde_json::to_value(collision).unwrap()),
    );
    assert_eq!(
        code, 409,
        "operation ID collision must not return another seat's receipt: {body}"
    );
    assert!(body.get("id").is_none());
    let mut moved = seats[0].3.clone();
    moved.channel = "riemann".into();
    let moved = moved.resign(&seats[0].0, now_ms());
    let (code, body, _) = request(
        &format!("{}/observations", server.url),
        Some(&seats[0].2),
        Some(serde_json::to_value(moved).unwrap()),
    );
    assert_eq!(code, 409, "a retry cannot move rooms: {body}");
    assert!(body.get("id").is_none());
    for (i, (key, seat, token, env)) in seats.iter().enumerate() {
        let (code, receipt, _) = request(
            &format!("{}/observations", server.url),
            Some(token),
            Some(serde_json::to_value(env).unwrap()),
        );
        assert_eq!(code, 200, "{receipt}");
        assert_eq!(receipt["duplicate"], true);
        assert_eq!(receipt["id"], results[i * 4].0["id"]);
        let next = Envelope::sign(
            key,
            "eth",
            seat,
            Kind::Say,
            "past quota",
            None,
            vec![],
            now_ms(),
        );
        let (code, body, retry) = request(
            &format!("{}/observations", server.url),
            Some(token),
            Some(serde_json::to_value(next).unwrap()),
        );
        assert_eq!(code, 429, "{body}");
        assert!(retry.unwrap().parse::<u64>().unwrap() > 0);
        let forbidden = Envelope::sign(
            key,
            "studio",
            seat,
            Kind::Say,
            "must not land",
            None,
            vec![],
            now_ms(),
        );
        let (code, body, _) = request(
            &format!("{}/observations", server.url),
            Some(token),
            Some(serde_json::to_value(forbidden).unwrap()),
        );
        assert_eq!(code, 403, "{body}");
    }
    let mut latency: Vec<f64> = results.iter().map(|(_, ms)| *ms).collect();
    latency.sort_by(f64::total_cmp);
    println!(
        "{}",
        json!({"schema":"oracle-public-release-gate/1","seats":8,"concurrent_requests":32,
        "unique_writes":8,"duplicate_receipts":24,"elapsed_ms":elapsed.as_secs_f64()*1000.0,
        "p50_ms":latency[15],"max_ms":latency[31],"reopen_retries":8,"persisted_quota_refusals":8,
        "studio_write_refusals":8,"scope":"loopback HTTP, graceful reopen; not crash recovery, backup restore or capacity certification"})
    );
}
