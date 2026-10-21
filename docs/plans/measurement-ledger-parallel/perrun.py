"""Per run with a difference the comparison read calls unexplained: each
difference and where it comes from (matrix.json cells, work.py), with a
check that the cells account for it. usage: perrun.py > perrun.md"""
import json, sys
from collections import Counter, defaultdict
sys.path.insert(0, __import__('os').path.dirname(__import__('os').path.abspath(__file__))); import lib
ms = lib.F['ms']
L = json.load(open('ledger.json')); M = json.load(open('matrix.json')); R = lib.load_runs()
CODE = {
 ('landing_queue','awaiting_slot'):'A', ('e2e_wait','awaiting_slot'):'A',
 ('exit','landing_answer'):'B', ('landing_queue','landing_answer'):'B', ('resume','landing_answer'):'B',
 ('revise','validating'):'C', ('resume','validating'):'C', ('conflict','validating'):'C', ('ask','validating'):'C', ('review','validating'):'C',
 ('ask','revise'):'D', ('conflict','revise'):'E',
 ('resume','landing_queue'):'F', ('resume','landing'):'F',
 ('revise','returning'):'G', ('revise','exiting'):'G', ('review','exiting'):'G', ('landing_queue','exiting'):'G',
 ('ask','resume'):'H', ('exit','review'):'J', ('rebase','landing'):'R', ('revise','waiting'):'K',
}
def code(s, ph): return CODE.get((s, ph), 'Z')
rows = []; resid_bad = []; codes_used = Counter()
for r in L['runs']:
    un = [f for f in r['fields'] if 'unexplained' in f['reasons']]
    if not un: continue
    rid = r['run_id']; cells = M.get(rid, {})
    revs, life = R[rid]
    reasons = set(); parts = []
    for f in un:
        name = f['field']
        if name == 'work':
            rec = next((ms(e['created_at']) for e in revs if e['kind']=='receipt_observed'), None)
            reasons.add('W2' if rec is None else 'W1'); parts.append(f"work {f['diff'] if f['diff'] is not None else 'stats なし'}")
            continue
        if name == 'wait_to_land':
            reasons.add('T'); parts.append(f"wait_to_land {f['diff']}"); continue
        ph = name.split('.')[-1]
        pred = 0
        for k, m in cells.items():
            s, raw, b = k.split('>')
            if s == '(start)':
                if ph == 'exit': pred += m
                continue
            if b == ph: pred += m; reasons.add(code(s, raw))
            if s == ph: pred -= m; reasons.add(code(s, raw))
        if abs(pred/1000 - f['diff']) > 3: resid_bad.append((rid, name, f['diff'], pred/1000))
        parts.append(f"{ph} {f['diff']:+d}")
    for c in reasons: codes_used[c] += 1
    rows.append((rid, r['task_id'], ', '.join(parts), ''.join(sorted(reasons))))
print('| run | task | 不明の差（秒） | 由来 |'); print('|---|---:|---|---|')
for rid, t, p, c in sorted(rows, key=lambda x: x[0]): print(f'| {rid} | {t} | {p} | {c} |')
print(); print('runs', len(rows), 'codes', dict(codes_used), file=sys.stderr)
print('Z cells', Counter(k.split('>')[0]+'>'+k.split('>')[1] for rid,_,_,c in rows if 'Z' in c for k in M[rid] if code(*k.split('>')[:2])=='Z' and not k.startswith('(')), file=sys.stderr)
print('not accounted (>3s):', len(resid_bad), resid_bad[:20], file=sys.stderr)
