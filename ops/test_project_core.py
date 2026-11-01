"""Projection skips rows that came from the graph, projects the rest, and never stalls the cursor; run: python3 ops/test_project_core.py"""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'compat'))
import project_core as p

rows = [{'seq': 1, 'tags': ['from:rocky']},
        {'seq': 2, 'tags': ['legacy:graph-memory', 'migrated_from:graph-oracle']},
        {'seq': 3, 'tags': None},
        {'seq': 4, 'tags': ['migrated_from:graph-oracle', 'legacy:graph-dotmessage']}]
projected, marks = [], []
skipped = p.project_rows(rows, lambda r: projected.append(r['seq']), marks.append)
assert projected == [1, 3], projected
assert marks == [1, 2, 3, 4], 'the cursor passes skipped rows, so a later live row is never blocked'
assert skipped == 2
assert not p.came_from_graph({'tags': ['migrated_from:graph-oracle-lookalike']})
print('projection skip ok')
