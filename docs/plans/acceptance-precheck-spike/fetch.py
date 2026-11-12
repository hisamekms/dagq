import json, subprocess, sys, os
WT=os.environ['WT']; OUT=sys.argv[1]; CUT=sys.argv[2]
def ev(args):
    res=[]; after=0
    while True:
        o=subprocess.run(['dagq','events','--full','--limit','1000','--after',str(after)]+args,cwd=WT,capture_output=True,text=True,check=True).stdout
        d=json.loads(o); es=d['events']
        res+=es
        if len(es)<1000: return res
        after=es[-1]['id']
rf=ev(['--kind','review_finished','--since','2026-10-26T00:00:00Z','--until','2026-11-10T00:00:00Z'])
runs=sorted({e['run_id'] for e in rf if e.get('run_id')})
print(len(rf),len(runs),file=sys.stderr)
os.makedirs(OUT,exist_ok=True)
with open(OUT+'/events.jsonl','w') as f:
    for i,r in enumerate(runs):
        for e in ev(['--all','--run',r,'--until',CUT]):
            e.pop('cursor',None); f.write(json.dumps(e,sort_keys=True,ensure_ascii=False)+'\n')
