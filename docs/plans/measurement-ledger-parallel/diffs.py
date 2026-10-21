"""(b): per field and reason the runs, sum and median of diff. usage: diffs.py ledger.json"""
import json, sys, statistics
from collections import defaultdict
L = json.load(open(sys.argv[1]))
g = defaultdict(list)
for r in L['runs']:
    for f in r['fields']:
        if f['diff'] in (0, None) and not f['reasons']:
            continue
        key = '+'.join(f['reasons']) or '(none)'
        g[(f['field'], key)].append(f['diff'] if f['diff'] is not None else 0)
print('| 値の名前 | 理由の分類 | run の件数 | 差の秒の合計 | 差の秒の中央値 |')
print('|---|---|---:|---:|---:|')
for (field, why), ds in sorted(g.items()):
    print(f'| {field} | {why} | {len(ds)} | {sum(ds)} | {statistics.median(ds):g} |')
nulls = [(r['run_id'], f['field'], f['stats'], f['ledger']) for r in L['runs'] for f in r['fields'] if f['diff'] is None]
print('null diffs', len(nulls), nulls[:10])
print('push', sum(1 for r in L['runs'] if r['push']), sum(r['push'] or 0 for r in L['runs']))
