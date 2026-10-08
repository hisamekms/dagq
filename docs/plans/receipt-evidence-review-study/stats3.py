import json, sys, re
from collections import Counter
D=sys.argv[1]; rows=json.load(open(f'{D}/rows.json'))
exec(open(sys.argv[2]).read().split('# paths per RE-selected attempt')[0].split("D=sys.argv[1]; rows=json.load(open(f'{D}/rows.json'))")[1])
st={ (e['run_id'],e['payload']['attempt']):e for e in json.load(open(f'{D}/start.json')) if e['run_id']}
for r in rows:
    e=st[(r['run_id'],r['attempt'])]; sub=e['payload'].get('subagents')
    r['paths']=next((a['paths'] for a in (sub or {}).get('agents',[]) if a['agent']=='receipt-evidence'),None)
re_rows=[r for r in rows if r['re_selected']]
def oc(r): return [a for a,g in CUR.items() if m(g,r['paths'])]
for opt,removed in [('B',[r for r in re_rows if not m(B,r['paths'])]),('D',re_rows)]:
    print('==',opt,len(removed))
    for c in ['docsだけ','configだけ','docsとconfigだけ','runtimeを含む','その他']:
        g=[r for r in removed if r['path_class']==c]
        cx=[r for r in g if r['switch_reason']=='subagents_unsupported' and not oc(r)]
        print(f"  {c}: removed={len(g)} codex可={len(cx)} tok有効(codex可を除く)={sum(r['tok_ok'] for r in g if r not in cx)} wall有効={sum(r['wall_secs']!='' for r in g)} act有効={sum(r['act_ok'] for r in g)} RE重い={sum(r['re_dest'] in ('send_back','ask') for r in g)}")
    print('  RE heavier in removed:',[(r['run_id'][:8],r['attempt'],r['path_class'],r['re_dest'],r['others_dest']) for r in removed if r['re_dest'] in ('send_back','ask')][:10])
print('m1 next start before end / none', Counter('next' if any(x['run_id']==r['run_id'] and x['attempt']>r['attempt'] for x in rows) else 'none' for r in rows if 'm1' in r['missing']))
print('m1 by provider', Counter((r['provider'],r['re_selected']) for r in rows if 'm1' in r['missing']))
print('codex m3 with RE in start', sum(1 for r in rows if 'm3' in r['missing'] and r['re_selected']))
# wall vs duration mismatches
print([(r['run_id'][:8],r['attempt'],r['wall_secs'],r['duration_secs'],r['result']) for r in rows if r['duration_secs']!='' and abs(r['duration_secs']-r['wall_secs'])>5])
print('retried causes',Counter(r['cause'] or '(cause無し)' for r in rows if r['result']=='retried'))
print('tok by date RE', Counter((r['started_at'][:10],r['re_selected'],r['provider']) for r in rows))
