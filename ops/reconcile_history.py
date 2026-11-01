#!/usr/bin/env python3
"""Preserve full legacy observation sources and repair lossy imported projections.

Run on Core with the legacy service's environment. Embeddings are derived indexes,
excluded from the source payload and explicitly listed. Native signed rows and old
mirror aliases are never rewritten. A refused state repair is reported, not bypassed.
No observation content or credential is printed. Resume is idempotent by ID/digest.
"""
import datetime as dt,json,os,sqlite3,sys,urllib.error,urllib.request
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'compat'))
from oracle_core_gateway import call
from neo4j import GraphDatabase

def ms(v):
    when=dt.datetime.fromisoformat(str(v).replace('Z','+00:00'))
    if when.tzinfo is None:when=when.replace(tzinfo=dt.timezone.utc)
    return int(when.timestamp()*1000)

def main():
    db=sqlite3.connect('file:/var/lib/oracle-core/oracle.db?mode=ro',uri=True);db.row_factory=sqlite3.Row
    driver=GraphDatabase.driver(os.environ['NEO4J_URI'],auth=(os.environ.get('NEO4J_USER','neo4j'),os.environ['NEO4J_PASSWORD']))
    cutoff=int(os.environ['ORACLE_HISTORY_MAX_SEQ'])  # recorded BEFORE custody cutover
    cursor=''; report={'preserved':0,'native_skipped':0,'authoritative_skipped':0,'aliases':0,'mirror_aliases':{},'refused':[],'omitted_derived_fields':['embedding','embedding_v2']}
    path=Path(os.environ.get('RECONCILE_REPORT','/var/backups/oracle/20260921-pre-authority/history-reconciliation.json'))
    while True:
        with driver.session() as session:
            records=list(session.run('MATCH (o:Observation) WHERE o.id > $cursor RETURN properties(o) AS p ORDER BY o.id LIMIT 100',cursor=cursor))
        if not records:break
        rows=[]
        for record in records:
            raw=record['p'];cursor=raw['id'];tags=raw.get('tags') or []
            if any(t.startswith('mirror-of:') for t in tags if isinstance(t,str)):
                report['aliases']+=1
                report['mirror_aliases'][raw['id']]=next(t[len('mirror-of:'):] for t in tags if t.startswith('mirror-of:'))
                continue
            old=db.execute('SELECT * FROM observations WHERE id=?',(raw['id'],)).fetchone()
            if old and old['envelope']==2:
                report['native_skipped']+=1;continue
            if old and (old['seq'] > cutoff or db.execute("SELECT 1 FROM legacy_sources WHERE obs_id=? AND json_extract(payload,'$.gateway_schema')=1 LIMIT 1",(raw['id'],)).fetchone()):
                report['authoritative_skipped']+=1;continue
            if not old and ms(raw.get('created_at') or raw['timestamp']) > int(os.environ['ORACLE_HISTORY_MAX_MS']):
                report['authoritative_skipped']+=1;continue
            # Keep original source properties. Generated vectors remain owned by their index.
            source=json.loads(json.dumps({k:v for k,v in raw.items() if k not in report['omitted_derived_fields']},default=str))
            author=old['author'] if old else next((t[5:] for t in tags if t.startswith('from:')),raw.get('agent_id') or 'unknown')
            row={'id':raw['id'],'channel':old['channel'] if old else raw.get('channel') or 'raw',
                 'author':author,'kind':old['kind'] if old else ('state' if 'live-state' in tags else 'say'),
                 'content':raw.get('statement') or '', 'cargo':raw.get('evidence') or None,
                 'tags':tags,'created_ms':old['created_ms'] if old else ms(raw.get('created_at') or raw['timestamp']),
                 'pubkey':raw.get('pubkey') or '', 'sig':raw.get('signature') or '', 'news':False,
                 'source':{'legacy_export_schema':1,'observation':source,'omitted_derived_fields':report['omitted_derived_fields']},
                 'repair_legacy':True}
            rows.append(row)
        if rows:
            try:
                call('/admin/import',{'rows':rows});report['preserved']+=len(rows)
            except RuntimeError:
                # Isolate a refused record; no batch member is silently counted as stored.
                for row in rows:
                    try:call('/admin/import',{'rows':[row]});report['preserved']+=1
                    except RuntimeError:report['refused'].append(row['id'])
        path.write_text(json.dumps({**report,'last_id':cursor},indent=2)+'\n')
        print(json.dumps({'preserved':report['preserved'],'refused':len(report['refused']),'aliases':report['aliases']}),flush=True)
    driver.close();db.close()
    print(json.dumps({'complete':not report['refused'],**{k:v for k,v in report.items() if k not in ('refused','mirror_aliases')},'refused_count':len(report['refused'])}))
    return bool(report['refused'])
if __name__=='__main__':sys.exit(main())
