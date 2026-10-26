#!/usr/bin/env python3
"""Read-only baseline for follow-up membership. See docs/plans/follow-up-membership.md.

Collect once, then analyse the saved inputs without touching the queue. Outputs
are derived evidence, never a second task queue. Only the fixed dagq CLI reads
production data; no SQL, task list/show, cmux or mutations are used.
"""
import argparse
import datetime as dt
import hashlib
import json
import math
from pathlib import Path
import statistics
import subprocess


def stamp(value):
    return dt.datetime.fromisoformat(value.replace('Z', '+00:00'))


def seconds(a, b):
    return (stamp(b['created_at']) - stamp(a['created_at'])).total_seconds()


def distribution(values):
    values = sorted(values)
    return {'n': len(values), 'total': round(sum(values), 3),
            'median': statistics.median(values) if values else None,
            'p90': values[math.ceil(.9 * len(values)) - 1] if values else None}


def read_cli(*args):
    return json.loads(subprocess.check_output([str(Path.home() / '.local/bin/dagq'), *args]))


def collect(directory):
    directory.mkdir(parents=True, exist_ok=True)
    # Snapshot is a bounded sequential read, not an atomic database snapshot.
    started = dt.datetime.now(dt.timezone.utc).isoformat(timespec='milliseconds').replace('+00:00', 'Z')
    goals = read_cli('goal', 'list')
    snapshots = []
    for g in goals:
        before = dt.datetime.now(dt.timezone.utc).isoformat(timespec='milliseconds').replace('+00:00', 'Z')
        snapshot = read_cli('goal', 'show', str(g['id']), '--full')
        snapshot['read_started'] = before
        snapshot['read_ended'] = dt.datetime.now(dt.timezone.utc).isoformat(timespec='milliseconds').replace('+00:00', 'Z')
        snapshots.append(snapshot)
    ended = dt.datetime.now(dt.timezone.utc).isoformat(timespec='milliseconds').replace('+00:00', 'Z')
    events, cursor = [], 0
    while True:
        page = read_cli('events', '--all', '--full', '--after', str(cursor),
                        '--limit', '1000', '--until', ended)
        if not page['events']:
            break
        events.extend(page['events'])
        cursor = page['cursor']
    data = {'snapshot_started': started, 'snapshot_ended': ended,
            'goals': goals, 'snapshots': snapshots, 'events': events}
    path = directory / 'inputs.json'
    path.write_text(json.dumps(data, ensure_ascii=False) + '\n')
    return path


def analyse(data, since, until):
    all_events = sorted(data['events'], key=lambda e: e['id'])
    events = [e for e in all_events if stamp(e['created_at']) < stamp(until)]
    cohort = {}
    for e in events:
        if e['kind'] == 'follow_up_registered' and e['payload'].get('task_id') is not None:
            if stamp(e['created_at']) >= stamp(since):
                cohort.setdefault(e['payload']['task_id'], e)
    by_task, by_proposal = {}, {}
    for e in events:
        by_task.setdefault(e['task_id'], []).append(e)
        if e['kind'] == 'plan_review_finished':
            by_proposal.setdefault(e['payload'].get('proposal_id'), []).append(e)
    review_starts = {e['payload'].get('plan_review_id'): e for e in events
                     if e['kind'] == 'plan_review_started'}
    rows = []
    for task_id, registered in cohort.items():
        history = [e for e in by_task.get(task_id, []) if e['id'] >= registered['id']]
        submits = [e for e in history if e['kind'] == 'task_submitted']
        ready = next((e for e in history if e['kind'] == 'task_status_changed'
                      and e['payload'].get('to') == 'ready'), None)
        adoption = next((e for e in history if e['kind'] == 'follow_up_adopted'), None)
        submits = [e for e in submits if ready is None or e['id'] <= ready['id']]
        proposals = {e['payload']['proposal_id'] for e in submits}
        first_submit = {p: min(e['id'] for e in submits if e['payload']['proposal_id'] == p)
                        for p in proposals}
        reviews = sorted([e for p in proposals for e in by_proposal.get(p, [])
                          if e['id'] >= first_submit[p]
                          and (ready is None or stamp(e['created_at']) <= stamp(ready['created_at'])
                               or (review_starts.get(e['payload'].get('plan_review_id')) is not None
                                   and review_starts[e['payload']['plan_review_id']]['id'] < ready['id']))],
                         key=lambda e: e['id'])
        first = reviews[0] if reviews else None
        def elapsed(start, end):
            return seconds(start, end) if start and end and seconds(start, end) >= 0 else None
        rows.append({'task_id': task_id, 'registered_event': registered['id'],
                     'source_task_id': registered['task_id'], 'source_run_id': registered['run_id'],
                     'submit_event': submits[0]['id'] if submits else None,
                     'adopt_event': adoption['id'] if adoption else None,
                     'ready_event': ready['id'] if ready else None,
                     'proposal_ids': sorted(proposals),
                     'review_events': [e['id'] for e in reviews],
                     'first_verdict': first['payload'].get('verdict') if first else None,
                     'registered_to_submit': elapsed(registered, submits[0] if submits else None),
                     'registered_to_first_verdict': elapsed(registered, first),
                     'registered_to_ready': elapsed(registered, ready),
                     'adopt_to_ready': elapsed(adoption, ready),
                     'revise': sum(e['payload'].get('verdict') == 'revise' for e in reviews)})
    reviewed = [r for r in rows if r['first_verdict'] is not None]
    approved = [r for r in rows if r['ready_event'] is not None]
    # A session is counted once even when its planner handles a bundle of tasks.
    planners = {e['payload']['planner_id'] for e in events
                if e['kind'] == 'draft_planner_opened' and e['task_id'] in cohort}
    cohort_proposals = {p for row in rows for p in row['proposal_ids']}
    closed = {e['payload'].get('opened_event_id'): e for e in events
              if e['kind'] == 'session_closed'}
    sessions = []
    for e in events:
        p = e['payload']
        if e['kind'] != 'session_opened' or p.get('kind') != 'runtime_planner':
            continue
        if (p.get('planner_id') not in planners and e['task_id'] not in cohort
                and p.get('proposal_id') not in cohort_proposals):
            continue
        end = closed.get(e['id'])
        sessions.append({'open_event': e['id'], 'close_event': end['id'] if end else None,
                         'planner_id': p['planner_id'],
                         'seconds': seconds(e, end) if end else None})
    # Roll current goal/task snapshots back through the same event history.
    goals = {s['goal']['id']: {**s['goal'], 'closed': s['closed'],
             'status': s['goal'].get('status', 'open')} for s in data['snapshots']}
    read_times = {s['goal']['id']: s.get('read_started', data['snapshot_started'])
                  for s in data['snapshots']}
    task_reads = {t['id']: read_times[s['goal']['id']]
                  for s in data['snapshots'] for t in s['tasks']}
    races = []
    for s in data['snapshots']:
        tids = {t['id'] for t in s['tasks']}
        for e in all_events:
            if (s.get('read_started', data['snapshot_started']) <= e['created_at']
                    <= s.get('read_ended', data['snapshot_ended'])
                    and e['kind'] in ('task_created', 'task_status_changed', 'task_goal_changed',
                                      'goal_created', 'goal_closed', 'goal_status_changed')
                    and (e['task_id'] in tids or e.get('goal_id') == s['goal']['id']
                         or (e['kind'] == 'task_goal_changed'
                             and s['goal']['id'] in (e['payload'].get('from'), e['payload'].get('to')))
                         or (e['kind'] == 'task_created'
                             and e['payload'].get('goal_id') == s['goal']['id']))):
                races.append(e['id'])
    tasks = {t['id']: {'status': t['status'], 'goal': s['goal']['id']}
             for s in data['snapshots'] for t in s['tasks']}
    for e in reversed(all_events):
        if stamp(e['created_at']) < stamp(until):
            continue
        p, tid, gid = e['payload'], e['task_id'], e.get('goal_id')
        read_at = task_reads.get(tid, data['snapshot_started']) if tid is not None else read_times.get(gid, data['snapshot_started'])
        if stamp(e['created_at']) >= stamp(read_at):
            continue
        if e['kind'] == 'task_status_changed':
            tasks.setdefault(tid, {'goal': None})['status'] = p['from']
        elif e['kind'] == 'task_goal_changed':
            tasks.setdefault(tid, {'status': 'draft'})['goal'] = p['from']
        elif e['kind'] == 'task_created':
            tasks.pop(tid, None)
        elif e['kind'] == 'goal_created':
            goals.pop(gid, None)
        elif e['kind'] == 'goal_closed' and gid in goals:
            goals[gid]['closed'] = False
        elif e['kind'] == 'goal_status_changed' and gid in goals:
            goals[gid]['status'] = p['from']
    origins = {e['payload']['task_id'] for e in events
               if e['kind'] == 'follow_up_registered' and e['payload'].get('task_id') is not None}
    open_goals = [g['id'] for g in goals.values() if not g['closed'] and g['status'] == 'open']
    remaining = {g: sorted(t for t, v in tasks.items() if v['goal'] == g
                          and v['status'] not in ('completed', 'canceled')) for g in open_goals}
    only_follow = {g: ts for g, ts in remaining.items() if ts and all(t in origins for t in ts)}
    return {'since': since, 'until_exclusive': until, 'cohort_n': len(rows),
            'evaluation': evaluate(all_events, events, rows, data['snapshot_started']),
            'first_review': {'n': len(reviewed), 'pass': sum(r['first_verdict'] == 'pass' for r in reviewed),
                             'unreviewed': len(rows) - len(reviewed)},
            'durations_seconds': {k: distribution([r[k] for r in rows if r[k] is not None])
                                  for k in ('registered_to_submit', 'registered_to_first_verdict',
                                            'registered_to_ready', 'adopt_to_ready')},
            'approved_revise': distribution([r['revise'] for r in approved]),
            'runtime_planner_seconds': distribution([s['seconds'] for s in sessions if s['seconds'] is not None]),
            'runtime_planner_unclosed': sum(s['seconds'] is None for s in sessions),
            'snapshot_race_events': sorted(set(races)),
            'open_goals_n': len(open_goals),
            'one_remaining': {g: ts for g, ts in remaining.items() if len(ts) == 1},
            'only_follow_up_remaining': only_follow,
            'rows': rows, 'planner_sessions': sessions}


# A revise reason that names membership; the matches are read by hand before use.
MEMBERSHIP_WORDS = ('所属', 'membership', 'judge-follow-up', 'follow_up_judged',
                    'out_of_scope', '範囲外', 'acceptance_items')
TOKEN_PARTS = ('input', 'cache_read', 'cache_creation', 'output')
PLAN_REVIEW_ENDS = ('plan_review_finished', 'plan_review_failed', 'plan_review_discarded')


def token_sum(executions):
    """Executions counted once by event id; one without tokens_source is unmeasured, not 0."""
    out = {'executions': len(executions), 'measured': 0, 'by_provider': {}}
    for e in executions:
        p = e['payload']
        if not p.get('tokens') or not p.get('tokens_source'):
            continue
        out['measured'] += 1
        side = out['by_provider'].setdefault(e['provider'], dict.fromkeys(TOKEN_PARTS + ('total',), 0))
        for part in TOKEN_PARTS:
            side[part] += p['tokens'].get(part) or 0
            side['total'] += p['tokens'].get(part) or 0
    out['total'] = sum(v['total'] for v in out['by_provider'].values())
    return out


# Where an Execution falls against the adoption-to-first-ready span of the tasks it served.
# An Execution serving several tasks takes the first of these that any of them gives it.
BOUNDARIES = ('adopt_to_ready', 'crossing_adoption', 'crossing_ready', 'before_adoption',
              'after_ready', 'unapproved_or_unadopted')


def by_boundary(rows, at, planner_turns, review_runs, started, starts, draft_planners, revised):
    """Metric (6): only the Executions inside a task's span count; the rest are shown apart."""
    span = {r['task_id']: (stamp(at[r['adopt_event']]), stamp(at[r['ready_event']]))
            for r in rows if r['adopt_to_ready'] is not None}
    of_proposal = {}
    for r in rows:
        for p in r['proposal_ids']:
            of_proposal.setdefault(p, set()).add(r['task_id'])
    executions = []
    for e in planner_turns:
        p = e['payload']
        tasks = set(draft_planners.get(p['planner_id'], ()))
        for proposal in revised.get(p['planner_id'], ()):
            tasks |= of_proposal.get(proposal, set())
        begun = started.get((p['planner_id'], p.get('turn')), e)['created_at']
        executions.append((e, 'planners', stamp(begun), tasks, False))
    for e in review_runs:
        p = e['payload']
        start = starts.get(p.get('plan_review_id'))
        begun = (stamp(start['created_at']) if start else
                 dt.datetime.fromtimestamp(p['started_at'], dt.timezone.utc) if p.get('started_at')
                 else stamp(e['created_at']))
        executions.append((e, 'plan_reviews', begun, of_proposal.get(p.get('proposal_id'), set()), True))
    out = {b: {'planners': [], 'plan_reviews': []} for b in BOUNDARIES}
    for e, source, begun, tasks, review in executions:
        ended, places = stamp(e['created_at']), []
        for t in tasks:
            if t not in span:
                places.append('unapproved_or_unadopted')
                continue
            adopt, ready = span[t]
            if begun < adopt <= ended:
                places.append('crossing_adoption')
            elif ended < adopt:
                places.append('before_adoption')
            elif begun >= ready:
                places.append('after_ready')
            elif ended <= ready or review:
                # The review that approves writes its verdict just after ready; it is still in.
                places.append('adopt_to_ready')
            else:
                places.append('crossing_ready')
        place = min(places, key=BOUNDARIES.index) if places else 'unapproved_or_unadopted'
        out[place][source].append(e)
    return {b: {s: token_sum(v) for s, v in sources.items()} for b, sources in out.items()}


def evaluate(all_events, events, rows, snapshot_started):
    """Metrics (1) (4) (6) (7) (8) of the plan that need the membership records."""
    by_id = {r['task_id']: r for r in rows}
    submit_at = {r['task_id']: r['submit_event'] for r in rows}
    judged, judgements = {}, {}
    for e in all_events:
        if e['kind'] != 'follow_up_judged':
            continue
        judgements.setdefault(e['payload']['id'], e)
        if e['task_id'] in by_id and (submit_at[e['task_id']] is None or e['id'] < submit_at[e['task_id']]):
            judged[e['task_id']] = e['payload']
    reviews = {e['id']: e for e in events if e['kind'] == 'plan_review_finished'}
    starts = {e['payload'].get('plan_review_id'): e for e in events if e['kind'] == 'plan_review_started'}
    adopted = {}
    for e in events:
        if e['kind'] == 'follow_up_adopted' and e['task_id'] in by_id:
            adopted.setdefault(e['task_id'], e['payload'])
    bundles = {}
    for e in events:
        if e['kind'] == 'task_submitted':
            bundles.setdefault(e['payload'].get('proposal_id'), set()).add(e['task_id'])
    first_reviews, strata = {}, {}
    for r in rows:
        if r['first_verdict'] is None:
            continue
        review = reviews[r['review_events'][0]]
        first_reviews.setdefault(review['payload']['proposal_id'], review)
        launch = (starts.get(review['payload'].get('plan_review_id')) or {}).get('payload', {}).get('launch') or {}
        members = len(bundles.get(review['payload']['proposal_id'], ()))
        adoption = adopted.get(r['task_id']) or {}
        for layer, value in (('judged_before_submit', r['task_id'] in judged),
                             ('plan_review_provider', launch.get('provider')),
                             ('bundle', 'single' if members == 1 else 'bundle'),
                             ('adopted_by', adoption.get('by')),
                             ('depth', adoption.get('depth'))):
            cell = strata.setdefault(layer, {}).setdefault(str(value), {'n': 0, 'pass': 0})
            cell['n'] += 1
            cell['pass'] += r['first_verdict'] == 'pass'
    membership_revises = sorted(
        e['id'] for e in first_reviews.values() if e['payload'].get('verdict') == 'revise'
        and any(w in reason for reason in e['payload'].get('reasons') or [] for w in MEMBERSHIP_WORDS))
    # (1) leaves out a judgement without a source goal: there is no acceptance to judge by.
    sourceless = sorted(t for t, p in judged.items() if p.get('source_goal_id') is None)
    classified = [r for r in rows if r['first_verdict'] is not None and r['task_id'] in judged
                  and r['task_id'] not in sourceless]
    # A turn started before its planner's first submit is pre-submit work; a later turn of the
    # same live planner (findings delivered to it) is revise work, as is a revise planner's turn.
    draft_planners, revise_planners, first_submit, revised = {}, set(), {}, {}
    proposals = {p for r in rows for p in r['proposal_ids']}
    for e in events:
        p = e['payload']
        if e['kind'] == 'draft_planner_opened' and set(p.get('members') or [e['task_id']]) & set(by_id):
            draft_planners.setdefault(p['planner_id'], set()).update(p.get('members') or [e['task_id']])
        elif e['kind'] == 'plan_revise_sent' and p.get('proposal_id') in proposals:
            revise_planners.add(p['planner_id'])
            revised.setdefault(p['planner_id'], set()).add(p['proposal_id'])
    for e in events:
        actor = (e.get('actor') or {}).get('id') or ''
        if e['kind'] == 'task_submitted' and actor.startswith('planner:') and actor[8:].isdigit():
            planner = int(actor[8:])
            if e['task_id'] in draft_planners.get(planner, ()):
                first_submit.setdefault(planner, e['id'])
    started = {(e['payload'].get('planner_id'), e['payload'].get('turn')): e for e in events
               if e['kind'] == 'turn_started'}
    pre_turns, revise_turns = [], []
    for e in events:
        p = e['payload']
        if e['kind'] != 'turn_finished' or p.get('planner_id') is None:
            continue
        planner, run = p['planner_id'], {**e, 'provider': p.get('provider')}
        if planner in draft_planners:
            begun = started.get((planner, p.get('turn')), e)['id']
            (pre_turns if planner not in first_submit or begun < first_submit[planner]
             else revise_turns).append(run)
        elif planner in revise_planners:
            revise_turns.append(run)
    ends = [e for e in events if e['kind'] in PLAN_REVIEW_ENDS and e['payload'].get('proposal_id') in proposals]
    ended_sessions = {e['payload'].get('session_id') for e in ends}
    # A takeover's stop of a job whose end is already written is not a second Execution.
    stops = [e for e in events if e['kind'] == 'headless_job_stopped' and e['payload'].get('kind') == 'plan_review'
             and e['payload'].get('proposal_id') in proposals
             and e['payload'].get('session_id') not in ended_sessions]
    review_runs = [{**e, 'provider': ((starts.get(e['payload'].get('plan_review_id')) or {}).get('payload', {})
                                      .get('launch') or {}).get('provider') or e['payload'].get('provider')}
                   for e in ends + stops]
    submitted = sum(r['submit_event'] is not None for r in rows)
    approved = [r for r in rows if r['ready_event'] is not None]
    at = {e['id']: e['created_at'] for e in events}
    ready_at = {r['task_id']: stamp(at[r['ready_event']]) for r in approved}
    boundary = by_boundary(rows, at, pre_turns + revise_turns, review_runs, started, starts,
                           draft_planners, revised)
    corrections, wrong = [], set()
    for e in all_events:
        p = e['payload']
        if (e['kind'] != 'follow_up_judged' or e['task_id'] not in by_id
                or p.get('corrects') is None or e['task_id'] not in ready_at):
            continue
        if not dt.timedelta(0) <= stamp(e['created_at']) - ready_at[e['task_id']] <= dt.timedelta(days=30):
            continue
        before = judgements.get(p['corrects'])
        changed = None if before is None else (
            before['payload'].get('classification'), before['payload'].get('destination_goal_id')) != (
            p.get('classification'), p.get('destination_goal_id'))
        corrections.append({'event': e['id'], 'task_id': e['task_id'], 'corrects': p['corrects'],
                            'classification_or_destination_changed': changed})
        if changed:
            wrong.add(e['task_id'])
    observed_30d = sum(stamp(snapshot_started) - at >= dt.timedelta(days=30) for at in ready_at.values())
    return {'judged_before_submit': len(judged), 'judged_without_source_goal': sourceless,
            'classification_first_pass': {'n': len(classified),
                                          'pass': sum(r['first_verdict'] == 'pass' for r in classified)},
            'first_pass_by_proposal': {'n': len(first_reviews),
                                       'pass': sum(e['payload'].get('verdict') == 'pass'
                                                   for e in first_reviews.values())},
            'first_pass_strata': strata,
            'first_revise_naming_membership': membership_revises,
            'tokens': {'pre_submit_planners': token_sum(pre_turns),
                       'by_boundary': boundary,
                       'submitted': submitted, 'approved': len(approved),
                       'approved_with_adoption': sum(r['adopt_to_ready'] is not None for r in rows)},
            'corrections_after_ready': {'events': corrections, 'wrong_tasks': sorted(wrong),
                                        'approved': len(approved), 'observed_30_days': observed_30d},
            'correct_goal_asks': sum(e['kind'] == 'ask_opened' and e['payload'].get('kind') == 'correct_goal'
                                     for e in all_events)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--collect', type=Path, help='save read-only CLI inputs in this directory')
    parser.add_argument('--input', type=Path, help='recompute from inputs.json; no CLI calls')
    parser.add_argument('--since', default='2026-09-22T00:00:00Z')
    parser.add_argument('--until', default='2026-10-04T00:00:00Z')
    args = parser.parse_args()
    if bool(args.collect) == bool(args.input):
        parser.error('choose exactly one of --collect and --input')
    path = collect(args.collect) if args.collect else args.input
    raw = path.read_bytes()
    data = json.loads(raw)
    if stamp(args.until) > stamp(data['snapshot_started']):
        parser.error('--until must precede snapshot_started for rollback')
    result = analyse(data, args.since, args.until)
    result['inputs_sha256'] = hashlib.sha256(raw).hexdigest()
    result['snapshot_started'] = data['snapshot_started']
    result['snapshot_ended'] = data['snapshot_ended']
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
