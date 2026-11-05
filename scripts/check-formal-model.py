#!/usr/bin/env python3
"""Check the finite TLA+ model and four deliberately broken variants with TLC."""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
TLC_URL = "https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar"
TLC_SHA256 = "936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"
CASES = {"correct": None, "BreakAppend": "AppendOnly", "DropWithheld": "WithheldWhole",
         "LeakScope": "ScopeSafety", "LeakPrivate": "PrivateSafety"}

with tempfile.TemporaryDirectory(prefix="oracle-formal-") as tmp:
    work = Path(tmp)
    jar = Path(os.environ.get("TLA2TOOLS_JAR", str(work / "tla2tools.jar"))).resolve()
    if not jar.exists():
        subprocess.run(["curl", "--fail", "--location", "--max-time", "120", TLC_URL, "-o", str(jar)], check=True)
    if hashlib.sha256(jar.read_bytes()).hexdigest() != TLC_SHA256:
        raise SystemExit("TLC jar digest mismatch")
    output = Path(os.environ.get("ORACLE_FORMAL_OUTPUT", str(work / "evidence"))).resolve()
    output.mkdir(parents=True, exist_ok=True)
    model = ROOT / "spec/OracleSecurity.tla"
    config = (ROOT / "spec/OracleSecurity.cfg").read_text()
    for name, violated in CASES.items():
        case = work / name
        case.mkdir()
        shutil.copyfile(model, case / model.name)
        text = config if violated is None else config.replace(f"{name} = FALSE", f"{name} = TRUE")
        (case / "OracleSecurity.cfg").write_text(text)
        result = subprocess.run([os.environ.get("JAVA", "java"), "-XX:+UseParallelGC", "-Xmx1g",
                                 "-cp", str(jar), "tlc2.TLC", "-workers", "1", "-config",
                                 "OracleSecurity.cfg", "OracleSecurity.tla"], cwd=case,
                                capture_output=True, text=True, timeout=120)
        log = result.stdout + result.stderr
        (output / f"{name}.log").write_text(log)
        if violated is None:
            valid = result.returncode == 0 and "Model checking completed. No error has been found." in log
        else:
            valid = result.returncode != 0 and f"Invariant {violated} is violated" in log
        if not valid:
            raise SystemExit(f"{name}: unexpected checker result\n{log}")
        print(f"{name}: {'no violations' if violated is None else violated + ' violation detected'}")
    print("Finite model and four negative controls passed; no Rust refinement claim.")
