# 費用の集計。review の回 = 前の review_finished の後の最初の review_programs_started / review_started から、
# 次の review_finished まで。job は payload の attempt で回に入れる（前の review_finished の attempt より大きく、
# この review_finished の attempt 以下）。再起動で review_finished の無い attempt の job は次の回に入る。
import json,collections,sys,statistics as st
from datetime import datetime,timezone
def T(s):
    s=s.rstrip('Z'); f='%Y-%m-%dT%H:%M:%S.%f' if '.' in s else '%Y-%m-%dT%H:%M:%S'
    return datetime.strptime(s,f).replace(tzinfo=timezone.utc).timestamp()
ev=collections.defaultdict(list)
for l in open('events.jsonl'):
    e=json.loads(l); ev[e['run_id']].append(e)
runs=json.load(open('runs.json'))['runs']
acc=json.load(open('items_final.json'))
acceid={i['eid'] for i in acc}
def tok(t):
    if not t: return None
    return sum(t.get(k,0) or 0 for k in('input','output','cache_read','cache_creation'))
R={}; rounds=[]; unassigned=collections.Counter()
for r in runs:
    es=sorted(ev[r],key=lambda e:e['id'])
    rfs=[e for e in es if e['kind']=='review_finished']
    info=dict(run=r,task=rfs[0]['task_id'],first=rfs[0]['created_at'],verdicts=[e['payload']['verdict'] for e in rfs],
              integrated=any(e['kind']=='run_integrated' for e in es))
    asks={}
    for e in es:
        p=e['payload']
        if e['kind']=='ask_opened': asks[p['ask_id']]=dict(kind=p['kind'],opened=T(e['created_at']),answered=None,id=p['ask_id'])
        if e['kind']=='ask_answered' and p.get('ask_id') in asks and asks[p['ask_id']]['answered'] is None: asks[p['ask_id']]['answered']=T(e['created_at'])
    firstbad=next((T(e['created_at']) for e in rfs if e['payload']['verdict']!='pass'),None)
    for a in asks.values(): a['after_sendback']= firstbad is not None and a['opened']>=firstbad
    info['asks']=list(asks.values())
    # jobs, deduped by session_id (review_job_finished first)
    jobs=[]; seen=set()
    for e in es:
        p=e['payload']
        if e['kind']=='review_job_finished':
            jobs.append(dict(att=p.get('attempt'),agent=p.get('agent'),tok=tok(p.get('tokens')),sec=p.get('duration_secs') or 0)); seen.add(p.get('session_id'))
    for e in es:
        p=e['payload']
        if e['kind']=='headless_job_stopped' and p.get('kind')=='review' and p.get('session_id') not in seen:
            jobs.append(dict(att=p.get('attempt'),agent=p.get('agent') or '(stopped)',tok=tok(p.get('tokens')),sec=0,stopped=True))
    prev_att=0; prev_id=0; prev_t=None
    for rf in rfs:
        att=rf['payload']['attempt']; tend=T(rf['created_at'])
        seg=[e for e in es if prev_id<e['id']<rf['id']]
        starts=[e for e in seg if e['kind'] in('review_programs_started','review_started')]
        t0=T(starts[0]['created_at']) if starts else None
        ps=[e for e in seg if e['kind']=='review_programs_started']; pf=[e for e in seg if e['kind']=='review_programs_finished']
        prog=(T(pf[-1]['created_at'])-T(ps[0]['created_at'])) if ps and pf else None
        mine=[j for j in jobs if j['att'] is not None and prev_att<j['att']<=att]
        by=collections.defaultdict(lambda:[0,0])
        miss=0
        for j in mine:
            k=j['agent'] or '(whole)'
            if j['tok'] is None: miss+=1
            by[k][0]+=j['tok'] or 0; by[k][1]+=j['sec']
        wtok=by['(whole)'][0] if '(whole)' in by else 0; wsec=by['(whole)'][1] if '(whole)' in by else 0
        stok=sum(v[0] for k,v in by.items() if k not in('(whole)','(stopped)')); htok=by['(stopped)'][0] if '(stopped)' in by else 0; ssec=sum(v[1] for k,v in by.items() if k!='(whole)')
        restarted=len({j['att'] for j in mine})>1
        nxt=next((e for e in es if e['id']>rf['id'] and e['kind'] in('review_programs_started','review_started')),None)
        rework=rework_noask=None
        if rf['payload']['verdict']!='pass' and nxt:
            a0=tend;a1=T(nxt['created_at']); rework=a1-a0; w=0
            for a in asks.values():
                if a['answered'] and a['opened']<a1 and a['answered']>a0: w+=min(a1,a['answered'])-max(a0,a['opened'])
            rework_noask=rework-w
        rounds.append(dict(run=r,attempt=att,eid=rf['id'],verdict=rf['payload']['verdict'],wtok=wtok,stok=stok,htok=htok,wsec=wsec,ssec=ssec,
            by={k:v for k,v in by.items()},wall=(tend-t0) if t0 else None,prog=prog,rework=rework,rework_noask=rework_noask,
            tokmiss=miss,restarted=restarted,acc=rf['id'] in acceid))
        prev_att=att; prev_id=rf['id']
    for j in jobs:
        if j['att'] is None or j['att']>prev_att: unassigned['jobs']+=1; unassigned['tok']+=j['tok'] or 0
    R[r]=info
json.dump({'runs':R,'rounds':rounds},open('cost.json','w'))
def ms(x):
    x=[v for v in x if v is not None]; return f"{st.median(x):,.0f} / {st.mean(x):,.0f}" if x else '-'
print('reviewed runs',len(R),'rounds',len(rounds),'restarted rounds',sum(x['restarted'] for x in rounds),'unassigned',dict(unassigned))
for lab,sel in [('all',lambda x:True),('sendback',lambda x:x['verdict']!='pass'),('acc',lambda x:x['acc'])]:
    rr=[x for x in rounds if sel(x)]
    print(lab,len(rr),'wtok',ms([x['wtok'] for x in rr]),'stok',ms([x['stok'] for x in rr]),'htok',ms([x['htok'] for x in rr]),'tot',ms([x['wtok']+x['stok']+x['htok'] for x in rr]),'wall',ms([x['wall'] for x in rr]),'compute',ms([x['wsec']+x['ssec'] for x in rr]),'prog',ms([x['prog'] for x in rr]),'rework',ms([x['rework'] for x in rr]),'noask',ms([x['rework_noask'] for x in rr]),'n_rework',sum(1 for x in rr if x['rework'] is not None),'tokmiss',sum(x['tokmiss'] for x in rr))
    print('  sums htok %.0fM'%(sum(x['htok'] for x in rr)/1e6)); print('  sums wtok %.0fM stok %.0fM compute %.1fh wsec %.1fh wall %.1fh rework %.1fh noask %.1fh'%(sum(x['wtok'] for x in rr)/1e6,sum(x['stok'] for x in rr)/1e6,sum(x['wsec']+x['ssec'] for x in rr)/3600,sum(x['wsec'] for x in rr)/3600,sum(x['wall'] or 0 for x in rr)/3600,sum(x['rework'] or 0 for x in rr)/3600,sum(x['rework_noask'] or 0 for x in rr)/3600))
ag=collections.defaultdict(lambda:[0,0,0])
for x in rounds:
    for k,v in x['by'].items(): ag[k][0]+=v[0]; ag[k][1]+=v[1]; ag[k][2]+=1
print('per agent (tokens M, compute h, rounds, mean tok per round M)')
for k,v in sorted(ag.items(),key=lambda kv:-kv[1][0]): print(' ',k,'%.0f'%(v[0]/1e6),'%.1f'%(v[1]/3600),v[2],'%.2f'%(v[0]/v[2]/1e6))
