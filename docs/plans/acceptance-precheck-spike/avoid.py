# 丸ごと無くせる回の見積もり（形ごと）。候補の回 = 全体の review の理由が全て受け入れ条件の理由で、
# job の非 pass が受け入れ条件の理由だけの回。形が捕まえる型の組 C について、理由が見つかる確率は
#   Q2 で差分を読んだ理由: その判定（holds=1、does_not_hold・unresolved=0）。ただし読んだ型（読み手の型）が C に無ければ 0
#   読んでいない理由: 型が C にあれば Q2 の型ごとの holds の率（指定の標本）、無ければ 0
# 回が無くなる確率はその回の理由の確率の積。
import json,collections,sys,statistics as st
ev=collections.defaultdict(list)
for l in open('events.jsonl'):
    e=json.loads(l); ev[e['run_id']].append(e)
items=json.load(open('items_final.json'))
cost=json.load(open('cost.json')); RO={(x['run'],x['attempt']):x for x in cost['rounds']}
import csv
# samples.csv: Q2 で差分を読んだ理由。sample_type は指定の標本の型（参考の読みは reference）
SAMPLES=sys.argv[1] if len(sys.argv)>1 else 'samples.csv'
reads={}; cnt=collections.defaultdict(lambda:[0,0])
for row in csv.DictReader(open(SAMPLES)):
    reads[row['item']]=dict(verdict=row['verdict'],type_manual=row['read_type'])
    if row['sample_type']!='reference':
        cnt[row['sample_type']][0]+=row['verdict']=='holds'; cnt[row['sample_type']][1]+=1
rate={t:h/n for t,(h,n) in cnt.items()}       # 型 -> 指定の標本の holds / 標本
byeid=collections.defaultdict(list)
for i in items: byeid[i['eid']].append(i)
runs=set(json.load(open('runs.json'))['runs'])
cand=[]
for r in runs:
    for e in ev[r]:
        if e['kind']!='review_finished' or e['payload']['verdict']=='pass': continue
        p=e['payload']; its=byeid.get(e['id'],[])
        if not its: continue
        acc_w=[i for i in its if i['src']=='whole']
        agbad=[a for a in p.get('agents') or [] if a.get('verdict')!='pass']
        nonacc_agent=[a for a in agbad if len([i for i in its if i['src']=='agent:'+a['agent']])!=len(a.get('reasons') or [])]
        if len(acc_w)==len(p.get('reasons') or []) and not nonacc_agent:
            cand.append(dict(run=r,att=p['attempt'],verdict=p['verdict'],its=its,x=RO[(r,p['attempt'])]))
def prob(i,C):
    rd=reads.get(str(i['id']))
    if rd: return 1.0 if rd['verdict']=='holds' and rd['type_manual'] in C else 0.0
    return rate[i['type']] if i['type'] in C else 0.0
def agg(C):
    out=dict(rounds=0,revise=0,attempt1=0,whole=0,agents=0,stopped=0,compute=0,wall=0,rework=0,noask=0); per=collections.Counter()
    for c in cand:
        v=1.0
        for i in c['its']: v*=prob(i,C)
        if not v: continue
        x=c['x']; out['rounds']+=v; out['revise']+=v*(c['verdict']=='revise'); out['attempt1']+=v*(c['att']==1)
        out['whole']+=v*x['wtok']; out['agents']+=v*x['stok']; out['stopped']+=v*x['htok']
        out['compute']+=v*(x['wsec']+x['ssec']); out['wall']+=v*(x['wall'] or 0); out['rework']+=v*(x['rework'] or 0); out['noask']+=v*(x['rework_noask'] or 0)
        for k,(t,s) in x['by'].items():
            if k not in('(whole)','(stopped)'): per[k]+=v*t
    f={k:round(out[k],1) for k in('rounds','revise','attempt1')}
    f.update({k:round(out[k]/1e6,1) for k in('whole','agents','stopped')}); f['total']=round((out['whole']+out['agents']+out['stopped'])/1e6,1)
    f.update({k+'_h':round(out[k]/3600,1) for k in('compute','wall','rework','noask')})
    f['per_agent']={k:round(v/1e6,1) for k,v in per.most_common()}
    return f
allr=cost['rounds']; W=sum(x['wtok'] for x in allr); WS=sum(x['wsec'] for x in allr); TOT=sum(x['wtok']+x['stok']+x['htok'] for x in allr)
print('candidates',len(cand),'rates',rate)
for name,C in [('(ii) a-e','abcde'),('(iii) b','b'),('(iii)+(ii) a,c,d','abcd'),('(ii) a,c,d only','acd'),('upper: (ii) a-e with f at its rate','abcdef')]:
    f=agg(C); print(name,f)
    for k in (0.15,0.05):
        add=k*W/1e6; print('   ratio',k,'add tok %.0fM compute %.1fh'%(add,k*WS/3600),'net %+.0fM (%.1f%% of %.0fM)'%(add-f['total'],100*(add-f['total'])/(TOT/1e6),TOT/1e6))
# 全ての差し戻しが無くなる run の見込み（形ごと）
np=collections.defaultdict(list)
for x in cost['rounds']:
    if x['verdict']!='pass': np[x['run']].append(x['attempt'])
for name,C in [('(ii) a-e','abcde'),('(iii) b','b'),('(iii)+(ii) a,c,d','abcd'),('upper','abcdef')]:
    cm={}
    for c in cand:
        v=1.0
        for i in c['its']: v*=prob(i,C)
        cm[(c['run'],c['att'])]=v
    tot=0.0
    for r,a in np.items():
        pr=1.0
        for at in a: pr*=cm.get((r,at),0.0)
        tot+=pr
    print('runs cleared',name,round(tot,1),'of',len(np))
