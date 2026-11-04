#!/bin/sh
# No phone home (Blaze, 4 Nov 2026, card, task oracle.no-phone-home): the first minute
# of a new Oracle, run where the only network is loopback, with every connect traced. Exit 0 only if no process tried
# to reach a host the user did not configure: a connect outside 127.0.0.0/8 or ::1 fails it, and so does any DNS query
# (port 53), because looking a name up is already reaching out. NO_PHONE_HOME_CONTROL=1 adds one outbound connect on
# purpose; the test must then fail (the negative control).
# Linux only; needs strace, and either unprivileged user namespaces or root.
set -eu
bin="${ORACLE_BIN_DIR:-$(dirname "$(command -v ox)")}"
if [ "${NO_PHONE_HOME_INNER:-}" != 1 ]; then
  command -v strace >/dev/null || { echo "no-phone-home: needs strace" >&2; exit 2; }
  work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
  if unshare -rn true 2>/dev/null; then ns="unshare -rn"; else ns="unshare -n"; fi
  NO_PHONE_HOME_INNER=1 WORK="$work" ORACLE_BIN_DIR="$bin" $ns sh -c \
    'ip link set lo up && exec strace -f -qq -e trace=connect -o "$WORK/trace" sh "$0"' "$0" > "$work/run.log" 2>&1 || {
    echo "no-phone-home: the first minute failed inside the sandbox" >&2; cat "$work/run.log" >&2; exit 1; }
  # Every connect to an inet address that is not loopback, and every connect to port 53.
  bad="$(grep -E 'sa_family=AF_INET6?' "$work/trace" \
    | grep -vE 'inet_addr\("127\.|inet_pton\(AF_INET6, "(::1|::ffff:127\.[0-9.]+)"' || true)"
  dns="$(grep -E 'htons\(53\)' "$work/trace" || true)"
  n="$(grep -c 'connect(' "$work/trace" || true)"
  if [ -n "$bad$dns" ]; then
    echo "no-phone-home: FAIL, a process reached outside loopback or asked DNS:" >&2
    printf '%s\n%s\n' "$bad" "$dns" | grep . >&2
    exit 1
  fi
  echo "NO PHONE HOME OK: $n connects traced, all loopback or local sockets"
  exit 0
fi
# Inside the sandbox: the README's first minute, plus the reads a seat does on waking.
export PATH="$ORACLE_BIN_DIR:$PATH" HOME="$WORK/home" ORACLE_HOME="$WORK/oracle"
mkdir -p "$HOME"
oracle init --owner smoke
oracle serve > "$WORK/serve.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true' EXIT
up=0; for _ in $(seq 1 60); do if curl -fsS http://127.0.0.1:8900/health >/dev/null 2>&1; then up=1; break; fi; sleep 1; done
[ "$up" = 1 ] || { echo "oracle serve did not answer /health in 60 s" >&2; cat "$WORK/serve.log" >&2; exit 1; }
oracle seat add agent
ox setup --url http://127.0.0.1:8900 --handle agent --token-file "$ORACLE_HOME/keys/agent.token" --key "$ORACLE_HOME/keys/agent.key"
ox doctor
ox say smoke "the first memory on this machine"
ox read smoke --limit 1 | grep -q "the first memory on this machine"
ox remember --kind lesson "a lesson written with no network at all" >/dev/null
ox recall "first memory" >/dev/null
ox brief >/dev/null
ox inbox --mine >/dev/null
ox now >/dev/null
oracle up --plan --home "$WORK/node" --ox-home "$WORK/node-ox" --no-service --agents none >/dev/null
if [ "${NO_PHONE_HOME_CONTROL:-}" = 1 ]; then curl -s -m 2 http://93.184.215.14/ >/dev/null 2>&1 || true; fi
echo "first minute done"
