#!/usr/bin/env python3
"""Consistent Core backup plus integrity check; never copy a live DB without its WAL."""
import datetime,json,os,sqlite3,hashlib
from pathlib import Path
os.umask(0o077)
root=Path(os.environ.get('ORACLE_BACKUP_DIR','/var/backups/oracle-core'))
root.mkdir(parents=True,exist_ok=True)
name=datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
p=root/(name+'.db'); tmp=p.with_suffix('.partial')
s=sqlite3.connect('file:/var/lib/oracle-core/oracle.db?mode=ro',uri=True);d=sqlite3.connect(tmp);s.backup(d);s.close()
check=d.execute('PRAGMA integrity_check').fetchone()[0]
count=d.execute('SELECT count(*) FROM observations').fetchone()[0];d.close()
if check!='ok':raise RuntimeError('backup integrity failed')
os.replace(tmp,p)
receipt={'backup':p.name,'integrity':check,'observations':count,'sha256':hashlib.file_digest(open(p,'rb'),'sha256').hexdigest()}
# What a restore cannot rebuild from oracle.db (Grok's deep dive, 2026-10-10: the backup skipped them): the seat keys
# this server holds (keys/, hosted/) and the auth secrets, as one root-only tar; and the meaning-search vectors, which
# take hours to re-embed on CPU, copied consistently like the store.
import tarfile
home=Path(os.environ.get('ORACLE_HOME','/var/lib/oracle-core'))
secrets=[x for x in ['keys','hosted','oauth-connect-code','oauth-admin-token','introspect.token','legacy-oauth.secret',
         'legacy-bindings.json','legacy-revoked.txt','legacy-revoked-sha256.txt'] if (home/x).exists()]
t=root/(name+'.secrets.tar'); tt=t.with_suffix('.partial')
with tarfile.open(tt,'w') as tar:
    for x in secrets: tar.add(home/x,arcname=x)
os.replace(tt,t)
receipt['secrets']={'file':t.name,'members':len(tarfile.open(t).getnames()),'sha256':hashlib.file_digest(open(t,'rb'),'sha256').hexdigest()}
vec=home/'vectors.db'
if vec.exists():
    v=root/(name+'.vectors.db'); vt=v.with_suffix('.partial')
    s=sqlite3.connect(f'file:{vec}?mode=ro',uri=True);d=sqlite3.connect(vt);s.backup(d);s.close()
    vcheck=d.execute('PRAGMA quick_check').fetchone()[0];d.close()
    if vcheck!='ok':raise RuntimeError('vectors backup check failed')
    os.replace(vt,v)
    receipt['vectors']={'file':v.name,'check':vcheck,'bytes':v.stat().st_size}
p.with_suffix('.json').write_text(json.dumps(receipt,indent=2)+'\n')
print(json.dumps(receipt))

# The store only grows (signed observations are never rewritten), so the newest backup holds every
# older one. One a day was kept forever: 7 days, 6 GB and growing ~1 GB a day on a 97%-full disk
# (Blaze, 2026-09-27). Keep the newest ORACLE_BACKUP_KEEP (default 3); a receipt goes with its backup.
keep=int(os.environ.get('ORACLE_BACKUP_KEEP','3'))
for old in sorted(x for x in root.glob('*.db') if not x.name.endswith('.vectors.db'))[:-keep]:
    old.unlink();old.with_suffix('.json').unlink(missing_ok=True)
    stem=old.name[:-3]
    (root/(stem+'.secrets.tar')).unlink(missing_ok=True);(root/(stem+'.vectors.db')).unlink(missing_ok=True)
