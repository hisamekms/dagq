import json, sys, math
from collections import Counter
D=sys.argv[1]; rows=json.load(open(f'{D}/rows.json'))
N=len(rows)
def p90(v): v=sorted(v); return v[math.ceil(0.9*len(v))-1] if v else None
def med(v):
    v=sorted(v); n=len(v)
    if not n: return None
    return v[n//2] if n%2 else (v[n//2-1]+v[n//2])/2
def summ(v): return f"n={len(v)} 合計={sum(v):,} 中央値={med(v):,} p90={p90(v):,}" if v else "n=0"
C=Counter
print('N',N)
print('results',C(r['result'] for r in rows))
print('provider',C((r['provider'],r['switched_from'],r['switch_reason']) for r in rows))
print('subagents_recorded',C(r['subagents_recorded'] for r in rows),'re_selected',C(r['re_selected'] for r in rows))
ms=C(); 
for r in rows:
    for m in r['missing'].split(';'):
        if m: ms[m]+=1
print('missing',sorted(ms.items()))
print('m3 by re_selected',C(r['re_selected'] for r in rows if 'm3' in r['missing']))
print('m3 by provider',C((r['provider'],r['re_selected']) for r in rows if 'm3' in r['missing']))
print('m4 by provider',C((r['provider'],r['re_selected'],'m3' in r['missing']) for r in rows if 'm4' in r['missing']))
print('m6 why',C(r.get('m6_why') for r in rows if 'm6' in r['missing']))
print('m7 why',C(r.get('m7_why') for r in rows if 'm7' in r['missing']))
print('m5 by provider/result',C((r['provider'],r['result']) for r in rows if 'm5' in r['missing']))
print('m1',[(r['run_id'],r['attempt'],r['started_at']) for r in rows if 'm1' in r['missing']])
tok=[r for r in rows if r['tok_ok']]; act=[r for r in rows if r['act_ok']]; wall=[r for r in rows if r['wall_secs']!='']
m5=sum('m5' in r['missing'] for r in rows); m6=sum('m6' in r['missing'] for r in rows); m7=sum('m7' in r['missing'] for r in rows)
print('valid tok',len(tok),'act',len(act),'wall',len(wall))
for name,grp in [('all',lambda r:True),('RE',lambda r:r['re_selected']),('noRE',lambda r:not r['re_selected'])]:
    print('==',name, 'pop',sum(grp(r) for r in rows))
    g=[r for r in tok if grp(r)]
    print(' tok valid',len(g),'excl m5',sum(grp(r) and 'm5' in r['missing'] for r in rows),'excl m6',sum(grp(r) and 'm6' in r['missing'] for r in rows))
    for k in ('input','output','cache_creation','cache_read','total','total_nocr'):
        print('  ',k,summ([r[k] for r in g]))
    g=[r for r in wall if grp(r)]
    print(' wall',summ([r['wall_secs'] for r in g]),'excl m1',sum(grp(r) and 'm1' in r['missing'] for r in rows))
    dd=[r for r in g if r['duration_secs']!='']
    print(' duration_secs',summ([r['duration_secs'] for r in dd]), 'diff>5s',sum(abs(r['duration_secs']-r['wall_secs'])>5 for r in dd), 'maxdiff', max([abs(r['duration_secs']-r['wall_secs']) for r in dd] or [0]))
    g=[r for r in act if grp(r)]
    print(' active',summ([r['active_secs'] for r in g]),'excl m5',sum(grp(r) and 'm5' in r['missing'] for r in rows),'excl m7',sum(grp(r) and 'm7' in r['missing'] for r in rows))
print('RE dest',C(r['re_dest'] for r in rows if r['re_selected']))
print('RE verdict',C(r['re_verdict'] for r in rows if r['re_selected']))
print('path class',C(r['path_class'] for r in rows))
print('path source',C(r['path_source'] for r in rows))
print('path x RE',C((r['path_class'],r['re_selected']) for r in rows))
print('path RE heavier',C(r['path_class'] for r in rows if r['re_dest'] in ('send_back','ask')))
print('result x RE',C((r['result'],r['re_selected']) for r in rows))
print('retried',[(r['run_id'],r['attempt'],r['cause'],r['error']) for r in rows if r['result']=='retried'])
print('failed codes',C((r['code'],'receipt-evidence' in r['error'], r['error'][:60]) for r in rows if r['result']=='failed'))
# only RE heavier
den=[r for r in rows if r['re_selected'] and not any(m in r['missing'] for m in ('m2','m3','m4','m1'))]
only=[r for r in den if r['re_dest'] in ('send_back','ask') and all(a['destination']=='land' for a in r['_rt']['agents'] if a['agent']!='receipt-evidence')]
only_inc_parent=[r for r in only if r['parent_dest']=='land']
print('den',len(den),'RE heavier & others land',len(only),'and parent land',len(only_inc_parent))
print('RE heavier any',sum(r['re_dest'] in ('send_back','ask') for r in den))
print('only by dest',C((r['re_dest'],r['parent_dest'],r['final_dest']) for r in only))
print('only by path',C(r['path_class'] for r in only))
json.dump([r['run_id']+'#'+str(r['attempt']) for r in only], open(f'{D}/only.json','w'))
# selected only RE
print('selected exactly RE',C((r['path_class'],r['switch_reason']) for r in rows if r['selected']=='receipt-evidence'))
print('sel combos for RE sw',C(r['selected'] for r in rows if r['switch_reason']=='subagents_unsupported').most_common(12))
