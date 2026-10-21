import json, sys
src = open(__import__('os').path.join(__import__('os').path.dirname(__import__('os').path.abspath(__file__)), 'fold.py')).read().replace('\nmain()\n', '\n')
ns = {}; exec(compile(src, 'fold', 'exec'), ns)
pushes = sorted((json.loads(l) for l in open('pushes.jsonl')), key=lambda e: e['id'])
for t in sys.argv[1:]:
    evs = sorted((json.loads(l) for l in open(f'tasks/{t}.jsonl')), key=lambda e: e['id'])
    tl = ns['fold_task'](evs, pushes, 1792497652000)
    for iv in tl['intervals']:
        if iv['phase'] == 'unattributed' and iv['end'] - iv['start'] > 1000:
            ctx = [ (e['id'], e['kind'], e['created_at'], (e['payload'] or {}).get('from'), (e['payload'] or {}).get('to')) for e in evs if e['kind'] in ('task_status_changed','run_claimed') ]
            print(t, iv['start'], iv['end'], (iv['end']-iv['start'])/1000)
            for c in ctx: print('   ', c)
