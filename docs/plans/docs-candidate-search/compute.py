#!/usr/bin/env python3
"""Read an acceptance-check snapshot. Emit derived tables and event IDs, never a queue dump.
Uses acceptance-check's build ancestry, mapping, work overlap, area and tercile rules.
"""
import argparse
import csv
import gzip
import importlib.util
import json
import math
import re
import statistics
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent

def module(name):
    spec = importlib.util.spec_from_file_location(name, HERE.parent / 'acceptance-check' / (name + '.py'))
    obj = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(obj)
    return obj

c, f = module('compute'), module('fetch')

def number(x):
    return isinstance(x, (int, float)) and not isinstance(x, bool) and math.isfinite(x)

def seconds(a, b):
    try:
        return (c.ms(b['created_at']) - c.ms(a['created_at'])) / 1000
    except (ValueError, TypeError, KeyError):
        return None

def next_start(review, evs):
    return next((e for e in evs if e['id'] > review['id'] and e['kind'] == 'review_started'), None)

def between(review, evs):
    start = next_start(review, evs)
    return [e for e in evs if e['id'] > review['id'] and (not start or e['id'] < start['id'])]

def route(review, evs):
    p = review['payload']
    if 'route' in p and p['route'] is not None:
        return (p['route'] or {}).get('destination')
    if p.get('verdict') == 'revise':
        return 'send_back'
    if p.get('verdict') == 'concern':
        return 'send_back' if any(e['kind'] == 'revise_requested' for e in between(review, evs)) else 'ask'
    return 'land'

def docs(review):
    return any('docs_drift' in item for item in review['payload'].get('reason_codes', []))

def cycle(review, evs):
    """First non-cancelled request before next start, matching attempts for unsent/end."""
    errors = []
    span = between(review, evs)
    requests = [e for e in span if e['kind'] == 'revise_requested']
    active = []
    for req in requests:
        cancelled = any(e['kind'] == 'revise_unsent' and e['id'] > req['id']
                        and e['payload'].get('attempt') == req['payload'].get('attempt')
                        and not any(r['id'] > req['id'] and r['id'] < e['id'] for r in requests)
                        for e in span)
        if cancelled:
            errors.append('cancelled')
        else:
            active.append(req)
    if not active:
        if not requests:
            errors.append('not_sent')
        return {'revise': None, 'rereview': None, 'total': None, 'errors': errors}
    start = next_start(review, evs)
    if not start:
        errors.append('revise_unfinished')
        return {'revise': None, 'rereview': None, 'total': None, 'errors': errors}
    revise = seconds(active[0], start)
    if revise is None:
        errors.append('revise_missing_time')
    end = next((e for e in evs if e['id'] > start['id'] and e['kind'] in ('review_finished', 'review_failed')
                and e['payload'].get('attempt') == start['payload'].get('attempt')), None)
    rereview = None
    if not end or end['kind'] == 'review_failed':
        errors.append('rereview_unfinished_or_failed')
    else:
        rereview = end['payload'].get('duration_secs')
        if not number(rereview):
            rereview = None
            errors.append('rereview_missing_duration')
    return {'revise': revise, 'rereview': rereview,
            'total': revise + rereview if revise is not None and rereview is not None else None,
            'errors': errors, 'end': end}

# A path is recorded evidence, not proof that its semantics were checked.
PATH = re.compile(r'(?:docs/[\w./-]+|AGENTS\.md|CLAUDE\.md|plugins/[\w./-]+|[\w-]+\.md)')
UNNEEDED = re.compile(r'(?i)(?:docs?|documents?).{0,100}(?:no\s+(?:change|update)|not\s+(?:needed|required)|unchanged|unnecessary)|(?:no|not).{0,40}(?:docs?|documents?).{0,40}(?:needed|required|change|update)|文書.{0,80}(?:不要|変更なし|更新なし)|(?:照合|更新).{0,40}不要')
SEARCH = re.compile(r'(?i)(?:names?\s+searched|searched\s+names?|searched\s+(?:names?|terms?|for)|search\s+terms?|探した名前|検索した(?:名前|語)|検索語)\s*[:：]?\s*\S+')

def checked(summary):
    return bool(PATH.search(summary) or UNNEEDED.search(summary))

def write_csv(path, rows):
    with path.open('w', newline='', encoding='utf-8') as out:
        if rows:
            writer = csv.DictWriter(out, fieldnames=list(rows[0]), lineterminator='\n')
            writer.writeheader()
            writer.writerows(rows)

def aggregate(rows):
    n = len(rows)
    result = {'n': n}
    for key in ('drift_any', 'drift_primary', 'drift_reference', 'human', 'route_absent', 'summary_missing', 'check_absent', 'searched', 'work_missing', 'wait_unfinished', 'waiting_deferred'):
        result[key] = sum(r[key] for r in rows)
    judged = n-result['summary_missing']
    result['summary_n'] = judged
    for key, denom in [('drift_any',n),('drift_primary',n),('check_absent',judged),('searched',judged)]:
        result[key+'_rate'] = result[key]/denom if denom else None
    for key in ('work','work_excl_wait','revise','rereview','total','primary_revise','primary_rereview','primary_total'):
        values = [r[key] for r in rows if number(r[key])]
        result[key+'_n'] = len(values)
        result[key+'_median'] = statistics.median(values) if values else None
        if key not in ('work','work_excl_wait'):
            result[key+'_sum'] = sum(values) if values else None
    return result

def main():
    p = argparse.ArgumentParser()
    p.add_argument('snapshot', type=Path)
    p.add_argument('--cutoff', required=True)
    p.add_argument('--out', type=Path, default=HERE/'out')
    a = p.parse_args()
    norm = a.snapshot/'normalized'
    evs = [e for e in c.load_jsonl(norm/'events.jsonl') if c.ms(e['created_at']) < c.ms(a.cutoff)]
    tasks = {t['task_id']: t for t in c.load_jsonl(norm/'tasks.jsonl')}
    commits = {t['commit']: t for t in c.load_jsonl(norm/'commits.jsonl')}
    versions = {t['version']: t for t in c.load_jsonl(norm/'versions.jsonl')}
    t_info = c.find_t(evs, versions, [1429,1460,1688], [], a.cutoff)
    landings = {part['task']: part['landing']['commit'] for part in t_info['parts']}
    by_run = defaultdict(list)
    for e in evs:
        if e.get('run_id'):
            by_run[e['run_id']].append(e)
    pool = {'before': [], 'after': []}
    ancestry = {}
    for run, events in by_run.items():
        events.sort(key=lambda e:e['id'])
        claim = next((e for e in events if e['kind']=='run_claimed'), None)
        review = next((e for e in events if e['kind']=='review_finished' and e['payload'].get('attempt')==1), None)
        if not claim or not review:
            continue
        build = f.version_commit('.', claim['payload'].get('dagq_version'))
        if build not in ancestry:
            ancestry[build] = {str(task): f.holds('.', commit, build) if build else None for task,commit in landings.items()}
        holds = ancestry[build]
        if holds['1429'] is not True or holds['1460'] is not True:
            continue
        if holds['1688'] not in (True,False):
            raise ValueError('unknown ancestry: '+str(build))
        pool['after' if holds['1688'] else 'before'].append((claim,review,events,build))
    for group in pool:
        pool[group].sort(key=lambda x:(c.ms(x[0]['created_at']),x[0]['id']))
        pool[group] = pool[group][-40:] if group=='before' else pool[group][:40]
    base_rows = c.run_rows(evs,tasks,commits,c.load_areas(norm/'areas.toml'),None)
    bases = {r['run_id']:r for r in base_rows}
    bounds = c.terciles([bases[x[0]['run_id']] for x in pool['before']])
    c.bins(base_rows,bounds)
    with gzip.open(a.snapshot/'reference/stats.json.gz','rt') as stream:
        stats = json.load(stream)
    stats_rows = {r['run_id']:r for r in stats['runs']}
    rows, selected, anomalies, detection = [], [], [], []
    fingerprints = {}
    for group, members in pool.items():
        for claim, review, events, build in members:
            run = claim['run_id']
            base = bases[run]
            start = next((e for e in events if e['kind']=='review_started' and e['payload'].get('attempt')==1), None)
            config = (start['payload'].get('subagents') or {}).get('commit') if start else None
            if config not in fingerprints:
                fingerprints[config] = f.run(['git','rev-parse',config+':.dagq/review-agents']).strip() if config else 'unknown'
            fingerprint = fingerprints[config]
            detection.append({'interval':group,'run_id':run,'review_start_event':start['id'] if start else None,'review_start_at':start['created_at'] if start else None,'config_commit':config,'definitions_tree':fingerprint,'agents':';'.join(agent['agent']+':'+agent['digest'] for agent in (start['payload'].get('subagents') or {}).get('agents',[])) if start else ''})
            # Same last pre-review validation receipt as mapping(), with stricter empty-summary missing rule.
            val = c.validation_receipt_before(events, review)
            summary = (val['payload']['receipt'].get('summary') if val else None)
            missing = not isinstance(summary,str) or not summary.strip()
            absent = 'route' not in review['payload'] or review['payload']['route'] is None
            sb = route(review,events)=='send_back'
            drift = sb and docs(review)
            primary = sb and review['payload'].get('primary_code')=='docs_drift'
            if primary and not drift:
                raise ValueError('primary docs_drift without reason_codes docs_drift')
            receipt = next((e for e in events if e['kind']=='receipt_observed'),None)
            work = stats_rows.get(run,{}).get('work') if receipt else None
            excl = base['work_excl_wait']
            if number(work) and number(base['work']) and work != base['work']:
                raise ValueError('stats/event work mismatch: '+run)
            deferred = any(e['kind']=='run_waiting_deferred' and claim['id']<=e['id']<=receipt['id'] for e in events) if receipt else False
            row = {'interval':group,'run_id':run,'task_id':claim['task_id'],'claim_event':claim['id'],'claimed_at':claim['created_at'],'build':build,'review_event':review['id'],'review_at':review['created_at'],'validation_event':val['id'] if val else None,'detection':fingerprint,'change':base['change'],'areas':base['areas'] or 'unknown','lines':base['lines'],'complexity':base['lines_bin'],'drift_any':int(drift),'drift_primary':int(primary),'drift_reference':int(docs(review)),'human':int(route(review,events)=='ask'),'route_absent':int(absent),'summary_missing':int(missing),'check_absent':int(not missing and not checked(summary)),'check_evidence':((PATH.search(summary) or UNNEEDED.search(summary)).group(0) if not missing and checked(summary) else ''),'searched':int(not missing and bool(SEARCH.search(summary))),'work':work if number(work) else None,'work_excl_wait':excl if number(excl) and number(work) else None,'work_missing':int(not number(work)),'wait_unfinished':int(excl==c.UNFINISHED),'waiting_deferred':int(deferred),'revise':None,'rereview':None,'total':None,'primary_revise':None,'primary_rereview':None,'primary_total':None,'extra_cycles':None,'extra_seconds':None}
            if drift:
                first = cycle(review,events)
                for key in ('revise','rereview','total'):
                    row[key]=first[key]
                    row['primary_'+key]=first[key] if primary else None
                for err in first['errors']:
                    anomalies.append({'interval':group,'run_id':run,'task_id':row['task_id'],'stage':'first','kind':err})
                extra = []
                end = first.get('end')
                while end and end['kind']=='review_finished' and route(end,events)=='send_back':
                    part = cycle(end,events)
                    extra.append(part)
                    end = part.get('end')
                for part in extra:
                    for err in part['errors']:
                        anomalies.append({'interval':group,'run_id':run,'task_id':row['task_id'],'stage':'additional','kind':err})
                errors = first['errors'] + [err for part in extra for err in part['errors']]
                if errors:
                    anomalies.append({'interval':group,'run_id':run,'task_id':row['task_id'],'stage':'additional','kind':'excluded'})
                else:
                    row['extra_cycles']=len(extra)
                    row['extra_seconds']=sum(part['total'] for part in extra)
            for key in ('human','route_absent','summary_missing','work_missing','wait_unfinished','waiting_deferred'):
                if row[key]:
                    anomalies.append({'interval':group,'run_id':run,'task_id':row['task_id'],'stage':'run','kind':key})
            rows.append(row)
            selected.extend({'interval':group,'run_id':run,'task_id':row['task_id'],'event_id':e['id'],'kind':e['kind'],'created_at':e['created_at'],'attempt':e['payload'].get('attempt')} for e in events if e['kind'] in ('run_claimed','receipt_observed','validation_finished','review_started','review_finished','review_failed','revise_requested','revise_unsent','run_waiting_started','run_waiting_ended','run_waiting_deferred','run_integrated'))
    table = []
    for group in ('before','after'):
        members = [r for r in rows if r['interval']==group]
        groups = defaultdict(list)
        groups[('all','all')] = members
        for r in members:
            groups[('change',r['change'])].append(r)
            groups[('detection',r['detection'])].append(r)
            groups[('complexity',r['complexity'])].append(r)
            for area in r['areas'].split(';'):
                groups[('area',area)].append(r)
        for (layer,value),rs in sorted(groups.items()):
            table.append(dict(interval=group,layer=layer,value=value,**aggregate(rs)))
    a.out.mkdir(parents=True,exist_ok=True)
    for name,data in [('runs',rows),('table',table),('selected-events',selected),('anomalies',anomalies),('detection',detection)]:
        write_csv(a.out/(name+'.csv'),data)
    pages = json.loads((a.snapshot/'pages.json').read_text())
    integrated = sorted({e['created_at'] for e in evs if e['kind']=='run_integrated' and c.ms(e['created_at'])>=c.ms(a.cutoff)-86400000})
    meta = {'cutoff':a.cutoff,'line_terciles':bounds,'t':{k:v for k,v in t_info.items() if k!='evidence'},'ancestry':ancestry,'pages':len(pages),'nonempty_pages':sum(p['count']>0 for p in pages),'paged_events_with_duplicates':sum(p['count'] for p in pages),'unique_events':len(evs),'landings_last_24h':len(integrated)}
    (a.out/'meta.json').write_text(json.dumps(meta,ensure_ascii=False,indent=2)+'\n')
    (a.out/'pages.json').write_text('[\n'+',\n'.join(json.dumps(page) for page in pages)+'\n]\n')
    print(json.dumps([r for r in table if r['layer']=='all'],indent=2))

if __name__=='__main__':
    main()
