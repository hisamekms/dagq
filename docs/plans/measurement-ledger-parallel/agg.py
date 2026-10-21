"""Sum (a) and (c) from fold.json. usage: python3 -I agg.py fold.json ledger.json"""
import json, sys
f = json.load(open(sys.argv[1])); L = json.load(open(sys.argv[2]))
lo, hi = L['window_ms']
for kind in ('run', 'task'):
    rows = [r for r in f[kind] if not r.get('legacy')]
    leg = [r['key'] for r in f[kind] if r.get('legacy')]
    out = [r['key'] for r in rows if r['end'] is None or not (lo <= r['end'] <= hi)]
    D = sum(r['duration'] for r in rows); U = sum(r['unattributed'] for r in rows)
    print(f"## {kind}: rows used {len(rows)}, legacy {len(leg)} {leg}, end outside window {out}")
    print(f"unattributed {U/1000:.0f} s / {D/1000:.0f} s = {100*U/D:.3f}%")
    nz = sorted(((r['unattributed']/1000, r['key']) for r in rows if r['unattributed']), reverse=True)
    print('nonzero unattributed rows', len(nz), nz[:15])
    keys = sorted({k for r in rows for k in r['blocker']})
    for k in keys:
        n = sum(r['blocker'].get(k, 0) for r in rows)
        print(f"| blocker: {k} | {kind} | {len(rows)} | {n/1000:.0f} | {D/1000:.0f} | {100*n/D:.1f}% |")
    for k in sorted({k for r in rows for k in r['holds']}):
        n = sum(r['holds'].get(k, 0) for r in rows)
        print(f"| holds: {k} | {kind} | {len(rows)} | {n/1000:.0f} | {D/1000:.0f} | {100*n/D:.1f}% |")
    if kind == 'run':
        S = sum(r['slot'] for r in rows)
        print(f"| slot | run | {len(rows)} | {S/1000:.0f} | {D/1000:.0f} | {100*S/D:.1f}% |")
    from collections import Counter
    print(Counter(r['outcome'] for r in rows))
