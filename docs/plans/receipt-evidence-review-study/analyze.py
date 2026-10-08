import json, sys, subprocess, math, csv, os, re
from datetime import datetime
D = sys.argv[1]; WT = sys.argv[2]; OUT = sys.argv[3]
def ts(s): return datetime.fromisoformat(s.replace('Z','+00:00'))
start = [e for e in json.load(open(f'{D}/start.json')) if e['run_id']]
end = json.load(open(f'{D}/end.json'))
W0, W1, RD = ts('2026-10-01T00:00:00Z'), ts('2026-10-08T00:00:00Z'), ts('2026-10-09T00:00:00Z')
start = [e for e in start if W0 <= ts(e['created_at']) < W1]
ends = {}
for e in end:
    if e['kind'] in ('review_finished','review_retried','review_failed') and e['run_id']:
        ends.setdefault((e['run_id'], e['payload'].get('attempt')), []).append(e)
closed = {}
for e in end:
    if e['kind']=='session_closed':
        closed.setdefault(e['payload'].get('session_id'), []).append(e)
starts_by_run = {}
for e in start: starts_by_run.setdefault(e['run_id'], []).append(e)
# landed commits
landed = {}
for line in open(f'{D}/landed.txt'):
    h, r = line.split(); landed.setdefault(r, h)
def git(*a): return subprocess.run(['git','-C',WT,*a],capture_output=True,text=True).stdout
def landed_paths(run):
    h = landed.get(run)
    if h: return [p for p in git('show','--name-only','--format=',h).splitlines() if p], '着地のcommit'
    ref = f'refs/dagq/runs/{run}'
    if git('rev-parse','--verify','-q',ref).strip():
        mb = git('merge-base',ref,'main').strip()
        return [p for p in git('diff','--name-only',mb,ref).splitlines() if p], 'runのref'
    return None, '不明'
def is_docs(p): return p.startswith('docs/') or ('/' not in p and p.endswith('.md'))
def is_cfg(p): return p=='dagq.toml' or p.startswith('.config/') or p.startswith('.dagq/')
def is_rt(p): return p.startswith('src/') or p.startswith('crates/') or p.startswith('migrations/') or p=='build.rs'
def classify(paths):
    if paths is None: return '不明'
    if not paths: return '差分なし'
    if any(is_rt(p) for p in paths): return 'runtimeを含む'
    d = all(is_docs(p) for p in paths); c = all(is_cfg(p) for p in paths)
    if d: return 'docsだけ'
    if c: return 'configだけ'
    if all(is_docs(p) or is_cfg(p) for p in paths): return 'docsとconfigだけ'
    return 'その他'
RANK = {'land':0,'send_back':1,'ask':2}
rows = []
for s in sorted(start, key=lambda e:(e['run_id'], e['payload']['attempt'])):
    p = s['payload']; run = s['run_id']; att = p['attempt']; st = ts(s['created_at'])
    launch = p.get('launch') or {}
    sub = p.get('subagents')
    sel = [a['agent'] for a in sub['agents']] if sub else None
    re_sel = bool(sel and 'receipt-evidence' in sel)
    # end
    cands = sorted([e for e in ends.get((run, att), []) if ts(e['created_at']) >= st and ts(e['created_at']) < RD], key=lambda e:(e['created_at'], e['id']))
    nxt = sorted([e for e in starts_by_run[run] if ts(e['created_at']) > st], key=lambda e:e['created_at'])
    # include next starts outside window too
    endev = cands[0] if cands else None
    nextstart = nxt[0] if nxt else None
    if endev and nextstart and ts(nextstart['created_at']) < ts(endev['created_at']): endev = None
    r = dict(run_id=run, task_id=s['task_id'], attempt=att, started_at=s['created_at'], start_event=s['id'],
             provider=launch.get('provider'), switched_from=launch.get('switched_from') or '', switch_reason=launch.get('switch_reason') or '',
             selected=';'.join(sel) if sel is not None else '', subagents_recorded=sub is not None, re_selected=re_sel)
    # paths
    if sub and sub['agents'] and re_sel:
        paths = next(a['paths'] for a in sub['agents'] if a['agent']=='receipt-evidence'); src = 'review_started.subagents'
    elif sub is not None and not sub['agents']:
        paths = []; src = 'review_started.subagents（agentなし）'
    else:
        paths, src = landed_paths(run)
    r['path_class'] = classify(paths); r['path_source'] = src; r['n_paths'] = len(paths) if paths is not None else ''
    miss = []
    re_verdict = re_dest = others_dest = parent = final = ''
    r['agents_recorded'] = r['route_recorded'] = False
    if not endev:
        r['result'] = '終了なし'; miss.append('m1'); r['end_at']=''; r['wall_secs']=''; r['duration_secs']=''
        r['cause']=r['error']=r['code']=''
    else:
        ep = endev['payload']; r['end_at']=endev['created_at']; r['end_event']=endev['id']
        r['wall_secs'] = round((ts(endev['created_at'])-st).total_seconds())
        r['duration_secs'] = ep.get('duration_secs','')
        r['cause'] = ep.get('cause','') or ''; r['error'] = (ep.get('error','') or '').replace('\n',' ')[:300]; r['code'] = ep.get('code','') or ''
        if endev['kind']=='review_finished':
            r['result'] = ep['verdict']
            ag = ep.get('agents'); rt = ep.get('route')
            r['agents_recorded'] = ag is not None; r['route_recorded'] = rt is not None
            if ag is None: miss.append('m3')
            if rt is None: miss.append('m4')
            if ag:
                for a in ag:
                    if a['agent']=='receipt-evidence': re_verdict = a.get('verdict') or a.get('status')
            if rt:
                parent = rt.get('parent'); final = rt.get('destination')
                od = []
                for a in rt.get('agents',[]):
                    if a['agent']=='receipt-evidence': re_dest = a['destination']
                    else: od.append(f"{a['agent']}={a['destination']}")
                others_dest = ';'.join(od)
            r['_reasons'] = {a['agent']: a.get('reasons',[]) for a in (ag or [])}
            r['_parent_reasons'] = ep.get('reasons',[])
            r['_rt'] = rt
        else:
            r['result'] = 'retried' if endev['kind']=='review_retried' else 'failed'
            miss.append('m2')
    r.update(re_verdict=re_verdict, re_dest=re_dest, others_dest=others_dest, parent_dest=parent or '', final_dest=final or '')
    # session
    sc = closed.get(p.get('session_id'))
    tok_ok = act_ok = False
    for k in ('input','output','cache_creation','cache_read','active_secs','session_reason','active_unavailable'): r[k]=''
    if not sc:
        miss.append('m5')
    else:
        c = sc[0]['payload']; r['session_reason']=c.get('reason',''); r['active_unavailable']=c.get('active_unavailable') or ''
        t = c.get('tokens')
        if t and all(isinstance(t.get(k),(int,float)) for k in ('input','output','cache_creation','cache_read')) and not (c.get('reason')=='inferred' and all(t[k]==0 for k in ('input','output','cache_creation','cache_read'))):
            tok_ok = True
            for k in ('input','output','cache_creation','cache_read'): r[k]=t[k]
        else:
            if not t: why = 'tokens無し:' + (c.get('active_unavailable') or c.get('reason'))
            elif not all(isinstance(t.get(k),(int,float)) for k in ('input','output','cache_creation','cache_read')): why='欄の欠け'
            else: why='inferredで全0'
            miss.append('m6'); r['m6_why']=why
        if isinstance(c.get('active_secs'),(int,float)) and c.get('active')!='unavailable':
            act_ok = True; r['active_secs']=c['active_secs']
        else:
            miss.append('m7'); r['m7_why']=c.get('active_unavailable') or 'active_secs無し'
    r['tok_ok']=tok_ok; r['act_ok']=act_ok; r['missing']=';'.join(miss)
    if tok_ok:
        r['total']=r['input']+r['output']+r['cache_creation']+r['cache_read']; r['total_nocr']=r['total']-r['cache_read']
    rows.append(r)
json.dump(rows, open(f'{D}/rows.json','w'), ensure_ascii=False, default=str)
cols = ['run_id','task_id','attempt','started_at','result','cause','error','code','path_class','path_source','n_paths','selected','re_verdict','re_dest','others_dest','parent_dest','final_dest','provider','switched_from','switch_reason','wall_secs','duration_secs','active_secs','input','output','cache_creation','cache_read','missing','m6_why','m7_why']
with open(OUT,'w',newline='') as f:
    w = csv.writer(f); w.writerow(cols)
    for r in rows: w.writerow([r.get(c,'') for c in cols])
print(len(rows))
