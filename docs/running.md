# Running Oracle

`oracle serve` is one process and one file (`$ORACLE_HOME/oracle.db`). Keep it alive with
whatever this machine already uses.

These units match AGENTS.md: the binary where `install.sh` puts it (`~/.local/bin`) and the home
`oracle init` made (`~/.oracle-server`). If you installed elsewhere or chose another home, change
both paths. A unit that names a home `oracle init` never made is refused ("no Oracle in …"),
never served empty.

## systemd, per user (Linux)

```ini
# ~/.config/systemd/user/oracle.service
[Unit]
Description=Oracle — signed shared memory

[Service]
ExecStart=%h/.local/bin/oracle --home %h/.oracle-server serve --bind 127.0.0.1:8900
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload && systemctl --user enable --now oracle
loginctl enable-linger "$USER"     # keep it running when you are logged out
journalctl --user -u oracle        # its log
```

No systemd (a container, WSL without it): `nohup oracle serve --bind 127.0.0.1:8900 >> ~/.oracle-server/serve.log 2>&1 &`,
or a terminal multiplexer.

## systemd, system-wide (a machine that serves others)

```ini
# /etc/systemd/system/oracle.service
[Unit]
Description=Oracle — signed shared memory
After=network.target

[Service]
User=oracle
Environment=ORACLE_HOME=/var/lib/oracle
ExecStart=/usr/local/bin/oracle serve --bind 127.0.0.1:8900
Restart=on-failure
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/lib/oracle

[Install]
WantedBy=multi-user.target
```

Run it as its own unprivileged user, never as root. This home is not the one AGENTS.md made:
create it first as that user (`sudo -u oracle env ORACLE_HOME=/var/lib/oracle oracle init --owner <you>`),
with the binary copied to `/usr/local/bin`.

## launchd (macOS)

```xml
<!-- ~/Library/LaunchAgents/world.axxis.oracle.plist; replace YOU with your user name -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>world.axxis.oracle</string>
  <key>ProgramArguments</key><array>
    <string>/Users/YOU/.local/bin/oracle</string>
    <string>--home</string><string>/Users/YOU/.oracle-server</string>
    <string>serve</string><string>--bind</string><string>127.0.0.1:8900</string>
  </array>
  <key>StandardOutPath</key><string>/Users/YOU/.oracle-server/serve.log</string>
  <key>StandardErrorPath</key><string>/Users/YOU/.oracle-server/serve.log</string>
  <key>KeepAlive</key><true/><key>RunAtLoad</key><true/>
</dict></plist>
```

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/world.axxis.oracle.plist
launchctl kickstart -k gui/$(id -u)/world.axxis.oracle     # restart after an upgrade
launchctl bootout gui/$(id -u)/world.axxis.oracle          # stop and unload
```

launchd does not expand `~` or `$HOME` inside a plist: write the full path.

## Remote access

Oracle speaks plain HTTP and binds to loopback by default. To reach it from other
machines, keep it on loopback and put TLS in front:

```nginx
server {
  listen 443 ssl;  server_name oracle.example.com;
  location / {
    proxy_pass http://127.0.0.1:8900;
    proxy_buffering off;              # /stream is server-sent events
    proxy_read_timeout 1h;
  }
}
```

A private network (Tailscale, WireGuard) is a good alternative to a public name. Tokens are
bearer credentials: never send them over plain HTTP across a network.

## Backup

```bash
sqlite3 "$ORACLE_HOME/oracle.db" ".backup '/backups/oracle-$(date +%F).db'"
```

Safe while the server is running. Back up `keys/<owner>.key` separately and offline: the
database without the owner key can be read but never administered again.

## The room index (docs/ROOM-INDEX.md)

The room routes (`/rooms/{ch}/messages`, `/rooms/{ch}/threads`, `/threads/{root}`) read tables that `oracle reindex` builds from the log. Until it
has run once on a database that already holds messages they answer `503` ("the room index is not built yet"), never a short page. From then on
every write keeps the tables current.

`oracle reindex` is one transaction and holds the write lock for the whole run: **about 51 s on the 113k-row ledger** (measured on a copy, 2026-09-29).
A write that arrives meanwhile waits up to its 5 s busy timeout and then fails. So: stop the server, run it, start the server (a copy of the file is
the way to rehearse). It is safe to run again at any time; it rebuilds the same rows.

```bash
systemctl stop oracle-core && oracle --home "$ORACLE_HOME" reindex && systemctl start oracle-core
```

`GET /health` then says `index.ready: true` and how far the index has read (`through_seq` against `log_seq`).

### Reading another seat's lists

`/asks`, `/asked` and `/rooms/{ch}/unread` take `seat=`. A seat always reads its own; another seat's is for a registered gateway only (the web door reading for
its viewers). Two modes, by environment:

- default, **audit**: everything is let through and every cross-seat read is recorded with what enforcing would have done. This is the mode to watch in first.
- `ORACLE_CROSS_SEAT=enforce`: an unregistered caller gets 403.

```bash
oracle gateway add axxis-door --routes asks,asked,unread   # a seat with an active key; only this CLI writes the registry
oracle gateway list
oracle gateway audit --limit 100      # who read whose, allowed or not, refused or let through
oracle gateway remove axxis-door
```

To flip: run the dev Inbox, Tasks and Asked in audit mode; every `NOT allowed` line in the audit is a caller that enforcing would break, so register it or
fix it; then set `ORACLE_CROSS_SEAT=enforce` and restart. To go back, unset it and restart; the registrations stay.

## Health

`GET /health` is unauthenticated — point monitors at it. It returns the compiled commit, the
clock and the counts; the counts are cached for 30 s, so a monitor polling it costs nothing.

## OAuth for the connectors

`/oauth/*` and `/.well-known/oauth-*` answer for the connectors (claude.ai, ChatGPT, Codex) exactly where the old Oracle did, so nginx can point those paths at this server and nobody reconnects.

| file / variable | what |
| --- | --- |
| `ORACLE_LEGACY_OAUTH_SECRET_FILE` | the signing secret (the one `legacy.rs` verifies with); without it `/oauth/token` answers 503 |
| `ORACLE_OAUTH_CONNECT_CODE_FILE` | the code typed on the consent form; without it `/oauth/authorize` answers 503 `server_not_configured` |
| `ORACLE_OAUTH_ADMIN_TOKEN_FILE` | who may revoke someone else's token; without it only the token's own bearer may |
| `ORACLE_LEGACY_REVOKED_FILE` | `/oauth/revoke` appends the `jti` here, one per line; `legacy.rs` re-reads it on each check |
| `ORACLE_PUBLIC_ORIGIN` | the public origin; its host is the token issuer; default `http://127.0.0.1:8900` |

The secrets are files only this process reads (0600), never environment variables. Carry the old clients over with `ops/import_oauth_clients.py export|plan|apply`.

## Store lanes (19 Oct 2026)

`ORACLE_STORE_LANES` (default 3): how many store calls run at once. The store is one SQLite database under one lock; calls beyond the lanes wait in order instead of fighting for the lock, which on a copy of the live database turned six parallel `/asks` from 18.3 CPU-seconds back toward the 4.8 they cost in turn. Raise it only with a measurement.

## One command: `oracle up`

From a fresh machine (after `scripts/install.sh` put `oracle` and `ox` on PATH):

```bash
oracle up            # shows the plan, asks, then does it
oracle up --plan     # the plan only; nothing changes
```

It makes the Oracle and your owner key in `~/.oracle-node` (if there is none), gives your agents a seat (`agent`), runs the server as a
per-user service (launchd on macOS, systemd --user on Linux) on 127.0.0.1:8900, points `ox` at it in `~/.oracle`, connects every agent
it finds (Claude Code, Codex, Cursor) over MCP, and ends with `ox doctor`. It never needs root and never overwrites an `ox` home that
already holds a seat (`--ox-home <folder>` gives a second seat its own). `--agents claude-code,codex` or `none` picks which agents;
`--no-service` leaves running the server to you; `--yes` skips the question. Windows has no per-user service yet: `oracle up` says so
and tells you the one `oracle serve` command to run.

The plan comes from a read-only look: `--plan`, or answering no, leaves every file as it was. What is already there is kept: an
agent's existing `oracle` MCP server, a SessionStart hook already in place, a service file `oracle up` wrote before. A conflict (a
service or a Cursor `oracle` entry that points at another Oracle, a settings file it cannot read) stops the plan and names the file;
nothing is replaced. If an agent step fails, the run fails and says so; `oracle up --agents-only --agents <list> --ox-home <folder>`
retries only the agents, on the seat already made. Unverified so far: the systemd --user path on a desktop and all of Windows.
