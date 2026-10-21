"""work: today's is claim -> first receipt_observed, the ledger's claim ->
its first validating. Per run with an unexplained work diff, the ledger
phases between the receipt and the first validating."""
import json, sys
from collections import Counter, defaultdict
sys.path.insert(0, __import__('os').path.dirname(__import__('os').path.abspath(__file__))); import lib
ms = lib.F['ms']
L = json.load(open('ledger.json')); R = lib.load_runs()
out = {}; agg = Counter()
for r in L['runs']:
    for f in r['fields']:
        if f['field'] in ('work', 'wait_to_land') and 'unexplained' in f['reasons']:
            revs, life = R[r['run_id']]
            iv = life['intervals']
            v = next((i for i in iv if i['phase'] == 'validating'), None)
            rec = next((ms(e['created_at']) for e in revs if e['kind'] == 'receipt_observed'), None)
            vf = next((ms(e['created_at']) for e in revs if e['kind'] == 'validation_finished'), None)
            integ = next((ms(e['created_at']) for e in revs if e['kind'] == 'run_integrated'), None)
            cells = defaultdict(int)
            if f['field'] == 'work':
                lo, hi = (rec, v['start']) if rec is not None and v else (None, None)
                if lo is not None:
                    for i in iv:
                        a, b = max(lo, i['start']), min(hi, i['end'])
                        if b > a: cells[i['phase']] += b - a
                note = 'no receipt_observed' if rec is None else ''
            else:
                note = f"stats from validation_finished {vf} to run_integrated {integ}; ledger validating end {v['end']} -> run_integrated: start apart {(v['end']-vf)}ms"
            out[(r['run_id'], f['field'])] = (f['diff'], dict(cells), note)
            for k in cells: agg[(f['field'], k)] += 1
for k, v in out.items(): print(k[0], k[1], v)
print(agg)
