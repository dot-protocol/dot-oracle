#!/usr/bin/env python3
"""Real dependency checks and a known-advisory negative control; no fixture is built."""
import json
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def run(args, **kwargs):
    return subprocess.run(args, cwd=ROOT, text=True, **kwargs)


with tempfile.TemporaryDirectory(prefix="oracle-dependency-security-") as tmp:
    tmp = Path(tmp)
    db = tmp / "advisory-db"
    # Use system git: audit's embedded HTTP client is not supported by every proxy.
    # Failure to fetch is fatal; never accept a stale/unavailable database as clean.
    run(["git", "clone", "--depth", "1", "https://github.com/RustSec/advisory-db", str(db)], check=True)
    run(["git", "-C", str(db), "rev-parse", "HEAD"], check=True)
    audit = ["cargo", "audit", "--db", str(db), "--no-fetch", "--deny", "warnings"]
    run(audit, check=True)
    run(["cargo", "deny", "--locked", "check", "advisories", "licenses", "bans", "sources"], check=True)

    fixture = tmp / "known-vulnerable"
    fixture.mkdir()
    (fixture / "Cargo.lock").write_text('''version = 3

[[package]]
name = "time"
version = "0.1.45"
source = "registry+https://github.com/rust-lang/crates.io-index"
''')
    result = run(audit + ["--file", str(fixture / "Cargo.lock"), "--json"], capture_output=True)
    report = json.loads(result.stdout)
    ids = [v["advisory"]["id"] for v in report["vulnerabilities"]["list"]]
    if result.returncode == 0 or "RUSTSEC-2020-0071" not in ids:
        raise SystemExit("audit negative control failed: expected RUSTSEC-2020-0071")

    # Give deny a real dependency graph. Resolve metadata only: never compile/run it.
    (fixture / "Cargo.toml").write_text('''[package]
name = "known-advisory-control"
version = "0.0.0"
edition = "2021"
[dependencies]
time = "=0.1.45"
[workspace]
''')
    (fixture / "src").mkdir()
    (fixture / "src/lib.rs").write_text("// Intentionally empty; metadata-only negative control.\n")
    run(["cargo", "generate-lockfile", "--manifest-path", str(fixture / "Cargo.toml")], check=True)
    result = run(["cargo", "deny", "--manifest-path", str(fixture / "Cargo.toml"),
                  "--locked", "--config", str(ROOT / "deny.toml"), "check", "advisories"], capture_output=True)
    if result.returncode == 0 or "RUSTSEC-2020-0071" not in result.stdout + result.stderr:
        raise SystemExit("deny negative control failed: expected RUSTSEC-2020-0071\n" + result.stderr)
    print("audit and deny rejected the known-advisory fixture; real dependencies passed")
