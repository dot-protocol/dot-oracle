//! `ox serve` — the Oracle service for this machine.
//!
//! One long-running process per machine user. It is the only thing that talks to the network: it holds each
//! seat's key and outbox, retries queued writes on its own, keeps the Oracle clock, and answers local clients
//! (the CLI, `ox mcp`) over a Unix socket. A client never signs and never names an author: it presents a
//! capability, and the capability decides which seat it speaks as.
//!
//! Wire (`oracle-local/1`): one JSON object per line each way, at most [`MAX_FRAME`] bytes. Request
//! `{"cap", "op", ...}`, reply `{"ok": true, ...}` or `{"ok": false, "code", "error"}`. Ops (`advisory_get` is the footer's bounded read): `hello`, `now`,
//! `get`, `write` (needs a caller `op_id`), `flush`, `queued`.
//!
//! Trust, stated plainly: the socket is 0600 inside a 0700 directory, so only this OS user reaches it, and each
//! seat's capabilities are 0600 files in that seat's own home. That separates seats from each other and from
//! other users; it does not defend against a hostile process already running as the same user with read access
//! to a seat's home (Emmy).

use super::{now_ms, write_durable, Ox, Sent};
use anyhow::{anyhow, bail, Context, Result};
use oracle_proto::{Envelope, Kind};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(not(unix))]
use no_socket::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PROTOCOL: &str = "oracle-local/1";
/// A seat's capability file, in the seat's own home: read and write as that seat.
pub const CAP_FILE: &str = "service.cap";
/// The same seat, read only: for a client that only reads (a status line, a viewer).
pub const READ_CAP_FILE: &str = "service.read.cap";
/// In a seat's home: the run directory of the service that serves it.
pub const RUN_FILE: &str = "service.run";
/// One request or reply line, at most: a write's content and cargo fit with room to spare.
pub const MAX_FRAME: usize = 4 << 20;
/// Connections that have not yet shown a capability; more are turned away with `busy`.
pub const MAX_UNNAMED: usize = 16;
/// Connections bound to one seat; more for that seat are turned away, other seats unaffected.
pub const MAX_PER_SEAT: usize = 16;
/// A seat whose outbox holds this many writes takes no more until it drains.
pub const MAX_QUEUED: usize = 10_000;
/// How long a new connection may wait before its first line, an identified one between lines, and any reply.
const FIRST_LINE: Duration = Duration::from_secs(5);
const IDLE: Duration = Duration::from_secs(120);
const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// After an unanswered call, writes queue without touching the network for this long.
const DOWN_FOR_MS: i64 = 30_000;

/// What a capability lets a client do.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Grant {
    Read,
    Write,
}

struct Seat {
    home: PathBuf,
    handle: String,
    url: String,
    /// One flush at a time per seat, and one write per op_id at a time: both touch the outbox and the op records.
    outbox_lock: Mutex<()>,
    /// Connections currently bound to this seat.
    conns: AtomicUsize,
    /// Last time the Oracle answered at all (ms; 0 = never), last time it accepted this seat's token, last
    /// time a call went unanswered.
    reached_ms: AtomicI64,
    authed_ms: AtomicI64,
    refused_ms: AtomicI64,
    missed_ms: AtomicI64,
    /// The last clock sample: server_ms - local midpoint, its round trip, and when it was taken (0 = none yet).
    offset: AtomicI64,
    rtt_ms: AtomicI64,
    sampled_ms: AtomicI64,
}

impl Seat {
    fn ox(&self) -> Result<Ox> {
        // The service itself always speaks to the Oracle directly, never back through a service.
        Ox::direct(self.home.clone(), true)
    }
    /// Known unreachable: the last call went unanswered within [`DOWN_FOR_MS`] and nothing has answered since.
    fn down(&self) -> bool {
        let missed = self.missed_ms.load(Ordering::Relaxed);
        missed > self.reached_ms.load(Ordering::Relaxed) && now_ms() - missed < DOWN_FOR_MS
    }
    fn answered(&self) {
        self.reached_ms.store(now_ms(), Ordering::Relaxed);
    }
    fn missed(&self) {
        self.missed_ms.store(now_ms(), Ordering::Relaxed);
    }
    /// Ask the Oracle who this seat is: one clock sample, and whether it is reachable and accepts the token.
    fn sync(&self) {
        let Ok(ox) = self.ox() else { return };
        let sent = now_ms();
        match ox.call("GET", "/whoami", None) {
            Sent::Ok(v) => {
                let back = now_ms();
                self.answered();
                self.authed_ms.store(back, Ordering::Relaxed);
                if let Some(server) = v["server_ms"].as_i64() {
                    self.offset.store(server - (sent + (back - sent) / 2), Ordering::Relaxed);
                    self.rtt_ms.store(back - sent, Ordering::Relaxed);
                    self.sampled_ms.store(back, Ordering::Relaxed);
                }
            }
            Sent::Refused(..) => {
                self.answered();
                self.refused_ms.store(now_ms(), Ordering::Relaxed);
            }
            Sent::Unreachable(_) => self.missed(),
        }
    }
    fn queued(&self) -> usize {
        std::fs::read_dir(self.home.join("outbox"))
            .map(|d| d.filter_map(|e| e.ok()).filter(|e| e.path().extension().is_some_and(|x| x == "json")).count())
            .unwrap_or(0)
    }
    fn flush(&self) -> Result<(usize, usize)> {
        let _one = self.outbox_lock.lock().map_err(|_| anyhow!("outbox lock poisoned"))?;
        let r = self.ox()?.flush();
        if r.as_ref().is_ok_and(|(sent, _)| *sent > 0) {
            self.answered();
        }
        r
    }

    /// The clock and the Oracle's health as this seat last saw them. Nothing is claimed before it was observed.
    fn health(&self) -> Value {
        let local = now_ms();
        let at = |a: &AtomicI64| match a.load(Ordering::Relaxed) {
            0 => Value::Null,
            t => json!(local - t),
        };
        let sampled = self.sampled_ms.load(Ordering::Relaxed);
        let clock = if sampled == 0 {
            Value::Null
        } else {
            let offset = self.offset.load(Ordering::Relaxed);
            json!({"now_ms": local + offset, "offset_ms": offset, "uncertainty_ms": self.rtt_ms.load(Ordering::Relaxed) / 2 + 1,
                   "sample_age_ms": local - sampled})
        };
        let (reached, missed) = (self.reached_ms.load(Ordering::Relaxed), self.missed_ms.load(Ordering::Relaxed));
        let (authed, refused) = (self.authed_ms.load(Ordering::Relaxed), self.refused_ms.load(Ordering::Relaxed));
        json!({
            "local_ms": local,
            "clock": clock,
            // null = not yet known. Reachable means the Oracle answered; authenticated means it accepted this seat.
            "reachable": if reached == 0 && missed == 0 { Value::Null } else { json!(reached >= missed) },
            "authenticated": if authed == 0 && refused == 0 { Value::Null } else { json!(authed >= refused) },
            "last_answer_ago_ms": at(&self.reached_ms),
            "last_miss_ago_ms": at(&self.missed_ms),
        })
    }
}

pub struct Service {
    seats: Vec<Arc<Seat>>,
    caps: HashMap<String, (usize, Grant)>,
}

/// A started service: it holds the run directory's lock and the bound socket, and its capabilities are published.
pub struct Running {
    service: Arc<Service>,
    listener: UnixListener,
    _lock: File,
    /// One lock per seat home, held for the service's life: a seat home is served by one service at a time,
    /// whatever run directory each was started with.
    _seat_locks: Vec<File>,
    unnamed: Arc<AtomicUsize>,
}

/// Only this OS user may reach `path`: `mode` on Unix. Elsewhere the file keeps the ACL of the per-user directory
/// it is in; no service runs there anyway (see [`no_socket`]).
fn private(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Windows has no Unix socket in std, so there is no local service there: binding fails, connecting fails as "not
/// running", and ox speaks to the Oracle directly, which is how it works with no service anywhere (`Ox::ask_service`).
/// The types are uninhabited, so no other method can ever run.
#[cfg(not(unix))]
mod no_socket {
    use std::convert::Infallible;
    use std::io::{self, Read, Write};
    use std::path::Path;
    use std::time::Duration;

    fn unsupported() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "the Oracle service needs a Unix socket, which this system does not have; ox speaks to the Oracle directly")
    }

    pub struct UnixStream(Infallible);
    pub struct UnixListener(Infallible);
    pub struct Addr(Infallible);

    impl Addr {
        pub fn as_pathname(&self) -> Option<&Path> {
            match self.0 {}
        }
    }
    impl UnixStream {
        pub fn connect<P: AsRef<Path>>(_: P) -> io::Result<UnixStream> {
            Err(unsupported())
        }
        pub fn set_read_timeout(&self, _: Option<Duration>) -> io::Result<()> {
            match self.0 {}
        }
        pub fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
            match self.0 {}
        }
        pub fn try_clone(&self) -> io::Result<UnixStream> {
            match self.0 {}
        }
    }
    impl Read for UnixStream {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            match self.0 {}
        }
    }
    impl Write for UnixStream {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            match self.0 {}
        }
        fn flush(&mut self) -> io::Result<()> {
            match self.0 {}
        }
    }
    impl Write for &UnixStream {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            match self.0 {}
        }
        fn flush(&mut self) -> io::Result<()> {
            match self.0 {}
        }
    }
    impl UnixListener {
        pub fn bind<P: AsRef<Path>>(_: P) -> io::Result<UnixListener> {
            Err(unsupported())
        }
        pub fn incoming(&self) -> std::iter::Empty<io::Result<UnixStream>> {
            match self.0 {}
        }
        pub fn local_addr(&self) -> io::Result<Addr> {
            match self.0 {}
        }
    }
}

/// The socket path for a run directory.
pub fn socket(run: &Path) -> PathBuf {
    run.join("oracle.sock")
}

/// A fault this process was told to inject (`OX_FAULT`). Debug builds only: a release build never reads it.
pub(crate) fn fault(name: &str) -> bool {
    cfg!(debug_assertions) && std::env::var("OX_FAULT").as_deref() == Ok(name)
}

pub(super) fn op_id_ok(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Service {
    /// Read every seat home. Nothing is published yet: capabilities are issued by [`Service::start`], after it holds the lock.
    pub fn load(homes: &[PathBuf]) -> Result<Service> {
        let mut seats: Vec<Arc<Seat>> = Vec::new();
        for home in homes {
            let ox = Ox::direct(home.clone(), true).with_context(|| format!("seat home {}", home.display()))?;
            let (handle, url) = (ox.cfg.handle.clone(), ox.cfg.url.trim_end_matches('/').to_string());
            // The same handle on two Oracles is two seats; twice on one Oracle is a mistake.
            if seats.iter().any(|s| s.handle == handle && s.url == url) {
                bail!("two homes are both seat {handle} on {url}");
            }
            seats.push(Arc::new(Seat {
                home: home.clone(),
                handle,
                url,
                outbox_lock: Mutex::new(()),
                conns: AtomicUsize::new(0),
                reached_ms: AtomicI64::new(0),
                authed_ms: AtomicI64::new(0),
                refused_ms: AtomicI64::new(0),
                missed_ms: AtomicI64::new(0),
                offset: AtomicI64::new(0),
                rtt_ms: AtomicI64::new(0),
                sampled_ms: AtomicI64::new(0),
            }));
        }
        if seats.is_empty() {
            bail!("no seat to serve");
        }
        Ok(Service { seats, caps: HashMap::new() })
    }

    /// Take the run directory for this process, bind the socket, and only then issue each seat fresh capabilities
    /// (the old ones stop working). A second service on the same run directory fails at the lock, before it has
    /// touched the socket or any capability file, so the first one's clients keep working.
    pub fn start(mut self, run: &Path) -> Result<Running> {
        std::fs::create_dir_all(run)?;
        private(run, 0o700)?;
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(run.join("oracle.lock"))?;
        if lock.try_lock().is_err() {
            bail!("another Oracle service is running on {}", run.display());
        }
        // Every seat home is this service's alone, in a fixed order (sorted canonical paths) so two services
        // starting at once cannot each hold half. Taken before any socket or capability is touched.
        let mut homes: Vec<PathBuf> = self.seats.iter().map(|s| std::fs::canonicalize(&s.home)).collect::<std::io::Result<_>>()?;
        homes.sort();
        let mut seat_locks = Vec::new();
        for h in &homes {
            let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(h.join("service.lock"))?;
            if f.try_lock().is_err() {
                bail!("seat home {} is already served by another Oracle service", h.display());
            }
            seat_locks.push(f);
        }
        // Holding the lock, any socket file left behind is a dead service's.
        let path = socket(run);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
        private(&path, 0o600)?;
        let run_abs = std::fs::canonicalize(run)?;
        for (i, seat) in self.seats.iter().enumerate() {
            for (file, grant) in [(CAP_FILE, Grant::Write), (READ_CAP_FILE, Grant::Read)] {
                let cap = oracle_proto::random_hex(32);
                write_durable(&seat.home.join(file), &cap)?;
                self.caps.insert(cap, (i, grant));
            }
            // Where this seat's clients find the service (one service may serve seats whose homes are elsewhere).
            write_durable(&seat.home.join(RUN_FILE), &run_abs.display().to_string())?;
        }
        Ok(Running { service: Arc::new(self), listener, _lock: lock, _seat_locks: seat_locks, unnamed: Arc::new(AtomicUsize::new(0)) })
    }

    fn seat_for(&self, cap: &str) -> Option<(usize, Grant)> {
        self.caps.get(cap).copied()
    }

    /// One request, answered. Never panics on input; every failure is `{"ok": false}` with a stable code.
    pub fn answer(&self, req: &Value) -> Value {
        let Some((i, grant)) = req["cap"].as_str().and_then(|c| self.seat_for(c)) else {
            return json!({"ok": false, "code": "unauthorised", "error": "no valid capability: read it from the seat's service.cap"});
        };
        let seat = &self.seats[i];
        let r: Result<Value> = (|| match req["op"].as_str().unwrap_or("") {
            "hello" => Ok(json!({"protocol": if fault("protocol-mismatch") { "oracle-local/0" } else { PROTOCOL }, "seat": seat.handle, "oracle": seat.url,
                                 "grant": if grant == Grant::Write { "write" } else { "read" }, "version": env!("CARGO_PKG_VERSION")})),
            "now" => Ok(seat.health()),
            "queued" => Ok(json!({"queued": seat.queued()})),
            "get" => {
                let path = req["path"].as_str().ok_or_else(|| anyhow!("get needs path"))?;
                if !path.starts_with('/') {
                    bail!("path must start with /");
                }
                let v = seat.ox()?.get(path)?;
                seat.answered();
                Ok(json!({"value": v}))
            }
            // The footer's read, executed here under the caller's remaining budget: one attempt on the live
            // address, no retry, no fallback walk (Emmy: a plain `get` on the
            // service side kept retrying and fanning out after the caller had given up). An older service that
            // lacks this op answers "unknown op", which the caller reads as unavailable.
            "advisory_get" => {
                let path = req["path"].as_str().ok_or_else(|| anyhow!("advisory_get needs path"))?;
                if !path.starts_with('/') {
                    bail!("path must start with /");
                }
                let budget = Duration::from_millis(req["budget_ms"].as_u64().ok_or_else(|| anyhow!("advisory_get needs budget_ms"))?);
                let v = seat.ox()?.advisory_get(path, budget)?;
                seat.answered();
                Ok(json!({"value": v}))
            }
            "write" => {
                if grant != Grant::Write {
                    bail!("this capability is read-only");
                }
                self.write(seat, req)
            }
            "post" => {
                if grant != Grant::Write {
                    bail!("this capability is read-only");
                }
                // Only read marks: a signed write goes through `write` (journaled) or `submit` (at once), never here.
                let path = req["path"].as_str().unwrap_or("");
                if path != "/fold" {
                    bail!("post is only for /fold (read marks), not {path:?}");
                }
                let v = seat.ox()?.post(path, &req["body"])?;
                seat.answered();
                Ok(json!({"value": v}))
            }
            "submit" => {
                if grant != Grant::Write {
                    bail!("this capability is read-only");
                }
                // A record the caller signed itself that must land now or fail, never queue (a ledger tip).
                let env: Envelope = serde_json::from_value(req["envelope"].clone()).map_err(|e| anyhow!("submit needs a signed envelope: {e}"))?;
                if env.author != seat.handle {
                    bail!("a capability for {} cannot submit a write by {}", seat.handle, env.author);
                }
                match seat.ox()?.call("POST", "/observations", Some(&serde_json::to_value(&env)?)) {
                    Sent::Ok(v) => {
                        seat.answered();
                        Ok(json!({"stored": v}))
                    }
                    Sent::Refused(c, d) => {
                        seat.answered();
                        bail!("NOT written — server refused ({c}): {d}")
                    }
                    Sent::Unreachable(r) => {
                        seat.missed();
                        bail!("Oracle unreachable ({r})")
                    }
                }
            }
            "flush" => {
                // Delivering queued writes is writing: a read capability cannot cause it.
                if grant != Grant::Write {
                    bail!("this capability is read-only");
                }
                let (sent, left) = seat.flush()?;
                Ok(json!({"sent": sent, "left": left}))
            }
            other => Err(anyhow!("unknown op {other:?}")),
        })();
        match r {
            Ok(mut v) => {
                v["ok"] = json!(true);
                v
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let code = if msg.contains("read-only") {
                    "forbidden"
                } else if msg.starts_with("conflict") {
                    "conflict"
                } else if msg.starts_with("outbox full") {
                    "busy"
                } else if msg.contains("refused (401)") || msg.contains("refused (403)") {
                    "refused"
                } else if msg.contains("NOT written") {
                    "rejected"
                } else if msg.contains("unreachable") {
                    "unreachable"
                } else {
                    "bad_request"
                };
                json!({"ok": false, "code": code, "error": msg})
            }
        }
    }

    /// A write, exactly once per caller `op_id`. The op record (the payload's digest and the signed envelope) is on
    /// disk before anything is sent; a retry of the same op_id returns the first outcome or re-delivers the same
    /// envelope (same client_op, so the Oracle stores it once); the same op_id with a different payload is refused.
    fn write(&self, seat: &Seat, req: &Value) -> Result<Value> {
        // The author is the seat behind the capability, never the caller's word.
        if req.get("author").is_some() || req.get("handle").is_some() {
            bail!("a client never names the author; the capability does");
        }
        let op_id = req["op_id"].as_str().filter(|o| op_id_ok(o)).ok_or_else(|| anyhow!("write needs an op_id: 8-64 of [A-Za-z0-9_-], the same on every retry"))?;
        let kind = match req["kind"].as_str().unwrap_or("say") {
            "say" => Kind::Say,
            "memory" => Kind::Memory,
            "state" => Kind::State,
            k => bail!("kind must be say, memory or state, not {k}"),
        };
        let room = req["room"].as_str().ok_or_else(|| anyhow!("write needs room"))?;
        let text = req["text"].as_str().ok_or_else(|| anyhow!("write needs text"))?;
        let tags: Vec<String> = req["tags"].as_array().map(|t| t.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        let cargo = req["cargo"].as_str().map(String::from);
        if seat.queued() >= MAX_QUEUED {
            bail!("outbox full: {MAX_QUEUED} writes wait for the Oracle");
        }
        let _one = seat.outbox_lock.lock().map_err(|_| anyhow!("outbox lock poisoned"))?;
        // The journal is shared with `ox` itself (Ox::journaled_write, under a file lock), so a retry of this op
        // through the CLI's direct path, with no service, resolves to the same envelope.
        match seat.ox()?.journaled_write(op_id, kind, room, text, cargo, tags, seat.down()) {
            Ok(out) => {
                if out.get("queued").is_some() {
                    seat.missed();
                } else {
                    seat.answered();
                }
                Ok(out)
            }
            Err(e) => {
                if format!("{e:#}").contains("NOT written") {
                    seat.answered();
                }
                Err(e)
            }
        }
    }
}

impl Running {
    #[cfg(test)]
    pub fn service(&self) -> &Service {
        &self.service
    }

    /// Serve until killed: one background thread per seat (clock, outbox), one thread per connection within the limits.
    pub fn serve(self) -> Result<()> {
        for seat in &self.service.seats {
            let s = seat.clone();
            std::thread::spawn(move || loop {
                s.sync();
                if s.queued() > 0 && !s.down() {
                    let _ = s.flush();
                }
                std::thread::sleep(Duration::from_secs(if s.queued() > 0 { 5 } else { 30 }));
            });
            eprintln!("serving seat {} ({}) from {}", seat.handle, seat.url, seat.home.display());
        }
        eprintln!("{PROTOCOL} on {}", self.listener.local_addr().ok().and_then(|a| a.as_pathname().map(|p| p.display().to_string())).unwrap_or_default());
        for conn in self.listener.incoming() {
            let Ok(conn) = conn else { continue };
            if self.unnamed.fetch_add(1, Ordering::SeqCst) >= MAX_UNNAMED {
                self.unnamed.fetch_sub(1, Ordering::SeqCst);
                let _ = conn.set_write_timeout(Some(Duration::from_secs(1)));
                let _ = writeln!(&conn, "{}", json!({"ok": false, "code": "busy", "error": "too many connections waiting to identify"}));
                continue;
            }
            let (svc, unnamed) = (self.service.clone(), self.unnamed.clone());
            std::thread::spawn(move || {
                let _ = svc.session(conn, &unnamed);
            });
        }
        Ok(())
    }
}

enum Frame {
    Line(Value),
    Bad(String),
    TooLarge,
    Closed,
}

fn read_frame(r: &mut BufReader<UnixStream>, buf: &mut Vec<u8>) -> Frame {
    buf.clear();
    match r.by_ref().take(MAX_FRAME as u64 + 1).read_until(b'\n', buf) {
        Ok(0) | Err(_) => Frame::Closed,
        Ok(_) if buf.len() > MAX_FRAME => Frame::TooLarge,
        Ok(_) if !buf.ends_with(b"\n") => Frame::Closed,
        Ok(_) => match serde_json::from_slice(buf) {
            Ok(v) => Frame::Line(v),
            Err(e) => Frame::Bad(format!("not JSON: {e}")),
        },
    }
}

impl Service {
    /// One connection. It starts unnamed (counted against [`MAX_UNNAMED`], [`FIRST_LINE`] to speak); its first
    /// valid capability binds it to that seat (counted against [`MAX_PER_SEAT`], [`IDLE`] between lines), and it
    /// may not switch seats. Oversized or idle connections are closed.
    fn session(&self, conn: UnixStream, unnamed: &AtomicUsize) -> Result<()> {
        conn.set_read_timeout(Some(FIRST_LINE))?;
        conn.set_write_timeout(Some(WRITE_DEADLINE))?;
        let mut out = conn.try_clone()?;
        let mut reader = BufReader::new(conn);
        let mut bound: Option<usize> = None;
        let mut buf = Vec::new();
        let result = loop {
            let mut is_write = false;
            let reply = match read_frame(&mut reader, &mut buf) {
                Frame::Closed => break Ok(()),
                Frame::TooLarge => {
                    let _ = writeln!(out, "{}", json!({"ok": false, "code": "too_large", "error": format!("a line is at most {MAX_FRAME} bytes")}));
                    break Ok(());
                }
                Frame::Bad(e) => json!({"ok": false, "code": "bad_request", "error": e}),
                Frame::Line(req) => {
                    is_write = req["op"] == "write";
                    let seat = req["cap"].as_str().and_then(|c| self.seat_for(c)).map(|(i, _)| i);
                    match (bound, seat) {
                        (None, Some(i)) => {
                            if self.seats[i].conns.fetch_add(1, Ordering::SeqCst) >= MAX_PER_SEAT {
                                self.seats[i].conns.fetch_sub(1, Ordering::SeqCst);
                                let _ = writeln!(out, "{}", json!({"ok": false, "code": "busy", "error": format!("seat {} has {MAX_PER_SEAT} connections open", self.seats[i].handle)}));
                                break Ok(());
                            }
                            bound = Some(i);
                            unnamed.fetch_sub(1, Ordering::SeqCst);
                            reader.get_ref().set_read_timeout(Some(IDLE))?;
                            self.answer(&req)
                        }
                        (Some(b), Some(i)) if b != i => json!({"ok": false, "code": "forbidden", "error": "one connection speaks for one seat"}),
                        _ => self.answer(&req),
                    }
                }
            };
            // Fault injection, debug builds only: what a client sees when a write was done and its reply was lost,
            // garbled or late (Emmy, PR 54 review).
            if is_write && reply["ok"] == true {
                if fault("drop-reply-after-write") {
                    break Ok(());
                }
                if fault("garbage-reply-after-write") {
                    let _ = writeln!(out, "%%% not a reply");
                    continue;
                }
                if fault("stall-reply-after-write") {
                    std::thread::sleep(Duration::from_secs(30));
                }
            }
            if writeln!(out, "{reply}").is_err() {
                break Ok(());
            }
        };
        match bound {
            Some(i) => self.seats[i].conns.fetch_sub(1, Ordering::SeqCst),
            None => unnamed.fetch_sub(1, Ordering::SeqCst),
        };
        result
    }
}

/// The run directory of the service serving this seat home, if one is running and answers to this seat's
/// capability. `OX_DIRECT=1` skips it (speak to the Oracle directly).
pub fn serving(home: &Path) -> Serving {
    if std::env::var_os("OX_DIRECT").is_some() {
        return Serving::No;
    }
    let Ok(run) = std::fs::read_to_string(home.join(RUN_FILE)) else { return Serving::No };
    let run = PathBuf::from(run.trim());
    match request(&run, home, json!({"op": "hello"})) {
        Ok(h) if h["ok"] == true && h["protocol"] == PROTOCOL => Serving::Yes(run),
        // A service that answers in another protocol is not bypassed: its policy may be the one in force.
        Ok(h) if h["ok"] == true => Serving::Incompatible(format!(
            "the Oracle service for this seat speaks {} and this ox speaks {PROTOCOL}: update ox (or restart the service with the matching ox); nothing was done",
            h["protocol"].as_str().unwrap_or("?"))),
        // Configured and not running: speaking directly is safe, through the same op journal.
        Err(Unanswered::NotSent(_)) => Serving::Unavailable,
        // Running and refusing (busy, a capability it does not know) or answering nonsense: not bypassed.
        Ok(h) => Serving::Incompatible(format!("the Oracle service for this seat refused: {}; nothing was done",
            h["error"].as_str().unwrap_or("no reason given"))),
        Err(e) => Serving::Incompatible(format!("the Oracle service for this seat did not answer usefully ({e}); nothing was done")),
    }
}

/// Whether a service serves this seat, as `ox` starts.
pub enum Serving {
    /// None configured for this seat (or OX_DIRECT): speak to the Oracle directly.
    No,
    /// Configured but not running: speak directly, through the same op journal.
    Unavailable,
    Yes(PathBuf),
    /// Running in another protocol, refusing, or answering nonsense: refused, never bypassed.
    Incompatible(String),
}

/// Why a request came back without an answer.
#[derive(Debug)]
pub enum Unanswered {
    /// Nothing reached a service (none running for this seat, no capability, could not connect): speaking to the
    /// Oracle directly instead is safe, because this request was never submitted.
    NotSent(String),
    /// The request was sent and no usable reply came back (closed, garbled, timed out): the service may have
    /// acted on it. A write must be retried as the same op, never re-signed as a new one.
    Uncertain(String),
    /// A service serves this seat in another protocol: refused, never bypassed.
    Incompatible(String),
}

impl std::error::Error for Unanswered {}

impl std::fmt::Display for Unanswered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unanswered::NotSent(m) | Unanswered::Uncertain(m) | Unanswered::Incompatible(m) => f.write_str(m),
        }
    }
}

/// A service reply as a result: `field` of a success (the whole reply when empty), or its error.
pub fn value(v: Value, field: &str) -> Result<Value> {
    if v["ok"] != true {
        bail!("{}", v["error"].as_str().unwrap_or("the Oracle service refused"));
    }
    Ok(if field.is_empty() { v } else { v[field].clone() })
}

/// A local client: one request to the service, one reply. The capability is read from the seat's home.
/// Up to `OX_SERVICE_TIMEOUT_MS` (default 60 s) for the reply.
pub fn request(run: &Path, seat_home: &Path, req: Value) -> std::result::Result<Value, Unanswered> {
    let wait = std::env::var("OX_SERVICE_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(60_000);
    request_within(run, seat_home, req, Duration::from_millis(wait))
}

/// Recompute the remaining exchange budget on every syscall. Socket timeouts are per-call;
/// a fragmented reply must not reset the caller's total deadline.
struct ExchangeDeadline {
    stream: UnixStream,
    start: std::time::Instant,
    wait: Duration,
}
impl ExchangeDeadline {
    fn remaining(&self) -> std::io::Result<Duration> {
        self.wait.checked_sub(self.start.elapsed()).filter(|d| !d.is_zero())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "service exchange deadline exceeded"))
    }
}
impl Read for ExchangeDeadline {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        let n = self.stream.read(buf)?;
        self.remaining()?;
        Ok(n)
    }
}
impl Write for ExchangeDeadline {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        let n = self.stream.write(buf)?;
        self.remaining()?;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.remaining()?;
        self.stream.flush()
    }
}

/// The same request with an explicit wait for the reply (the footer's budget; `request` takes the seat's default).
pub fn request_within(run: &Path, seat_home: &Path, mut req: Value, wait: Duration) -> std::result::Result<Value, Unanswered> {
    let start = std::time::Instant::now();
    let cap = std::fs::read_to_string(seat_home.join(CAP_FILE)).map_err(|e| Unanswered::NotSent(format!("no service capability for this seat ({e})")))?;
    req["cap"] = json!(cap.trim());
    let stream = UnixStream::connect(socket(run)).map_err(|e| Unanswered::NotSent(format!("the Oracle service is not running ({e})")))?;
    let mut s = ExchangeDeadline { stream, start, wait };
    let uncertain = |what: String| Unanswered::Uncertain(format!("the Oracle service may have acted on this request: {what}"));
    writeln!(s, "{req}").map_err(|e| uncertain(format!("sending failed ({e})")))?;
    let mut line = String::new();
    BufReader::new(s).take(MAX_FRAME as u64 + 1).read_line(&mut line).map_err(|e| uncertain(format!("no reply ({e})")))?;
    if line.trim().is_empty() {
        return Err(uncertain("it closed without a reply".into()));
    }
    serde_json::from_str(&line).map_err(|e| uncertain(format!("the reply is not JSON ({e})")))
}

#[cfg(test)]
mod tests {
    //! The service's own answers, against a real Oracle with seats rocky and mallory. The process-level gates
    //! (a second start, kill -9 and retry, oversized lines, idle saturation) are in tests/serve.rs, against `ox serve`.
    use super::*;
    use crate::signed_read_tests::oracle;
    use std::time::Instant;

    const DEAD: &str = "http://127.0.0.1:9";

    fn run_dir() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!("oxs-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)))
    }
    fn start(homes: &[PathBuf]) -> Running {
        Service::load(homes).unwrap().start(&run_dir()).unwrap()
    }
    fn cap(home: &Path) -> String {
        std::fs::read_to_string(home.join(CAP_FILE)).unwrap()
    }
    fn read_cap(home: &Path) -> String {
        std::fs::read_to_string(home.join(READ_CAP_FILE)).unwrap()
    }
    fn write(svc: &Service, home: &Path, op: &str, text: &str) -> Value {
        svc.answer(&json!({"cap": cap(home), "op": "write", "op_id": op, "room": "svc", "text": text}))
    }
    fn count(svc: &Service, home: &Path, word: &str) -> usize {
        let v = svc.answer(&json!({"cap": cap(home), "op": "get", "path": format!("/search?q={word}&channel=svc&author=rocky")}));
        v["value"]["items"].as_array().map_or(0, |a| a.len())
    }
    fn url_of(home: &Path) -> String {
        serde_json::from_str::<Value>(&std::fs::read_to_string(home.join("config.json")).unwrap()).unwrap()["url"].as_str().unwrap().into()
    }
    fn set_url(home: &Path, url: &str) {
        let p = home.join("config.json");
        let mut c: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        c["url"] = json!(url);
        std::fs::write(p, c.to_string()).unwrap();
    }
    fn set_fallback(home: &Path, url: &str) {
        let p = home.join("config.json");
        let mut c: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        c["fallback_urls"] = json!([url]);
        std::fs::write(p, c.to_string()).unwrap();
    }

    /// The footer's read through the service (Emmy): executed on the service
    /// side as one attempt within the caller's budget, no retry, no fallback walk, nothing arriving later; the
    /// asks half gets the rest of the budget the same way; an unknown op is unavailable, never unrestricted.
    /// NEGATIVE CONTROL: the ordinary `get` still walks to the fallback.
    #[test]
    fn an_advisory_get_through_the_service_is_one_attempt_within_its_budget_and_never_walks_the_fallback() {
        use crate::signed_read_tests::Oracle;
        let o = oracle();
        let (stalled, seen) = Oracle::fake(&[], Duration::from_secs(4));
        let (fallback, seen_fallback) = Oracle::fake(&["/"], Duration::from_secs(4));
        let rocky = o.seat_home("rocky", 2, Some(&stalled));
        set_fallback(&rocky, &fallback);
        let dir = run_dir();
        let run = Service::load(std::slice::from_ref(&rocky)).unwrap().start(&dir).unwrap();
        // the acceptor: `start` only binds the socket, `serve` answers on it (the real service's loop)
        let svc = run.service.clone();
        let acceptor = std::thread::spawn(move || {
            let _ = run.serve();
        });
        // in-process: the service's own answer
        let t = Instant::now();
        let v = svc.answer(&json!({"cap": cap(&rocky), "op": "advisory_get", "path": "/fold?limit=50", "budget_ms": 700}));
        assert!(t.elapsed() < Duration::from_secs(2), "within the budget: {:?}", t.elapsed());
        assert_eq!(v["ok"], false, "{v}");
        assert!(v["error"].as_str().unwrap_or("").contains("footer budget"), "{v}");
        // the asks half, the remaining budget
        let t = Instant::now();
        let v = svc.answer(&json!({"cap": cap(&rocky), "op": "advisory_get", "path": "/asks?state=open", "budget_ms": 300}));
        assert!(t.elapsed() < Duration::from_secs(1) && v["ok"] == false, "{v} {:?}", t.elapsed());
        // over the socket, the way the footer asks: the caller is back within its budget
        let t = Instant::now();
        let r = request_within(&dir, &rocky, json!({"op": "advisory_get", "path": "/fold?limit=50", "budget_ms": 500}), Duration::from_millis(900));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(matches!(&r, Ok(v) if v["ok"] == false) || r.is_err(), "unavailable, one way or the other: {r:?}");
        // after the primary's hold has passed: nothing more arrived anywhere
        std::thread::sleep(Duration::from_secs(5));
        // (the service's own liveness probe at start, GET /whoami, is not the footer's)
        let primary: Vec<String> = seen.lock().unwrap().iter().filter(|r| !r.starts_with("GET /whoami")).cloned().collect();
        assert_eq!(primary, vec!["GET /fold?limit=50", "GET /asks?state=open", "GET /fold?limit=50"], "one request per attempt, no delayed retry: {primary:?}");
        let fallback_seen: Vec<String> = seen_fallback.lock().unwrap().iter().filter(|r| !r.starts_with("GET /whoami")).cloned().collect();
        assert!(fallback_seen.is_empty(), "the fallback never received the advisory read: {fallback_seen:?}");
        // an op this service does not know is refused, so an older service never performs an unrestricted get
        let v = svc.answer(&json!({"cap": cap(&rocky), "op": "advisory_get_v0", "path": "/fold?limit=50", "budget_ms": 500}));
        assert_eq!(v["ok"], false, "{v}");
        assert!(value(v, "value").is_err(), "the caller reads it as unavailable");
        // NEGATIVE CONTROL: the ordinary get keeps its fallback walk
        let v = svc.answer(&json!({"cap": cap(&rocky), "op": "get", "path": "/health"}));
        assert_eq!(v["ok"], true, "{v}");
        let fallback_seen: Vec<String> = seen_fallback.lock().unwrap().iter().filter(|r| !r.starts_with("GET /whoami")).cloned().collect();
        assert_eq!(fallback_seen, vec!["GET /health".to_string()], "the ordinary get walked to the fallback once");
        drop(acceptor);
    }

    #[test]
    fn each_capability_speaks_only_as_its_own_seat() {
        let o = oracle();
        let (rocky, mallory) = (o.seat_home("rocky", 2, None), o.seat_home("mallory", 3, None));
        let run = start(&[rocky.clone(), mallory.clone()]);
        let svc = run.service();
        let r = write(svc, &rocky, "op-rocky-1", "from rocky");
        let m = svc.answer(&json!({"cap": cap(&mallory), "op": "write", "op_id": "op-mallory-1", "room": "svc", "text": "from mallory"}));
        let author = |id: &Value| svc.answer(&json!({"cap": cap(&rocky), "op": "get", "path": format!("/search?id={}", id.as_str().unwrap())}))["value"]["author"].clone();
        assert_eq!(author(&r["stored"]["id"]), "rocky", "{r}");
        assert_eq!(author(&m["stored"]["id"]), "mallory", "{m}");
        let named = svc.answer(&json!({"cap": cap(&mallory), "op": "write", "op_id": "op-mallory-2", "room": "svc", "text": "x", "author": "rocky"}));
        assert_eq!(named["code"], "bad_request", "{named}");
        for c in [json!(null), json!("0".repeat(64))] {
            assert_eq!(svc.answer(&json!({"cap": c, "op": "hello"}))["code"], "unauthorised");
        }
        // The read capability reads as rocky and can neither write nor deliver.
        assert_eq!(svc.answer(&json!({"cap": read_cap(&rocky), "op": "hello"}))["grant"], "read");
        let w = svc.answer(&json!({"cap": read_cap(&rocky), "op": "write", "op_id": "op-read-1", "room": "svc", "text": "x"}));
        assert_eq!(w["code"], "forbidden", "{w}");
        // A restart issues fresh capabilities: the old one stops working.
        let old = cap(&rocky);
        drop(run);
        let again = start(std::slice::from_ref(&rocky));
        assert_eq!(again.service().answer(&json!({"cap": old, "op": "hello"}))["code"], "unauthorised");
        assert_eq!(again.service().answer(&json!({"cap": cap(&rocky), "op": "hello"}))["seat"], "rocky");
    }

    #[test]
    fn a_read_capability_cannot_deliver_queued_writes() {
        let o = oracle();
        let home = o.seat_home("rocky", 2, Some(DEAD));
        let run = start(std::slice::from_ref(&home));
        let svc = run.service();
        assert!(write(svc, &home, "op-queued-1", "svcheld").get("queued").is_some());
        set_url(&home, &url_of(&o.seat_home("rocky", 2, None)));
        let f = svc.answer(&json!({"cap": read_cap(&home), "op": "flush"}));
        assert_eq!(f["code"], "forbidden", "{f}");
        assert_eq!(svc.answer(&json!({"cap": cap(&home), "op": "queued"}))["queued"], 1);
        assert_eq!(count(svc, &home, "svcheld"), 0, "nothing was delivered");
    }

    #[test]
    fn one_op_id_is_one_event_and_cannot_be_reused_for_another_write() {
        let o = oracle();
        let home = o.seat_home("rocky", 2, None);
        let run = start(std::slice::from_ref(&home));
        let svc = run.service();
        let first = write(svc, &home, "op-once-0001", "svconce");
        let id = first["stored"]["id"].clone();
        // The caller lost the reply and asks again: the same outcome, marked as a replay, and still one event.
        let again = write(svc, &home, "op-once-0001", "svconce");
        assert_eq!((again["stored"]["id"].clone(), again["replayed"].clone()), (id.clone(), json!(true)), "{again}");
        assert_eq!(count(svc, &home, "svconce"), 1);
        // The same op_id for a different write is refused, never silently mapped to the first.
        let other = write(svc, &home, "op-once-0001", "svcsomethingelse");
        assert_eq!(other["code"], "conflict", "{other}");
        // A record that was cut off before its outcome (the process died after sending) re-delivers the same
        // envelope: the Oracle answers the repeated client_op with the original, so it is still one event.
        let rec_path = home.join("ops/op-once-0001.json");
        let mut rec: Value = serde_json::from_str(&std::fs::read_to_string(&rec_path).unwrap()).unwrap();
        rec["outcome"] = Value::Null;
        std::fs::write(&rec_path, rec.to_string()).unwrap();
        let replay = write(svc, &home, "op-once-0001", "svconce");
        assert_eq!(replay["stored"]["id"], id, "{replay}");
        assert_eq!(replay["stored"]["duplicate"], true, "{replay}");
        assert_eq!(count(svc, &home, "svconce"), 1);
        // A torn or corrupt record fails closed: no guess that it was never written, no second write.
        std::fs::write(home.join("ops/op-torn-0001.json"), "{\"op_id\": \"op-torn-0001\", \"digest\": \"ab").unwrap();
        let torn = write(svc, &home, "op-torn-0001", "svctorn");
        assert_eq!(torn["ok"], false, "{torn}");
        assert!(torn["error"].as_str().unwrap().contains("unreadable"), "{torn}");
        assert_eq!(count(svc, &home, "svctorn"), 0);
        // An op_id must look like one.
        assert_eq!(write(svc, &home, "short", "x")["code"], "bad_request");
    }

    #[test]
    fn offline_writes_queue_at_once_survive_a_restart_and_land_once() {
        let o = oracle();
        let live = url_of(&o.seat_home("rocky", 2, None));
        let home = o.seat_home("rocky", 2, Some(DEAD));
        let run = start(std::slice::from_ref(&home));
        for (op, word) in [("op-off-0001", "svcalpha"), ("op-off-0002", "svcbeta")] {
            let t = Instant::now();
            let v = write(run.service(), &home, op, word);
            assert!(v["queued"].is_string(), "{v}");
            assert!(t.elapsed() < Duration::from_secs(6), "a write must not stall: {:?}", t.elapsed());
        }
        let t = Instant::now();
        write(run.service(), &home, "op-off-0003", "svcgamma");
        assert!(t.elapsed() < Duration::from_millis(500), "known down queues without the network: {:?}", t.elapsed());
        drop(run);
        let run = start(std::slice::from_ref(&home));
        let svc = run.service();
        assert_eq!(svc.answer(&json!({"cap": cap(&home), "op": "queued"}))["queued"], 3, "a restart keeps the outbox");
        set_url(&home, &live);
        // The caller retries a queued op after the restart: delivered once, and the flush that follows sends only the rest.
        let retried = write(svc, &home, "op-off-0001", "svcalpha");
        assert!(retried["stored"]["id"].is_string(), "{retried}");
        let f = svc.answer(&json!({"cap": cap(&home), "op": "flush"}));
        assert_eq!((f["sent"].clone(), f["left"].clone()), (json!(2), json!(0)), "{f}");
        assert_eq!(count(svc, &home, "svcalpha"), 1);
        assert_eq!(count(svc, &home, "svcbeta") + count(svc, &home, "svcgamma"), 2);
    }

    #[test]
    fn a_late_delivery_keeps_when_it_was_written() {
        let o = oracle();
        let home = o.seat_home("rocky", 2, None);
        let live = url_of(&home);
        let run = start(std::slice::from_ref(&home));
        let svc = run.service();
        let now = write(svc, &home, "op-time-0001", "svcnow");
        let got = svc.answer(&json!({"cap": cap(&home), "op": "get", "path": format!("/search?id={}", now["stored"]["id"].as_str().unwrap())}));
        assert!(got["value"].get("authored_ms").is_none(), "a write delivered at once claims no authored time: {got}");
        set_url(&home, DEAD);
        write(svc, &home, "op-time-0002", "svclate");
        let file = std::fs::read_dir(home.join("outbox")).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "json")).unwrap();
        let mut env: Envelope = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let written = now_ms() - 20 * 60_000;
        env.ts_ms = written; // as if it had sat in the outbox for 20 minutes (delivery re-signs it)
        std::fs::write(&file, serde_json::to_string(&env).unwrap()).unwrap();
        set_url(&home, &live);
        assert_eq!(svc.answer(&json!({"cap": cap(&home), "op": "flush"}))["sent"], 1);
        let hit = svc.answer(&json!({"cap": cap(&home), "op": "get", "path": "/search?q=svclate&channel=svc&author=rocky"}));
        let obs = svc.answer(&json!({"cap": cap(&home), "op": "get", "path": format!("/search?id={}", hit["value"]["items"][0]["id"].as_str().unwrap())}));
        assert_eq!(obs["value"]["authored_ms"], written, "{obs}");
        assert!(obs["value"]["created_ms"].as_i64().unwrap() - written >= 20 * 60_000 - 1000, "received now, authored then");
    }

    #[test]
    fn writes_go_on_the_author_chain_even_from_the_outbox() {
        // docs/CHAIN.md: ox signs each write over the head the Oracle reports, a queued write included (its prev is
        // chosen at delivery, not when it was queued), and the chain reads back whole.
        let o = oracle();
        let home = o.seat_home("rocky", 2, None);
        let live = url_of(&home);
        let run = start(std::slice::from_ref(&home));
        let svc = run.service();
        write(svc, &home, "op-chain-0001", "chainone");
        set_url(&home, DEAD);
        write(svc, &home, "op-chain-0002", "chainqueued");
        set_url(&home, &live);
        assert_eq!(svc.answer(&json!({"cap": cap(&home), "op": "flush"}))["sent"], 1);
        write(svc, &home, "op-chain-0003", "chainthree");
        let got = svc.answer(&json!({"cap": cap(&home), "op": "get", "path": "/chain/rocky"}));
        let recs: Vec<oracle_proto::ChainRecord> = serde_json::from_value(got["value"]["records"].clone()).unwrap();
        let r = oracle_proto::check_chain(&recs);
        assert_eq!((r.records, r.chained, r.unchained, r.from_genesis), (3, 3, 0, true), "{r:?}");
        assert!(r.breaks.is_empty(), "{r:?}");
        // Negative control: the same face with its middle record removed is broken.
        let gap = [recs[0].clone(), recs[2].clone()];
        assert_eq!(oracle_proto::check_chain(&gap).breaks.len(), 1);
    }

    #[test]
    fn a_key_the_registry_does_not_hold_fails_closed() {
        let o = oracle();
        let home = o.seat_home("rocky", 9, None);
        let live = url_of(&home);
        let run = start(std::slice::from_ref(&home));
        let svc = run.service();
        let v = write(svc, &home, "op-bad-0001", "should not land");
        assert_eq!(v["ok"], false, "{v}");
        assert_eq!(svc.answer(&json!({"cap": cap(&home), "op": "queued"}))["queued"], 0, "a refusal is never queued for retry");
        // ...and a retry of that op is the same refusal, never success and never queued.
        let again = write(svc, &home, "op-bad-0001", "should not land");
        assert_eq!((again["ok"].clone(), again["code"].clone()), (json!(false), v["code"].clone()), "{again}");
        assert!(again.get("stored").is_none() && again.get("queued").is_none());
        assert_eq!(svc.answer(&json!({"cap": cap(&home), "op": "queued"}))["queued"], 0);
        // A queued envelope the server later refuses is set aside, not retried forever.
        set_url(&home, DEAD);
        write(svc, &home, "op-bad-0002", "queued then refused");
        set_url(&home, &live);
        let f = svc.answer(&json!({"cap": cap(&home), "op": "flush"}));
        assert_eq!((f["sent"].clone(), f["left"].clone()), (json!(0), json!(0)), "{f}");
        assert!(std::fs::read_dir(home.join("outbox")).unwrap().any(|e| e.unwrap().path().extension().is_some_and(|x| x == "rejected")));
    }

    #[test]
    fn the_clock_and_health_are_unknown_until_observed() {
        let o = oracle();
        let (home, dead) = (o.seat_home("rocky", 2, None), o.seat_home("mallory", 3, Some(DEAD)));
        let run = start(&[home.clone(), dead.clone()]);
        let svc = run.service();
        let before = svc.answer(&json!({"cap": cap(&home), "op": "now"}));
        assert_eq!((before["clock"].clone(), before["reachable"].clone(), before["authenticated"].clone()), (Value::Null, Value::Null, Value::Null), "{before}");
        svc.seats[0].sync();
        let after = svc.answer(&json!({"cap": cap(&home), "op": "now"}));
        assert_eq!((after["reachable"].clone(), after["authenticated"].clone()), (json!(true), json!(true)), "{after}");
        assert!(after["clock"]["now_ms"].as_i64().unwrap() > 1_700_000_000_000 && after["clock"]["uncertainty_ms"].as_i64().unwrap() >= 1, "{after}");
        assert!(after["clock"]["sample_age_ms"].as_i64().unwrap() >= 0);
        svc.seats[1].sync();
        let down = svc.answer(&json!({"cap": cap(&dead), "op": "now"}));
        assert_eq!((down["reachable"].clone(), down["clock"].clone()), (json!(false), Value::Null), "{down}");
        // Reachable but not this seat's token: reachable, not authenticated.
        let wrong = o.seat_home("rocky", 2, None);
        let p = wrong.join("config.json");
        let mut c: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        c["token"] = json!("not-a-token");
        std::fs::write(&p, c.to_string()).unwrap();
        svc.seats[0].sync();
        let refused = svc.answer(&json!({"cap": cap(&home), "op": "now"}));
        assert_eq!((refused["reachable"].clone(), refused["authenticated"].clone()), (json!(true), json!(false)), "{refused}");
    }

    #[test]
    fn post_is_only_read_marks_and_submit_only_for_the_seat_itself() {
        let o = oracle();
        let (rocky, mallory) = (o.seat_home("rocky", 2, None), o.seat_home("mallory", 3, None));
        let run = start(&[rocky.clone(), mallory.clone()]);
        let svc = run.service();
        let fold = svc.answer(&json!({"cap": cap(&rocky), "op": "post", "path": "/fold", "body": {"all": true}}));
        assert_eq!(fold["ok"], true, "{fold}");
        let other = svc.answer(&json!({"cap": cap(&rocky), "op": "post", "path": "/observations", "body": {}}));
        assert_eq!(other["code"], "bad_request", "a signed write never goes through post: {other}");
        assert_eq!(svc.answer(&json!({"cap": read_cap(&rocky), "op": "post", "path": "/fold", "body": {"all": true}}))["code"], "forbidden");
        // submit: a record the seat signed itself lands at once; one signed by another seat is refused here.
        let mine = Envelope::sign(&crate::signed_read_tests::sk(2), "svc", "rocky", Kind::Say, "svcsubmitted", None, vec![], now_ms());
        let ok = svc.answer(&json!({"cap": cap(&rocky), "op": "submit", "envelope": mine}));
        assert!(ok["stored"]["id"].is_string(), "{ok}");
        let theirs = Envelope::sign(&crate::signed_read_tests::sk(3), "svc", "mallory", Kind::Say, "not yours", None, vec![], now_ms());
        let refused = svc.answer(&json!({"cap": cap(&rocky), "op": "submit", "envelope": theirs}));
        assert_eq!(refused["ok"], false, "{refused}");
        // Unreachable: refused at once, never queued.
        set_url(&rocky, DEAD);
        let late = Envelope::sign(&crate::signed_read_tests::sk(2), "svc", "rocky", Kind::Say, "svcnever", None, vec![], now_ms());
        assert_eq!(svc.answer(&json!({"cap": cap(&rocky), "op": "submit", "envelope": late}))["code"], "unreachable");
        assert_eq!(svc.answer(&json!({"cap": cap(&rocky), "op": "queued"}))["queued"], 0);
    }

    #[test]
    fn one_handle_on_two_oracles_is_two_seats_but_twice_on_one_is_refused() {
        let o = oracle();
        assert!(Service::load(&[o.seat_home("rocky", 2, None), o.seat_home("rocky", 2, Some(DEAD))]).is_ok());
        let a = o.seat_home("rocky", 2, None);
        let b = o.home.join("rocky-copy");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::copy(a.join("config.json"), b.join("config.json")).unwrap();
        assert!(Service::load(&[a, b]).is_err());
    }
}
#[cfg(all(test, unix))]
mod emmy_ipc_deadline_review {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    #[test]
    fn emmy_trickling_service_reply_obeys_the_total_budget() {
        let root=std::env::temp_dir().join(format!("emmy-trickle-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(CAP_FILE),"disposable-test-cap").unwrap();
        let listener=std::os::unix::net::UnixListener::bind(socket(&root)).unwrap();
        let worker=std::thread::spawn(move || {
            let (mut stream,_)=listener.accept().unwrap();
            let mut req=String::new();
            BufReader::new(stream.try_clone().unwrap()).read_line(&mut req).unwrap();
            let reply = [b"{\"ok\":true".as_slice(), &[b' '; 160], b"}\n".as_slice()].concat();
            for byte in &reply {
                if stream.write_all(&[*byte]).is_err(){break;}
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let start=std::time::Instant::now();
        let result=request_within(&root,&root,json!({"op":"advisory_get","path":"/fold?limit=50","budget_ms":60}),Duration::from_millis(60));
        let elapsed=start.elapsed();
        println!("budget=60ms, request elapsed={elapsed:?}, result={result:?}");
        worker.join().unwrap();
        assert!(elapsed<Duration::from_millis(180),"fragmented reply bypassed total deadline: {elapsed:?}");
        assert!(result.is_err(),"a reply completed after the deadline must not count as timely");
    }
}
