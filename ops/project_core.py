#!/usr/bin/env python3
"""Durable one-way Core -> legacy graph projection, retaining original IDs.

The cursor advances only after a graph transaction commits. Failed rows are retried,
never skipped. No sender impersonation, room/say call, or signature fabrication.
"""
import datetime as dt
import json
import os
from pathlib import Path
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'compat'))
from oracle_core_gateway import call


def fresh_rows(fetch, cursor):
    before = None
    rows = []
    while True:
        path = '/search?limit=500' + (f'&before={before}' if before is not None else '')
        page = fetch(path)
        items = page.get('items', [])
        rows.extend(x for x in items if x['seq'] > cursor)
        if not items or min(x['seq'] for x in items) <= cursor:
            break
        nxt = page.get('next_before')
        if nxt is None:
            break
        if before is not None and nxt >= before:
            raise RuntimeError('non-advancing Core page')
        before = nxt
    return sorted(rows, key=lambda x: x['seq'])


def checkpoint(path, seq):
    path = Path(path)
    tmp = path.with_suffix('.tmp')
    with open(tmp, 'w') as f:
        f.write(str(seq)); f.flush(); os.fsync(f.fileno())
    os.replace(tmp, path)
    fd = os.open(path.parent, os.O_RDONLY)
    try: os.fsync(fd)
    finally: os.close(fd)


def properties(row, source=None):
    iso = dt.datetime.fromtimestamp(row['created_ms']/1000, dt.timezone.utc).isoformat()
    tags = list(dict.fromkeys(row.get('tags', []) + [f"from:{row['author']}", f"kind:{row['kind']}"]))
    # This marker states Core verified v2; historical imported trust tags are preserved.
    if row.get('envelope') == 2:
        tags = [t for t in tags if not t.startswith(('signed:', 'sig:', 'pk:'))]
        tags += ['signed:ok', 'via:oracle-core', 'signature-scheme:oracle-say/2']
    if row['kind'] == 'state' and row.get('envelope') == 2:
        cargo = json.loads(row.get('cargo') or '{}')
        tags += ['live-state', 'subject:'+cargo['subject'], 'state:'+cargo['state'], 'owner:'+cargo.get('owner',row['author'])]
        if cargo.get('next_touch'): tags.append('clock:'+cargo['next_touch'])
    src = source or {}
    return {**src, 'id':row['id'], 'statement':row['content'], 'evidence':row.get('cargo') or '',
            'confidence':src.get('confidence','speculative'), 'channel':row['channel'],
            'source_session':src.get('source_session','oracle-core'), 'tags':tags,
            'tags_text':' '.join(tags), 'pubkey':row.get('pubkey',''), 'signature':row.get('sig',''),
            'agent_id':src.get('agent_id',row['author']), 'run_id':src.get('run_id','unknown'),
            'app_id':src.get('app_id','unknown'), 'chain_hash':src.get('chain_hash',''),
            'created_at':src.get('created_at',iso), 'timestamp':src.get('timestamp',iso), 'core_seq':row['seq']}


def project(row=None):
    import urllib.request
    token = os.environ.get('TREE_AUTH_TOKEN')
    if not token: raise RuntimeError('legacy projection credential missing')
    req = urllib.request.Request('http://127.0.0.1:8892/internal/' + ('core-project' if row else 'core-index'),
        data=json.dumps({'id':row['id']} if row else {}).encode(),headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'})
    with urllib.request.urlopen(req,timeout=30) as response:
        result=json.load(response)
    if not result.get('ok') or (row and result.get('id') != row['id']):
        raise RuntimeError('projection receipt mismatch')


def came_from_graph(row):
    """History imported FROM the graph (ops/import_graph_legacy.py) is already there."""
    return 'migrated_from:graph-oracle' in (row.get('tags') or [])


def project_rows(rows, project, checkpoint_to):
    """Project each row in seq order; the cursor still passes a row that needs no projection."""
    skipped = 0
    for row in rows:
        if came_from_graph(row):
            skipped += 1
        else:
            project(row)
        checkpoint_to(row['seq'])
    return skipped


def main():
    cursor = Path(os.environ.get('ORACLE_PROJECTION_CURSOR','/var/lib/oracle-projection/cursor'))
    if not cursor.exists():
        raise SystemExit('Initialize cursor explicitly after recording the cutover watermark')
    while True:
        try:
            mark = int(cursor.read_text())
            rows = fresh_rows(call,mark)
            skipped = project_rows(rows, project, lambda seq: checkpoint(cursor, seq))
            if rows: print(f"projected through {rows[-1]['seq']} ({len(rows)-skipped} rows, {skipped} graph-origin skipped)",flush=True)
        except Exception as exc:
            print('projection paused; retrying: '+type(exc).__name__,flush=True)
        time.sleep(2)

if __name__ == '__main__':
    if '--index' in sys.argv: project()
    else: main()
