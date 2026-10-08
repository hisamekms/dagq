import json, sys
D=sys.argv[1]; rows=json.load(open(f'{D}/rows.json'))
idx={r['run_id']+'#'+str(r['attempt']):r for r in rows}
only=sorted(json.load(open(f'{D}/only.json')))
n=len(only); k=min(20,n)
pick=[only[(i*n)//k] for i in range(k)]
json.dump(pick,open(f'{D}/pick.json','w'))
for i,key in enumerate(pick):
    r=idx[key]
    print(f"### S{i+1} {key} task {r['task_id']} class={r['path_class']} RE={r['re_verdict']}/{r['re_dest']} parent={r['parent_dest']} sel={r['selected']}")
    for t in r['_reasons'].get('receipt-evidence',[]): print('  RE:',(t if isinstance(t,str) else json.dumps(t,ensure_ascii=False))[:700])
    for t in r['_parent_reasons'][:4]: print('  P:',(t if isinstance(t,str) else json.dumps(t,ensure_ascii=False))[:400])
