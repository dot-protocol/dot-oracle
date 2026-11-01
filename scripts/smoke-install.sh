#!/bin/sh
# The README's install and first minute on a clean macOS or Linux machine, as written there. Exit 0 only if every step
# passes: the installer, a new Oracle, a seat, ox doctor, and one signed memory read back. The release workflow runs it
# on a fresh machine per platform before anything is published (Blaze, 1 Nov 2026: "failing is failing").
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
sh "$here/install.sh"
export PATH="${ORACLE_BIN_DIR:-$HOME/.local/bin}:$PATH"
export ORACLE_HOME="${SMOKE_HOME:-$HOME/.oracle-server}"
oracle init --owner smoke
oracle serve > "$ORACLE_HOME/serve.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true' EXIT
up=0; for _ in $(seq 1 60); do if curl -fsS http://127.0.0.1:8900/health >/dev/null 2>&1; then up=1; break; fi; sleep 1; done
[ "$up" = 1 ] || { echo "oracle serve did not answer /health in 60 s" >&2; cat "$ORACLE_HOME/serve.log" >&2; exit 1; }
oracle seat add agent
ox setup --url http://127.0.0.1:8900 --handle agent --token-file "$ORACLE_HOME/keys/agent.token" --key "$ORACLE_HOME/keys/agent.key"
ox doctor
ox say smoke "the first memory on this machine"
ox read smoke --limit 1 | grep -q "the first memory on this machine" || { echo "the memory was not read back" >&2; exit 1; }
echo "SMOKE OK $(uname -s) $(uname -m)"
