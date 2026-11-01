import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
ROOT=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT/'compat'))
import oracle_core_gateway as gate
spec=importlib.util.spec_from_file_location('project',ROOT/'ops/project_core.py')
project=importlib.util.module_from_spec(spec);spec.loader.exec_module(project)

class Gateway(unittest.TestCase):
    def test_lossless_unsigned_identity_and_failure(self):
        source={'id':'OBS-test','statement':'hello @kin','evidence':'x'*1000001,'confidence':'speculative','channel':'room','timestamp':'2026-09-21T00:00:00Z','tags':['from:rocky','signed:token']+['t:'+str(i) for i in range(40)],'signature':'','pubkey':'','custom':'retained'}
        class Obs:
            def model_dump_json(self): return json.dumps(source)
        row=gate.observation_row(Obs())
        self.assertEqual(row['source']['observation'],source)
        self.assertEqual(len(row['cargo']),1000001)
        self.assertEqual(row['tags'],source['tags'])
        self.assertEqual(row['sig'],'')
        with patch.dict(os.environ,{'ORACLE_CORE_AUTHORITY':'1'}),patch.object(gate,'call',side_effect=RuntimeError('down')):
            with self.assertRaises(RuntimeError): gate.accept_observation(Obs())

    def test_catchup_spans_more_than_one_window(self):
        pages={None:{'items':[{'seq':n} for n in range(1300,800,-1)],'next_before':801},801:{'items':[{'seq':n} for n in range(800,300,-1)],'next_before':301},301:{'items':[{'seq':n} for n in range(300,0,-1)],'next_before':None}}
        def fetch(path): return pages[int(path.split('before=')[1]) if 'before=' in path else None]
        rows=project.fresh_rows(fetch,100)
        self.assertEqual([r['seq'] for r in rows],list(range(101,1301)))

    def test_original_id_and_trust_are_retained(self):
        r={'seq':1,'id':'OBS-native','channel':'room','author':'rocky','content':'hello','created_ms':1,'kind':'say','envelope':2,'tags':[],'sig':'original'}
        p=project.properties(r)
        self.assertEqual(p['id'],r['id']);self.assertEqual(p['signature'],'original')
        self.assertIn('signature-scheme:oracle-say/2',p['tags'])
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'cursor';project.checkpoint(path,123);self.assertEqual(path.read_text(),'123')

if __name__=='__main__': unittest.main()

class ProjectionCredential(unittest.TestCase):
    def test_uses_the_running_legacy_servers_token_variable(self):
        from unittest.mock import MagicMock
        response=MagicMock();response.__enter__.return_value=response
        response.read.return_value=b'{"ok":true,"id":"OBS-test"}'
        with patch.dict(os.environ,{'TREE_AUTH_TOKEN':'right','ORACLE_AUTH_TOKEN':'stale'}),patch('urllib.request.urlopen',return_value=response) as send:
            project.project({'id':'OBS-test'})
            self.assertEqual(send.call_args.args[0].get_header('Authorization'),'Bearer right')
