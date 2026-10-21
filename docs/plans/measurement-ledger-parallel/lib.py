import json, os
src = open(os.path.join(os.path.dirname(__file__), 'fold.py')).read().replace('\nmain()\n', '\n')
F = {}; exec(compile(src, 'fold', 'exec'), F)
def load_runs(tdir='tasks', pushes_path='pushes.jsonl', now=1792497652000):
    pushes = sorted((json.loads(l) for l in open(pushes_path)), key=lambda e: e['id'])
    out = {}
    for f in os.listdir(tdir):
        evs = sorted((json.loads(l) for l in open(os.path.join(tdir, f))), key=lambda e: e['id'])
        runs = {}
        for e in evs:
            if e.get('run_id'): runs.setdefault(e['run_id'], []).append(e)
        for rid, revs in runs.items():
            out[rid] = (revs, F['fold_run'](revs, pushes, now))
    return out
LAND = {"review":"review","review_held":"review","revise":"revise","waiting":"ask","landing_answer":"ask",
 "needs_session":"resume","resume":"resume","recovery":"resume","landing_queue":"landing_queue",
 "awaiting_e2e":"e2e_wait","e2e":"e2e","landing":"verify"}
def bucket(i):
    a = i.get('attempt') or {}
    if i['phase'] == 'revise' and isinstance(a, dict) and a.get('kind') == 'conflict': return 'conflict'
    return LAND.get(i['phase'], 'exit')
def buckets(life):
    """per land phase bucket, raw phase -> ms within [validated, push.start]"""
    iv = life['intervals']
    v = next((i for i in iv if i['phase']=='validating'), None)
    p = next((i for i in iv if i['phase']=='push'), None)
    if not v or not p: return None
    b = {}
    for i in iv:
        f, t = max(i['start'], v['end']), min(i['end'], p['start'])
        if t > f:
            name = bucket(i)
            b.setdefault(name, {}); b[name][i['phase']] = b[name].get(i['phase'], 0) + t - f
    return b
