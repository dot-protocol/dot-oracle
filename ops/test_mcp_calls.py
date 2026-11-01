# The call log against a fake Oracle: a read is repeated and its answer kept; a write is never repeated; no token on disk.
import importlib.util, json, os, sys, tempfile, threading, time, unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
SEEN = []

class Fake(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _j(self, body):
        b = json.dumps(body).encode(); self.send_response(200); self.send_header('Content-Type', 'application/json'); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        self._j({'handle': 'claude-room'})
    def do_POST(self):
        m = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        SEEN.append(m.get('method') + ':' + str((m.get('params') or {}).get('name')))
        if m['method'] == 'tools/list':
            return self._j({'result': {'tools': [{'name': 'oracle_recall', 'annotations': {'readOnlyHint': True}}, {'name': 'oracle_say', 'annotations': {'readOnlyHint': False}}]}})
        return self._j({'result': {'content': [{'type': 'text', 'text': 'found 3 things about ' + m['params']['arguments'].get('q', '')}]}})

class T(unittest.TestCase):
    def test_log(self):
        fake = ThreadingHTTPServer(('127.0.0.1', 0), Fake); threading.Thread(target=fake.serve_forever, daemon=True).start()
        d = tempfile.mkdtemp()
        os.environ['ORACLE_MCP_UPSTREAM'] = 'http://127.0.0.1:%d' % fake.server_address[1]
        os.environ['ORACLE_CALLS_FILE'] = os.path.join(d, 'calls.jsonl')
        spec = importlib.util.spec_from_file_location('mcp_calls', os.path.join(HERE, 'mcp_calls.py')); mc = importlib.util.module_from_spec(spec); spec.loader.exec_module(mc)
        secret = 'tok-SECRET-123'
        hdr = {'Authorization': 'Bearer ' + secret, 'User-Agent': 'Claude-User'}
        mc.handle_mirror(json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call', 'params': {'name': 'oracle_recall', 'arguments': {'q': 'search'}}}).encode(), hdr)
        mc.handle_mirror(json.dumps({'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call', 'params': {'name': 'oracle_say', 'arguments': {'room': 'x', 'text': 'hi'}}}).encode(), hdr)
        mc.handle_mirror(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}).encode(), hdr)
        mc.handle_mirror(json.dumps({'jsonrpc': '2.0', 'id': 3, 'method': 'tools/list'}).encode(), hdr)
        calls = mc.read_calls()
        self.assertEqual([c.get('tool') or c['method'] for c in calls], ['tools/list', 'oracle_say', 'oracle_recall'], 'a notification is not a call; tools/list is recorded')
        calls = calls[1:]
        self.assertEqual(calls[1]['seat'], 'claude-room')
        self.assertEqual(calls[1]['answer']['text'], 'found 3 things about search')
        self.assertIsNone(calls[0]['answer'])
        self.assertNotIn('tools/call:oracle_say', SEEN, 'a write is never repeated')
        self.assertIn('tools/call:oracle_recall', SEEN)
        self.assertNotIn(secret, open(os.environ['ORACLE_CALLS_FILE']).read(), 'no token on disk')
        self.assertEqual(oct(os.stat(os.environ['ORACLE_CALLS_FILE']).st_mode & 0o777), '0o600')
        # nginx forwards an HTTP/2 client's headers in lower case (2026-10-08: every real call was skipped as anonymous)
        mc.handle_mirror(json.dumps({'jsonrpc': '2.0', 'id': 4, 'method': 'tools/call', 'params': {'name': 'oracle_recall', 'arguments': {'q': 'lower'}}}).encode(),
                         {'authorization': 'Bearer ' + secret, 'user-agent': 'Claude-User'})
        last = mc.read_calls()[0]
        self.assertEqual((last['tool'], last['args'], last['client']), ('oracle_recall', {'q': 'lower'}, 'Claude-User'))
        fake.shutdown()

if __name__ == '__main__':
    unittest.main()
