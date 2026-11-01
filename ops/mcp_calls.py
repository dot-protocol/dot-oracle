#!/usr/bin/env python3
"""What the rooms get from Oracle (Blaze, 2026-10-08: "when the room is querying Oracle right now, what kind of output
they are getting, what kind of calls they are running ... add logging, measure calls ... we will rank that").

nginx mirrors every request the connectors send to POST /mcp here (`mirror`: the live request is not touched, and this
service being down costs it nothing). For each tools/call it records who called (the seat, from /whoami), the tool and
its arguments. A read-only tool (the server's own readOnlyHint) is called again at once with the caller's token, and
the answer is recorded with its time and size: what the room got. A writing tool is recorded, never repeated.
Tokens are used in memory only and never written; a caller is its seat and a short hash of its token.

  POST /mirror           nginx's copy of a /mcp request (answers 204 at once; the work happens after)
  GET  /calls?limit=&after=   newest first, for the AXXIS Search › Calls view (127.0.0.1 only)
"""
import hashlib, json, os, threading, time, urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

UPSTREAM = os.environ.get('ORACLE_MCP_UPSTREAM', 'http://127.0.0.1:8893')
STORE = os.environ.get('ORACLE_CALLS_FILE', '/var/lib/oracle-calls/calls.jsonl')
BIND = os.environ.get('ORACLE_CALLS_BIND', '127.0.0.1:8913')
KEEP_TEXT = 60_000
_lock = threading.Lock()
_seq = [0]
_tools = {}   # token hash -> (read_only: dict tool->bool, at)
_seats = {}   # token hash -> seat


def _post(token, payload, timeout=60):
    req = urllib.request.Request(UPSTREAM + '/mcp', data=json.dumps(payload).encode(), method='POST',
                                 headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json',
                                          'Accept': 'application/json, text/event-stream'})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read() or b'null')


def _seat(token, th):
    if th not in _seats:
        try:
            req = urllib.request.Request(UPSTREAM + '/whoami', headers={'Authorization': 'Bearer ' + token})
            with urllib.request.urlopen(req, timeout=10) as r:
                d = json.loads(r.read())
            _seats[th] = str(d.get('handle') or d.get('seat') or d.get('principal') or '?')
        except Exception:
            _seats[th] = '?'
    return _seats[th]


def _read_only(token, th, tool):
    have = _tools.get(th)
    if not have or time.time() - have[1] > 600:
        try:
            d = _post(token, {'jsonrpc': '2.0', 'id': 'calls-log', 'method': 'tools/list'}, timeout=20)
            tools = {t['name']: bool((t.get('annotations') or {}).get('readOnlyHint')) for t in d['result']['tools']}
        except Exception:
            tools = {}
        have = (tools, time.time())
        _tools[th] = have
    return have[0].get(tool) is True


def _text(result):
    parts = []
    for c in (result or {}).get('content') or []:
        if c.get('type') == 'text':
            parts.append(str(c.get('text', '')))
        else:
            parts.append('[' + str(c.get('type')) + ']')
    return '\n'.join(parts)


def _record(entry):
    with _lock:
        _seq[0] += 1
        entry['seq'] = _seq[0]
        os.makedirs(os.path.dirname(STORE), exist_ok=True)
        fd = os.open(STORE, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        with os.fdopen(fd, 'a') as f:
            f.write(json.dumps(entry, ensure_ascii=False) + '\n')


def handle_mirror(body, headers):
    headers = {k.lower(): v for k, v in headers.items()}
    auth = headers.get('authorization', '')
    token = auth[7:] if auth.startswith('Bearer ') else ''
    try:
        msg = json.loads(body or b'null')
    except Exception:
        return
    if not token:
        return  # Oracle refuses it (401); nothing a room got
    msgs = msg if isinstance(msg, list) else [msg]
    th = hashlib.sha256(token.encode()).hexdigest()[:10] if token else 'anonymous'
    for m in msgs:
        if not isinstance(m, dict):
            continue
        method = m.get('method')
        base = {'at': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()), 'at_ms': int(time.time() * 1000),
                'caller': th, 'seat': _seat(token, th) if token else '?', 'client': headers.get('user-agent', ''),
                'method': method}
        if method == 'initialize':
            info = (m.get('params') or {}).get('clientInfo') or {}
            _record({**base, 'kind': 'connect', 'client_name': info.get('name'), 'client_version': info.get('version')})
            continue
        if method != 'tools/call':
            if method and not str(method).startswith('notifications/'):
                _record({**base, 'kind': 'rpc'})  # tools/list, resources/list ...: what the client looked at
            continue
        params = m.get('params') or {}
        tool, args = params.get('name'), params.get('arguments') or {}
        entry = {**base, 'kind': 'call', 'tool': tool, 'args': args}
        if not token or not _read_only(token, th, tool):
            entry['answer'] = None
            entry['note'] = 'writes or unknown: recorded, not repeated'
            _record(entry)
            continue
        started = time.time()
        try:
            d = _post(token, {'jsonrpc': '2.0', 'id': 'calls-log', 'method': 'tools/call', 'params': {'name': tool, 'arguments': args}})
            ms = int((time.time() - started) * 1000)
            if 'error' in d:
                entry['answer'] = {'ms': ms, 'error': d['error'].get('message'), 'bytes': 0}
            else:
                text = _text(d.get('result'))
                entry['answer'] = {'ms': ms, 'bytes': len(text.encode()), 'chars': len(text), 'is_error': bool((d.get('result') or {}).get('isError')),
                                   'text': text[:KEEP_TEXT], 'truncated': len(text) > KEEP_TEXT}
        except Exception as e:
            entry['answer'] = {'ms': int((time.time() - started) * 1000), 'error': str(e)[:300], 'bytes': 0}
        _record(entry)


def read_calls(limit=200, after=0):
    if not os.path.exists(STORE):
        return []
    out = []
    with open(STORE, encoding='utf-8') as f:
        for line in f:
            try:
                e = json.loads(line)
            except Exception:
                continue
            if e.get('at_ms', 0) > after:
                out.append(e)
    return out[::-1][:limit]


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        if urlparse(self.path).path != '/mirror':
            self.send_response(404); self.end_headers(); return
        body = self.rfile.read(int(self.headers.get('Content-Length') or 0))
        headers = {k.lower(): v for k, v in self.headers.items()}  # HTTP/2 clients send lowercase names
        self.send_response(204); self.end_headers()
        threading.Thread(target=handle_mirror, args=(body, headers), daemon=True).start()

    def do_GET(self):
        u = urlparse(self.path)
        if u.path == '/health':
            return self._json({'ok': True, 'store': STORE})
        if u.path != '/calls':
            self.send_response(404); self.end_headers(); return
        q = parse_qs(u.query)
        limit = max(1, min(1000, int((q.get('limit') or ['200'])[0])))
        after = int((q.get('after') or ['0'])[0])
        self._json({'calls': read_calls(limit, after)})

    def _json(self, body):
        b = json.dumps(body, ensure_ascii=False).encode()
        self.send_response(200); self.send_header('Content-Type', 'application/json'); self.send_header('Content-Length', str(len(b))); self.end_headers(); self.wfile.write(b)


def _resume_seq():
    if os.path.exists(STORE):
        with open(STORE, encoding='utf-8') as f:
            for line in f:
                try:
                    _seq[0] = max(_seq[0], int(json.loads(line).get('seq', 0)))
                except Exception:
                    pass


if __name__ == '__main__':
    _resume_seq()
    host, port = BIND.rsplit(':', 1)
    ThreadingHTTPServer((host, int(port)), H).serve_forever()
