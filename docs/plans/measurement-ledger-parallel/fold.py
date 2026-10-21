"""Re-fold run and task lives from `dagq events --full` output, following
src/domain/ledger/fold.rs (fold_run / fold_task), to read the totals the
ledger rows hold but no read-only command prints (task unattributed,
blocker / holds / slot per row). Overlays (pre-claim waits) are not
needed for the totals and are skipped.

usage: python3 -I fold.py <tasks_dir> <pushes.jsonl> <ledger.json> <out.json>
"""
import json, os, sys, statistics
from datetime import datetime, timezone

def ms(text):
    if not text:
        return None
    t = text.rstrip('Z')
    d, tm = t.replace(' ', 'T').split('T')
    if '.' in tm:
        s, f = tm.split('.')
    else:
        s, f = tm, ''
    f = (f + '000')[:3]
    dt = datetime.strptime(d + 'T' + s, '%Y-%m-%dT%H:%M:%S').replace(tzinfo=timezone.utc)
    return int(dt.timestamp()) * 1000 + int(f)

HELD_TAGS = ('human', 'none')

def interval(phase, blocker, holds, start, end, run=None, attempt=None):
    return dict(phase=phase, blocker=blocker, holds=holds, start=start, end=end, run=run, attempt=attempt)

def unattr(start, end):
    return interval('unattributed', None, None, start, end)

def finished(events):
    return any(e['kind'] == 'run_integrated' or (e['payload'] or {}).get('status') in
               ('failed', 'interrupted', 'succeeded', 'integrated') for e in events)

def later_push(events, pushes, after):
    failed = None
    for e in reversed(events):
        if e['kind'] == 'push_failed' and e['id'] < after:
            failed = e
            break
    for p in pushes:
        if p['kind'] != 'push_finished' or p['id'] <= after:
            continue
        if failed is None:
            return p
        ok = True
        for k in ('remote', 'branch'):
            fv = failed['payload'].get(k)
            if fv is not None and fv != p['payload'].get(k):
                ok = False
        if ok:
            return p
    return None

def fold_run(events, pushes, now):
    recs = [(e, ms(e['created_at'])) for e in events if e['kind'] == 'run_phase_changed']
    recs = [(e, a) for e, a in recs if a is not None]
    if not recs and finished(events):
        return None
    claimed = next((ms(e['created_at']) for e in events if e['kind'] == 'run_claimed'), None)
    first_at = recs[0][1] if recs else now
    start = claimed if claimed is not None else (ms(events[0]['created_at']) if events else first_at)
    start = min(start, first_at)
    life = dict(start=start, intervals=[], end=None, outcome=None,
                integrated=any(e['kind'] == 'run_integrated' for e in events))
    if first_at > start:
        life['intervals'].append(unattr(start, first_at))
    cursor = start
    for i, (e, a) in enumerate(recs):
        p = e['payload'] or {}
        frm = max(a, cursor)
        def iv(s, t):
            if p.get('blocker') is None or p.get('holds') is None:
                return unattr(s, t)
            return interval(p.get('phase', 'unattributed'), p['blocker'], p['holds'], s, t, attempt=p.get('attempt'))
        if i + 1 < len(recs):
            to = max(recs[i + 1][1], frm)
            life['intervals'].append(iv(frm, to))
            cursor = to
            continue
        if p.get('phase') == 'ended':
            life['end'] = frm
            life['outcome'] = p.get('cause')
        elif p.get('phase') == 'push_pending' and (push := later_push(events, pushes, e['id'])):
            to = max(ms(push['created_at']) or frm, frm)
            life['intervals'].append(iv(frm, to))
            life['end'] = to
            life['outcome'] = 'pushed'
        else:
            life['intervals'].append(iv(frm, max(now, frm)))
    return life

def statuses(events, start):
    moves = [(ms(e['created_at']), e['payload']) for e in events
             if e['kind'] in ('task_status_changed', 'run_claimed')]
    moves = [(a, p) for a, p in moves if a is not None]
    first = (moves[0][1].get('from') if moves else None) or 'unattributed'
    out = [(start, first)]
    began = start
    for a, p in moves:
        to = p.get('to')
        if to is None:
            continue
        began = max(began, a)
        out.append((began, to))
    return out

def hold_spans(events):
    spans = []
    for e in events:
        a = ms(e['created_at'])
        if a is None:
            continue
        hid = (e['payload'] or {}).get('hold_id')
        if e['kind'] == 'task_held':
            spans.append([hid, a, None])
        elif e['kind'] == 'task_released':
            for s in spans:
                if s[2] is None and (s[0] is None or s[0] == hid):
                    s[2] = max(a, s[1])
                    break
    return [(s[1], s[2] if s[2] is not None else 2**62) for s in spans]

def fold_task(events, pushes, now):
    runs = []
    for c in events:
        if c['kind'] != 'run_claimed' or not c.get('run_id'):
            continue
        own = [e for e in events if e.get('run_id') == c['run_id']]
        ready_at = ms((c['payload'] or {}).get('ready_at'))
        runs.append((c['run_id'], ready_at, fold_run(own, pushes, now)))
    if any(l is None for _, _, l in runs):
        return None
    created = next((e for e in events if e['kind'] == 'task_created'), events[0] if events else None)
    start = ms(created['created_at']) if created else now
    sts = statuses(events, start)
    end = outcome = None
    if sts and sts[-1][1] == 'canceled':
        end, outcome = sts[-1][0], 'canceled'
    elif sts and sts[-1][1] == 'completed':
        landed = next((l for _, _, l in reversed(runs) if l['integrated']), None)
        if landed:
            end, outcome = landed['end'], ('completed' if landed['end'] is not None else None)
        else:
            end, outcome = sts[-1][0], 'completed'
    horizon = max(end if end is not None else now, start)
    holds = hold_spans(events)
    claims = [(l['start'], r) for _, r, l in runs]
    ivs = []

    def push_ready(phase, frm, to):
        cur = frm
        hs = sorted((max(s, frm), min(e, to)) for s, e in holds if min(e, to) > max(s, frm))
        for s, e in hs:
            s = max(s, cur)
            if e <= s:
                continue
            if s > cur:
                ivs.append(interval(phase, 'queue', 'none', cur, s))
            ivs.append(interval('held', *HELD_TAGS, s, e))
            cur = e
        if to > cur:
            ivs.append(interval(phase, 'queue', 'none', cur, to))

    def status(st, frm, to):
        if st == 'draft':
            ivs.append(interval('draft', 'human', 'none', frm, to))
        elif st == 'submitted':
            ivs.append(interval('plan_review', 'ai', 'none', frm, to))
        elif st == 'ready':
            ra = next((r for c, r in claims if c >= frm), None)
            if ra is not None:
                ra = min(max(ra, frm), to)
            if ra is not None and ra > frm:
                push_ready('dependency_wait', frm, ra)
                if to > ra:
                    push_ready('ready', ra, to)
                return
            push_ready('ready', frm, to)
        else:
            ivs.append(unattr(frm, to))

    def between(frm, to):
        for i, (b, st) in enumerate(sts):
            until = sts[i + 1][0] if i + 1 < len(sts) else 2**62
            a, z = max(b, frm), min(until, to)
            if z > a:
                status(st, a, z)

    cursor = start
    for run, _, life in runs:
        rs = min(max(life['start'], cursor), horizon)
        between(cursor, rs)
        for iv in life['intervals']:
            f, t = max(iv['start'], rs), min(iv['end'], horizon)
            if t > f:
                ivs.append(dict(iv, start=f, end=t, run=run))
        rend = life['intervals'][-1]['end'] if life['intervals'] else life['start']
        cursor = max(cursor, min(rend, horizon))
    between(cursor, horizon)
    return dict(start=start, end=end, outcome=outcome, intervals=ivs, runs=runs)

def totals(life):
    t = dict(duration=0, unattributed=0, slot=0, blocker={}, holds={})
    for iv in life['intervals']:
        d = iv['end'] - iv['start']
        t['duration'] += d
        if iv['phase'] == 'unattributed':
            t['unattributed'] += d
            continue
        t['blocker'][iv['blocker']] = t['blocker'].get(iv['blocker'], 0) + d
        t['holds'][iv['holds']] = t['holds'].get(iv['holds'], 0) + d
        if iv['holds'] in ('worker_slot', 'landing_slot'):
            t['slot'] += d
    return t

def main():
    tdir, pushes_path, ledger_path, out_path = sys.argv[1:5]
    pushes = sorted((json.loads(l) for l in open(pushes_path)), key=lambda e: e['id'])
    ledger = json.load(open(ledger_path))
    now = ledger['window_ms'][1]  # every row is final: none folds up to now
    rows = {(r['kind'], r['key']): r for r in ledger['reconcile']['rows']
            if r['kind'] in ('run', 'task')}
    cmp_unattr = {r['run_id']: r['unattributed'] for r in ledger['runs']}
    use = {k: r for k, r in rows.items() if r['row'] and r['is_final'] and r['up_to_date']}
    out = dict(run=[], task=[])
    run_lives = {}
    for f in sorted(os.listdir(tdir)):
        evs = sorted((json.loads(l) for l in open(os.path.join(tdir, f))), key=lambda e: e['id'])
        tid = f.split('.')[0]
        tl = fold_task(evs, pushes, now)
        if ('task', tid) in use:
            if tl is None:
                out['task'].append(dict(key=tid, legacy=True))
            else:
                out['task'].append(dict(key=tid, end=tl['end'], outcome=tl['outcome'], **totals(tl)))
        runs = {}
        for e in evs:
            if e.get('run_id'):
                runs.setdefault(e['run_id'], []).append(e)
        for rid, revs in runs.items():
            if ('run', rid) in use:
                rl = fold_run(revs, pushes, now)
                if rl is None:
                    out['run'].append(dict(key=rid, legacy=True))
                else:
                    t = totals(rl)
                    out['run'].append(dict(key=rid, task=tid, end=rl['end'], outcome=rl['outcome'],
                                           ledger_unattributed_s=cmp_unattr.get(rid), **t))
    out['missing'] = sorted('%s/%s' % k for k in use
                            if k[1] not in {r['key'] for r in out[k[0]]})
    json.dump(out, open(out_path, 'w'), indent=1)

main()
