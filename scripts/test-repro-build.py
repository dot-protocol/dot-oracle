#!/usr/bin/env python3
"""Exercise the reproducibility gate with fake compilers, not a reproducibility claim."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

SCRIPT = Path(__file__).with_name("repro-build.sh").resolve()


def run_case(mode, mismatch=False):
    with tempfile.TemporaryDirectory(prefix="oracle-repro-test-") as tmp:
        root = Path(tmp)
        repo = root / "repo"
        (repo / "scripts").mkdir(parents=True)
        shutil.copyfile(SCRIPT, repo / "scripts/repro-build.sh")
        for args in (["init", "-q"], ["add", "."],
                     ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                      "-c", "commit.gpgsign=false", "commit", "-qm", "fixture"]):
            subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True)
        bin_dir = root / "bin"
        bin_dir.mkdir()
        # Compile logs must show different source and target paths, with no prior target state.
        cargo = bin_dir / "cargo"
        cargo.write_text('''#!/usr/bin/env bash
set -eu
if [ "$1" = -V ]; then echo fixture-cargo; exit; fi
test ! -e "$CARGO_TARGET_DIR"
printf '%s|%s\\n' "$PWD" "$CARGO_TARGET_DIR" >> "$FIXTURE_LOG"
mkdir -p "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release"
for name in oracle ox; do
  text=identical
  if [ "$FIXTURE_MODE" = diverge ]; then text="$PWD"; fi
  printf '%s' "$text" > "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/$name"
done
''')
        cargo.chmod(0o700)
        rustc = bin_dir / "rustc"
        rustc.write_text("#!/bin/sh\necho fixture-rustc\n")
        rustc.chmod(0o700)
        reference = root / "reference"
        reference.mkdir()
        for name in ("oracle", "ox"):
            (reference / name).write_text("wrong-release" if mismatch else "identical")
        env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}",
                   REPRO_OUTPUT=str(root / "evidence"), REPRO_REFERENCE=str(reference),
                   FIXTURE_LOG=str(root / "builds"), FIXTURE_MODE=mode)
        result = subprocess.run(["bash", "scripts/repro-build.sh"], cwd=repo, env=env,
                                capture_output=True, text=True)
        expected = mode == "same" and not mismatch
        assert (result.returncode == 0) == expected, result.stdout + result.stderr
        paths = (root / "builds").read_text().splitlines()
        assert len(paths) == 2 and paths[0] != paths[1], paths
        for row in paths:
            source, target = row.split("|")
            assert target == source + "/target"
            assert not Path(source).exists(), "temporary source was not cleaned"
        assert (root / "evidence/release.sha256").exists()
        print(f"{mode}, release mismatch={mismatch}: passed")


run_case("same")
run_case("diverge")
run_case("same", mismatch=True)
