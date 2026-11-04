//! `oracle` — run and administer an Oracle.
//!
//!   oracle init  --owner blaze        create the database, the owner key and its token
//!   oracle serve                      serve the HTTP API
//!   oracle seat add rocky             register a seat (signed by the owner key)
//!   oracle token new rocky            mint another token for a seat
//!
//! A minted token is written to a new file, mode 0600, and only its path is printed (`token_file`).

mod up;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use oracle_proto::{keyfile, pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::{
    http::{now_ms, router, App},
    store::{Scope, Store},
};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "oracle",
    version,
    about = "A signed, shared memory for agents. One binary, one SQLite file."
)]
struct Cli {
    /// Directory holding oracle.db and keys/.
    #[arg(long, env = "ORACLE_HOME", global = true, default_value = "./data")]
    home: PathBuf,
    /// Seat/token administration and the retention sweep against an initialized live store, without startup migrations or
    /// reindexing (4 Nov 2026: the hourly sweep opened the store whole, restarted the FTS shadow rebuild under the serving
    /// writer every hour and held the write lock long enough that live writes answered 503).
    #[arg(long, global = true)]
    online_admin: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the database and the owner seat. The owner key is the root of trust.
    Init {
        /// Handle of the human (or agent) who owns this Oracle.
        #[arg(long)]
        owner: String,
        /// Where the owner's token is written (a new file, mode 0600). Default: keys/<owner>.token.
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// One command from nothing to a working Oracle with your agents connected (src/up.rs): owner key, an agent seat, the server as a
    /// per-user service, ox, then each agent found here over MCP. Shows the plan and asks first; --plan changes nothing.
    /// Its home is ~/.oracle-node unless --home or ORACLE_HOME says otherwise.
    Up {
        /// Your handle (default: your login name).
        #[arg(long)]
        owner: Option<String>,
        /// The seat your agents speak as.
        #[arg(long, default_value = "agent")]
        seat: String,
        #[arg(long, default_value = "127.0.0.1:8900")]
        bind: String,
        /// Which agents to connect: auto (default: every one found), none, or a list of claude-code, codex, cursor.
        #[arg(long, value_delimiter = ',')]
        agents: Vec<String>,
        /// Do it without asking.
        #[arg(long)]
        yes: bool,
        /// Print the plan and change nothing.
        #[arg(long)]
        plan: bool,
        /// Do not install the per-user service (you run `oracle serve` yourself).
        #[arg(long)]
        no_service: bool,
        /// Where ox keeps this seat (default: OX_HOME or ~/.oracle). Never overwritten if it already holds a seat.
        #[arg(long)]
        ox_home: Option<PathBuf>,
        /// Only connect the agents, to the seat a previous `oracle up` made (the retry after a failed agent step).
        #[arg(long)]
        agents_only: bool,
    },
    /// Serve the HTTP API.
    Serve {
        #[arg(long, env = "ORACLE_BIND", default_value = "127.0.0.1:8900")]
        bind: String,
    },
    /// Retention (retention.rs): `oracle retention` lists the rules; `oracle retention feeds --ttl 48` makes #feeds forget a record 48 h
    /// after it was written (records already there included); `--clear` removes the rule; `oracle retention --sweep` counts the expired
    /// records of every ruled room and `--sweep --yes` removes them from the store. Rules are signed with the owner key.
    Retention {
        room: Option<String>,
        /// Hours a record stays, 1 to 8784.
        #[arg(long)]
        ttl: Option<u32>,
        #[arg(long)]
        clear: bool,
        #[arg(long)]
        sweep: bool,
        /// With --sweep: remove the expired records (without it, only count them).
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        owner: Option<String>,
    },
    /// Manage seats (each change is signed by the owner key).
    Seat {
        #[command(subcommand)]
        cmd: SeatCmd,
    },
    /// Manage bearer tokens.
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// Import observations from an earlier Oracle (JSON lines; see docs/import.md).
    Import { file: PathBuf },
    /// Counts, for a quick look.
    Stats,
    /// The recall scoreboard (docs/RECALL-SCOREBOARD.md): run the arms a signed pin names against the gold set, refuse
    /// unless the gold's hash, the arms and the signer match the pin, and print the receipt (post it with `ox say oracle
    /// --cargo`). Read-only on the store.
    Scoreboard {
        /// The pin message's id (tagged scoreboard:pin, signed by a trusted seat).
        #[arg(long)]
        pin: String,
        #[arg(long, default_value = "/var/lib/oracle-search-eval/gold.jsonl")]
        gold: PathBuf,
        /// Arms to run, a subset of the pin's; default every arm in the pin.
        #[arg(long = "arm")]
        arms: Vec<String>,
        /// Seats whose pin is trusted.
        #[arg(long, default_value = "blaze,rocky,emmy")]
        trust: String,
        /// The SEO terms file, hashed and checked against the pin's terms_sha256 when the pin has one.
        #[arg(long)]
        terms: Option<PathBuf>,
        /// Write the receipt here too.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Read the pin from this database instead (a later copy of Oracle): a pin that freezes a snapshot's hash cannot
        /// be inside that snapshot. The arms still run on ORACLE_HOME's store.
        #[arg(long)]
        pin_db: Option<PathBuf>,
        /// Print the embedder probe (sha256 of the probe's query vector, its dimension) and the index's coverage, for a
        /// pin's embed_probe_sha256 and embed_dim; runs nothing.
        #[arg(long)]
        probe: bool,
        /// Score one half of a gold file whose questions carry `split` (gold 100): dev or test.
        #[arg(long)]
        split: Option<String>,
        /// Write what the pin's rerank arms send out (ids, rooms, counts; no text) to this file, mode 0600, and run
        /// nothing else: no reranker call (Emmy).
        #[arg(long)]
        egress_inventory: Option<PathBuf>,
    },
    /// A name a seat once used stands for that seat (card seat-alias-migration): `oracle alias set emmy-core emmy`. One owner-signed record in #oracle, no second signer.
    Alias {
        #[command(subcommand)]
        cmd: AliasCmd,
    },
    /// Who may read other seats' asks, asked and unread (docs/ROOM-INDEX.md, Emmy's Q1): named service identities, never self-declared.
    Gateway {
        #[command(subcommand)]
        cmd: GatewayCmd,
    },
    /// One oracle-sync/1 round with a peer node (docs/SYNC.md): replay its key events, pull what it has that this node
    /// lacks, push what this node has that it lacks. `oracle serve` runs the same round every few seconds when
    /// ORACLE_SYNC_PEER is set.
    Sync {
        /// The peer's base URL, e.g. https://peer.example/oracle-core
        #[arg(long)]
        peer: String,
        /// The peer node's name (where entries from it are recorded as coming from).
        #[arg(long)]
        peer_name: String,
        /// This node's name.
        #[arg(long)]
        node: String,
        /// A file holding an admin-scope token of the peer.
        #[arg(long)]
        token_file: std::path::PathBuf,
    },
    /// Build the room index (docs/ROOM-INDEX.md) again from the log. One transaction that holds the database for the
    /// whole run: stop the server first, or run it on a copy.
    Reindex,
    /// Apply the live-state rule (keep a subject's newest row of each stretch of one state word) to rooms that already hold the old revisions.
    CompactLive {
        /// A room to compact; repeat for more.
        #[arg(long = "room", required = true)]
        rooms: Vec<String>,
        /// Count what would be deleted and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Rank the words that matter in Oracle and the Time Machine and write them for `GET /terms` (docs/TERMS.md). Reads
    /// both stores read-only and never opens this Oracle's home; it was ops/terms.py.
    Terms {
        #[arg(long, default_value = "/var/lib/oracle-terms/terms.json")]
        out: PathBuf,
        #[arg(long, default_value_t = 500)]
        top: usize,
        /// The Oracle database to read; default `<ORACLE_HOME>/oracle.db`.
        #[arg(long, env = "ORACLE_DB")]
        oracle_db: Option<PathBuf>,
        /// The Time Machine database to read (no default: the deployment names it).
        #[arg(long, env = "TIMEMACHINE_DB")]
        chats_db: PathBuf,
    },
    /// Delete a room or move its messages into another (src/rooms.rs). Reports what it would do unless `--yes`. The
    /// running server keeps a cached board per room: prefer `POST /admin/rooms` against a live server, or restart it after.
    Room {
        #[command(subcommand)]
        cmd: RoomCmd,
    },
}

#[derive(Subcommand)]
enum RoomCmd {
    /// Delete every message in a room and everything derived from them.
    Delete {
        room: String,
        /// Delete even when other rooms' messages point into this one (they stay, as their own threads).
        #[arg(long)]
        force: bool,
        /// Do it; without this, only report.
        #[arg(long)]
        yes: bool,
    },
    /// Move every message of a room into another room (imported history only; signed messages cannot be re-homed).
    Move {
        from: String,
        to: String,
        #[arg(long)]
        yes: bool,
    },
    /// The record of room operations, newest first.
    Log {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
}

#[derive(Subcommand)]
enum AliasCmd {
    /// `label` now stands for `seat`: asks addressed to the label are answered by the seat's own signed replies. Signed with the owner key on disk.
    Set {
        label: String,
        seat: String,
        #[arg(long)]
        owner: Option<String>,
    },
    /// Take an alias away: the name is a name again. Nothing already written is changed.
    Remove {
        label: String,
        #[arg(long)]
        owner: Option<String>,
    },
    /// The aliases in force.
    List,
}

#[derive(Subcommand)]
enum GatewayCmd {
    /// Register a seat as a gateway for some routes (asks, asked, unread; default all three).
    Add {
        handle: String,
        #[arg(long, value_delimiter = ',', default_value = "asks,asked,unread")]
        routes: Vec<String>,
    },
    /// Take a gateway registration away.
    Remove { handle: String },
    /// The registered gateways.
    List,
    /// The newest cross-seat reads: who read whose, whether policy allowed it and whether a refusal was actually returned.
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
}

#[derive(Subcommand)]
enum SeatCmd {
    /// Register a seat. Generates its key unless --pubkey is given (the seat then keeps
    /// its private key and nobody else ever sees it).
    Add {
        handle: String,
        #[arg(long)]
        pubkey: Option<String>,
        /// Owner handle whose key signs this (default: the only owner key on disk).
        #[arg(long)]
        owner: Option<String>,
        /// Where the seat's token is written (a new file, mode 0600). Default: keys/<handle>.token.
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Give an existing seat another key — a second device it speaks from (a laptop, a web
    /// gateway). The new key can speak as the seat; it can never change the registry.
    AddKey {
        handle: String,
        #[arg(long)]
        pubkey: String,
        #[arg(long)]
        owner: Option<String>,
    },
    /// Revoke a seat and all of its tokens — or, with --pubkey, just one of its keys.
    Revoke {
        handle: String,
        #[arg(long)]
        pubkey: Option<String>,
        #[arg(long)]
        owner: Option<String>,
    },
    /// List seats.
    List,
    /// Confine a seat to rooms (seatrooms.rs): it writes only in --write rooms, reads only those and --read rooms, and every other route is
    /// refused to it. --clear makes it an ordinary seat again; no handle lists the table. Signed with the owner key on disk.
    Rooms {
        handle: Option<String>,
        #[arg(long, value_delimiter = ',')]
        write: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        read: Vec<String>,
        #[arg(long)]
        clear: bool,
        #[arg(long)]
        owner: Option<String>,
    },
    /// Make a room private to its members (privaterooms.rs): every other seat, studio seats included, is refused its routes and its
    /// writes, and has its records scrubbed from every answer. --clear opens it again; no room lists the table. Signed with the owner key.
    Private {
        room: Option<String>,
        #[arg(long, value_delimiter = ',')]
        members: Vec<String>,
        #[arg(long)]
        clear: bool,
        #[arg(long)]
        owner: Option<String>,
    },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Mint a token bound to a seat, written to a new file (mode 0600); only its hash is stored.
    New {
        handle: String,
        #[arg(long, default_value = "write")]
        scope: String,
        #[arg(long, default_value = "")]
        label: String,
        /// Where the token is written (a new file, mode 0600). Default: keys/<handle>.token.
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
}

fn db_path(home: &Path) -> String {
    home.join("oracle.db").display().to_string()
}

fn key_path(home: &Path, handle: &str) -> PathBuf {
    home.join("keys").join(format!("{handle}.key"))
}

/// A minted token goes into a new file, mode 0600, never to stdout (Blaze, 31 Oct 2026: `seat add` printed Pythia's token,
/// it reached a log, and the seat had to be revoked). The file is created before anything else changes, so a path that
/// cannot be written fails with the seat untouched (Emmy's review of 2b1cc32), and it is never overwritten: an existing
/// one is an unused token or a seat's live one. If the command fails after the file was made, the empty file is removed.
/// `ox setup --token-file` reads it; delete it after.
struct TokenFile {
    path: PathBuf,
    file: Option<std::fs::File>,
}

impl TokenFile {
    fn reserve(home: &Path, handle: &str, given: Option<PathBuf>) -> Result<Self> {
        let path = given.unwrap_or_else(|| home.join("keys").join(format!("{handle}.token")));
        if path.exists() {
            bail!("{} already exists: run `ox setup --token-file` with it and delete it, or pass --token-file <new path>", path.display());
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("cannot create {} for the token; nothing was changed", dir.display()))?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(&path).with_context(|| format!("cannot create {}; nothing was changed", path.display()))?;
        Ok(Self { path, file: Some(file) })
    }

    fn fill(mut self, token: &str) -> Result<PathBuf> {
        use std::io::Write;
        let mut f = self.file.take().context("token file already written")?;
        f.write_all(format!("{token}\n").as_bytes())?;
        f.sync_all()?;
        Ok(std::mem::take(&mut self.path))
    }
}

impl Drop for TokenFile {
    fn drop(&mut self) {
        if self.file.is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn owner_handle(store: &Store, given: Option<String>) -> Result<String> {
    if let Some(h) = given {
        return Ok(h);
    }
    store
        .keys()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .into_iter()
        .find(|k| k.role == "owner")
        .map(|k| k.handle)
        .context("this Oracle has no owner yet; run `oracle init --owner <you>`")
}

fn main() -> Result<()> {
    let mut cli = Cli::parse();
    if cli.online_admin && !matches!(cli.cmd, Cmd::Seat { .. } | Cmd::Token { .. } | Cmd::Retention { sweep: true, .. }) {
        bail!("--online-admin is only for seat and token commands and `retention --sweep`");
    }
    // `oracle up` is the stranger's first command: its home is ~/.oracle-node, not ./data in whatever folder they stood in.
    if matches!(cli.cmd, Cmd::Up { .. }) && cli.home == Path::new("./data") && std::env::var_os("ORACLE_HOME").is_none() {
        if let Some(h) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            cli.home = PathBuf::from(h).join(".oracle-node");
        }
    }
    // Before the store is opened: `terms` reads two databases read-only and must not create or migrate a home.
    if let Cmd::Terms { out, top, oracle_db, chats_db } = &cli.cmd {
        let started = std::time::Instant::now();
        let generated_at = format!("{}Z", &oracle_proto::iso_ms(now_ms())[..19]);
        let db = oracle_db.clone().unwrap_or_else(|| PathBuf::from(db_path(&cli.home)));
        let v = oracle_server::terms::build(&db, chats_db, *top, &generated_at, started)?;
        oracle_server::terms::write_atomic(out, &v)?;
        let head: serde_json::Map<String, serde_json::Value> =
            ["generated_at", "seconds", "documents", "stores"].iter().map(|k| (k.to_string(), v[*k].clone())).collect();
        println!("{}", serde_json::Value::Object(head));
        return Ok(());
    }
    // Before the store is opened: `oracle up` plans from a read-only look and creates the home only after a yes (src/up.rs).
    if let Cmd::Up { owner, seat, bind, agents, yes, plan, no_service, ox_home, agents_only } = cli.cmd {
        let home = cli.home.clone();
        let db = PathBuf::from(db_path(&home));
        return up::run(&home, &db, |h| key_path(&home, h), up::Up { owner, seat, bind, agents, yes, plan, no_service, ox_home, agents_only });
    }
    // A server pointed at a home that was never initialised would create an empty database with no owner and answer
    // healthy, and every token would then fail (measured 1 Nov 2026: a launchd or systemd unit naming another ORACLE_HOME
    // than the one `oracle init` used). Refused before anything is created.
    if matches!(cli.cmd, Cmd::Serve { .. }) && !Path::new(&db_path(&cli.home)).exists() {
        bail!("no Oracle in {}: run `oracle init --owner <you>` with this same home first (--home or ORACLE_HOME); nothing was created", cli.home.display());
    }
    let fresh_home = matches!(cli.cmd, Cmd::Init { .. }) && !cli.home.exists();
    std::fs::create_dir_all(&cli.home)
        .with_context(|| format!("cannot create {}", cli.home.display()))?;
    // A home this command creates is this user's alone: it holds the database and the owner key, and a umask of 002
    // (Ubuntu's default for a user) left it 0775 with the database 0644, readable by every local user.
    #[cfg(unix)]
    if fresh_home {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cli.home, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot make {} private", cli.home.display()))?;
    }
    #[cfg(not(unix))]
    let _ = fresh_home;
    // The scoreboard hashes the files it reads before the store is opened: opening may checkpoint and change them.
    let snapshot = if matches!(cli.cmd, Cmd::Scoreboard { .. }) {
        let vec_db = std::env::var("ORACLE_VEC_DB").ok().filter(|p| !p.is_empty());
        oracle_server::scoreboard::Snapshot {
            store_sha256: Some(oracle_server::scoreboard::file_sha256(&db_path(&cli.home))?),
            vectors_sha256: vec_db.map(|p| oracle_server::scoreboard::file_sha256(&p)).transpose()?,
        }
    } else {
        oracle_server::scoreboard::Snapshot::default()
    };
    let store = if cli.online_admin { Store::open_control(&db_path(&cli.home)) } else { Store::open(&db_path(&cli.home)) }
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let err = |e: oracle_server::store::StoreError| anyhow::anyhow!(e.to_string());

    match cli.cmd {
        Cmd::Up { .. } => unreachable!("handled before the store is opened"),
        Cmd::Init { owner, token_file: given } => {
            let reserved = TokenFile::reserve(&cli.home, &owner, given)?;
            let path = key_path(&cli.home, &owner);
            let sk = oracle_proto::generate_key();
            keyfile::create(&path, &sk)?;
            store
                .init_owner(&owner, &pubkey_hex(&sk), now_ms())
                .map_err(err)?;
            let token = store
                .mint_token(&owner, Scope::Admin, "init", now_ms())
                .map_err(err)?;
            let tpath = reserved.fill(&token)?;
            println!("Oracle initialised in {}", cli.home.display());
            println!("  owner   {owner}");
            println!("  pubkey  {}", pubkey_hex(&sk));
            println!(
                "  key     {}   (mode 0600 — back this up; it is the root of trust)",
                path.display()
            );
            println!("  token   {}   (mode 0600, admin; never printed)", tpath.display());
            println!("Next: `oracle serve`, then `ox setup --token-file {}`, then delete the token file.", tpath.display());
        }
        Cmd::Alias { cmd } => {
            // The table lives in the room index, so an index that has not been built would read as an empty table and a `set` would drop the others.
            if !store.index_ready() {
                bail!("the room index is not built yet: run `oracle reindex` first (seat aliases are read from it)");
            }
            let post = |owner: Option<String>, table: &oracle_server::alias::Aliases, what: String| -> Result<()> {
                oracle_server::alias::parse(&oracle_server::alias::cargo(table)).map_err(|e| anyhow::anyhow!(e))?;
                let owner = owner_handle(&store, owner)?;
                let sk = keyfile::load(&key_path(&cli.home, &owner))?;
                let now = now_ms();
                let env = Envelope::sign(&sk, oracle_server::alias::ROOM, &owner, Kind::Say, &format!("{} {what}", oracle_server::alias::FACE), Some(oracle_server::alias::cargo(table)), vec!["schema:seat-alias/1".into(), "intent:decision".into(), "ask:none".into()], now);
                let r = store.write(&oracle_server::store::Principal { handle: owner, scope: Scope::Write }, &env, now).map_err(err)?;
                println!("{what}  ({})", r.id);
                Ok(())
            };
            match cmd {
                AliasCmd::Set { label, seat, owner } => {
                    if store.active_keys(&seat).map_err(err)?.is_empty() {
                        bail!("{seat} is not a seat with an active key");
                    }
                    let mut t = store.seat_aliases();
                    t.insert(label.clone(), seat.clone());
                    post(owner, &t, format!("{label} → {seat}"))?;
                }
                AliasCmd::Remove { label, owner } => {
                    let mut t = store.seat_aliases();
                    if t.remove(&label).is_none() {
                        bail!("{label} is not an alias");
                    }
                    post(owner, &t, format!("{label} is a name again"))?;
                }
                AliasCmd::List => {
                    for (l, s) in store.seat_aliases() {
                        println!("{l} → {s}");
                    }
                }
            }
        }
        Cmd::Gateway { cmd } => match cmd {
            GatewayCmd::Add { handle, routes } => {
                store.gateway_add(&handle, &routes, now_ms())?;
                println!("gateway {handle}: {}", routes.join(","));
            }
            GatewayCmd::Remove { handle } => println!("{}", if store.gateway_remove(&handle)? { format!("gateway {handle} removed") } else { format!("{handle} was not a gateway") }),
            GatewayCmd::List => {
                for (h, r) in store.gateways()? {
                    println!("{h}  {}", r.join(","));
                }
            }
            GatewayCmd::Audit { limit } => {
                for r in store.cross_seat_audit(limit)? {
                    println!("{}  {} → {}  {}  {}{}", oracle_proto::iso_ms(r.at_ms), r.caller, r.target, r.route, if r.permitted { "allowed" } else { "NOT allowed" }, if r.enforced { " (refused)" } else if !r.permitted { " (audit mode: let through)" } else { "" });
                }
            }
        },
        Cmd::CompactLive { rooms, dry_run } => {
            for c in store.compact_live(&rooms, dry_run)? {
                println!("#{}: {} state rows, {} {}", c.room, c.states, if dry_run { "would delete" } else { "deleted" }, c.deleted);
            }
        }
        Cmd::Room { cmd } => {
            let report = match cmd {
                RoomCmd::Delete { room, force, yes } => store.delete_room(&room, force, !yes, "owner-cli", now_ms()).map_err(err)?,
                RoomCmd::Move { from, to, yes } => store.move_room(&from, &to, !yes, "owner-cli", now_ms()).map_err(err)?,
                RoomCmd::Log { limit } => {
                    for op in store.room_ops(limit).map_err(err)? {
                        println!("{op}");
                    }
                    return Ok(());
                }
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::Sync { peer, peer_name, node, token_file } => {
            let token = std::fs::read_to_string(&token_file).with_context(|| format!("cannot read {}", token_file.display()))?;
            let p = oracle_server::sync::HttpPeer::new(&peer, &token);
            let r = oracle_server::sync::round(&store, &node, &peer_name, &p, oracle_server::store::now_ms_pub())?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        Cmd::Reindex => {
            let t = std::time::Instant::now();
            let n = store.reindex()?;
            println!("reindexed {} messages in {} threads in {:.1?}", n.messages, n.threads, t.elapsed());
        }
        Cmd::Serve { bind } => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind(&bind)
                    .await
                    .with_context(|| format!("cannot bind {bind}"))?;
                eprintln!(
                    "oracle {} listening on http://{bind}",
                    env!("CARGO_PKG_VERSION")
                );
                let mut app = App::new(store);
                // The tools at POST /mcp call back through this same door, as the caller.
                app.self_url = format!("http://{}", listener.local_addr().map(|a| a.to_string()).unwrap_or(bind.clone()));
                app.hosted_keys = Some(cli.home.join("hosted"));
                if let Some(sem) = &app.semantic {
                    sem.spawn_indexer(db_path(&cli.home));
                }
                spawn_sync(db_path(&cli.home));
                axum::serve(listener, router(app))
                    .with_graceful_shutdown(async {
                        let _ = tokio::signal::ctrl_c().await;
                    })
                    .await?;
                anyhow::Ok(())
            })?;
        }
        Cmd::Retention { room, ttl, clear, sweep, yes, owner } => {
            if sweep {
                let now = now_ms();
                let swept = store.sweep_expired(now, yes).map_err(err)?;
                if swept.is_empty() {
                    println!("no room has a retention rule");
                }
                for (room, n) in swept {
                    println!("#{room}  {n} expired record(s) {}", if yes { "removed" } else { "(dry run: --yes removes them)" });
                }
                return Ok(());
            }
            let mut t = store.retention_table().map_err(err)?;
            let Some(room) = room else {
                for (r, rule) in &t {
                    println!("#{r}  {} h", rule.ttl_hours);
                }
                if t.is_empty() {
                    println!("no retention rules: every room keeps everything");
                }
                return Ok(());
            };
            let what = if clear {
                if t.remove(&room).is_none() {
                    bail!("#{room} has no retention rule");
                }
                format!("#{room} keeps everything again")
            } else {
                let Some(h) = ttl else { bail!("--ttl <hours> or --clear") };
                t.insert(room.clone(), oracle_server::retention::Rule { ttl_hours: h });
                format!("#{room} forgets a record {h} h after it was written")
            };
            let cargo = oracle_server::retention::cargo(&t);
            oracle_server::retention::parse(&cargo).map_err(|e| anyhow::anyhow!(e))?;
            let owner = owner_handle(&store, owner)?;
            let sk = keyfile::load(&key_path(&cli.home, &owner))?;
            let now = now_ms();
            let env = Envelope::sign(&sk, oracle_server::retention::ROOM, &owner, Kind::Say, &format!("{} {what}", oracle_server::retention::FACE), Some(cargo), vec!["schema:retention/1".into()], now);
            let r = store.write(&oracle_server::store::Principal { handle: owner, scope: Scope::Write }, &env, now).map_err(err)?;
            println!("{what}  ({})", r.id);
        }
        Cmd::Seat { cmd } => match cmd {
            SeatCmd::Add {
                handle,
                pubkey,
                owner,
                token_file: given,
            } => {
                let reserved = TokenFile::reserve(&cli.home, &handle, given)?;
                let owner = owner_handle(&store, owner)?;
                let owner_sk = keyfile::load(&key_path(&cli.home, &owner))?;
                let (pk, generated) = match pubkey {
                    Some(pk) => (pk, None),
                    None => {
                        let sk = oracle_proto::generate_key();
                        let path = key_path(&cli.home, &handle);
                        keyfile::create(&path, &sk)?;
                        (pubkey_hex(&sk), Some(path))
                    }
                };
                let ev = KeyEvent::sign(&owner_sk, &owner, KeyAction::Add, &handle, &pk, now_ms());
                store.apply_key_event(&ev, now_ms()).map_err(err)?;
                let token = store
                    .mint_token(&handle, Scope::Write, "seat-add", now_ms())
                    .map_err(err)?;
                let tpath = reserved.fill(&token)?;
                println!("seat    {handle}");
                println!("pubkey  {pk}");
                if let Some(p) = generated {
                    println!(
                        "key     {}   (hand this file to the seat, then delete it here)",
                        p.display()
                    );
                }
                println!("token   {}   (mode 0600; `ox setup --token-file` it, then delete it here)", tpath.display());
            }
            SeatCmd::Private { room, members, clear, owner } => {
                let mut t = store.private_rooms_table().map_err(err)?;
                let Some(room) = room else {
                    for (r, m) in &t {
                        println!("#{r}  {}", m.iter().cloned().collect::<Vec<_>>().join(","));
                    }
                    return Ok(());
                };
                let what = if clear {
                    if t.remove(&room).is_none() {
                        bail!("#{room} is not private");
                    }
                    format!("#{room} is open again")
                } else {
                    for m in &members {
                        if store.active_keys(m).map_err(err)?.is_empty() {
                            bail!("{m} is not a seat with an active key");
                        }
                    }
                    let w = format!("#{room} is private to {}", members.join(","));
                    t.insert(room.clone(), members.into_iter().collect());
                    w
                };
                let cargo = oracle_server::privaterooms::cargo(&t);
                oracle_server::privaterooms::parse(&cargo).map_err(|e| anyhow::anyhow!(e))?;
                let owner = owner_handle(&store, owner)?;
                let sk = keyfile::load(&key_path(&cli.home, &owner))?;
                let now = now_ms();
                let env = Envelope::sign(&sk, oracle_server::privaterooms::ROOM, &owner, Kind::Say, &format!("{} {what}", oracle_server::privaterooms::FACE), Some(cargo), vec!["schema:private-rooms/1".into(), "intent:decision".into(), "ask:none".into()], now);
                let r = store.write(&oracle_server::store::Principal { handle: owner, scope: Scope::Write }, &env, now).map_err(err)?;
                println!("{what}  ({})", r.id);
            }
            SeatCmd::Rooms { handle, write, read, clear, owner } => {
                let mut t = store.seat_scope_table().map_err(err)?;
                let Some(handle) = handle else {
                    for (seat, r) in &t {
                        println!("{seat}  write {}  read {}", r.write.iter().cloned().collect::<Vec<_>>().join(","), r.read.iter().cloned().collect::<Vec<_>>().join(","));
                    }
                    return Ok(());
                };
                let what = if clear {
                    if t.remove(&handle).is_none() {
                        bail!("{handle} is not scoped");
                    }
                    format!("{handle} is an ordinary seat again")
                } else {
                    if store.active_keys(&handle).map_err(err)?.is_empty() {
                        bail!("{handle} is not a seat with an active key");
                    }
                    let r = oracle_server::seatrooms::Rooms { write: write.into_iter().collect(), read: read.into_iter().collect() };
                    let w = format!("{handle} writes in {}, reads {}", r.write.iter().cloned().collect::<Vec<_>>().join(","), r.read.iter().cloned().collect::<Vec<_>>().join(","));
                    t.insert(handle.clone(), r);
                    w
                };
                let cargo = oracle_server::seatrooms::cargo(&t);
                oracle_server::seatrooms::parse(&cargo).map_err(|e| anyhow::anyhow!(e))?;
                let owner = owner_handle(&store, owner)?;
                let sk = keyfile::load(&key_path(&cli.home, &owner))?;
                let now = now_ms();
                let env = Envelope::sign(&sk, oracle_server::seatrooms::ROOM, &owner, Kind::Say, &format!("{} {what}", oracle_server::seatrooms::FACE), Some(cargo), vec!["schema:seat-rooms/1".into(), "intent:decision".into(), "ask:none".into()], now);
                let r = store.write(&oracle_server::store::Principal { handle: owner, scope: Scope::Write }, &env, now).map_err(err)?;
                println!("{what}  ({})", r.id);
            }
            SeatCmd::AddKey {
                handle,
                pubkey,
                owner,
            } => {
                let owner = owner_handle(&store, owner)?;
                let owner_sk = keyfile::load(&key_path(&cli.home, &owner))?;
                let ev = KeyEvent::sign(
                    &owner_sk,
                    &owner,
                    KeyAction::Add,
                    &handle,
                    &pubkey,
                    now_ms(),
                );
                store.apply_key_event(&ev, now_ms()).map_err(err)?;
                println!(
                    "{handle} may now also sign with {}…",
                    &pubkey[..16.min(pubkey.len())]
                );
            }
            SeatCmd::Revoke {
                handle,
                pubkey,
                owner,
            } => {
                let owner = owner_handle(&store, owner)?;
                let owner_sk = keyfile::load(&key_path(&cli.home, &owner))?;
                let ev = KeyEvent::sign(
                    &owner_sk,
                    &owner,
                    KeyAction::Revoke,
                    &handle,
                    pubkey.as_deref().unwrap_or(""),
                    now_ms(),
                );
                store.apply_key_event(&ev, now_ms()).map_err(err)?;
                match pubkey {
                    Some(pk) => {
                        println!("revoked one key of {handle}: {}…", &pk[..16.min(pk.len())])
                    }
                    None => println!("revoked {handle} and all of its tokens"),
                }
            }
            SeatCmd::List => {
                for k in store.keys().map_err(err)? {
                    let status = if k.revoked_ms.is_some() {
                        "revoked"
                    } else {
                        "active"
                    };
                    println!(
                        "{:<20} {:<6} {:<8} {}…",
                        k.handle,
                        k.role,
                        status,
                        &k.pubkey[..16]
                    );
                }
            }
        },
        Cmd::Token { cmd } => match cmd {
            TokenCmd::New {
                handle,
                scope,
                label,
                token_file: given,
            } => {
                let Some(scope) = Scope::parse(&scope) else {
                    bail!("scope must be read, write or admin")
                };
                let reserved = TokenFile::reserve(&cli.home, &handle, given)?;
                let token = store.mint_token(&handle, scope, &label, now_ms()).map_err(err)?;
                let tpath = reserved.fill(&token)?;
                println!("token   {}   (mode 0600; never printed)", tpath.display());
            }
        },
        Cmd::Import { file } => {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(
                std::fs::File::open(&file)
                    .with_context(|| format!("cannot open {}", file.display()))?,
            );
            let started = std::time::Instant::now();
            let (mut rows, mut bad) = (Vec::new(), 0usize);
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<oracle_server::store::LegacyRow>(&line) {
                    Ok(row) => rows.push(row),
                    Err(_) => bad += 1,
                }
            }
            // Sequence numbers are the order of `recent`, so history must go in in TIME order
            // whatever order the file is in.
            rows.sort_by(|a, b| (a.created_ms, &a.id).cmp(&(b.created_ms, &b.id)));
            let read = rows.len() + bad;
            let mut added = 0usize;
            for batch in rows.chunks(2000) {
                added += store.import_legacy(batch).map_err(err)?;
            }
            println!(
                "read {read}, imported {added}, already present {}, unreadable {bad} — {:.1}s",
                read - added - bad,
                started.elapsed().as_secs_f64()
            );
        }
        Cmd::Terms { .. } => unreachable!("answered before the store is opened"),
        Cmd::Scoreboard { pin, gold, arms, trust, terms, out, pin_db, probe, split, egress_inventory } => {
            if probe {
                let sem = oracle_server::semantic::Semantic::from_env().ok_or_else(|| anyhow::anyhow!("meaning search is off: {}", oracle_server::semantic::OFF_REASON.get().cloned().unwrap_or_default()))?;
                let (sha, dim) = oracle_server::scoreboard::probe(&sem)?;
                let (index_dim, windows, covered) = sem.coverage();
                println!("{}", serde_json::json!({"embed_probe_sha256": sha, "embed_dim": dim, "index_dim": index_dim, "windows": windows, "observations_covered": covered, "store_sha256": snapshot.store_sha256, "vectors_sha256": snapshot.vectors_sha256}));
                return Ok(());
            }
            let gold_text = std::fs::read_to_string(&gold).with_context(|| format!("gold {}", gold.display()))?;
            let questions = oracle_server::scoreboard::read_gold(&gold_text)?;
            let trusted: Vec<&str> = trust.split(',').map(str::trim).filter(|t| !t.is_empty()).collect();
            let (p, _) = match &pin_db {
                Some(path) => {
                    let pins = Store::open(&path.display().to_string()).map_err(|e| anyhow::anyhow!(e.to_string()))?;
                    oracle_server::scoreboard::read_pin(&pins, &pin, &trusted)?
                }
                None => oracle_server::scoreboard::read_pin(&store, &pin, &trusted)?,
            };
            let terms_sha = match &terms {
                Some(path) => Some(oracle_proto::sha_hex(&std::fs::read(path).with_context(|| format!("terms {}", path.display()))?)),
                None => None,
            };
            let wants_meaning = p.arms.iter().chain(arms.iter()).any(|a| oracle_server::scoreboard::needs_meaning(a));
            let sem = if wants_meaning { oracle_server::semantic::Semantic::from_env() } else { None };
            if let Some(path) = egress_inventory {
                let n = p.arms.iter().filter(|a| oracle_server::scoreboard::is_rerank(a)).count();
                let mut inv = oracle_server::scoreboard::egress_inventory(&store, sem.as_deref(), &questions, &p, split.as_deref(), n)?;
                let exe = std::env::current_exe()?;
                inv["pin"] = serde_json::json!(pin);
                inv["cutoff_seq"] = serde_json::json!(p.cutoff_seq);
                inv["snapshot"] = serde_json::json!({"store_sha256": snapshot.store_sha256, "vectors_sha256": snapshot.vectors_sha256, "max_seq": p.max_seq});
                inv["gold_sha256"] = serde_json::json!(oracle_proto::sha_hex(gold_text.as_bytes()));
                inv["binary"] = serde_json::json!({"sha256": oracle_server::scoreboard::file_sha256(&exe.display().to_string())?, "source_commit": env!("ORACLE_BUILD_COMMIT")});
                use std::io::Write;
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.mode(0o600);
                }
                let mut f = opts.open(&path).with_context(|| format!("inventory {}", path.display()))?;
                f.write_all(serde_json::to_string_pretty(&inv)?.as_bytes())?;
                inv.as_object_mut().map(|o| o.remove("records"));
                println!("{}", serde_json::to_string_pretty(&inv)?);
                return Ok(());
            }
            // The reranker (section 2e): the vault's anthropic capability over its socket, with a scoped token read from
            // a 0600 file (ORACLE_RERANK_TOKEN_FILE), never printed; only when a rerank arm runs.
            let wants_rerank = if arms.is_empty() { p.arms.iter().any(|a| oracle_server::scoreboard::is_rerank(a)) } else { arms.iter().any(|a| oracle_server::scoreboard::is_rerank(a)) };
            let reranker = match (&p.rerank, wants_rerank) {
                (Some(r), true) => {
                    let file = std::env::var("ORACLE_RERANK_TOKEN_FILE").map_err(|_| anyhow::anyhow!("refused: a rerank arm needs ORACLE_RERANK_TOKEN_FILE (the vault's scoped anthropic token)"))?;
                    let token = std::fs::read_to_string(&file).with_context(|| format!("rerank token {file}"))?.trim().to_string();
                    let socket = std::env::var("ORACLE_RERANK_SOCKET").map_err(|_| anyhow::anyhow!("refused: a rerank arm needs ORACLE_RERANK_SOCKET (the vault's socket)"))?;
                    Some(oracle_server::scoreboard::VaultRerank { socket, token, model: r.model.clone() })
                }
                _ => None,
            };
            let setup = oracle_server::scoreboard::Setup { gold: &questions, gold_text: &gold_text, pin: &p, pin_id: &pin, only: &arms, terms_sha: terms_sha.as_deref(), build: env!("ORACLE_BUILD_COMMIT"), sem: sem.as_deref(), snapshot: &snapshot, split: split.as_deref(), reranker: reranker.as_ref().map(|r| r as &dyn oracle_server::scoreboard::Rerank) };
            let mut receipt = oracle_server::scoreboard::run(&store, &setup)?;
            // The exact artifact: the binary's own sha256 beside its source commit (Emmy).
            let exe = std::env::current_exe()?;
            receipt["binary"] = serde_json::json!({"sha256": oracle_server::scoreboard::file_sha256(&exe.display().to_string())?, "source_commit": env!("ORACLE_BUILD_COMMIT")});
            let text = serde_json::to_string_pretty(&receipt)?;
            if let Some(path) = out {
                std::fs::write(&path, &text).with_context(|| format!("write {}", path.display()))?;
            }
            println!("{text}");
        }
        Cmd::Stats => println!(
            "{}",
            serde_json::to_string_pretty(&store.stats().map_err(err)?)?
        ),
    }
    Ok(())
}

/// oracle-sync/1 in the background (docs/SYNC.md): when ORACLE_SYNC_PEER, ORACLE_SYNC_PEER_NAME, ORACLE_NODE and
/// ORACLE_SYNC_TOKEN_FILE are set, one round with the peer every ORACLE_SYNC_EVERY_S seconds (default 5), on its own store
/// handle. A failed round is logged and tried again; it never stops the server.
fn spawn_sync(db: String) {
    let (Ok(peer), Ok(peer_name), Ok(node), Ok(token_file)) = (
        std::env::var("ORACLE_SYNC_PEER"),
        std::env::var("ORACLE_SYNC_PEER_NAME"),
        std::env::var("ORACLE_NODE"),
        std::env::var("ORACLE_SYNC_TOKEN_FILE"),
    ) else {
        return;
    };
    let every = std::env::var("ORACLE_SYNC_EVERY_S").ok().and_then(|v| v.parse().ok()).unwrap_or(5u64);
    std::thread::spawn(move || {
        let store = match oracle_server::store::Store::open(&db) {
            Ok(s) => s,
            Err(e) => return eprintln!("sync: cannot open the store: {e}"),
        };
        let token = match std::fs::read_to_string(&token_file) {
            Ok(t) => t,
            Err(e) => return eprintln!("sync: cannot read {token_file}: {e}"),
        };
        let p = oracle_server::sync::HttpPeer::new(&peer, &token);
        eprintln!("sync: node {node} with {peer_name} at {peer}, every {every} s");
        loop {
            match oracle_server::sync::round(&store, &node, &peer_name, &p, oracle_server::store::now_ms_pub()) {
                Ok(r) if r.keys + r.pulled.added + r.pushed.added > 0 || !r.pulled.quarantined.is_empty() || !r.pulled.refused.is_empty() => {
                    eprintln!("sync: keys {} pulled +{} (dup {}, set aside {}) pushed +{} (dup {})", r.keys, r.pulled.added, r.pulled.duplicate,
                        r.pulled.quarantined.len() + r.pulled.refused.len(), r.pushed.added, r.pushed.duplicate);
                }
                Ok(_) => {}
                Err(e) => eprintln!("sync: round failed: {e}"),
            }
            std::thread::sleep(std::time::Duration::from_secs(every));
        }
    });
}
