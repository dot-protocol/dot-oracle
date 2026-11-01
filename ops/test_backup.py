"""Backup keeps the newest ORACLE_BACKUP_KEEP backups and their receipts; run: python3 ops/test_backup.py"""
import os, sqlite3, subprocess, sys, tempfile
from pathlib import Path

here = Path(__file__).resolve().parent
src = (here / "backup.py").read_text().replace("file:/var/lib/oracle-core/oracle.db", "file:{db}")
with tempfile.TemporaryDirectory() as t:
    t = Path(t); db = t / "oracle.db"; out = t / "backups"; out.mkdir()
    c = sqlite3.connect(db); c.execute("CREATE TABLE observations(id)"); c.execute("INSERT INTO observations VALUES (1)"); c.commit(); c.close()
    for day in range(20, 25):
        (out / f"202609{day}T031000Z.db").write_text("x"); (out / f"202609{day}T031000Z.json").write_text("{}")
    (out / "keep-me.txt").write_text("not a backup")
    script = t / "backup.py"; script.write_text(src.replace("{db}", str(db)))
    subprocess.run([sys.executable, script], env={**os.environ, "ORACLE_BACKUP_DIR": str(out)}, check=True, capture_output=True)
    dbs = sorted(p.name for p in out.glob("*.db"))
    assert len(dbs) == 3 and dbs[:2] == ["20260923T031000Z.db", "20260924T031000Z.db"], dbs
    assert sorted(p.stem for p in out.glob("*.json")) == [p[:-3] for p in dbs], "each kept backup keeps its receipt, removed ones lose theirs"
    assert (out / "keep-me.txt").exists()
print("backup retention ok")
