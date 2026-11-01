"""Core-first observation custody behind the unchanged legacy MCP/auth edge.

No token reinterpretation or signing takes place here. The authenticated, validated
legacy writer supplies its original record. Core retains it with envelope=1, and the
legacy graph remains a compatibility/read projection. Fail closed on Core failure.
"""
import contextvars
import datetime as dt
import json
import os
from pathlib import Path
import urllib.request


_projecting = contextvars.ContextVar("core_projection", default=False)

def enabled():
    return os.environ.get('ORACLE_CORE_AUTHORITY') == '1'


# Bounded allowlist. Any status not named here reports only its number.
_REASONS = {
    400: 'malformed request',
    401: 'custody token not accepted',
    403: 'insufficient scope',
    404: 'no such record or route',
    409: 'conflicts with the stored record',
    413: 'body exceeds the import limit',
    422: 'rejected by validation',
    500: 'Core internal error',
    503: 'Core unavailable',
}


def call(path, body=None):
    token = Path(os.environ['ORACLE_CORE_CUSTODY_TOKEN_FILE']).read_text().strip()
    url = os.environ.get('ORACLE_CORE_URL', 'http://127.0.0.1:8893')
    data = None if body is None else json.dumps(body, ensure_ascii=False).encode()
    req = urllib.request.Request(url + path, data=data, headers={
        'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    try:
        with urllib.request.urlopen(req, timeout=20) as response:
            return json.load(response)
    except urllib.error.HTTPError as exc:
        # SAY WHICH REFUSAL IT WAS.
        #
        # This used to collapse to the exception type name alone. A single 413 —
        # one legacy row whose body exceeded the import limit — then reached the
        # reconciliation report as an unexplained refusal, and cost two agents a
        # separate investigation each to recover a fact the response had already
        # stated. A status code is diagnosis, not disclosure.
        #
        # The reason is chosen from THIS table, never read from the response body,
        # so an upstream error page can never smuggle content into a log line.
        # Headers, credentials and both bodies remain unlogged and unexposed.
        raise RuntimeError('Core custody refused: HTTP %d (%s)'
                           % (exc.code, _REASONS.get(exc.code, 'unexpected status'))) from None
    except Exception as exc:
        # Never expose request headers, credentials, or source body in errors.
        raise RuntimeError('Core custody unavailable: ' + type(exc).__name__) from None


def observation_row(obs):
    source = json.loads(obs.model_dump_json()) if hasattr(obs, 'model_dump_json') else json.loads(obs.json())
    tags = list(source.get('tags') or [])
    # Origin IDs are retained. New Core writes are projected separately, never re-minted.
    author = next((t[5:] for t in tags if t.startswith('from:')), None)
    author = author or source.get('agent_id') or 'unknown'
    channel = source.get('channel') or 'raw'
    timestamp = source.get('timestamp')
    when = dt.datetime.fromisoformat(timestamp.replace('Z', '+00:00'))
    if when.tzinfo is None:
        when = when.replace(tzinfo=dt.timezone.utc)
    evidence = source.get('evidence')
    return {'id': source['id'], 'channel': channel, 'author': author,
            'kind': 'state' if 'live-state' in tags else ('say' if 'kind:say' in tags else 'memory'),
            'content': source['statement'], 'cargo': evidence if isinstance(evidence, str) else json.dumps(evidence),
            'tags': tags, 'created_ms': int(when.timestamp() * 1000),
            'pubkey': source.get('pubkey') or '', 'sig': source.get('signature') or '',
            'source': {'gateway_schema': 1, 'observation': source}, 'news': True}


def accept_observation(obs):
    """Called after legacy authorization/validation, before its graph commit."""
    if not enabled() or _projecting.get():
        return
    row = observation_row(obs)
    result = call('/admin/import', {'rows': [row]})
    if not result.get('ok'):
        raise RuntimeError('Core did not acknowledge observation custody')


def project_into_legacy(obs_id):
    from tree_schema import Observation
    from tree_store import TreeStore
    from project_core import properties
    import urllib.parse
    row = call('/search?id=' + urllib.parse.quote(obs_id, safe=''))
    source = None
    if row.get('envelope') == 1:
        versions = call('/search?include=source&id=' + urllib.parse.quote(obs_id, safe='')).get('versions', [])
        sources = [v['source'].get('observation') for v in versions if isinstance(v.get('source'),dict) and v['source'].get('gateway_schema') == 1]
        source = sources[-1] if sources else None
    p = properties(row, source)
    store = TreeStore(lazy=True)
    with store._session() as session:
        exists = session.run('MATCH (o:Observation {id:$id}) RETURN o.id AS id, o.statement AS content, o.evidence AS evidence',id=obs_id).single()
    if exists and (exists['content'] != row['content'] or (exists['evidence'] or '') != (row.get('cargo') or '')):
        raise RuntimeError('Core and graph bodies differ; reconciliation required')
    if row.get('envelope') == 1 and not source:
        if not exists: raise RuntimeError('legacy source missing; cannot skip projection')
        return {'ok':True,'id':obs_id,'projection':'already-legacy'}
    if not exists:
        obs = Observation(**p)
        marker = _projecting.set(True)
        try: store.create_observation(obs, [])
        finally: _projecting.reset(marker)
    # Run even on retry: a crash between observation and metadata must be recoverable.
    with store._session() as session:
        session.run('MATCH (o:Observation {id:$id}) SET o.core_seq=$seq, o.created_at=datetime($created)',
                    id=obs_id,seq=row['seq'],created=p['created_at']).consume()
        if row.get('envelope') == 2:
            proof = call('/search?include=envelope&id=' + urllib.parse.quote(obs_id,safe=''))
            session.run('MATCH (o:Observation {id:$id}) SET o.core_envelope=$envelope',
                        id=obs_id,envelope=json.dumps(proof)).consume()
    if source:
        dates = {k:source.get(k) for k in ('event_date','valid_from','valid_until','ingested_at')}
        with store._session() as session:
            session.run('MATCH (o:Observation {id:$id}) SET o.event_date=datetime($event_date), '
                'o.valid_from=datetime($valid_from), o.valid_until=datetime($valid_until), o.ingested_at=datetime($ingested_at)',
                id=obs_id,**dates).consume()
    supersedes = [t[len('supersedes:'):] for t in row.get('tags',[]) if t.startswith('supersedes:')]
    if supersedes:
        with store._session() as session:
            session.run('MATCH (new:Observation {id:$id}) UNWIND $olds AS oid '
                'MATCH (old:Observation {id:oid}) MERGE (new)-[r:SUPERSEDES]->(old) '
                'SET r.source="oracle-core-projection"',id=obs_id,olds=supersedes).consume()
    if row['kind'] == 'state' and row.get('envelope') == 2:
        cargo = json.loads(row.get('cargo') or '{}')
        subject = cargo['subject']
        head = call('/states?subject=' + urllib.parse.quote(subject, safe=''))
        if head.get('observation', {}).get('id') == obs_id:
            with store._session() as session:
                session.run('MATCH (new:Observation {id:$id}), (old:Observation) '
                    'WHERE $tag IN old.tags AND old.id <> $id AND "live-state" IN old.tags '
                    'AND NOT EXISTS { MATCH (:Observation)-[:SUPERSEDES]->(old) } '
                    'MERGE (new)-[r:SUPERSEDES]->(old) SET r.source="oracle-core-projection"',
                    id=obs_id,tag='subject:'+subject).consume()
    return {'ok':True,'id':obs_id,'seq':row['seq'],'duplicate':bool(exists)}


def index_pending():
    """Retry derived semantic indexing separately from durable acceptance/projection."""
    from tree import get_vectors
    from tree_store import TreeStore
    store = TreeStore(lazy=True)
    with store._session() as session:
        rows = list(session.run('MATCH (o:Observation) WHERE o.core_seq IS NOT NULL AND o.embedding IS NULL '
            'RETURN o.id AS id,o.statement AS content ORDER BY o.core_seq LIMIT 5'))
    vectors = get_vectors()
    done = 0
    for row in rows:
        emb = vectors.embed(row['content'])
        if not store._embedding_is_valid(emb): raise RuntimeError('embedding unavailable')
        with store._session() as session:
            session.run('MATCH (o:Observation {id:$id}) SET o.embedding=$embedding,o.embedding_status="ready"',
                        id=row['id'],embedding=emb).consume()
        done += 1
    return {'ok':True,'indexed':done}
