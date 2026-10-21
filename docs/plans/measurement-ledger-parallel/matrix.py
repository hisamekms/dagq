"""Per run: the time today's LandClock phase S and the ledger's raw phase R
(its land_phases bucket B) share, for S != B: where each difference of
land_phases comes from. usage: matrix.py ledger.json -> matrix.json"""
import json, sys
from collections import defaultdict
sys.path.insert(0, __import__('os').path.dirname(__import__('os').path.abspath(__file__))); import lib, landclock
L = json.load(open(sys.argv[1])); R = lib.load_runs()
out = {}; agg = defaultdict(lambda: [0, 0])
for r in L['runs']:
    revs, life = R[r['run_id']]
    segs = landclock.segments(revs)
    if not segs: continue
    iv = life['intervals']
    v = next((i for i in iv if i['phase'] == 'validating'), None)
    p = next((i for i in iv if i['phase'] == 'push'), None)
    if not v or not p: continue
    cells = defaultdict(int)
    lo, hi = v['end'], p['start']
    for s, a, b in segs:
        for i in iv:
            f, t = max(a, i['start'], lo), min(b, i['end'], hi)
            if t > f:
                bucket = lib.bucket(i)
                if s != bucket: cells[(s, i['phase'], bucket)] += t - f
    # time one side has and the other not (the two starts and ends)
    cells[('(start)', 'stats_first_validation_vs_ledger_validating_end', '')] = segs[0][1] - lo
    out[r['run_id']] = {f'{s}>{ph}>{b}': m for (s, ph, b), m in cells.items() if abs(m) >= 1000}
    for (s, ph, b), m in cells.items():
        if abs(m) >= 1000 and s != '(start)': agg[(s, ph, b)][0] += 1; agg[(s, ph, b)][1] += m
json.dump(out, open('matrix.json', 'w'), indent=1)
print('| today (LandClock) | ledger raw phase | ledger bucket | runs | secs |')
for k, (n, m) in sorted(agg.items(), key=lambda x: -x[1][1]):
    print(f'| {k[0]} | {k[1]} | {k[2]} | {n} | {m//1000} |')
