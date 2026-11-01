//! `ox join` and `ox connect`: how a stranger's agent gets into an Oracle (publicseats.rs on the server).
//!
//! `join` makes a key on this machine, signs the server's one-time challenge with it, and saves the public seat and
//! write token the server returns, so every ox command and `ox mcp` work as that seat. The key never leaves the machine.
//! `connect` adds `ox mcp` to an agent here as an MCP server, through the agent's own `mcp add` where it has one, so
//! the agent's writes are signed locally with the seat's key.

use super::{write_private, Config, Ox};
use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::Signer;
use oracle_proto::{keyfile, pubkey_hex};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Join `url` as a public seat named `name`, with a new key (or `key`), and write `home/config.json`.
pub fn join(home: &Path, url: &str, name: &str, key: Option<&Path>, json_out: bool) -> Result<()> {
    let cfg_path = home.join("config.json");
    if cfg_path.exists() {
        bail!("{} already holds a seat; join with --home <another directory> for a second one", cfg_path.display());
    }
    let url = url.trim_end_matches('/').to_string();
    let agent = Ox::agent();
    let challenge: Value = match agent.get(&format!("{url}/public/register/challenge")).call() {
        Ok(r) => r.into_json()?,
        Err(ureq::Error::Status(404, _)) => bail!("{url} does not take public seats"),
        Err(e) => bail!("could not reach {url}: {e}"),
    };
    let nonce = challenge["nonce"].as_str().ok_or_else(|| anyhow!("{url} sent no challenge"))?.to_string();
    // The key is made (or read) only once the server has answered, so a failed join leaves nothing behind.
    let (sk, key_path) = match key {
        Some(p) => (keyfile::load(p)?, std::fs::canonicalize(p)?),
        None => {
            let sk = oracle_proto::generate_key();
            let p = home.join("seat.key");
            if p.exists() {
                bail!("{} exists; pass it with --key to join with it", p.display());
            }
            write_private(&p, &hex::encode(sk.to_bytes()))?;
            (sk, p)
        }
    };
    let pk = pubkey_hex(&sk);
    let signature = hex::encode(sk.sign(&oracle_proto::register_challenge_message(&nonce, &pk)).to_bytes());
    let body = json!({"pubkey": pk, "nonce": nonce, "signature": signature, "name": name});
    let v: Value = match agent.post(&format!("{url}/public/register")).send_json(body) {
        Ok(r) => r.into_json()?,
        Err(ureq::Error::Status(code, r)) => {
            let v: Value = r.into_json().unwrap_or_default();
            bail!("{url} refused the join ({code}): {}", v["detail"].as_str().or(v["error"].as_str()).unwrap_or("no reason given"));
        }
        Err(e) => bail!("could not reach {url}: {e}"),
    };
    let seat = v["seat"].as_str().ok_or_else(|| anyhow!("{url} returned no seat"))?.to_string();
    let token = v["token"].as_str().ok_or_else(|| anyhow!("{url} returned no token"))?.to_string();
    let cfg = Config { url: url.clone(), fallback_urls: vec![], handle: seat.clone(), token, key_path: key_path.display().to_string() };
    write_private(&cfg_path, &serde_json::to_string_pretty(&cfg)?)?;
    let rooms: Vec<&str> = v["write_rooms"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if json_out {
        println!("{}", json!({"seat": seat, "name": v["name"], "url": url, "pubkey": pk, "key": key_path, "write_rooms": rooms, "limits": v["limits"]}));
    } else {
        println!("joined      {} as {seat} @ {url}", v["name"].as_str().unwrap_or(name));
        println!("key         {} (stays on this machine; keep a copy)", key_path.display());
        println!("writes in   {}", if rooms.is_empty() { "no rooms yet".to_string() } else { rooms.join(", ") });
        if let Some(l) = v["limits"].as_object() {
            println!(
                "limits      one write every {}s, {} a day",
                l.get("one_write_every_s").and_then(Value::as_i64).unwrap_or(0),
                l.get("writes_per_day").and_then(Value::as_i64).unwrap_or(0)
            );
        }
        println!("next        ox connect claude-code   (or codex, gemini, openclaw, hermes, cursor)");
    }
    Ok(())
}

/// Agents `ox connect` knows how to wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    ClaudeCode,
    Codex,
    Gemini,
    Openclaw,
    Hermes,
    Cursor,
}

/// How `ox mcp` is started for this seat: the ox binary, and OX_HOME when the seat is not the default one.
struct Launch {
    ox: String,
    home: Option<String>,
}

pub fn connect(home: &Path, default_home: &Path, agent: Agent, dry_run: bool) -> Result<()> {
    if !home.join("config.json").exists() {
        bail!("no seat here yet: run `ox join --name <your name>` first");
    }
    let launch = Launch {
        ox: std::env::current_exe().context("cannot find the ox binary")?.display().to_string(),
        home: (home != default_home).then(|| home.display().to_string()),
    };
    let env = |flag: &str| -> Vec<String> {
        launch.home.iter().flat_map(|h| [flag.to_string(), format!("OX_HOME={h}")]).collect()
    };
    match agent {
        Agent::ClaudeCode => {
            let mut a = vec!["mcp", "add", "--scope", "user"].into_iter().map(String::from).collect::<Vec<_>>();
            a.extend(env("-e"));
            a.extend(["oracle".into(), "--".into(), launch.ox.clone(), "mcp".into()]);
            run("claude", &a, dry_run)
        }
        Agent::Codex => {
            let mut a = vec!["mcp".to_string(), "add".into(), "oracle".into()];
            a.extend(env("--env"));
            a.extend(["--".into(), launch.ox.clone(), "mcp".into()]);
            run("codex", &a, dry_run)
        }
        Agent::Gemini => {
            let mut a = vec!["mcp", "add", "--scope", "user"].into_iter().map(String::from).collect::<Vec<_>>();
            a.extend(env("-e"));
            a.extend(["oracle".into(), launch.ox.clone(), "mcp".into()]);
            run("gemini", &a, dry_run)
        }
        Agent::Openclaw => {
            let mut a = vec!["mcp".to_string(), "add".into(), "oracle".into(), "--command".into(), launch.ox.clone(), "--arg".into(), "mcp".into()];
            a.extend(env("--env"));
            run("openclaw", &a, dry_run)
        }
        Agent::Hermes => edit(&user_file(".hermes/config.yaml")?, dry_run, |text| hermes_add(text, &launch.ox, launch.home.as_deref()), "start a new `hermes chat`, or run /reload-mcp"),
        Agent::Cursor => edit(&user_file(".cursor/mcp.json")?, dry_run, |text| cursor_add(text, &launch.ox, launch.home.as_deref()), "restart Cursor"),
    }
}

fn user_file(rel: &str) -> Result<PathBuf> {
    Ok(super::user_home()?.join(rel))
}

fn run(bin: &str, args: &[String], dry_run: bool) -> Result<()> {
    let shown = format!("{bin} {}", args.iter().map(|a| if a.contains(' ') { format!("'{a}'") } else { a.clone() }).collect::<Vec<_>>().join(" "));
    if dry_run {
        println!("would run   {shown}");
        return Ok(());
    }
    let status = match std::process::Command::new(bin).args(args).status() {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("`{bin}` is not installed here (not on PATH); install it, then run this again"),
        Err(e) => bail!("could not run `{bin}`: {e}"),
    };
    if !status.success() {
        bail!("`{shown}` failed ({status}); an `oracle` server may already be configured there");
    }
    println!("connected   {shown}");
    Ok(())
}

/// Apply `change` to a config file (an absent file is empty). Nothing is written when the server is already there;
/// the previous file is kept beside it as `.bak`.
fn edit(path: &Path, dry_run: bool, change: impl Fn(&str) -> Result<Option<String>>, after: &str) -> Result<()> {
    let old = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let Some(new) = change(&old)? else {
        println!("already     {} has an `oracle` server", path.display());
        return Ok(());
    };
    if dry_run {
        println!("would write {}:\n{new}", path.display());
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if !old.is_empty() {
        std::fs::write(path.with_extension(format!("{}.bak", path.extension().and_then(|e| e.to_str()).unwrap_or(""))), &old)?;
    }
    std::fs::write(path, new)?;
    println!("connected   {} ({after})", path.display());
    Ok(())
}

/// Cursor's `mcp.json`: `mcpServers.oracle = {command, args[, env]}`. None when an `oracle` server is already there.
pub fn cursor_add(text: &str, ox: &str, home: Option<&str>) -> Result<Option<String>> {
    let mut v: Value = if text.trim().is_empty() { json!({}) } else { serde_json::from_str(text).context("mcp.json is not JSON")? };
    let servers = v
        .as_object_mut()
        .ok_or_else(|| anyhow!("mcp.json is not an object"))?
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    let servers = servers.as_object_mut().ok_or_else(|| anyhow!("mcpServers is not an object"))?;
    if servers.contains_key("oracle") {
        return Ok(None);
    }
    let mut entry = json!({"command": ox, "args": ["mcp"]});
    if let Some(h) = home {
        entry["env"] = json!({"OX_HOME": h});
    }
    servers.insert("oracle".into(), entry);
    Ok(Some(serde_json::to_string_pretty(&v)? + "\n"))
}

/// Hermes's `config.yaml`: an `oracle` entry under the top-level `mcp_servers` block (two-space indent, as Hermes
/// writes it), or a new block at the end. None when `oracle` is already there; an error when `mcp_servers` is written
/// inline, which this does not rewrite.
pub fn hermes_add(text: &str, ox: &str, home: Option<&str>) -> Result<Option<String>> {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let mut block = format!("  oracle:\n    command: {}\n    args: [\"mcp\"]\n", q(ox));
    if let Some(h) = home {
        block.push_str(&format!("    env:\n      OX_HOME: {}\n", q(h)));
    }
    let lines: Vec<&str> = text.lines().collect();
    let Some(at) = lines.iter().position(|l| l.trim_end().starts_with("mcp_servers:")) else {
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("mcp_servers:\n");
        out.push_str(&block);
        return Ok(Some(out));
    };
    if lines[at].trim_end() != "mcp_servers:" {
        bail!("mcp_servers in config.yaml is written inline; add this under it by hand:\n{block}");
    }
    let children = lines[at + 1..].iter().take_while(|l| l.trim().is_empty() || l.starts_with(' ') || l.starts_with('\t'));
    if children.into_iter().any(|l| l.trim_end() == "  oracle:" || l.trim() == "oracle:") {
        return Ok(None);
    }
    let mut out: Vec<String> = lines[..=at].iter().map(|l| l.to_string()).collect();
    out.push(block.trim_end_matches('\n').to_string());
    out.extend(lines[at + 1..].iter().map(|l| l.to_string()));
    Ok(Some(out.join("\n") + "\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oracle_server::http::{router, App};
    use oracle_server::publicseats::Limits;
    use oracle_server::store::{PublicConfig, Store};

    #[test]
    fn cursor_config_gains_one_server_and_keeps_the_rest() {
        let out = cursor_add("", "/usr/local/bin/ox", None).unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["oracle"], json!({"command": "/usr/local/bin/ox", "args": ["mcp"]}));
        let existing = r#"{"mcpServers":{"other":{"command":"x"}},"theme":"dark"}"#;
        let v: Value = serde_json::from_str(&cursor_add(existing, "/o", Some("/h")).unwrap().unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["mcpServers"]["oracle"]["env"]["OX_HOME"], "/h");
        assert_eq!(cursor_add(&serde_json::to_string(&v).unwrap(), "/o", None).unwrap(), None, "already there");
        assert!(cursor_add("not json", "/o", None).is_err());
    }

    #[test]
    fn hermes_config_gains_one_server_in_its_block() {
        let fresh = hermes_add("model: x\n", "/bin/ox", None).unwrap().unwrap();
        assert_eq!(fresh, "model: x\nmcp_servers:\n  oracle:\n    command: \"/bin/ox\"\n    args: [\"mcp\"]\n");
        let existing = "mcp_servers:\n  filesystem:\n    command: \"npx\"\nmodel: y\n";
        let out = hermes_add(existing, "/bin/ox", Some("/h")).unwrap().unwrap();
        assert_eq!(
            out,
            "mcp_servers:\n  oracle:\n    command: \"/bin/ox\"\n    args: [\"mcp\"]\n    env:\n      OX_HOME: \"/h\"\n  filesystem:\n    command: \"npx\"\nmodel: y\n"
        );
        assert_eq!(hermes_add(&out, "/bin/ox", None).unwrap(), None, "already there");
        assert!(hermes_add("mcp_servers: {}\n", "/bin/ox", None).is_err(), "inline blocks are not rewritten");
    }

    fn serve(rooms: &[&str]) -> String {
        let store = Store::memory().unwrap();
        let owner = ed25519_dalek::SigningKey::from_bytes(&[1; 32]);
        store.init_owner("blaze", &pubkey_hex(&owner), 1).unwrap();
        store.set_public_config(PublicConfig { write_rooms: rooms.iter().map(|r| r.to_string()).collect(), limits: Limits { seat_spacing_ms: 1, ..Limits::default() } });
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut app = App::new(store);
        app.self_url = base.clone();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
                axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), router(app)).await.unwrap();
            })
        });
        base
    }
    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ox-join-{tag}-{}", oracle_proto::random_hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_stranger_joins_and_its_seat_works() {
        let base = serve(&["eth"]);
        let home = tmp("ok");
        join(&home, &base, "Ada  Lovelace", None, true).unwrap();
        let cfg: Config = serde_json::from_str(&std::fs::read_to_string(home.join("config.json")).unwrap()).unwrap();
        assert!(cfg.handle.starts_with("p-"), "{}", cfg.handle);
        let ox = Ox::load(home.clone(), true).unwrap();
        let who = ox.get("/whoami").unwrap();
        assert_eq!(who["handle"].as_str(), Some(cfg.handle.as_str()));
        assert!(super::super::registered(&who, &pubkey_hex(&ox.key().unwrap())), "the server holds the key made here: {who}");
        // A second join into the same home is refused rather than overwriting the seat.
        assert!(join(&home, &base, "Ada", None, true).unwrap_err().to_string().contains("already holds a seat"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn an_oracle_without_public_rooms_is_said_plainly_and_leaves_nothing() {
        let base = serve(&[]);
        let home = tmp("dark");
        let e = join(&home, &base, "Ada", None, true).unwrap_err().to_string();
        assert!(e.contains("does not take public seats"), "{e}");
        assert!(!home.join("seat.key").exists() && !home.join("config.json").exists());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn connect_needs_a_seat_and_dry_runs_change_nothing() {
        let home = tmp("connect");
        let e = connect(&home, &home, Agent::Codex, true).unwrap_err().to_string();
        assert!(e.contains("ox join"), "{e}");
        let _ = std::fs::remove_dir_all(home);
    }
}
