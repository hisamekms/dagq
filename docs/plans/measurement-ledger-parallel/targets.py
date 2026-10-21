"""Count what ended in the window from the terminal events themselves
(ADR-t1662-1 decision 16), independently of the ledger's own `ended_at`:

- a landed run ends at its first `push_finished` or `push_skipped` after
  `run_integrated`; a `push_failed` with neither after it leaves the run
  in `push_pending` (counted apart, at the `push_failed`);
- a run that does not land ends at its last `run_unfinished_changed` with
  `unfinished: false` (to failed, interrupted or succeeded: the failure,
  the cancel, the interruption, a closed `already_satisfied`), unless a
  later one makes it unfinished again or it lands later;
- a task ends at its last `task_status_changed` to `completed` or
  `canceled`, unless a later move takes it out again.

Then each target against the ledger's rows as the comparison read gave
them (`reconcile.rows` of ledger.json): a row or not, final, push_pending,
up to date.

usage: python3 -I targets.py <ledger.json> <out.json>
reads with the fixed binary (read only): dagq events --full ... (below)
"""
import json, subprocess, sys

LO, HI = '2026-10-13T12:00:52Z', '2026-10-20T12:00:52Z'


def events(*args):
    out, after = [], 0
    while True:
        page = json.loads(subprocess.run(
            ['dagq', 'events', '--full', *args, '--limit', '1000', '--after', str(after)],
            check=True, capture_output=True, text=True).stdout)
        out += page['events']
        if len(page['events']) < 1000:
            return out
        after = page['cursor']


def in_window(e):
    return LO <= e['created_at'][:19] + 'Z' < HI if len(e['created_at']) >= 19 else False


def main():
    ledger_path, out_path = sys.argv[1:3]
    window = ['--since', LO, '--until', HI]
    push = events('--kind', 'push_finished', '--kind', 'push_skipped', '--kind', 'push_failed', *window)
    unfin = events('--kind', 'run_unfinished_changed', *window)
    moves = events('--kind', 'task_status_changed', *window)
    cand_runs = sorted({e['run_id'] for e in push} |
                       {e['run_id'] for e in unfin
                        if e['payload'].get('unfinished') is False and e['payload'].get('to') != 'integrated'})
    runs, run_ends = {}, {}
    for rid in cand_runs:
        evs = sorted(events('--all', '--run', rid), key=lambda e: e['id'])
        runs[rid] = evs
        integrated = next((e for e in evs if e['kind'] == 'run_integrated'), None)
        if integrated:
            pushes = [e for e in evs if e['id'] > integrated['id']
                      and e['kind'] in ('push_finished', 'push_skipped', 'push_failed')]
            closed = next((e for e in pushes if e['kind'] != 'push_failed'), None)
            failed = next((e for e in pushes if e['kind'] == 'push_failed'), None)
            if closed and (failed is None or closed['id'] < failed['id']):
                run_ends[rid] = (closed, 'ended')
            elif failed:
                run_ends[rid] = (failed, 'push_pending' if closed is None else 'push_pending_closed')
            continue
        changes = [e for e in evs if e['kind'] == 'run_unfinished_changed']
        if changes and changes[-1]['payload'].get('unfinished') is False:
            run_ends[rid] = (changes[-1], 'ended')
    cand_tasks = sorted({e['task_id'] for e in moves if e['payload'].get('to') in ('completed', 'canceled')})
    task_ends, reopened = {}, []
    for tid in cand_tasks:
        evs = sorted(events('--all', '--task', str(tid), '--kind', 'task_status_changed'),
                     key=lambda e: e['id'])
        last = evs[-1]
        terminal = [e for e in evs if e['payload'].get('to') in ('completed', 'canceled')]
        if last['payload'].get('to') in ('completed', 'canceled'):
            task_ends[str(tid)] = last
        else:
            reopened.append((tid, terminal[-1]['id'], last['id'], last['payload'].get('to')))
    L = json.load(open(ledger_path))
    rows = {(r['kind'], r['key']): r for r in L['reconcile']['rows']}
    out = {}
    for kind, ends in (('run', {k: v[0] for k, v in run_ends.items()}), ('task', task_ends)):
        target = sorted(k for k, e in ends.items() if in_window(e))
        state = {k: rows.get((kind, k)) for k in target}
        pending = sorted(k for k in target if kind == 'run' and run_ends[k][1] == 'push_pending')
        led = sorted(key for (kk, key) in rows if kk == kind)
        out[kind] = dict(
            ended=len(target),
            rows=sum(1 for s in state.values() if s and s['row']),
            unknown=sorted(k for k, s in state.items() if s is None),
            missing=sorted(k for k, s in state.items() if s is not None and not s['row']),
            not_final=sorted(k for k, s in state.items() if s and s['row'] and not s['is_final']
                             and k not in pending),
            push_pending=pending,
            dirty=sorted(k for k, s in state.items() if s and s['row'] and not s['up_to_date']),
            ledger_only=sorted(set(led) - set(target)),
            ends={k: [ends[k]['id'], ends[k]['kind'], ends[k]['created_at']] for k in target},
        )
    out['run_classes'] = {k: v[1] for k, v in run_ends.items()}
    out['reopened_tasks'] = reopened
    out['ended_outside'] = {
        'run': sorted(k for k, v in run_ends.items() if not in_window(v[0])),
        'task': sorted(k for k, e in task_ends.items() if not in_window(e)),
    }
    json.dump(out, open(out_path, 'w'), indent=1)
    for kind in ('run', 'task'):
        o = out[kind]
        print(kind, {k: (v if isinstance(v, int) else len(v)) for k, v in o.items() if k != 'ends'})


main()
