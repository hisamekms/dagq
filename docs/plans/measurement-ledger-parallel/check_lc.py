import json, sys
sys.path.insert(0, __import__('os').path.dirname(__import__('os').path.abspath(__file__))); import lib, landclock
L = json.load(open('ledger.json')); R = lib.load_runs()
bad = 0; n = 0
for r in L['runs']:
    revs, life = R[r['run_id']]
    segs = landclock.segments(revs)
    st = {f['field'].split('.')[-1]: f['stats'] for f in r['fields'] if f['field'].startswith('land_phases')}
    if segs is None or st.get('exit') is None: continue
    tot = {}
    for ph, a, b in segs: tot[ph] = tot.get(ph, 0) + (b - a)
    n += 1
    # verify+rebase merge since segments from landing are coarse
    for ph in ('exit','review','revise','ask','resume','landing_queue','conflict','e2e_wait'):
        if abs(tot.get(ph, 0)//1000 - (st.get(ph) or 0)) > 2:
            bad += 1; print(r['run_id'], ph, tot.get(ph,0)//1000, st.get(ph)); break
print('runs', n, 'mismatch', bad)
