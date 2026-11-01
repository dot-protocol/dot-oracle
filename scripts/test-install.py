"""Offline installer integrity checks; never downloads or executes a release binary."""
import hashlib
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile


installer = Path(__file__).resolve().with_name("install.sh")
with tempfile.TemporaryDirectory(prefix="oracle-install-test-") as tmp:
    root = Path(tmp)
    archive = root / "release.tar.gz"
    with tarfile.open(archive, "w:gz") as tar:
        for name in ("oracle", "ox"):
            body = b"test fixture, never executed\n"
            entry = tarfile.TarInfo(name)
            entry.size = len(body)
            tar.addfile(entry, io.BytesIO(body))
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    mock = root / "bin"
    mock.mkdir()
    curl = mock / "curl"
    curl.write_text("""#!/bin/sh
set -eu
url=; dest=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) dest="$2"; shift 2 ;;
    http*) url="$1"; shift ;;
    *) shift ;;
  esac
done
case "$url" in
  *.sha256)
    [ "$TEST_CASE" != missing ] || exit 22
    cp "$TEST_CHECKSUM" "$dest" ;;
  *) cp "$TEST_ARCHIVE" "$dest" ;;
esac
""")
    curl.chmod(0o755)
    for case, checksum in (
        ("valid", digest + "  release.tar.gz\n"),
        ("missing", ""),
        ("mismatch", "0" * 64 + "  release.tar.gz\n"),
        ("malformed", "not-a-sha256\n"),
    ):
        dest = root / case
        dest.mkdir()
        for name in ("oracle", "ox"):
            (dest / name).write_bytes(b"existing installation\n")
        sums = root / "checksum"
        sums.write_text(checksum)
        env = dict(os.environ, PATH=str(mock) + os.pathsep + os.environ["PATH"],
                   ORACLE_BIN_DIR=str(dest), TEST_CASE=case,
                   TEST_ARCHIVE=str(archive), TEST_CHECKSUM=str(sums))
        result = subprocess.run(["sh", str(installer)], env=env, capture_output=True, text=True)
        assert (result.returncode == 0) == (case == "valid"), (case, result.stderr)
        expected = b"test fixture, never executed\n" if case == "valid" else b"existing installation\n"
        assert all((dest / name).read_bytes() == expected for name in ("oracle", "ox")), case
        print(f"{case}: passed")
