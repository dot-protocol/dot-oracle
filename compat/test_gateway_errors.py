"""The custody gateway must say WHICH refusal it was, without leaking anything.

A 413 on one oversized legacy row reached the reconciliation report as an
unexplained refusal, because every exception collapsed to its type name. These
pin the repaired behaviour: the status survives, the reason comes from our own
bounded table, and nothing from the request or the response body escapes.

Run: python3 compat/test_gateway_errors.py
"""
import json
import os
import socket
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

SECRET = 'tok_SUPERSECRET_must_never_appear'
LEAK = 'LEAKED_UPSTREAM_BODY_marker'

_status = [200]
_delay = [0.0]


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get('content-length') or 0)
        self.rfile.read(length)
        if _delay[0]:
            import time
            time.sleep(_delay[0])
        code = _status[0]
        # The upstream body is deliberately poisonous: if any of it reaches the
        # raised error, the allowlist is not doing its job.
        payload = json.dumps({'ok': code == 200, 'error': LEAK, 'token': SECRET}).encode()
        self.send_response(code)
        self.send_header('content-type', 'application/json')
        self.send_header('x-poison', LEAK)
        self.send_header('content-length', str(len(payload)))
        self.end_headers()
        # The timeout case hangs up mid-response; a broken pipe here is the test
        # working, not failing, and must not print a traceback into the report.
        try:
            self.wfile.write(payload)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, *a):
        pass


class GatewayErrors(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = HTTPServer(('127.0.0.1', 0), Handler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.tokenfile = tempfile.NamedTemporaryFile('w', suffix='.token', delete=False)
        cls.tokenfile.write(SECRET)
        cls.tokenfile.close()
        os.environ['ORACLE_CORE_CUSTODY_TOKEN_FILE'] = cls.tokenfile.name
        os.environ['ORACLE_CORE_URL'] = 'http://127.0.0.1:%d' % cls.server.server_address[1]
        global gateway
        import oracle_core_gateway as gateway

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        os.unlink(cls.tokenfile.name)

    def refusal(self, status):
        _status[0] = status
        with self.assertRaises(RuntimeError) as caught:
            gateway.call('/admin/import', {'rows': []})
        return str(caught.exception)

    def test_413_names_the_import_limit(self):
        message = self.refusal(413)
        self.assertIn('413', message)
        self.assertIn('body exceeds the import limit', message)
        # This is the whole point: the reconciler can now record WHY.
        self.assertIn('Core custody refused', message)

    def test_401_is_distinguishable_from_413(self):
        message = self.refusal(401)
        self.assertIn('401', message)
        self.assertIn('custody token not accepted', message)
        self.assertNotIn('413', message)

    def test_unlisted_status_reports_only_its_number(self):
        message = self.refusal(418)
        self.assertIn('418', message)
        self.assertIn('unexpected status', message)

    def test_timeout_stays_generic_and_is_not_an_http_refusal(self):
        _status[0] = 200
        _delay[0] = 1.5
        original = gateway.urllib.request.urlopen

        def impatient(req, timeout=None):
            return original(req, timeout=0.2)

        gateway.urllib.request.urlopen = impatient
        try:
            with self.assertRaises(RuntimeError) as caught:
                gateway.call('/admin/import', {'rows': []})
            message = str(caught.exception)
            # A transport failure has no status, so it must NOT claim one.
            self.assertIn('Core custody unavailable', message)
            self.assertNotIn('refused', message)
        finally:
            gateway.urllib.request.urlopen = original
            _delay[0] = 0.0

    def test_nothing_leaks_into_any_error(self):
        for status in (400, 401, 403, 404, 409, 413, 422, 500, 503, 418):
            message = self.refusal(status)
            self.assertNotIn(SECRET, message, 'credential leaked at %d' % status)
            self.assertNotIn(LEAK, message, 'upstream body or header leaked at %d' % status)
            self.assertNotIn('Authorization', message)
            self.assertNotIn('Bearer', message)

    def test_success_still_returns_the_parsed_body(self):
        _status[0] = 200
        self.assertEqual(gateway.call('/admin/import', {'rows': []})['ok'], True)


if __name__ == '__main__':
    unittest.main(verbosity=2)
