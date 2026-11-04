#!/usr/bin/env bash
# End-to-end proof: a fresh Oracle, two seats, every ox verb, and the trust rules
# exercised over real HTTP. Exits non-zero on the first broken claim.
set -euo pipefail
# A check reads its producer's whole output (grep >/dev/null, never grep -q): grep -q exits at the first match and
# closes the pipe, the producer then dies writing to it (oracle panics on EPIPE, curl exits 23), and pipefail turns a
# passing check into a failure. That race failed main at ed082cd ("the audit is missing rows").
cd "$(dirname "$0")/.."
python3 scripts/test-install.py
cargo build --quiet --release
BIN="${CARGO_TARGET_DIR:-$PWD/target}/release"
T="$(mktemp -d)"; PORT="${PORT:-18900}"; URL="http://127.0.0.1:$PORT"
trap 'kill ${SRV:-} 2>/dev/null || true; rm -rf "$T"' EXIT
ok(){ printf '  \033[32m✓\033[0m %s\n' "$1"; }
die(){ printf '  \033[31m✗ %s\033[0m\n' "$1"; exit 1; }
field(){ awk -v k="$1" '$1==k{print $2}'; }

# No server or configured seat exists: the installed artifact must carry its docs.
OX_HOME="$T/no-seat" "$BIN/ox" manual >"$T/manual.md"
cmp docs/ox.md "$T/manual.md" || die "embedded reference differs from source"
OX_HOME="$T/no-seat" "$BIN/ox" manual workflow >"$T/workflow.md"
cmp docs/ox-workflow.md "$T/workflow.md" || die "embedded workflow differs from source"
OX_HOME="$T/no-seat" "$BIN/ox" manual --check docs/ox.md
sed '/^| `ox doctor` /d' docs/ox.md >"$T/missing-row.md"
if OX_HOME="$T/no-seat" "$BIN/ox" manual --check "$T/missing-row.md" >"$T/check.log" 2>&1; then
    die "missing manual row incorrectly passed"
fi
grep 'ox doctor' "$T/check.log" >/dev/null || die "negative control failed for wrong reason"
OX_HOME="$T/no-seat" "$BIN/ox" --json manual workflow | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["topic"] == "workflow" and "signed:ok" in d["text"]'
OX_HOME="$T/no-seat" "$BIN/ox" manual commands | grep '^ox dot authority sign-request$' >/dev/null
if OX_HOME="$T/no-seat" "$BIN/ox" manual unknown >"$T/unknown.log" 2>&1; then
    die "unknown manual topic incorrectly accepted"
fi
[ ! -e "$T/no-seat" ] || die "manual created a seat directory"
ok "offline embedded manual and missing-row negative control"

export ORACLE_HOME="$T/oracle"
INIT="$("$BIN/oracle" init --owner blaze)"
"$BIN/oracle" serve --bind "127.0.0.1:$PORT" 2>"$T/server.log" & SRV=$!
for _ in $(seq 1 50); do curl -sf "$URL/health" >/dev/null && break; sleep 0.1; done
curl -sf "$URL/health" >/dev/null || die "server did not start"
ok "oracle init + serve"

ROCKY="$("$BIN/oracle" --online-admin seat add rocky)"; KIN="$("$BIN/oracle" seat add kin)"
tok(){ cat "$T/oracle/keys/$1.token"; }
# A minted token is never printed (31 Oct 2026): it is in a new mode-0600 file, and stdout names the file.
case "$ROCKY$KIN$INIT" in *orc_*) die "a minted token reached stdout";; esac
[ "$(stat -c %a "$T/oracle/keys/rocky.token" 2>/dev/null || stat -f %Lp "$T/oracle/keys/rocky.token")" = 600 ] || die "the token file is not mode 0600"
if "$BIN/oracle" token new rocky >"$T/token-exists.log" 2>&1; then die "token new overwrote an existing token file"; fi
grep 'already exists' "$T/token-exists.log" >/dev/null || die "token new refused for the wrong reason"
"$BIN/oracle" token new rocky --token-file "$T/rocky-2.token" | grep -v orc_ >/dev/null && [ -s "$T/rocky-2.token" ] || die "token new --token-file"
touch "$T/blocker"
if "$BIN/oracle" seat add half-made --token-file "$T/blocker/half.token" >"$T/half.log" 2>&1; then die "seat add wrote a token under a regular file"; fi
grep 'nothing was changed' "$T/half.log" >/dev/null || die "seat add failed for the wrong reason"
[ ! -e "$T/oracle/keys/half-made.key" ] || die "a refused token path left a half-made seat key"
"$BIN/oracle" seat list | grep -w half-made >/dev/null && die "a refused token path left a half-made seat"
ok "a minted token goes to a new 0600 file, never stdout, and is never overwritten, and a bad token path changes nothing"
if "$BIN/oracle" --online-admin stats >"$T/admin-mode.log" 2>&1; then
    die "online admin accepted a non-control command"
fi
grep 'only for seat and token' "$T/admin-mode.log" >/dev/null || die "online admin refused for the wrong reason"
ok "live control registration and non-control negative check"
ox_as(){ local who="$1"; shift; OX_HOME="$T/ox-$who" "$BIN/ox" "$@"; }
ORACLE_TOKEN=invalid-inherited-token ox_as rocky setup --url "$URL" --handle rocky --token-file "$T/oracle/keys/rocky.token" --key "$T/oracle/keys/rocky.key" >/dev/null
ox_as kin   setup --url "$URL" --handle kin   --token "$(tok kin)" --key "$T/oracle/keys/kin.key"   >/dev/null
ox_as rocky doctor >/dev/null && ok "ox doctor: key, token, registry, clock, signed round trip"
if ox_as rocky setup --url "$URL" --handle rocky --token dummy --token-file "$T/oracle/keys/rocky.token" --key "$T/oracle/keys/rocky.key" >"$T/conflicting-token.log" 2>&1; then die "two explicit token options were accepted"; fi
grep "cannot be used with" "$T/conflicting-token.log" >/dev/null || die "explicit token conflict failed for another reason"
ok "explicit token file overrides inherited environment, while explicit token options conflict"
ox_as rocky --json state orientation-empty | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["orientation"]["status"] == "missing" and d["orientation"]["enforcement"] == "advisory"'
ox_as rocky state orientation-empty --set purpose=Testing --set current=Ready >/dev/null
ox_as rocky --json state orientation-empty | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["orientation"]["status"] == "partial" and d["orientation"]["missing_fields"] == ["next"]'
ox_as rocky state orientation-empty --set next=Review >/dev/null
ox_as rocky --json state orientation-empty | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["orientation"]["status"] == "available" and d["orientation"]["freshness"] == "not_assessed"'
ok "room orientation: missing, partial and repaired over HTTP"

ox_as kin say room $'hello @rocky\nthis is a multi-line message with a 🐝' | grep >/dev/null 'signed:ok' && ok "kin speaks (multi-line, unicode)"
ox_as kin say room "general chatter" >/dev/null
ox_as rocky inbox | grep >/dev/null '^FOR YOU (1)' && ok "fold: rocky sees what is new and what is his" || die "inbox did not show rocky's one message"
ox_as rocky --json inbox --ack | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["acked"] is True and d["for_me"] and d["marks"], d' || die "ack did not acknowledge the delivered page"
ox_as rocky --json inbox | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["new"] == 0 and d["page"] == 0 and d["for_me"] == [] and d["rooms"] == [] and d["has_more"] is False, d' && ok "ack advances the cursor" || die "ack did not advance the cursor"
# (after the inbox checks: ox context acks the room it shows)
# A thread's root is shown whole: a long ask must not lose its last questions (Blaze, 2026-09-28: "why is the text cutting off").
LONG="$(for i in $(seq 1 90); do printf 'a long ask with many questions '; done) and the last question is TAIL-MARK-7731"
LID=$(ox_as kin say room "$LONG" | grep -o 'OBS-[A-Za-z0-9-]*' | head -1)
ox_as rocky context room --re "$LID" | grep >/dev/null 'TAIL-MARK-7731' && ok "context --re shows the whole root, never clipped" || die "context --re clipped the root"
ox_as rocky read room | grep >/dev/null 'general chatter' && ok "read"
ox_as rocky remember "SQLite in WAL mode is enough for a personal Oracle" --kind decision >/dev/null
ox_as rocky recall "WAL personal" | grep >/dev/null 'SQLite in WAL' && ok "remember + recall"
ox_as rocky remember "cpu 93% disk 40%" --kind status >/dev/null 2>&1 && die "status accepted as knowledge" || ok "status lines are refused as knowledge"
ox_as rocky task ship-v1 running "cutting the release" --next 2026-10-01 >/dev/null
ox_as rocky brief --full | grep >/dev/null '\[running\] ship-v1' && ok "task + brief" || die "brief did not show the running task"
ox_as rocky task no-clock running "forgot the clock" >/dev/null 2>&1 && die "open work accepted without a clock" || ok "open work without next-touch is refused"

# trust, over the wire
cp "$T/ox-kin/config.json" "$T/kin.json"
python3 - "$T" <<'PY'
import json,sys; t=sys.argv[1]
c=json.load(open(f"{t}/ox-kin/config.json")); c["handle"]="rocky"; json.dump(c,open(f"{t}/ox-kin/config.json","w"))
PY
ox_as kin say room "i am rocky, honest" >/dev/null 2>&1 && die "kin posted as rocky" || ok "a seat cannot speak under another seat's name"
cp "$T/kin.json" "$T/ox-kin/config.json"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$URL/observations" -H 'Content-Type: application/json' -d '{}')
[ "$code" = 401 ] && ok "no token → 401" || die "unauthenticated write got $code"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$URL/keys" -H 'Content-Type: application/json' \
  -d '{"action":"rotate","handle":"rocky","pubkey":"'"$(printf 'ab%.0s' {1..32})"'","ts_ms":'"$(date +%s000)"',"nonce":"'"$(printf '00%.0s' {1..16})"'","signer":"blaze","sig":"'"$(printf '00%.0s' {1..64})"'"}')
[ "$code" = 401 ] || [ "$code" = 422 ] && ok "a forged key change is refused ($code)" || die "forged key change got $code"

# offline → queue → flush exactly once
kill $SRV; wait $SRV 2>/dev/null || true
set +e; ox_as rocky say room "written while oracle was down" >/dev/null 2>&1; rc=$?; set -e
[ $rc = 3 ] && ok "server down → queued, exit 3, never a false ok" || die "expected exit 3, got $rc"
# …and this time the server is also told the old Oracle's token secret, as it will be at the cutover.
printf 'e2e-legacy-secret-4471' > "$T/legacy.secret"
ORACLE_LEGACY_OAUTH_SECRET_FILE="$T/legacy.secret" ORACLE_LEGACY_GUESTS=1 "$BIN/oracle" serve --bind "127.0.0.1:$PORT" 2>>"$T/server.log" & SRV=$!
for _ in $(seq 1 50); do curl -sf "$URL/health" >/dev/null && break; sleep 0.1; done
ox_as rocky flush | grep >/dev/null 'flushed 1, still queued 0'; ox_as rocky flush | grep >/dev/null 'flushed 0'
n=$(ox_as rocky --json read room --limit 100 | python3 -c "import json,sys; print(sum('while oracle was down' in i['content'] for i in json.load(sys.stdin)['items']))")
[ "$n" = 1 ] && ok "flushed twice, landed exactly once" || die "queued write landed $n times"

# ── the old Oracle's tokens, minted by its own recipe (Python hmac/base64), accepted here ────────
legacy_token(){ python3 - "$1" "$2" <<'PY'
import base64, hashlib, hmac, json, sys, time
secret, handle = sys.argv[1].encode(), sys.argv[2]
b64u = lambda b: base64.urlsafe_b64encode(b).decode().rstrip("=")
p = {"iss": "oracle.example", "aud": "oracle-mcp", "sub": "client_e2e", "scope": "oracle:read oracle:write",
     "iat": int(time.time()), "exp": int(time.time()) + 86400, "jti": "jti-e2e"}
if handle: p["handle"] = handle
body = b64u(json.dumps(p, separators=(",", ":"), sort_keys=True).encode())
print("ocg_" + body + "." + b64u(hmac.new(secret, body.encode(), hashlib.sha256).digest()))
PY
}
OLD_ROCKY=$(legacy_token e2e-legacy-secret-4471 rocky); OLD_NOBODY=$(legacy_token e2e-legacy-secret-4471 ""); OLD_FORGED=$(legacy_token wrong-secret rocky)
curl -sf -H "Authorization: Bearer $OLD_ROCKY" "$URL/whoami" | grep >/dev/null '"handle":"rocky"' && ok "an old ocg_ token bound to rocky is rocky here — no re-auth at the cutover" || die "old token not accepted"
curl -sf -X POST "$URL/mcp/" -H "Authorization: Bearer $OLD_ROCKY" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | grep >/dev/null '"title":"Oracle · rocky"' && ok "…and opens the MCP door as rocky" || die "old token refused at /mcp"
curl -sf -H "Authorization: Bearer $OLD_NOBODY" "$URL/whoami" | grep >/dev/null '"handle":"guest"' && ok "an old token bound to nobody reads as guest (operator allowed guests)" || die "unbound old token"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$URL/observations" -H "Authorization: Bearer $OLD_NOBODY" -H 'Content-Type: application/json' -d '{}')
[ "$code" = 403 ] && ok "a guest cannot write" || die "guest write got $code"
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $OLD_FORGED" "$URL/whoami")
[ "$code" = 401 ] && ok "an old-style token with the wrong secret is nobody" || die "forged legacy token got $code"

# a seat with two devices; a device key under the owner's handle cannot administer
newkey(){ openssl genpkey -algorithm ed25519 -out "$1" 2>/dev/null; chmod 600 "$1"; openssl pkey -in "$1" -pubout -outform DER | tail -c 32 | xxd -p -c 64; }
LAPTOP=$(newkey "$T/rocky-laptop.pem"); "$BIN/oracle" seat add-key rocky --pubkey "$LAPTOP" >/dev/null
mkdir -p "$T/ox-laptop"; sed "s#\"key_path\": *\"[^\"]*\"#\"key_path\": \"$T/rocky-laptop.pem\"#" "$T/ox-rocky/config.json" > "$T/ox-laptop/config.json"; chmod 600 "$T/ox-laptop/config.json"
ox_as laptop say room "from my second device" | grep >/dev/null 'signed:ok' && ox_as rocky say room "and from my first" | grep >/dev/null 'signed:ok' \
  && ok "one seat, two devices: both keys sign as rocky" || die "second device could not sign"
"$BIN/oracle" seat revoke rocky --pubkey "$LAPTOP" >/dev/null
ox_as laptop say room "from the lost laptop" >/dev/null 2>&1 && die "a revoked device key still signs" || true
ox_as rocky say room "seat survives" | grep >/dev/null 'signed:ok' && ok "revoking one device leaves the seat and its token alive" || die "revoking a device killed the seat"

# a second implementation of the envelope (a web gateway's): the real server must accept it
if command -v node >/dev/null; then
  WEB=$(newkey "$T/web.pem"); "$BIN/oracle" seat add-key kin --pubkey "$WEB" >/dev/null
  openssl pkey -in "$T/web.pem" -outform DER | tail -c 32 | xxd -p -c 64 > "$T/web.seed"
  node examples/sign.mjs "$URL" "$(tok kin)" "$T/web.seed" kin room "signed in node — tags 🐝 sorted by byte" "zeta" "𝛼-astral" "ｚ-fullwidth" "alpha" | grep >/dev/null 'signed:ok' \
    && ok "a gateway's own signer (Node, no deps) is verified by the server" || die "the Node signer and oracle-proto disagree"
  out=$(node examples/sign.mjs "$URL" "$(tok rocky)" "$T/web.seed" kin room "kin's key, rocky's token" 2>&1 || true)
  echo "$out" | grep >/dev/null 'refused (403)' \
    && ok "…and still cannot use another seat's token" || die "token/seat binding failed for the Node signer"
  # the TypeScript SDK (sdk/typescript): its signer against the golden vector, then the whole client against this server
  (cd sdk/typescript && node --test test/golden.test.mjs >"$T/sdk-golden.log" 2>&1) \
    && ok "the TypeScript SDK signs the golden vector byte for byte" || { cat "$T/sdk-golden.log"; die "the TypeScript SDK and oracle-proto disagree"; }
  (cd sdk/typescript && ORACLE_URL="$URL" ORACLE_TOKEN="$(tok kin)" ORACLE_SEED_FILE="$T/web.seed" ORACLE_ROOM=sdk node test/live.mjs) | grep >/dev/null 'signed:ok' \
    && ok "the TypeScript SDK writes, reads, threads, supersedes, recalls and remembers; a forged author is refused" || die "the TypeScript SDK failed against the server"
fi

# the MCP door: same seat, same signature, same rules
mcp(){ printf '%s\n' "$1" | ox_as rocky mcp; }
mcp '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"oracle_say","arguments":{"room":"room","text":"said over mcp","to":["kin"]}}}' | grep >/dev/null 'signed:ok' || die "mcp say was not signed:ok"
ox_as kin --json inbox | python3 -c "import json,sys; i=[x for x in json.load(sys.stdin)['for_me'] if x['text']=='said over mcp']; assert i and i[0]['author']=='rocky'" \
  && ok "MCP: a tool call is a signed write by this seat, and reaches who it addresses" || die "mcp write not seen by kin"
mcp '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"oracle_task","arguments":{"subject":"t","state":"running","note":"no clock"}}}' | grep >/dev/null '"isError":true' \
  && ok "MCP: a refused write is an error result, never a false ok" || die "mcp accepted a running task without a clock"

# the API: one door per job — every read of messages is GET /search, specifics are parameters
TOK="$(tok rocky)"
g(){ curl -sf -H "Authorization: Bearer $TOK" "$URL$1"; }
g "/states?channel=board" | python3 -c "import json,sys; h=json.load(sys.stdin)[\"heads\"]; assert h and all(x[\"channel\"]==\"board\" and x[\"id\"].startswith(\"OBS-\") and x[\"note\"] for x in h), h" && ok "/states?channel= gives a room's heads with room, id and note" || die "/states?channel="
g "/states?channel=nowhere" | grep >/dev/null '"heads":\[\]' && ok "another room's board is empty" || die "channel filter leaks"

# ── POST /mcp: the same tools over HTTP, signed with a key the server holds for the seat ─────────
mcp(){ curl -sf -X POST "$URL/mcp/" -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' -d "$1"; }
mcp '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}' | grep >/dev/null '"title":"Oracle · rocky"' && ok "POST /mcp initialize names the seat" || die "MCP initialize"
mcp '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | grep >/dev/null '"name":"oracle_about"' && ok "POST /mcp lists the old Oracle's tool names too" || die "MCP legacy names missing"
mcp '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"oracle_say","arguments":{"room":"room","text":"over http"}}}' | grep >/dev/null 'no signing key for' && ok "no hosted key → a write is refused with the reason, never faked" || die "MCP write without a hosted key"
mkdir -p "$T/oracle/hosted" && cp "$T/oracle/keys/rocky.key" "$T/oracle/hosted/rocky.key"
mcp '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"room_say","arguments":{"channel":"room","content":"over http, signed by the server @kin"}}}' | grep >/dev/null 'signed:ok  OBS-room-' && ok "hosted key → room_say (old name) lands signed as rocky" || die "MCP hosted-key write"
mcp '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"oracle_about","arguments":{"room":"room","terms":"over http signed"}}}' | grep >/dev/null 'signed by the server' && ok "oracle_about (old name) = recall in one room" || die "MCP oracle_about"
ox_as rocky read room | grep >/dev/null 'over http, signed by the server' && ok "what the HTTP door wrote is what ox reads" || die "MCP write not visible to ox"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$URL/mcp" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}')
[ "$code" = 401 ] && ok "POST /mcp without a token → 401" || die "anonymous MCP got $code"
has(){ # has <label> <path> <python expression over d>
  g "$2" | python3 -c "import json,sys; d=json.load(sys.stdin); sys.exit(0 if ($3) else 1)" && ok "$1" || die "$1"
}
ID=$(g "/search?channel=room&limit=1" | python3 -c "import json,sys; print(json.load(sys.stdin)['items'][0]['id'])")
has "GET /search pages a room, newest first"         "/search?channel=room&limit=50"        "d['count']>=3 and all(i['channel']=='room' for i in d['items'])"
has "GET /search?q= ranks full text"                  "/search?q=WAL%20personal"             "d['hits'] and 'SQLite in WAL' in d['hits'][0]['content']"
has "GET /search?about= follows one thing"            "/search?about=chatter&channel=room"   "isinstance(d, dict) and len(json.dumps(d))>2"
has "GET /search?tag= narrows by tag"                 "/search?tag=from:kin&limit=50"        "d['items'] and all(i['author']=='kin' for i in d['items'])"
has "GET /search?id= is one observation"              "/search?id=$ID"                       "(d.get('observation') or d)['id']=='$ID'"
has "…with include=envelope,source"                   "/search?id=$ID&include=envelope,source" "'envelope' in d and 'pubkey' in d and 'versions' in d"
has "GET /threads?id= is one whole thread"            "/threads?id=$ID"                      "'root' in d"
has "GET /threads?view=digest is the room in brief"   "/threads?channel=room&roots=kin&view=digest" "'as_of_ms' in d"
has "GET /states?subject= is one head"                "/states?subject=ship-v1"              "'ship-v1' in json.dumps(d)"
has "GET /keys?history=true is the signed key log"    "/keys?history=true"                   "d['events']"
has "GET /fold?marks=true shows every cursor"         "/fold?marks=true&channel=room"        "'marks' in d"
curl -sf "$URL/health" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['status']=='ok' and d['ms']>0 and d['stats']['observations']>0" \
  && ok "GET /health: liveness, clock and counts in one" || die "health is missing the clock or counts"
for old in /say /state /remember /recent /tag/x /about /observation/x /thread /digest /now /health/deep /keys/events /fold/marks /fold/ack; do
  code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOK" "$URL$old")
  [ "$code" = 404 ] || [ "$code" = 405 ] || die "old path $old still answers ($code)"
done; ok "the old paths are gone (404/405)"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' "$URL/search" -d '{"query":"x"}')
[ "$code" = 405 ] && ok "search is a GET; POST /search is refused" || die "POST /search answered $code"

# the message shape (Emmy, 2026-09-22): ask + by + ref authored; answered_by / overdue derived
ASK=$(ox_as kin say room "@rocky please rebuild the index" --intent question --ask act --by 2h --ref abc1234 --needs OBS-x --verify "index count matches" | awk '{print $2}')
has "ask, by and ref are signed tags; needs/verify ride in cargo" "/search?id=$ASK&include=envelope" \
  "(d.get('observation') or d)['ask']=='act' and (d.get('observation') or d)['by_ms']>0 and 'ref:abc1234' in (d.get('observation') or d)['tags'] and 'OBS-x' in json.dumps((d.get('observation') or d)['cargo']) and 'index count' in json.dumps((d.get('observation') or d)['cargo'])"
has "a question asks for a reply by default; a note asks nothing" "/search?channel=room&limit=50" \
  "{i['intent']:i['ask'] for i in d['items']}.get('note')=='none' and all(i['ask']=='reply' for i in d['items'] if i['intent']=='question' and i['id']!='$ASK')"
LATE=$(ox_as kin say room "@rocky yesterday's ask" --intent question --by 1000 | awk '{print $2}')
has "an unmet ask past its clock is overdue"                    "/search?id=$LATE"                    "(d.get('observation') or d).get('overdue') is True"
ox_as rocky say room "rebuilt, 81k rows" --reply "$ASK" --intent receipt >/dev/null
has "a reply is the claimed answer, derived, never authored"     "/search?id=$ASK"                     "(d.get('observation') or d)['answered_by'].startswith('OBS-') and not (d.get('observation') or d).get('overdue')"
has "the room view scores each seat: sent, asks, answers, answered" "/threads?channel=room&scope=room&roots=kin,rocky" \
  "isinstance(d['messages']['by_author']['kin'],int) and d['messages']['by_author']['kin']>=2 and d['messages']['by_author_signals']['kin']['asks']>=2 and d['messages']['by_author_signals']['kin']['answered']>=1 and d['messages']['by_author_signals']['rocky']['answers']>=1"
ox_as kin say room "x" --ask maybe >/dev/null 2>&1 && die "a made-up ask was accepted" || ok "ox refuses an ask it does not know"
ox_as kin inbox | grep -E 'FOR YOU \([1-9]' >/dev/null && ok "a reply to my message reaches my inbox without my name in it" || die "kin's answered ask did not ring kin"

# annotations (Emmy, 2026-09-22): the classifier fills blanks; the author's own tags cannot be overridden
RAW=$(ox_as kin say room "can someone check the disk on core?" | awk '{print $2}')
ox_as rocky annotate room "$RAW" --why "question · ask act · disk check — why: 'can someone'" --set intent=question --set ask=act --set by=model >/dev/null
has "a model annotation fills intent and ask the author left blank" "/search?id=$RAW" \
  "(d.get('observation') or d)['intent']=='question' and (d.get('observation') or d)['ask']=='act' and (d.get('observation') or d)['classified_by'].startswith('OBS-')"
ox_as rocky annotate room "$RAW" --why "proposal — a person read it" --set intent=proposal >/dev/null
has "a person's annotation outranks the model's"                "/search?id=$RAW"                     "(d.get('observation') or d)['intent']=='proposal'"
OWN=$(ox_as kin say room "we ship on Friday" --intent decision | awk '{print $2}')
ox_as rocky annotate room "$OWN" --why "question? no." --set intent=question --set by=model >/dev/null
has "the author's own intent is never overridden"                "/search?id=$OWN"                     "(d.get('observation') or d)['intent']=='decision' and 'classified_by' not in (d.get('observation') or d)"
LOW=$(ox_as kin say room "hmm" | awk '{print $2}')
ox_as rocky annotate room "$LOW" --why "unclassified · too little to say — why: two letters" --set unclassified=1 --set by=model --confidence 0.2 >/dev/null
has "an unclassifiable message says so instead of staying silent"  "/search?id=$LOW"                  "(d.get('observation') or d).get('unclassified') is True and (d.get('observation') or d)['intent']=='note'"

# items (Blaze, 2026-09-22): a message is items; a reply is a state change; only the asker accepts
ITEMS=$(ox_as kin say room "windows on landscape, and pinch zoom" --ask act --item R1=requirement --item R2=requirement | awk '{print $2}')
has "GET /threads?id= includes the root's item rows" "/threads?id=$ITEMS" \
  "[i['k'] for i in d['items']]==['R1','R2']"
ox_as rocky context room --re "$ITEMS" | grep -q 'ITEMS    2 of 2 not accepted' \
  && ok "ox context --re shows item rows before a reply" || die "ox context --re hid the root's items"
ox_as rocky say room "R1 is on dev" --reply "$ITEMS" --item R1=built --ref abc1234 >/dev/null
ox_as rocky say room "and I accept my own work (no)" --reply "$ITEMS" --item R1=accepted >/dev/null
has "a reply moves an item; the thread word is needs_you; a non-author cannot accept" "/search?id=$ITEMS" \
  "[i['state'] for i in (d.get('observation') or d)['items']]==['built','open'] and (d.get('observation') or d)['fold']=='needs_you'"
ox_as kin say room "R1 good, R2 not needed" --reply "$ITEMS" --item R1=accepted --item R2=dropped >/dev/null
has "the asker accepts and drops; every item settled closes the thread word"  "/search?id=$ITEMS" "(d.get('observation') or d)['fold']=='closed'"
WORDS=$(ox_as kin say room "a seat may add requirements" --ask act | awk '{print $2}')
DECL=$(ox_as rocky annotate room "$WORDS" --set item:R3=requirement --why "R3 requirement: handle narrow screens" \
  --cargo '{"schema":"items/1","items":[{"k":"R3","kind":"requirement","text":"handle narrow screens"}]}' | awk '{print $2}')
has "a thread item names the annotation that declared its words" "/threads?id=$WORDS" \
  "len(d['items'])==1 and d['items'][0]['k']=='R3' and d['items'][0]['from']=='$DECL'"
ox_as rocky context room --re "$WORDS" | grep -q 'handle narrow screens' \
  && ok "ox context --re carries an annotated item's words" || die "ox context --re lost the item's words"

MAP=$(ox_as kin say room "Map acceptance" --item D=destination:Ship --item R=research:Investigate --item T=task:Implement --tag needs:T=R --ask act | awk '{print $2}')
ox_as rocky map room "$MAP" | grep '1 frontier · 1 in fog' >/dev/null && ok "ox map shows unmet needs" || die "map lost its dependency"
RESOLVED=$(ox_as rocky say room "Research delivered" --reply "$MAP" --item R=built --tag resolves:R | awk '{print $2}')
ox_as rocky map room "$MAP" | grep '2 frontier · 0 in fog' >/dev/null && ok "ox map advances from server projection" || die "map did not advance"
has "map resolution links effective move" "/threads?id=$MAP" "next(i for i in d['items'] if i['k']=='R')['resolved_by']=='$RESOLVED'"
ox_as kin say room "x" --item R9=maybe >/dev/null 2>&1 && die "a made-up item state was accepted" || ok "ox refuses an item kind or state it does not know"

# Asks (docs/ASKS-AND-EVENTS.md, Layer 1): a decide-ask carries its options signed, a choice answers it, and the
# three lists are read out of the messages.
DEC=$(ox_as kin say room "ship the release?" --to rocky --ask decide --option "Ship now" --option "Wait for CI" --recommend 2 --why "CI has not passed yet" --by 2h | awk '{print $2}')
ox_as rocky asks | grep >/dev/null "$DEC.*options: 1) Ship now · 2) Wait for CI" && ok "ox asks: the decide-ask is waiting on rocky with its options" || die "ox asks does not list the decide-ask"
ox_as kin asks --state open | grep -A20 "I'M WAITING ON" | grep >/dev/null "held by rocky.*$DEC" && ok "ox asks: kin is waiting on rocky" || die "ox asks: I'm waiting on"
ox_as kin say room "x" --ask decide --option "only one" >/dev/null 2>&1 && die "a decide-ask with one option was accepted" || ok "ox refuses a decide-ask without 2 to 6 options"
ox_as kin say room "x" --ask decide --option "a" --option "b" >/dev/null 2>&1 && die "a decide-ask without --recommend was accepted" || ok "ox refuses a decide-ask without the asker's pick"
ox_as kin say room "x" --option "a" --option "b" >/dev/null 2>&1 && die "--option without --ask decide was accepted" || ok "ox refuses --option without --ask decide"
ox_as rocky say room "3?" --reply "$DEC" --choice 3 >/dev/null 2>&1 && die "choice 3 of 2 options was accepted" || ok "the server refuses a choice the ask does not offer"
ox_as rocky say room "1?" --choice 1 >/dev/null 2>&1 && die "--choice without --reply was accepted" || ok "ox refuses --choice without --reply"
ox_as rocky say room "wait for CI" --reply "$DEC" --choice 2 >/dev/null
ox_as rocky asks --state done | grep >/dev/null "$DEC.*chose 2: Wait for CI" && ok "a choice answers the ask and is read back" || die "the choice was not read back"
ox_as rocky asks --state open | grep -A20 "WAITING ON ME" | grep >/dev/null "$DEC" && die "an answered ask is still waiting" || ok "an answered ask leaves 'waiting on me'"
ox_as rocky asks --state maybe >/dev/null 2>&1 && die "ox asks accepted a made-up state" || ok "ox asks refuses a state it does not know"
REV=$(ox_as kin say room "look at this" --to rocky --ask review --ref abc1234 | awk '{print $2}')
ox_as kin say room "x" --to rocky --ask review >/dev/null 2>&1 && die "a review-ask without a ref was accepted" || ok "a review-ask needs a ref"
ox_as rocky asks --room room | grep >/dev/null "review.*$REV" && ok "ox asks --room lists the review-ask" || die "review-ask missing"
ox_as kin card room x-card queue "t" --summary s --acceptance a --group "Room UX" >/dev/null 2>&1 && die "a card the board would refuse was written" || ok "ox refuses a card group the board would refuse (nine cards were silently refused on 2026-09-22)"
ox_as kin card room h-card review "t" --summary s --acceptance a >/dev/null 2>&1 && die "a card moved to review without a handoff note was written" || ok "ox refuses review/done without a handoff note (how-we-work: a status flip hands the next person no map)"
ox_as kin card room h-card blocked "t" --summary "waiting on the key" --acceptance a >/dev/null 2>&1 && die "a blocked card with no unblocker was written" || ok "ox refuses blocked without who can unblock it"
HC=$(ox_as kin card room h-card review "t" --summary s --acceptance a --handoff "changed X · test: run Y · gotcha: Z" | awk '{print $2}')
has "a handoff note rides in the signed card"                  "/search?id=$HC" \
  "'changed X' in ((d.get('observation') or d).get('cargo') or '') and '\"handoff\"' in ((d.get('observation') or d).get('cargo') or '')"

# Presence (docs/DESIGN.md): who is making requests, from each seat's last authenticated request. Every route feeds it.
presence(){ curl -sf -H "Authorization: Bearer $(tok rocky)" "$URL/presence"; }
presence | python3 -c "
import json,sys
d=json.load(sys.stdin); s={r['handle']:r for r in d['seats']}
assert d['live_s']==180 and d['idle_s']==1800
assert s['rocky']['state']=='live' and s['rocky']['calls']>5 and s['rocky']['last_route'], s['rocky']
assert s['kin']['state']=='live' and s['kin']['calls']>0, s['kin']
assert list(s)[:2]==['blaze','rocky'] or 'blaze' in s, list(s)
" && ok "/presence: the seats that just called are live, with their calls and last route" || die "presence did not show the seats that called"
[ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer not-a-token' "$URL/presence")" = 401 ] && ok "/presence needs a token" || die "presence answered without a token"
presence | python3 -c "import json,sys; d=json.load(sys.stdin); assert 'not-a-token' not in json.dumps(d)" && ok "a refused token leaves no trace in /presence" || die "a refused token appeared in presence"

# The tip anchor dot-terminal's CommandAnchor runs: stdout and the exit code are the contract.
HA=$(printf '1%.0s' {1..64}); HB=$(printf '2%.0s' {1..64})
[ "$(ox_as rocky anchor tip node-e2e)" = "null" ] && ok "ox anchor: a fresh journal prints null" || die "fresh journal did not print null"
ox_as rocky anchor advance node-e2e 1 "$HA" && ox_as rocky anchor advance node-e2e 2 "$HB" \
  && ox_as rocky anchor tip node-e2e | python3 -c "import json,sys; d=json.load(sys.stdin); assert d=={'seq':2,'hash':'$HB'}" \
  && ok "ox anchor: advance, then tip reads the signed line back" || die "anchor did not read back its tip"
ox_as rocky anchor advance node-e2e 1 "$HA" >/dev/null 2>&1 && die "anchor accepted a lower seq" || ok "ox anchor: a lower seq exits non-zero"
ox_as rocky anchor advance node-e2e 2 "$HA" >/dev/null 2>&1 && die "anchor accepted a sideways move" || ok "ox anchor: the same seq with another hash exits non-zero"
[ "$(ox_as kin anchor tip node-e2e)" = "null" ] && ok "ox anchor: another seat's journal is its own" || die "a seat read another seat's tip"

# Updates are judged but never listed: a newest page full of them is not an empty room.
UPD=$(ox_as kin say busy "the only message here" | awk '{print $2}')
for n in 1 2 3 4; do ox_as rocky annotate busy "$UPD" --why "status · ask none · $n" --set ask=none --set by=model >/dev/null; done
has "a room whose newest page is all updates still lists its message" "/search?channel=busy&limit=2" \
  "d['count']==1 and d['items'][0]['id']=='$UPD'"

# The global board: cards written by `ox card` in two rooms fold into one list, and the seat's brief shows its share.
ox_as kin card room e2e-ship queue "Ship the thing" --summary "s" --acceptance "a" >/dev/null
ox_as kin card busy e2e-ship queue "Same id, another room" --summary "s" --acceptance "a" >/dev/null
ox_as rocky card room e2e-review review "Review me" --summary "s" --acceptance "a" --handoff "changed X · test: Y · gotcha: Z" --unblock "kin: review it" >/dev/null
has "GET /cards folds every room's cards into one list, same id kept apart" "/cards?owner=kin" \
  "sorted((c['room'],c['id']) for c in d['cards'] if c['id'].startswith('e2e-'))==[('busy','e2e-ship'),('room','e2e-ship')] and {r['room'] for r in d['rooms']}>={'room','busy'}"
has "GET /cards?room= narrows to one room" "/cards?room=busy" "d['cards'] and all(c['room']=='busy' for c in d['cards'])"
has "GET /cards?status= takes a list and 'open'" "/cards?status=review,done" "d['cards'] and all(c['status'] in ('review','done') for c in d['cards'])"
has "GET /cards?waiting_on= is review or blocked naming the seat" "/cards?waiting_on=kin" \
  "[(c['room'],c['id'],c['owner']) for c in d['cards']]==[('room','e2e-review','rocky')]"
has "a card carries the moment it entered its status" "/cards?waiting_on=kin" "d['cards'][0]['since_ms']>0 and d['cards'][0]['unblock']=='kin: review it'"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$URL/cards")" = 401 ] && ok "/cards needs a token" || die "cards answered without a token"
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOK" "$URL/cards?status=maybe")" = 422 ] && ok "/cards refuses a status it does not know" || die "cards accepted a made-up status"
ox_as kin cards --mine | grep >/dev/null "queue .*#room .*e2e-ship" && ok "ox cards --mine lists what kin owns" || die "ox cards --mine"
ox_as kin cards --waiting | grep >/dev/null "review .*e2e-review.*rocky.*kin: review it" && ok "ox cards --waiting shows the card and who it waits on" || die "ox cards --waiting"
ox_as kin cards --room busy | grep >/dev/null "e2e-ship" && ok "ox cards --room" || die "ox cards --room"
ox_as kin brief --full | sed -n '/YOUR CARDS/,$p' | grep >/dev/null "e2e-ship" && ox_as kin brief --full | grep -A5 "WAITING ON YOU (1)" | grep >/dev/null "e2e-review" && ok "ox brief: YOUR CARDS and WAITING ON YOU" || die "ox brief cards"

# The room index's routes: the whole room in keyset pages, its threads, one thread; every class, no window.
has "GET /rooms/{ch}/messages is the whole room, newest first, with its cursor" "/rooms/room/messages?limit=2" \
  "d['count']==2 and len(d['items'])==2 and d['items'][0]['seq']>d['items'][1]['seq'] and d['next_before']==d['items'][1]['seq'] and d['items'][0]['room']=='room' and d['items'][0]['signed'] is True"
python3 - "$URL" "$TOK" <<'PY' || die "the page chain over HTTP repeats or skips a message"
import json,sys,urllib.request
url,tok=sys.argv[1:3]
def get(p):
    return json.load(urllib.request.urlopen(urllib.request.Request(url+p,headers={"Authorization":"Bearer "+tok})))
whole=get("/rooms/room/messages?limit=200")
assert whole["next_before"] is None, "the e2e room fits one page"
seqs=[]; before=None
while True:
    d=get("/rooms/room/messages?limit=3"+("&before=%d"%before if before else ""))
    seqs+= [i["seq"] for i in d["items"]]
    before=d["next_before"]
    if before is None: break
assert seqs==[i["seq"] for i in whole["items"]] and len(set(seqs))==len(seqs), (seqs,)
oldest=seqs[-1]
fwd=get("/rooms/room/messages?after=%d&limit=200"%(oldest))
assert [i["seq"] for i in fwd["items"]]==sorted(seqs[:-1]), "after= is oldest first"
PY
ok "a page chain over HTTP reaches the room's first message, once each; after= goes forward"
has "after= always returns a cursor to resume from, and says whether to ask again" "/rooms/room/messages?after=999999999&limit=5" "d['items']==[] and d['next_after']==999999999 and d['more'] is False and d['next_before'] is None"
has "an item says how far to trust its signature" "/rooms/room/messages?limit=1" "d['items'][0]['verification']=='verified' and d['items'][0]['signed'] is True"
BODY_HDR=$(curl -s -D - -o "$T/body.out" -H "Authorization: Bearer $TOK" "$URL/observations/$ID/content")
echo "$BODY_HDR" | grep -qi '^x-content-sha256: ' && [ "$(sha256sum "$T/body.out" | cut -d' ' -f1)" = "$(echo "$BODY_HDR" | tr -d '\r' | awk 'tolower($1)=="x-content-sha256:"{print $2}')" ] \
  && [ "$(cat "$T/body.out")" = "$(g "/search?id=$ID" | python3 -c "import json,sys; d=json.load(sys.stdin); print((d.get('observation') or d)['content'])")" ] \
  && ok "GET /observations/{id}/content returns the exact bytes, with a hash to check them by" || die "the item endpoint does not return the exact bytes"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$URL/observations/$ID/content")" = 401 ] && ok "the item endpoint has the same auth as every read" || die "item endpoint answered without a token"
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOK" "$URL/observations/OBS-room-nope/content")" = 404 ] && ok "an unknown item is 404" || die "unknown item was not 404"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$URL/rooms/room/messages")" = 401 ] && ok "/rooms/{ch}/messages needs a token" || die "room messages answered without a token"
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOK" "$URL/rooms/room/messages?class=gossip")" = 422 ] && ok "an unknown class is refused" || die "unknown class accepted"
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOK" "$URL/rooms/room/messages?limit=201")" = 422 ] && ok "a limit over 200 is refused" || die "limit 201 accepted"
has "GET /rooms/{ch}/threads lists threads with their root" "/rooms/room/threads" "d['threads'] and all(t['root']['id'].startswith('OBS-room-') and 'state' in t for t in d['threads'])"
has "GET /threads/{root} returns one thread, the root first" "/threads/$ID" "d['root']['thread']['root'] is not None and 'items' in d and d['limit']==50"
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOK" "$URL/threads/OBS-room-nope")" = 404 ] && ok "an unknown thread is 404" || die "unknown thread was not 404"
has "the room index is one of the routes /rooms/... on an empty room" "/rooms/nowhere/messages" "d['items']==[] and d['next_before'] is None"

# The thread observer's verdicts: set:done= becomes the thread's state (author, then person, then model), answered stays derived from replies.
code_as(){ curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $1" "$URL$2"; }
ox_as rocky annotate room "$LID" --set done=stale --set by=model --why "old, overtaken by events" >/dev/null
has "a model's set:done=stale is the thread's state" "/rooms/room/threads?state=stale" "any(t['root']['id']=='$LID' and t['state']=='stale' for t in d['threads'])"
ox_as kin annotate room "$LID" --set done=settled --why "kin, a person, says it is settled" >/dev/null
has "a person's set:done beats the model's" "/rooms/room/threads?state=settled" "any(t['root']['id']=='$LID' for t in d['threads'])"
[ "$(code_as "$TOK" '/rooms/room/threads?state=finished')" = 422 ] && ok "an unknown thread state is refused" || die "unknown thread state accepted"

# A head's lineage: /states?subject=&history=<n>, newest first, current and superseded_by worked out at read time.
ox_as rocky task lineage-x running "first" --next 2026-12-01 >/dev/null
# Use UTC today so the blocked-head acceptance fixture does not expire as the calendar advances.
RECHECK_TODAY=$(python3 -c 'from datetime import datetime, timezone; print(datetime.now(timezone.utc).date().isoformat())')
if ox_as rocky task lineage-x blocked "invalid" --next "$RECHECK_TODAY" >"$T/blocked-rule.log" 2>&1; then
  die "a blocked task without a dependency was written"
fi
grep 'who or what must act' "$T/blocked-rule.log" >/dev/null || die "blocked task failed for an unrelated reason"
has "a refused blocked task leaves the prior head untouched" "/states?subject=lineage-x&history=10" "d['count']==1 and d['revisions'][0]['state']=='running'"
ox_as rocky task lineage-x blocked "second" --next "$RECHECK_TODAY" --needs kin --blocks "complete fixture review" --check "read the fixture review receipt" >/dev/null
ox_as rocky task lineage-x done "third" >/dev/null
has "GET /states?subject=&history=n is the head's lineage, newest first" "/states?subject=lineage-x&history=10" \
  "d['count']==3 and d['truncated'] is False and [r['state'] for r in d['revisions']]==['done','blocked','running'] and d['revisions'][0]['current'] is True and d['revisions'][1]['superseded_by']==[d['revisions'][0]['id']] and d['fork'] is False"
has "a chain of exactly the limit is not truncated; one more than it is" "/states?subject=lineage-x&history=3" "d['count']==3 and d['truncated'] is False"
has "…and asking for fewer says it cut" "/states?subject=lineage-x&history=2" "d['count']==2 and d['truncated'] is True and d['heads']==[d['revisions'][0]['id']]"
has "without history= the head is answered as ever" "/states?subject=lineage-x" "d['state']=='done' and d['subject']=='lineage-x' and 'revisions' not in d"
code_as(){ curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $1" "$URL$2"; }
[ "$(code_as "$TOK" '/states?subject=lineage-x&history=0')" = 422 ] && [ "$(code_as "$TOK" '/states?subject=lineage-x&history=201')" = 422 ] && ok "a history outside 1 to 200 is refused" || die "bad history accepted"
[ "$(code_as "$TOK" '/states?subject=no-such-head&history=5')" = 404 ] && ok "a subject with no revisions is 404" || die "unknown lineage was not 404"
[ "$(code_as "$TOK" '/states?history=5')" = 422 ] && ok "history without a subject is refused" || die "history without subject accepted"
[ "$(code_as "" '/states?subject=lineage-x&history=5')" = 401 ] && ok "a lineage needs a token" || die "lineage answered without a token"

# Route 5: search as a page of the room's own items, filtered by room, author and class.
code_as(){ curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $1" "$URL$2"; }
has "GET /search?q= with author= is a page of items, scoped to the room and the author" "/search?q=chatter&channel=room&author=kin" \
  "d['order']=='newest' and d['count']>=1 and all(i['author']=='kin' and i['room']=='room' and 'score' in i and i['signed'] is True for i in d['items'])"
has "the legacy /search?q= is unchanged: hits" "/search?q=chatter" "'hits' in d and d['count']>=1 and 'items' not in d"
has "a class narrows a search (nothing here is history)" "/search?q=chatter&channel=room&class=history" "d['items']==[] and d['next_before'] is None"
has "a room's search never returns another room" "/search?q=chatter&channel=busy&format=page" "d['items']==[]"
has "order=rank is the best matches and says truncated" "/search?q=chatter&channel=room&order=rank&limit=1" "d['order']=='rank' and d['count']==1 and d['next_before'] is None and 'truncated' in d"
[ "$(code_as "$TOK" '/search?q=x&limit=201&format=page')" = 422 ] && ok "a search limit over 200 is refused" || die "search limit 201 accepted"
[ "$(code_as "$TOK" '/search?q=x&order=best')" = 422 ] && ok "an unknown search order is refused" || die "unknown order accepted"
[ "$(code_as "" '/search?q=x&format=page')" = 401 ] && ok "a paged search needs a token" || die "paged search answered without a token"

# Seat aliases: a name a seat once used stands for it, by one owner-signed record, with the original addressee kept.
AL=$(ox_as rocky say room "check the release notes" --intent proposal --ask act --to kin-old | grep -o 'OBS-[A-Za-z0-9-]*' | head -1)
ox_as kin say room "checked, all fine" --reply "$AL" --intent receipt >/dev/null
ox_as kin asks --seat kin-old | grep -A6 'WAITING ON ME' | grep >/dev/null "$AL" && ok "without an alias a reply from kin does not close an ask to kin-old" || die "the ask to the old label was closed with no alias"
"$BIN/oracle" alias set nobody-here nobody >/dev/null 2>&1 && die "an alias to a seat with no key was accepted" || ok "an alias must point at a seat with a key"
"$BIN/oracle" alias set kin kin >/dev/null 2>&1 && die "a self-alias was accepted" || ok "a seat cannot be an alias of itself"
"$BIN/oracle" alias set kin-old kin | grep >/dev/null 'kin-old → kin' && ok "oracle alias set writes one owner-signed record" || die "alias set failed"
"$BIN/oracle" alias list | grep >/dev/null '^kin-old → kin$' && ok "oracle alias list shows the table in force" || die "alias list"
ox_as kin asks --seat kin-old | grep -A6 'WAITING ON ME' | grep >/dev/null "$AL" && die "the ask stayed open under the old label after the alias" || ok "after the alias, kin's signed reply has closed the ask to kin-old"
g "/asks?seat=kin-old&state=all" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['canonical']=='kin' and all('$AL' not in [r['id'] for r in d[k]] for k in ('waiting_on_me',)), d" && ok "/asks says which seat the label meant" || die "/asks did not name the canonical seat"
AL2=$(ox_as rocky say room "and the changelog?" --intent proposal --ask act --to kin-old | grep -o 'OBS-[A-Za-z0-9-]*' | head -1)
g "/asks?seat=kin" | python3 -c "import json,sys; d=json.load(sys.stdin); r=[x for x in d['waiting_on_me'] if x['id']=='$AL2']; assert r and r[0]['held_by']==['kin-old'] and r[0]['addressee_aliases']=={'kin-old':'kin'}, d['waiting_on_me']" && ok "an ask to the old label is waiting on kin, still addressed to kin-old" || die "the original addressee was not kept beside the alias"
"$BIN/oracle" alias remove kin-old >/dev/null && ox_as kin asks --seat kin-old | grep -A6 'WAITING ON ME' | grep >/dev/null "$AL2" && ok "removing the alias makes the name a name again" || die "alias remove"

# The request ledger and unread, and the rule for reading another seat's lists (Emmy's Q1): audit mode records, enforce refuses an agent, a registered gateway reads.
KIN_TOK="$(tok kin)"
code_as(){ curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $1" "$URL$2"; }
has "GET /asked is a seat's request ledger, own by default" "/asked" "d['seat']=='rocky' and 'asked' in d and d['limit']==50 and d['next_before'] is None"
has "GET /rooms/{ch}/unread says what a seat has not read" "/rooms/room/unread" "d['seat']=='rocky' and d['room']=='room' and isinstance(d['unread'],int) and d['latest_seq'] is not None"
[ "$(code_as "$KIN_TOK" '/asked?state=finished')" = 422 ] && ok "an unknown item state is refused" || die "unknown item state accepted"
[ "$(code_as "" '/asked')" = 401 ] && ok "/asked needs a token" || die "asked answered without a token"
[ "$(code_as "$KIN_TOK" '/asked?seat=rocky')" = 200 ] && ok "audit mode (the default): another seat's ledger is let through" || die "audit mode refused a cross-seat read"
"$BIN/oracle" gateway audit --limit 5 | grep >/dev/null 'kin → rocky  asked  NOT allowed (audit mode: let through)' && ok "…and recorded as what enforcing would have refused" || die "the cross-seat read was not audited"
curl -sf "$URL/health" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['index']['ready'] is True and d['index']['through_seq']>0 and d['cross_seat']=='audit', d" && ok "/health: index ready, cross-seat mode audit" || die "/health index status"
kill $SRV; wait $SRV 2>/dev/null || true
ORACLE_CROSS_SEAT=enforce "$BIN/oracle" serve --bind "127.0.0.1:$PORT" 2>>"$T/server.log" & SRV=$!
for _ in $(seq 1 50); do curl -sf "$URL/health" >/dev/null && break; sleep 0.1; done
curl -sf "$URL/health" | python3 -c "import json,sys; assert json.load(sys.stdin)['cross_seat']=='enforce'" && ok "the server runs enforcing when told to" || die "enforce mode not reported"
for r in "asks?seat=rocky" "asked?seat=rocky" "rooms/room/unread?seat=rocky"; do
  [ "$(code_as "$KIN_TOK" "/$r")" = 403 ] || die "an agent token read another seat's $r"
done; ok "enforcing: an agent token reading another seat's asks, asked or unread gets 403"
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $KIN_TOK" -H 'X-Gateway: 1' -H 'X-Seat: rocky' "$URL/asked?seat=rocky&gateway=true&as=axxis-door")" = 403 ] && ok "no header or parameter makes a token a gateway" || die "a claim made a token a gateway"
[ "$(code_as "$KIN_TOK" '/asks')" = 200 ] && [ "$(code_as "$KIN_TOK" '/asked')" = 200 ] && [ "$(code_as "$KIN_TOK" '/rooms/room/unread')" = 200 ] && ok "enforcing: a seat still reads its own" || die "a seat could not read its own lists"
DOOR="$("$BIN/oracle" seat add axxis-door)"; DOOR_TOK="$(tok axxis-door)"
[ "$(code_as "$DOOR_TOK" '/asked?seat=rocky')" = 403 ] && ok "a seat that is not registered as a gateway is refused, whatever its name" || die "an unregistered door read another seat"
"$BIN/oracle" gateway add axxis-door --routes asked,unread >/dev/null
[ "$(code_as "$DOOR_TOK" '/asked?seat=rocky')" = 200 ] && [ "$(code_as "$DOOR_TOK" '/rooms/room/unread?seat=rocky')" = 200 ] && ok "a registered gateway reads another seat's asked and unread" || die "the gateway was refused"
[ "$(code_as "$DOOR_TOK" '/asks?seat=rocky')" = 403 ] && ok "and only the routes it was registered for" || die "the gateway read a route it was not registered for"
"$BIN/oracle" gateway audit --limit 30 | grep >/dev/null 'kin → rocky  asks  NOT allowed (refused)' && "$BIN/oracle" gateway audit --limit 30 | grep >/dev/null 'axxis-door → rocky  asked  allowed' && ok "every cross-seat read is audited, refused or allowed" || die "the audit is missing rows"
"$BIN/oracle" gateway remove axxis-door >/dev/null
[ "$(code_as "$DOOR_TOK" '/asked?seat=rocky')" = 403 ] && ok "removing the registration takes the access away" || die "a removed gateway still reads"

"$BIN/oracle" seat revoke kin >/dev/null
ox_as kin say room "still here?" >/dev/null 2>&1 && die "revoked seat can still write" || ok "a revoked seat and its tokens are dead"
KIN_CALLS=$(presence | python3 -c "import json,sys; print({r['handle']:r for r in json.load(sys.stdin)['seats']}['kin']['calls'])")
ox_as kin say room "still here?" >/dev/null 2>&1 || true
presence | python3 -c "
import json,sys
s={r['handle']:r for r in json.load(sys.stdin)['seats']}
assert s['kin']['state']=='retired', s['kin']
assert s['kin']['calls']==$KIN_CALLS, ('a refused request was counted', s['kin'])
" && ok "/presence: a revoked seat is retired, and its refused requests are not signs of life" || die "presence counted a refused request or missed the retirement"

# ── every route a seat may call, and every MCP tool, has an ox command (docs/ox.md, Blaze 17 Oct: "add the ox commands that are missing") ──
ox_as rocky health | grep >/dev/null '^oracle 0.1.0' && ox_as rocky health | grep >/dev/null '^index ' && ok "ox health: version, commit, index, search" || die "ox health"
ox_as rocky --json health | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["status"]=="ok" and "index" in d, d' || die "ox --json health is not the server's JSON"
# (the server restarted above to enforce, so its metrics start there: /presence is the route surely called since)
ox_as rocky metrics | grep >/dev/null '/presence' && ok "ox metrics: every called route with calls, errors, p50/p95/max" || die "ox metrics: $(ox_as rocky metrics 2>&1 | head -5)"
ox_as rocky metrics --route /presence | awk 'NR>2 && $2 !~ /presence/ {bad=1} END{exit bad}' && ok "ox metrics --route keeps only matching routes" || die "metrics --route let another route through"
ox_as rocky metrics --sort speed >/dev/null 2>&1 && die "metrics accepted a sort it does not have" || ok "ox metrics refuses an unknown --sort"
ox_as rocky presence | grep >/dev/null 'rocky .*live' && ox_as rocky presence | grep >/dev/null 'kin .*retired' && ok "ox presence: live and retired seats" || die "ox presence"
ox_as rocky seats | grep >/dev/null '^  kin .*REVOKED' && ox_as rocky seats | grep >/dev/null '^  rocky ' && ok "ox seats: the registry, a revoked key says so" || die "ox seats"
ox_as rocky seats --history | grep >/dev/null 'signed by blaze' && ok "ox seats --history: the signed key events" || die "ox seats --history"
ox_as rocky rooms | grep >/dev/null '#room ' && ox_as rocky rooms --since 1h | grep >/dev/null '#room ' && ok "ox rooms (and --since)" || die "ox rooms"
ox_as rocky rooms --since soon >/dev/null 2>&1 && die "rooms accepted a span that is not one" || ok "ox rooms refuses a span that is not one"
ox_as rocky --json threads room | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["room"]=="room" and d["threads"], d' && ox_as rocky threads room | grep >/dev/null '^#room — ' && ok "ox threads: a room's threads, paged" || die "ox threads"
ox_as rocky heads | grep >/dev/null 'ship-v1' && ox_as rocky heads --all | grep >/dev/null 'ship-v1' && ok "ox heads: mine and everyone's" || die "ox heads"
ox_as rocky heads --owner kin | grep >/dev/null 'ship-v1' && die "another owner's heads showed mine" || ok "ox heads --owner reads that owner's only"
ox_as rocky where general chatter | grep >/dev/null 'room' && ok "ox where: which rooms say it" || die "ox where"
ox_as rocky terms >/dev/null && ox_as rocky experiments >/dev/null && ok "ox terms and ox experiments answer" || die "ox terms / experiments"
ox_as rocky hello | grep >/dev/null 'rocky' && ok "ox hello: the first call for a new seat" || die "ox hello"
ox_as rocky state room --full | grep >/dev/null '^== ' && ox_as rocky digest room --agent >/dev/null && ok "ox state --full and digest --agent: the room reads for a seat arriving" || die "state --full / digest --agent"
ox_as rocky --json hello | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["tool"]=="oracle_hello" and d["text"], d' || die "ox --json hello"
ox_as rocky say room "tagged for the filter" --tag e2e:tagfilter >/dev/null
ox_as rocky say room "untagged neighbour" >/dev/null
TAGGED=$(ox_as rocky read room --tag e2e:tagfilter --limit 50)
echo "$TAGGED" | grep >/dev/null 'tagged for the filter' && ! echo "$TAGGED" | grep >/dev/null 'untagged neighbour' && ok "ox read --tag: only messages carrying the tag" || die "read --tag let an untagged message through"
# ox watch: live, narrowed to one room; a message elsewhere does not appear.
ox_as rocky watch --room room >"$T/watch.out" 2>"$T/watch.err" & WATCH=$!
for _ in $(seq 1 50); do grep >/dev/null 'watching' "$T/watch.err" 2>/dev/null && break; sleep 0.1; done
ox_as rocky say room "live in the room" >/dev/null; ox_as rocky say elsewhere "live elsewhere" >/dev/null
for _ in $(seq 1 50); do grep >/dev/null 'live in the room' "$T/watch.out" 2>/dev/null && break; sleep 0.1; done
grep >/dev/null 'live in the room' "$T/watch.out" && ! grep >/dev/null 'live elsewhere' "$T/watch.out" && ok "ox watch: new messages live, --room narrows" || die "ox watch: $(cat "$T/watch.out" "$T/watch.err")"
# docs/STREAM.md: a resume by Last-Event-ID replays only what came after that seq, each event with its seq as id.
R1=$(ox_as rocky say room "replay one" | awk '{print $2}'); ox_as rocky say room "replay two" >/dev/null
S1=$(g "/search?id=$R1" | python3 -c 'import json,sys; d=json.load(sys.stdin); print((d.get("observation") or d)["seq"])')
curl -sN --max-time 2 -H "Authorization: Bearer $TOK" -H "Last-Event-ID: $S1" "$URL/stream?room=room" >"$T/replay.out" || true
grep >/dev/null 'replay two' "$T/replay.out" && ! grep >/dev/null 'replay one' "$T/replay.out" && grep >/dev/null "^id: $((S1 + 1))\$" "$T/replay.out" \
  && ok "/stream: Last-Event-ID replays what came after it, with id: <seq>" || die "/stream resume: $(head -c 600 "$T/replay.out")"
# ox watch outlives a server restart: it reconnects with Last-Event-ID and carries on.
kill $SRV; wait $SRV 2>/dev/null || true
ORACLE_CROSS_SEAT=enforce "$BIN/oracle" serve --bind "127.0.0.1:$PORT" 2>>"$T/server.log" & SRV=$!
for _ in $(seq 1 50); do curl -sf "$URL/health" >/dev/null && break; sleep 0.1; done
for _ in $(seq 1 150); do grep >/dev/null 'reconnected' "$T/watch.err" 2>/dev/null && break; sleep 0.1; done
ox_as rocky say room "live after the restart" >/dev/null
for _ in $(seq 1 50); do grep >/dev/null 'live after the restart' "$T/watch.out" 2>/dev/null && break; sleep 0.1; done
kill $WATCH 2>/dev/null || true; wait $WATCH 2>/dev/null || true
grep >/dev/null 'live after the restart' "$T/watch.out" && [ "$(grep -c 'replay two' "$T/watch.out")" = 1 ] \
  && ok "ox watch: reconnects after a server restart, nothing twice" || die "ox watch after restart: $(cat "$T/watch.out" "$T/watch.err")"
# Parity: every tool the MCP door lists is named in docs/ox.md, so a new tool without an ox command fails here.
mcp '{"jsonrpc":"2.0","id":91,"method":"tools/list"}' | python3 -c '
import json,re,sys
tools={t["name"] for t in json.load(sys.stdin)["result"]["tools"] if t["name"].startswith("oracle_")}
doc=open("docs/ox.md").read()
missing=sorted(t for t in tools if "`"+t+"`" not in doc)
assert not missing, "MCP tools with no ox row in docs/ox.md: %s" % missing
' && ok "parity: every oracle_* MCP tool has its ox command in docs/ox.md" || die "an MCP tool has no ox command"
echo "e2e: all claims hold."
