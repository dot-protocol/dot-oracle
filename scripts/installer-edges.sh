#!/bin/sh
# The installer's edges on a clean macOS or Linux machine (plan.oracle.everyone J7; docs/OPEN-SOURCE-RELEASE.md §2):
# what smoke-install.sh does not show. Given a directory of real release files, it proves five things:
#   space      a home whose path holds a space installs and runs;
#   rerun      running the installer again changes nothing (same bytes, exit 0);
#   data       re-installing over an Oracle in use leaves its home (database, keys) byte for byte as it was;
#   tampered   an archive changed after its checksum was taken is refused, and the installed binaries stay as they were;
#   offline    with no route to the release, it fails within a minute, says why, and installs nothing.
# INSTALLER_EDGES_CONTROL=1 serves the untampered archive in the tampered case, so that case must fail (the negative
# control: it proves the test can see a refusal that did not happen).
# Usage: sh scripts/installer-edges.sh <dist-dir>   (the files the release workflow builds: oracle-<target>.tar.gz + .sha256)
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
dist="$(cd "${1:?usage: installer-edges.sh <dist-dir>}" && pwd)"
work="$(mktemp -d)"
pids=
cleanup() { for p in $pids; do kill "$p" 2>/dev/null || true; done; rm -rf "$work"; }
trap cleanup EXIT
fail() { echo "installer-edges: FAIL $*" >&2; exit 1; }
sum() { (sha256sum "$1" 2>/dev/null || shasum -a 256 "$1") | cut -d' ' -f1; }

# Two copies of the release, each served over HTTP as GitHub serves one: as built, and with the archive tampered.
serve() { # dir port
  python3 -m http.server "$2" --bind 127.0.0.1 --directory "$1" > /dev/null 2>&1 &
  pids="$pids $!"
  for _ in $(seq 1 50); do curl -fsS "http://127.0.0.1:$2/" > /dev/null 2>&1 && return 0; sleep 0.1; done
  fail "the test server on port $2 did not start"
}
mkdir "$work/good" "$work/bad"
cp "$dist"/oracle-*.tar.gz "$dist"/oracle-*.tar.gz.sha256 "$work/good/"
cp "$work/good/"* "$work/bad/"
if [ "${INSTALLER_EDGES_CONTROL:-}" != 1 ]; then
  for a in "$work/bad"/oracle-*.tar.gz; do printf 'x' >> "$a"; done   # one byte appended; the .sha256 is the original
fi
serve "$work/good" 8781
serve "$work/bad" 8782

# space: HOME holds a space, and the default install directory under it does too.
home="$work/home with space"; mkdir -p "$home"
HOME="$home" ORACLE_DOWNLOAD_BASE=http://127.0.0.1:8781 sh "$here/install.sh" > "$work/space.log" 2>&1 \
  || { cat "$work/space.log" >&2; fail "space: the install failed"; }
bin="$home/.local/bin"
"$bin/ox" --version > /dev/null || fail "space: the installed ox does not run"
"$bin/oracle" --version > /dev/null || fail "space: the installed oracle does not run"
echo "space: passed ($bin)"

# rerun: the same release again; the same bytes, exit 0.
before="$(sum "$bin/ox") $(sum "$bin/oracle")"
HOME="$home" ORACLE_DOWNLOAD_BASE=http://127.0.0.1:8781 sh "$here/install.sh" > "$work/rerun.log" 2>&1 \
  || { cat "$work/rerun.log" >&2; fail "rerun: the second install failed"; }
[ "$before" = "$(sum "$bin/ox") $(sum "$bin/oracle")" ] || fail "rerun: the second install changed the binaries"
echo "rerun: passed"

# data: an Oracle in use (init writes its database and keys); re-install; every file of its home unchanged.
ohome="$home/.oracle-server"
ORACLE_HOME="$ohome" "$bin/oracle" init --owner edges > /dev/null
snap() { (cd "$ohome" && find . -type f | LC_ALL=C sort | while IFS= read -r f; do printf '%s %s\n' "$(sum "$f")" "$f"; done); }
snap > "$work/data.before"
[ -s "$work/data.before" ] || fail "data: init wrote nothing to compare"
HOME="$home" ORACLE_DOWNLOAD_BASE=http://127.0.0.1:8781 sh "$here/install.sh" > /dev/null 2>&1 || fail "data: the re-install failed"
snap > "$work/data.after"
cmp -s "$work/data.before" "$work/data.after" || { diff "$work/data.before" "$work/data.after" >&2 || true; fail "data: re-installing changed the Oracle's home"; }
echo "data: passed ($(wc -l < "$work/data.before" | tr -d ' ') files unchanged)"

# tampered: refused, says so, and the installed binaries are the ones from before.
if HOME="$home" ORACLE_DOWNLOAD_BASE=http://127.0.0.1:8782 sh "$here/install.sh" > "$work/tampered.log" 2>&1; then
  fail "tampered: a changed archive was installed"
fi
grep -q "checksum mismatch; nothing installed" "$work/tampered.log" || { cat "$work/tampered.log" >&2; fail "tampered: refused without saying why"; }
[ "$before" = "$(sum "$bin/ox") $(sum "$bin/oracle")" ] || fail "tampered: the refused install changed the binaries"
echo "tampered: passed (refused: checksum mismatch)"

# offline: port 9 on loopback is closed; nothing listens, as when there is no network.
empty="$work/offline-bin"
start="$(date +%s)"
if ORACLE_BIN_DIR="$empty" ORACLE_DOWNLOAD_BASE=http://127.0.0.1:9 sh "$here/install.sh" > "$work/offline.log" 2>&1; then
  fail "offline: the install claimed success with no route to the release"
fi
took=$(( $(date +%s) - start ))
grep -q "download failed: .*nothing installed" "$work/offline.log" || { cat "$work/offline.log" >&2; fail "offline: failed without saying why"; }
[ ! -e "$empty/ox" ] && [ ! -e "$empty/oracle" ] || fail "offline: something was installed"
[ "$took" -le 60 ] || fail "offline: it took ${took} s to fail"
echo "offline: passed (${took} s, nothing installed)"

echo "INSTALLER EDGES OK $(uname -s) $(uname -m)"
