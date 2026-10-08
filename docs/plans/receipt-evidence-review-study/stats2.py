import json, sys, math, re
from collections import Counter
D=sys.argv[1]; rows=json.load(open(f'{D}/rows.json'))
def med(v):
    v=sorted(v); n=len(v)
    return None if not n else (v[n//2] if n%2 else (v[n//2-1]+v[n//2])/2)
def g2r(g):
    out=''; i=0
    while i<len(g):
        if g.startswith('**/',i): out+='(?:.*/)?'; i+=3
        elif g.startswith('**',i): out+='.*'; i+=2
        elif g[i]=='*': out+='[^/]*'; i+=1
        elif g[i]=='?': out+='[^/]'; i+=1
        else: out+=re.escape(g[i]); i+=1
    return re.compile('^'+out+'$')
CUR={ 'design-consistency':["src/**","crates/**","docs/design/**"],
 'test-rules':["tests/**","src/**","crates/**","Cargo.toml",".config/e2e-quarantine.toml",".config/it-slow-allow.toml"],
 'migration-rules':["migrations/**","build.rs","src/infrastructure/schema.rs","src/infrastructure/schema/**"],
 'adr-rules':["docs/adr/**"], 'config-rules':["dagq.toml",".config/**"], 'plugin-generic':["plugins/**"],
 'architecture-boundaries':["src/**","crates/**"]}
CUR={k:[g2r(x) for x in v] for k,v in CUR.items()}
B=[g2r(x) for x in ["src/**","crates/**","migrations/**","build.rs","tests/**","Cargo.toml","Cargo.lock","scripts/**","plugins/**",".github/**",".config/**"]]
def m(globs,paths): return any(g.match(p) for g in globs for p in paths)
# paths per RE-selected attempt
st={ (e['run_id'],e['payload']['attempt']):e for e in json.load(open(f'{D}/start.json')) if e['run_id']}
other_paths=Counter()
for r in rows:
    e=st[(r['run_id'],r['attempt'])]; sub=e['payload'].get('subagents')
    r['paths']=next((a['paths'] for a in (sub or {}).get('agents',[]) if a['agent']=='receipt-evidence'),None)
    if r['path_class']=='その他' and r['paths']:
        for p in r['paths']: other_paths[p.split('/')[0] if '/' in p else p]+=1
print('other top-level',other_paths.most_common(20))
re_rows=[r for r in rows if r['re_selected']]
notB=[r for r in re_rows if not m(B,r['paths'])]
print('B removes',len(notB),Counter(r['path_class'] for r in notB))
print('B keeps but class docs/config',Counter(r['path_class'] for r in re_rows if m(B,r['paths']) and r['path_class'] in ('docsだけ','configだけ','docsとconfigだけ')))
sw=[r for r in re_rows if r['switch_reason']=='subagents_unsupported']
def others_cur(r): return [a for a,g in CUR.items() if m(g,r['paths'])]
print('sw',len(sw))
print('D(no RE) no other agent (current globs)',sum(not others_cur(r) for r in sw), Counter(r['path_class'] for r in sw if not others_cur(r)))
print('D no other agent (recorded selection)',sum(r['selected']=='receipt-evidence' for r in sw))
print('B no required (current globs)',sum((not others_cur(r)) and (not m(B,r['paths'])) for r in sw))
print('B no required (recorded)',sum(r['selected']=='receipt-evidence' and not m(B,r['paths']) for r in sw))
# proxy by class
classes=['docsだけ','configだけ','docsとconfigだけ','runtimeを含む','その他']
for metric,ok in [('total',lambda r:r['tok_ok']),('total_nocr',lambda r:r['tok_ok']),('wall_secs',lambda r:r['wall_secs']!=''),('active_secs',lambda r:r['act_ok'])]:
    print('==',metric)
    for c in classes:
        a=[r[metric] for r in rows if r['path_class']==c and r['re_selected'] and ok(r)]
        b=[r[metric] for r in rows if r['path_class']==c and not r['re_selected'] and ok(r)]
        b_cl=[r[metric] for r in rows if r['path_class']==c and not r['re_selected'] and ok(r) and r['provider']=='claude']
        print(f'  {c}: RE n={len(a)} med={med(a)}  noRE n={len(b)} med={med(b)} (claudeのみ n={len(b_cl)} med={med(b_cl)})  diff={None if not a or not b else med(a)-med(b)}')
# removed counts for B by class and by metric validity
print('B removed by class',Counter(r['path_class'] for r in notB))
print('B removed tok_ok',sum(r['tok_ok'] for r in notB),'wall ok',sum(r['wall_secs']!='' for r in notB),'act ok',sum(r['act_ok'] for r in notB))
# time added by retries naming RE
for r in rows:
    if r['result'] in ('retried','failed') and 'receipt-evidence' in r['error']:
        nx=[x for x in rows if x['run_id']==r['run_id'] and x['attempt']==r['attempt']+1]
        print('RE retry',r['run_id'],r['attempt'],r['wall_secs'],[(x['attempt'],x['result'],x['wall_secs']) for x in nx])
# m1 detail: next start before end vs none
