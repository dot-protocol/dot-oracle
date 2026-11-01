#!/usr/bin/env python3
"""Exercise the unchanged public MCP endpoint with an existing credential.

Print only tool names/digests and response status; never token or source content.
"""
import hashlib,json,os,sys,urllib.request
from pathlib import Path
URL=os.environ.get('MCP_URL','http://127.0.0.1:8900/mcp/')

def token():
    p=Path(os.environ.get('MCP_TOKEN_FILE','/etc/oracle/codex-desktop-token.env'))
    text=p.read_text().strip()
    if '=' not in text: return text
    for line in text.splitlines():
        if line.startswith('ORACLE_MCP_TOKEN='): return line.split('=',1)[1].strip().strip('\"\'')
    raise RuntimeError('token variable missing')

class Client:
    def __init__(self): self.token=token();self.session=None;self.n=0
    def call(self,method,params=None,notification=False):
        self.n+=1
        b={'jsonrpc':'2.0','method':method,'params':params or {}}
        if not notification:b['id']=self.n
        h={'User-Agent':'Mozilla/5.0 OracleMigrationVerifier/1.0','Authorization':'Bearer '+self.token,'Accept':'application/json, text/event-stream','Content-Type':'application/json','MCP-Protocol-Version':'2025-03-26'}
        if self.session:h['Mcp-Session-Id']=self.session
        req=urllib.request.Request(URL,data=json.dumps(b).encode(),headers=h)
        with urllib.request.urlopen(req,timeout=90) as r:
            self.session=r.headers.get('Mcp-Session-Id') or self.session
            text=r.read().decode()
        if not text:return {}
        if text.startswith('event:') or text.startswith('data:'):
            msgs=[json.loads(line[5:].strip()) for line in text.splitlines() if line.startswith('data:')]
            return next((x for x in msgs if x.get('id')==self.n),{})
        return json.loads(text)
    def initialize(self):
        r=self.call('initialize',{'protocolVersion':'2025-03-26','capabilities':{},'clientInfo':{'name':'oracle-migration-verifier','version':'1'}})
        if 'error' in r: raise RuntimeError('initialize failed')
        self.call('notifications/initialized',notification=True)
        return r

if __name__=='__main__':
    c=Client();c.initialize();r=c.call('tools/list')
    tools=r.get('result',{}).get('tools',[])
    if not tools:raise SystemExit('no tools')
    print(json.dumps({'url':URL,'initialize':'ok','tools':len(tools),'schema_sha256':hashlib.sha256(json.dumps(tools,sort_keys=True).encode()).hexdigest(),'names':[t['name'] for t in tools]}))
