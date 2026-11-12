import json,re,collections,sys
W0,W1='2026-10-26T00:00:00Z','2026-11-10T00:00:00Z'
ev=collections.defaultdict(list)
for l in open(sys.argv[1]):
    e=json.loads(l); ev[e['run_id']].append(e)
ACC=re.compile(r'受け入れ(条件)?の?(項目)?\s*([0-9０-９]|[(（]\s*[0-9０-９A-Za-z])|\bAC\s?[(（]?\s*[0-9]|acceptance\s*(criteri(on|a)|items?)?\s*([0-9]|[(（#]\s*[0-9A-Za-z])',re.I)  # 受け入れ条件の番号（記号）を名指す文だけ
runs={}
for r,es in ev.items():
    es.sort(key=lambda e:e['id'])
    rfs=[e for e in es if e['kind']=='review_finished']
    if not rfs: continue
    if not (W0<=rfs[0]['created_at']<W1): continue
    runs[r]=es
items=[]
def accrel(text,codes):
    return ('acceptance_unmet' in (codes or [])) or bool(ACC.search(text))
for r,es in runs.items():
    for e in es:
        if e['kind']!='review_finished': continue
        p=e['payload']; v=p['verdict']
        if v in('revise','concern'):
            rc=p.get('reason_codes') or []
            for i,t in enumerate(p.get('reasons') or []):
                codes=rc[i] if i<len(rc) else []
                if accrel(t,codes): items.append(dict(run=r,task=e['task_id'],attempt=p['attempt'],at=e['created_at'],src='whole',verdict=v,codes=codes,primary=p.get('primary_code'),text=t,eid=e['id']))
        for a in p.get('agents') or []:
            if a.get('verdict') in('revise','concern'):
                rc=a.get('reason_codes') or []
                for i,t in enumerate(a.get('reasons') or []):
                    codes=rc[i] if i<len(rc) else []
                    if accrel(t,codes): items.append(dict(run=r,task=e['task_id'],attempt=p['attempt'],at=e['created_at'],src='agent:'+a['agent'],verdict=a['verdict'],codes=codes,text=t,eid=e['id']))
json.dump({'runs':sorted(runs)},open('runs.json','w'))
with open('acc_items.jsonl','w') as f:
    for it in items: f.write(json.dumps(it,ensure_ascii=False)+'\n')
print('reviewed runs',len(runs),'acc items',len(items),'runs w/ acc',len({i['run'] for i in items}))
print(collections.Counter((i['src'] if i['src']=='whole' else 'agent',i['verdict'],'code' if 'acceptance_unmet' in i['codes'] else 'text') for i in items))
print(collections.Counter(i['src'] for i in items).most_common())
