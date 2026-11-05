//! `oracle up`: a fresh machine to a working Oracle with your agent connected, in one command (docs/running.md "One command").
//!
//! It does what a new user did by hand in six steps (init, serve, seat add, ox setup, ox doctor, `claude mcp add`): makes the Oracle
//! and your owner key if there is none, gives your agent a seat, runs the server as a per-user service (launchd on macOS, systemd
//! --user on Linux) so it survives a reboot, points `ox` at it, finds the agents installed here and, after showing the plan and asking,
//! connects each of them over MCP. It never needs root.
//!
//! The plan is made from a read-only look at the machine, before anything is created or opened for writing: `--plan` and a declined
//! prompt leave every byte as it was (Emmy's review of 5005a36). What is already there is kept: an `ox` home that holds a seat, a
//! service another Oracle registered, an agent's existing "oracle" server. A conflict stops the plan and says so; nothing is replaced.
//! A failed agent step fails the run, and `--agents-only` retries the agent steps on the seat already made.

use anyhow::{bail, Context, Result};
use oracle_proto::{keyfile, pubkey_hex, KeyAction, KeyEvent};
use oracle_server::http::now_ms;
use oracle_server::store::{Scope, Store};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Up {
    pub owner: Option<String>,
    pub seat: String,
    pub bind: String,
    pub agents: Vec<String>,
    pub yes: bool,
    pub plan: bool,
    pub no_service: bool,
    pub ox_home: Option<PathBuf>,
    pub agents_only: bool,
    /// Capture sessions into memories (hooks on compact and end). Off unless asked (Blaze, 5 Nov 2026: 'Capture off until
    /// explicitly enabled').
    pub capture: bool,
}

use oracle_server::naming::handle_ok;

/// The login name, made into a handle: lower case, letters, digits, '-' and '_' only.
fn default_owner() -> String {
    let raw = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "owner".into()).to_lowercase();
    let h: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    if handle_ok(&h) { h } else { "owner".into() }
}

fn on_path(bin: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ""] } else { &[""] };
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p).find_map(|d| exts.iter().map(|e| d.join(format!("{bin}{e}"))).find(|f| f.is_file()))
    })
}

/// `ox` beside this binary (the installer puts both in one folder), else the one on PATH.
fn ox_bin() -> Option<PathBuf> {
    let name = if cfg!(windows) { "ox.exe" } else { "ox" };
    std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(name))).filter(|p| p.is_file()).or_else(|| on_path("ox"))
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).context("no HOME directory")
}

/// One word for a POSIX shell: as is when it is plain, else in single quotes ('it''s' is written 'it'\''s').
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+:=@,".contains(&b)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// One argument of a systemd ExecStart line: double-quoted, with '\' and '"' escaped and the specifier '%' and variable '$' doubled.
pub fn systemd_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$"))
}

/// Text inside a plist <string>.
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// A file read for merging: absent is empty, anything else that fails to read stops the plan (a permission error must never
/// turn into an empty config that is then written over the real one).
fn read_json_or_empty(file: &Path) -> Result<serde_json::Value> {
    match std::fs::read_to_string(file) {
        Ok(t) => {
            let v: serde_json::Value = serde_json::from_str(&t).with_context(|| format!("{} is not JSON; not touched", file.display()))?;
            if !v.is_object() {
                bail!("{} is not a JSON object; not touched", file.display());
            }
            Ok(v)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e).with_context(|| format!("cannot read {}; not touched", file.display())),
    }
}

fn read_or_absent(file: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(file) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", file.display())),
    }
}

/// What the Oracle home already holds, read without creating, opening for writing or migrating anything.
#[derive(Debug, Default, PartialEq)]
pub struct Existing {
    pub owner: Option<String>,
    pub seat_exists: bool,
}

pub fn inspect(db: &Path, seat: &str) -> Result<Existing> {
    if !db.is_file() {
        return Ok(Existing::default());
    }
    use rusqlite::{Connection, OpenFlags};
    // A read-only open of a WAL database still creates its -wal and -shm files. With no -wal there is no writer, so the file is
    // opened immutable and nothing is created; with one, a server is (or was) writing and the files it needs are already there.
    let wal = PathBuf::from(format!("{}-wal", db.display()));
    let path: String = db.display().to_string().chars().map(|c| match c { '%' => "%25".into(), '?' => "%3f".into(), '#' => "%23".into(), c => c.to_string() }).collect();
    let uri = format!("file:{path}?mode=ro{}", if wal.exists() { "" } else { "&immutable=1" });
    let c = Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("cannot read {}", db.display()))?;
    let has_keys: bool = c.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='keys'", [], |r| r.get::<_, i64>(0))? > 0;
    if !has_keys {
        return Ok(Existing::default());
    }
    let owner = c
        .query_row("SELECT handle FROM keys WHERE role='owner' AND revoked_ms IS NULL ORDER BY created_ms LIMIT 1", [], |r| r.get(0))
        .ok();
    let seat_exists = c.query_row("SELECT count(*) FROM keys WHERE handle=?1 AND revoked_ms IS NULL", [seat], |r| r.get::<_, i64>(0))? > 0;
    Ok(Existing { owner, seat_exists })
}

/// One step of the plan: what it does, said plainly, and the action (None: already in place, nothing to do).
struct Step {
    say: String,
    run: Option<Box<dyn FnOnce() -> Result<()>>>,
}

/// Whether `<bin> mcp get oracle` finds a server of that name already.
fn has_mcp_oracle(bin: &Path) -> bool {
    Command::new(bin).args(["mcp", "get", "oracle"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// The agents found here, and the command that connects each to `ox mcp`. Existing entries are kept, never replaced.
fn agent_steps(wanted: &[String], ox: &Path, ox_home: &Path, capture_on: bool) -> Result<Vec<Step>> {
    let all = wanted.is_empty() || wanted.iter().any(|a| a == "auto");
    let want = |a: &str| all || wanted.iter().any(|w| w == a);
    let mut out: Vec<Step> = Vec::new();
    if wanted.iter().any(|a| a == "none") {
        return Ok(out);
    }
    let ox_s = ox.display().to_string();
    let home_s = ox_home.display().to_string();
    for (agent, bin) in [("claude-code", "claude"), ("codex", "codex")] {
        if !want(agent) {
            continue;
        }
        match on_path(bin) {
            Some(b) if has_mcp_oracle(&b) => out.push(Step {
                say: format!("{agent} already has an MCP server named \"oracle\"; kept as it is (`{bin} mcp get oracle` shows it)"),
                run: None,
            }),
            Some(b) => {
                let (ox_s, home_s) = (ox_s.clone(), home_s.clone());
                out.push(Step {
                    say: format!("connect {agent}: {bin} mcp add oracle -- {} --home {} mcp", sh_quote(&ox_s), sh_quote(&home_s)),
                    run: Some(Box::new(move || {
                        let st = Command::new(b).args(["mcp", "add", "oracle", "--", &ox_s, "--home", &home_s, "mcp"]).status()?;
                        if !st.success() {
                            bail!("{agent}: `mcp add` exited {st}");
                        }
                        Ok(())
                    })),
                });
            }
            None if !all => bail!("{agent} was asked for, but `{bin}` is not on PATH"),
            None => {}
        }
    }
    // Each agent wakes with the brief and is captured when a session compacts or ends (docs/CAPTURE.md): a hook in the agent's
    // own file and shape. A start hook's output is added to the agent's context, so every session starts knowing what changed,
    // what waits and what it owes; `ox capture --hook` spools the session and returns at once, and the memories it finds are
    // signed by this seat. Hooks already there are kept; only the missing ones are added.
    let brief = format!("{} --home {} brief", sh_quote(&ox_s), sh_quote(&home_s));
    let capture = |agent: &str| format!("{} --home {} capture --hook {agent}", sh_quote(&ox_s), sh_quote(&home_s));
    let cursor_dir = home_dir()?.join(".cursor");
    // Capture is off until the owner turns it on (Blaze, 5 Nov 2026, Emmy's card): by default
    // an agent only wakes with the brief; `oracle up --capture` adds the compact and end hooks.
    let keep = |v: Vec<(&'static str, String)>| -> Vec<(&'static str, String)> { v.into_iter().enumerate().filter(|(i, _)| *i == 0 || capture_on).map(|(_, x)| x).collect() };
    let plans: [HookPlan; 3] = [
        ("claude-code", on_path("claude").is_some(), home_dir()?.join(".claude/settings.json"), HookShape::Grouped,
            keep(vec![("SessionStart", brief.clone()), ("PreCompact", capture("claude-code")), ("SessionEnd", capture("claude-code"))])),
        ("codex", on_path("codex").is_some(), home_dir()?.join(".codex/hooks.json"), HookShape::Grouped,
            keep(vec![("SessionStart", brief.clone()), ("PreCompact", capture("codex")), ("SessionEnd", capture("codex"))])),
        ("cursor", cursor_dir.is_dir(), cursor_dir.join("hooks.json"), HookShape::Flat,
            keep(vec![("sessionStart", format!("{brief} --hook cursor")), ("preCompact", capture("cursor")), ("sessionEnd", capture("cursor"))])),
    ];
    for (agent, found, file, shape, wants) in plans {
        if !want(agent) || !found {
            continue;
        }
        let mut probe = read_json_or_empty(&file)?;
        let missing = add_hooks(&mut probe, shape, &wants).with_context(|| format!("{} not touched", file.display()))?;
        if missing.is_empty() {
            out.push(Step { say: format!("{agent} already wakes with the brief{} ({}); kept", if capture_on { " and is captured" } else { "" }, file.display()), run: None });
            continue;
        }
        let trust = if agent == "codex" { "; Codex asks you to trust new hooks once (/hooks)" } else { "" };
        out.push(Step {
            say: format!("{agent} hooks in {}: {}{trust}", file.display(), missing.join(", ")),
            run: Some(Box::new(move || {
                // Read again at the moment of writing: the file may have changed since the plan.
                let mut v = read_json_or_empty(&file)?;
                add_hooks(&mut v, shape, &wants).with_context(|| format!("{} not touched", file.display()))?;
                std::fs::create_dir_all(file.parent().unwrap())?;
                std::fs::write(&file, serde_json::to_string_pretty(&v)? + "\n")?;
                Ok(())
            })),
        });
    }
    if want("cursor") {
        let dir = home_dir()?.join(".cursor");
        if dir.is_dir() || !all {
            let file = dir.join("mcp.json");
            let entry = serde_json::json!({"command": ox_s, "args": ["--home", home_s, "mcp"]});
            let v = read_json_or_empty(&file)?;
            match v.get("mcpServers").and_then(|s| s.get("oracle")) {
                Some(e) if *e == entry => out.push(Step { say: format!("cursor is already connected ({}); kept", file.display()), run: None }),
                // Only the path and the server's name: the entry may carry headers or env tokens, and an error is often pasted.
                Some(_) => bail!(
                    "{} already has an MCP server named \"oracle\" that points elsewhere; `oracle up` never replaces it. Remove it yourself, or connect cursor by hand: docs/mcp.md",
                    file.display()
                ),
                None => out.push(Step {
                    say: format!("connect cursor: add \"oracle\" to {}", file.display()),
                    run: Some(Box::new(move || {
                        let mut v = read_json_or_empty(&file)?;
                        let servers = v.as_object_mut().unwrap().entry("mcpServers").or_insert_with(|| serde_json::json!({}));
                        let servers = servers.as_object_mut().context("cursor 'mcpServers' is not an object; not touched")?;
                        if servers.contains_key("oracle") {
                            bail!("{} gained an \"oracle\" server since the plan; not replaced", file.display());
                        }
                        servers.insert("oracle".into(), entry);
                        std::fs::create_dir_all(file.parent().unwrap())?;
                        std::fs::write(&file, serde_json::to_string_pretty(&v)? + "\n")?;
                        Ok(())
                    })),
                }),
            }
        }
    }
    Ok(out)
}

/// How an agent's hook file nests its commands: Claude Code and Codex group them (`{"hooks": {Event: [{"hooks": [{"type":
/// "command", "command": …}]}]}}`), Cursor lists them (`{"version": 1, "hooks": {event: [{"command": …}]}}`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HookShape {
    Grouped,
    Flat,
}

/// One agent's hooks: its name, whether it is here, its hook file, the file's shape, and (event, command) pairs.
type HookPlan<'a> = (&'a str, bool, PathBuf, HookShape, Vec<(&'a str, String)>);

/// Add each (event, command) the file does not already run, keeping every other hook; the events added, in order. A file
/// whose `hooks` is not an object, or an event that is not a list, is refused and left as it is.
pub fn add_hooks(v: &mut serde_json::Value, shape: HookShape, wants: &[(&str, String)]) -> Result<Vec<String>> {
    if !v.is_object() {
        bail!("the file is not a JSON object");
    }
    let root = v.as_object_mut().unwrap();
    if shape == HookShape::Flat {
        root.entry("version").or_insert_with(|| serde_json::json!(1));
    }
    let hooks = root.entry("hooks").or_insert_with(|| serde_json::json!({})).as_object_mut().context("'hooks' is not an object")?;
    let mut added = Vec::new();
    for (event, cmd) in wants {
        let list = hooks.entry(*event).or_insert_with(|| serde_json::json!([])).as_array_mut()
            .with_context(|| format!("'hooks.{event}' is not a list"))?;
        let runs = |e: &serde_json::Value| match shape {
            HookShape::Flat => e["command"].as_str() == Some(cmd.as_str()),
            HookShape::Grouped => e["hooks"].as_array().is_some_and(|h| h.iter().any(|x| x["command"].as_str() == Some(cmd.as_str()))),
        };
        if list.iter().any(runs) {
            continue;
        }
        list.push(match shape {
            HookShape::Flat => serde_json::json!({"command": cmd}),
            HookShape::Grouped => serde_json::json!({"hooks": [{"type": "command", "command": cmd}]}),
        });
        added.push(event.to_string());
    }
    Ok(added)
}

/// The launchd plist and systemd unit `oracle up` writes, so the plan can compare them with what is already registered.
pub fn plist_body(exe: &str, home: &str, bind: &str, log: &str) -> String {
    let (exe, home, bind, log) = (xml_escape(exe), xml_escape(home), xml_escape(bind), xml_escape(log));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>world.axxis.oracle</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>--home</string><string>{home}</string><string>serve</string><string>--bind</string><string>{bind}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#
    )
}

pub fn unit_body(exe: &str, home: &str, bind: &str) -> String {
    let line = [exe, "--home", home, "serve", "--bind", bind].iter().map(|a| systemd_quote(a)).collect::<Vec<_>>().join(" ");
    format!("[Unit]\nDescription=Oracle (oracle up)\n\n[Service]\nExecStart={line}\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n")
}

/// The per-user service that keeps `oracle serve` running across reboots, or None where this OS has no supported one yet.
/// A service file already there is kept when it is this one, and stops the plan when it is another Oracle's.
fn service_step(home: &Path, bind: &str) -> Result<Option<Step>> {
    let exe = std::env::current_exe()?.display().to_string();
    let home_s = home.display().to_string();
    let refuse = |file: &Path| -> anyhow::Error {
        anyhow::anyhow!(
            "{} already runs an Oracle (another home, bind or binary); `oracle up` never replaces it. Use --no-service, or remove that service yourself",
            file.display()
        )
    };
    if cfg!(target_os = "macos") {
        let plist = home_dir()?.join("Library/LaunchAgents/world.axxis.oracle.plist");
        let body = plist_body(&exe, &home_s, bind, &home.join("serve.log").display().to_string());
        let uid = String::from_utf8(Command::new("id").arg("-u").output()?.stdout)?.trim().to_string();
        let target = format!("gui/{uid}");
        let loaded = Command::new("launchctl").args(["print", &format!("{target}/world.axxis.oracle")]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
        match read_or_absent(&plist)? {
            Some(t) if t != body => return Err(refuse(&plist)),
            Some(_) if loaded => return Ok(Some(Step { say: format!("the server already runs at login: {} (launchd); kept", plist.display()), run: None })),
            Some(_) => {}
            None if loaded => bail!("launchd already runs world.axxis.oracle from somewhere other than {}; not replaced. Use --no-service", plist.display()),
            None => {}
        }
        return Ok(Some(Step {
            say: format!("run the server at login: {} (launchd)", plist.display()),
            run: Some(Box::new(move || {
                std::fs::create_dir_all(plist.parent().unwrap())?;
                std::fs::write(&plist, body)?;
                let st = Command::new("launchctl").args(["bootstrap", &target, &plist.display().to_string()]).status()?;
                if !st.success() {
                    bail!("launchctl bootstrap exited {st}");
                }
                Ok(())
            })),
        }));
    }
    if cfg!(target_os = "linux") {
        let unit = home_dir()?.join(".config/systemd/user/oracle.service");
        let body = unit_body(&exe, &home_s, bind);
        let same = match read_or_absent(&unit)? {
            Some(t) if t != body => return Err(refuse(&unit)),
            Some(_) => true,
            None => false,
        };
        return Ok(Some(Step {
            say: if same {
                format!("the server runs at login: {} (systemd --user, already written; enabled and started)", unit.display())
            } else {
                format!("run the server at login: {} (systemd --user)", unit.display())
            },
            run: Some(Box::new(move || {
                if !same {
                    std::fs::create_dir_all(unit.parent().unwrap())?;
                    std::fs::write(&unit, body)?;
                }
                for args in [&["--user", "daemon-reload"][..], &["--user", "enable", "--now", "oracle.service"][..]] {
                    let st = Command::new("systemctl").args(args).status()?;
                    if !st.success() {
                        bail!("systemctl {} exited {st}", args.join(" "));
                    }
                }
                Ok(())
            })),
        }));
    }
    Ok(None)
}

fn healthy(bind: &str, secs: u64) -> bool {
    let url = format!("http://{bind}/health");
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(3)).build();
    (0..secs).any(|_| {
        let ok = agent.get(&url).call().map(|r| r.status() == 200).unwrap_or(false);
        if !ok {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        ok
    })
}

fn ask(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    print!("Go ahead? [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().eq_ignore_ascii_case("y") || line.trim().eq_ignore_ascii_case("yes"))
}

/// Runs the agent steps and says which failed, with the command that retries only those.
fn connect(agents: Vec<Step>, wanted: &[String], ox: &Path, ox_home: &Path) -> Result<()> {
    let mut failed = Vec::new();
    for s in agents {
        let Some(run) = s.run else { continue };
        match run() {
            Ok(()) => println!("done     {}", s.say),
            Err(e) => {
                println!("FAILED   {}: {e}", s.say);
                failed.push(s.say);
            }
        }
    }
    let st = Command::new(ox).args(["--home", &ox_home.display().to_string(), "doctor"]).status()?;
    if !failed.is_empty() {
        let list = if wanted.is_empty() { "auto".to_string() } else { wanted.join(",") };
        bail!(
            "{} agent step(s) failed (above). The seat and server are up; retry only the agents: oracle up --agents-only --agents {list} --ox-home {}",
            failed.len(),
            sh_quote(&ox_home.display().to_string())
        );
    }
    if !st.success() {
        bail!("ox doctor did not pass; the steps above say where it stopped");
    }
    Ok(())
}

pub fn run(home: &Path, db: &Path, key_path: impl Fn(&str) -> PathBuf, a: Up) -> Result<()> {
    let owner = a.owner.clone().unwrap_or_else(default_owner);
    if !handle_ok(&owner) || !handle_ok(&a.seat) {
        bail!("handles are lower case letters, digits, '-' and '_' (owner {owner:?}, seat {:?})", a.seat);
    }
    let ox = ox_bin().context("`ox` was not found beside `oracle` or on PATH; install both (scripts/install.sh)")?;
    let ox_home = match &a.ox_home {
        Some(h) => h.clone(),
        None => std::env::var_os("OX_HOME").map(PathBuf::from).unwrap_or(home_dir()?.join(".oracle")),
    };
    let cfg = ox_home.join("config.json");

    // The retry path: the seat exists, only the agents are connected again.
    if a.agents_only {
        if !cfg.is_file() {
            bail!("--agents-only connects agents to a seat `oracle up` already made, and {} holds none", cfg.display());
        }
        let agents = agent_steps(&a.agents, &ox, &ox_home, a.capture)?;
        println!("oracle up --agents-only: here is the plan");
        for s in &agents {
            println!("  agent         {}", s.say);
        }
        if a.plan {
            println!("(--plan: nothing was changed)");
            return Ok(());
        }
        if !ask(a.yes)? {
            println!("nothing changed");
            return Ok(());
        }
        connect(agents, &a.agents, &ox, &ox_home)?;
        println!("Agents connected.");
        return Ok(());
    }

    if cfg.exists() {
        bail!(
            "{} already holds a seat; `oracle up` never overwrites one. Use --ox-home <new folder> for another seat on this machine, or --agents-only to connect agents to it",
            cfg.display()
        );
    }
    let found = inspect(db, &a.seat)?;

    println!("oracle up: here is the plan");
    println!("  oracle home   {}", home.display());
    match &found.owner {
        Some(o) => println!("  owner         {o} (already set up; nothing changes)"),
        None => println!("  owner         {owner}: a new root key, kept in {}", key_path(&owner).display()),
    }
    println!("  agent seat    {}{}", a.seat, if found.seat_exists { " (exists; a new token is minted)" } else { ": a new key and token" });
    println!("  ox            {} -> {}", ox_home.display(), a.bind);
    let svc = if a.no_service { None } else { service_step(home, &a.bind)? };
    match &svc {
        Some(s) => println!("  server        {}", s.say),
        None if a.no_service => println!("  server        not installed as a service (--no-service): run `oracle --home {} serve --bind {}`", sh_quote(&home.display().to_string()), a.bind),
        None => println!("  server        no per-user service on this OS yet: run `oracle --home {} serve --bind {}` (it stays up while that runs)", sh_quote(&home.display().to_string()), a.bind),
    }
    let agents = agent_steps(&a.agents, &ox, &ox_home, a.capture)?;
    if agents.is_empty() {
        println!("  agents        none found (Claude Code, Codex, Cursor); connect any later: docs/mcp.md");
    }
    for s in &agents {
        println!("  agent         {}", s.say);
    }
    if a.plan {
        println!("(--plan: nothing was changed)");
        return Ok(());
    }
    if !ask(a.yes)? {
        println!("nothing changed");
        return Ok(());
    }

    // Only now is anything created: the home, the database and its schema.
    std::fs::create_dir_all(home).with_context(|| format!("cannot create {}", home.display()))?;
    let store = Store::open(&db.display().to_string()).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    // 1. The owner.
    let owner = match found.owner {
        Some(o) => o,
        None => {
            let sk = oracle_proto::generate_key();
            keyfile::create(&key_path(&owner), &sk)?;
            store.init_owner(&owner, &pubkey_hex(&sk), now_ms()).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!("owner    {owner}  (root key {}: back it up)", key_path(&owner).display());
            owner
        }
    };
    // 2. The agent's seat: its key lives in the ox home, never beside the owner's.
    std::fs::create_dir_all(&ox_home)?;
    let seat_key = ox_home.join(format!("{}.key", a.seat));
    if !found.seat_exists {
        let owner_sk = keyfile::load(&key_path(&owner))?;
        let sk = oracle_proto::generate_key();
        keyfile::create(&seat_key, &sk)?;
        let ev = KeyEvent::sign(&owner_sk, &owner, KeyAction::Add, &a.seat, &pubkey_hex(&sk), now_ms());
        store.apply_key_event(&ev, now_ms()).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    } else if !seat_key.is_file() {
        bail!("seat {} exists but its key is not at {}; pick another --seat", a.seat, seat_key.display());
    }
    let token = store.mint_token(&a.seat, Scope::Write, "oracle-up", now_ms()).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    drop(store);
    println!("seat     {}", a.seat);
    // 3. The server.
    if let Some(Step { run: Some(run), .. }) = svc {
        run()?;
    }
    if !healthy(&a.bind, 60) {
        bail!(
            "the server is not answering at http://{}/health yet; start it (`oracle --home {} serve --bind {}`), then connect the rest with --agents-only",
            a.bind,
            sh_quote(&home.display().to_string()),
            a.bind
        );
    }
    println!("server   http://{} answers", a.bind);
    // 4. ox, with the token passed in its environment, never on a command line another user could read.
    let st = Command::new(&ox)
        .args(["--home", &ox_home.display().to_string(), "setup", "--url", &format!("http://{}", a.bind), "--handle", &a.seat, "--key", &seat_key.display().to_string()])
        .env("ORACLE_TOKEN", &token)
        .status()?;
    if !st.success() {
        bail!("ox setup exited {st}");
    }
    // 5. The agents, then 6. the proof (ox doctor).
    connect(agents, &a.agents, &ox, &ox_home)?;
    println!("Oracle is up. A new Claude Code, Codex or Cursor session wakes with the brief, and what it decides and learns is captured when it compacts or ends (docs/CAPTURE.md; Codex asks you to trust the hooks once with /hooks). Any other agent: ask it to call oracle_brief.");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn capture_hooks_are_added_only_when_asked() {
        // Blaze, 5 Nov 2026: capture off until explicitly enabled. The start hook alone by default; with --capture, all three.
        let keep = |on: bool, v: Vec<(&'static str, String)>| -> Vec<&'static str> { v.into_iter().enumerate().filter(|(i, _)| *i == 0 || on).map(|(_, x)| x.0).collect() };
        let v = || vec![("SessionStart", "b".to_string()), ("PreCompact", "c".to_string()), ("SessionEnd", "c".to_string())];
        assert_eq!(keep(false, v()), ["SessionStart"]);
        assert_eq!(keep(true, v()), ["SessionStart", "PreCompact", "SessionEnd"]);
    }

    use super::*;

    fn sha(p: &Path) -> String {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(std::fs::read(p).unwrap()))
    }

    #[test]
    fn the_plan_reads_a_missing_home_without_creating_it() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("node");
        assert_eq!(inspect(&home.join("oracle.db"), "agent").unwrap(), Existing::default());
        assert!(!home.exists(), "inspect must not create the home");
    }

    #[test]
    fn the_plan_reads_an_existing_database_and_leaves_every_byte() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("oracle.db");
        {
            let s = Store::open(&db.display().to_string()).unwrap();
            let sk = oracle_proto::generate_key();
            s.init_owner("grace", &pubkey_hex(&sk), 1).unwrap();
            s.apply_key_event(&KeyEvent::sign(&sk, "grace", KeyAction::Add, "agent", &pubkey_hex(&oracle_proto::generate_key()), 2), 2).unwrap();
        }
        let files = |d: &Path| {
            let mut v: Vec<(String, String)> =
                std::fs::read_dir(d).unwrap().map(|e| e.unwrap().path()).map(|p| (p.display().to_string(), sha(&p))).collect();
            v.sort();
            v
        };
        let before = files(d.path());
        let got = inspect(&db, "agent").unwrap();
        assert_eq!(got, Existing { owner: Some("grace".into()), seat_exists: true });
        assert!(!inspect(&db, "other").unwrap().seat_exists);
        assert_eq!(files(d.path()), before, "inspect changed the database files");
    }

    #[test]
    fn the_plan_reads_a_running_servers_database_without_changing_its_data() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("odd ?#% name.db");
        let s = Store::open(&db.display().to_string()).unwrap(); // held open: its -wal exists, as under a running server
        s.init_owner("grace", &pubkey_hex(&oracle_proto::generate_key()), 1).unwrap();
        let wal = PathBuf::from(format!("{}-wal", db.display()));
        assert!(wal.exists());
        let (db0, wal0) = (sha(&db), sha(&wal));
        assert_eq!(inspect(&db, "agent").unwrap(), Existing { owner: Some("grace".into()), seat_exists: false });
        assert_eq!((sha(&db), sha(&wal)), (db0, wal0), "inspect changed the data of a live database");
    }

    #[test]
    fn shell_words_survive_spaces_apostrophes_and_ampersands() {
        assert_eq!(sh_quote("/usr/local/bin/ox"), "/usr/local/bin/ox");
        assert_eq!(sh_quote("/Users/Ada Lovelace/.oracle"), "'/Users/Ada Lovelace/.oracle'");
        assert_eq!(sh_quote("/home/o'brien & co/ox"), r"'/home/o'\''brien & co/ox'");
        // The quoted word is one argument to a real shell, byte for byte.
        let weird = "/tmp/a b/o'brien & $HOME `x`";
        let out = Command::new("sh").args(["-c", &format!("printf %s {}", sh_quote(weird))]).output().unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), weird);
    }

    #[test]
    fn unit_and_plist_values_are_escaped() {
        let u = unit_body("/opt/my bin/oracle", "/home/o'brien/100%$HOME", "127.0.0.1:8900");
        assert!(u.contains(r#"ExecStart="/opt/my bin/oracle" "--home" "/home/o'brien/100%%$$HOME" "serve""#), "{u}");
        let p = plist_body("/a&b/oracle", "/x<y>/\"q\"", "127.0.0.1:8900", "/l'og");
        assert!(p.contains("<string>/a&amp;b/oracle</string>"), "{p}");
        assert!(p.contains("<string>/x&lt;y&gt;/&quot;q&quot;</string>"), "{p}");
        assert!(p.contains("<string>/l&apos;og</string>"), "{p}");
        assert!(!p.contains("/a&b"), "a raw ampersand makes the plist invalid XML");
    }

    #[test]
    fn hooks_are_added_once_in_each_agent_s_shape_and_others_are_kept() {
        let wants = vec![("SessionStart", "ox brief".to_string()), ("SessionEnd", "ox capture --hook codex".to_string())];
        let mut v = serde_json::json!({"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "theirs"}]}]}, "model": "x"});
        assert_eq!(add_hooks(&mut v, HookShape::Grouped, &wants).unwrap(), ["SessionStart", "SessionEnd"]);
        assert_eq!(v["hooks"]["SessionStart"][0]["hooks"][0]["command"], "theirs", "a hook already there is kept");
        assert_eq!(v["hooks"]["SessionStart"][1]["hooks"][0]["command"], "ox brief");
        assert_eq!(v["model"], "x");
        assert!(add_hooks(&mut v, HookShape::Grouped, &wants).unwrap().is_empty(), "a second run adds nothing");
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        let mut c = serde_json::json!({});
        let flat = vec![("sessionStart", "ox brief --hook cursor".to_string())];
        assert_eq!(add_hooks(&mut c, HookShape::Flat, &flat).unwrap(), ["sessionStart"]);
        assert_eq!(c, serde_json::json!({"version": 1, "hooks": {"sessionStart": [{"command": "ox brief --hook cursor"}]}}));
        for bad in [serde_json::json!({"hooks": []}), serde_json::json!({"hooks": {"SessionStart": {}}}), serde_json::json!([])] {
            let before = bad.clone();
            let mut b = bad;
            assert!(add_hooks(&mut b, HookShape::Grouped, &wants).is_err(), "{before} is refused");
        }
    }

    #[test]
    fn a_settings_file_that_cannot_be_read_is_never_treated_as_empty() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(read_json_or_empty(&d.path().join("absent.json")).unwrap(), serde_json::json!({}));
        // A directory where the file should be: a read error that is not NotFound.
        let dir = d.path().join("settings.json");
        std::fs::create_dir(&dir).unwrap();
        assert!(read_json_or_empty(&dir).is_err());
        std::fs::write(d.path().join("bad.json"), "{not json").unwrap();
        assert!(read_json_or_empty(&d.path().join("bad.json")).is_err());
    }
}
