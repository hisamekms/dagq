"""The before/after comparison of the landing verification switch (mark 123128)
from the records read once: stats --full of each window, the events of both
windows (fetch_events.py), the main CI runs and ci-failure issues (gh), and the
replay of landing-it's choice (replay_choice.py). Writes the CSVs into OUT and
prints summary.json. Usage:
  python3 compute.py IN_DIR OUT_DIR > OUT_DIR/summary.json
IN_DIR holds stats_before.json, stats_after.json, kpi_compare.json, events.json, ci_<day>.json
(gh run list per day), ci_issues.json and choice.csv."""
import csv, datetime as D, glob, json, math, os, re, statistics as S, sys

IN, OUT = sys.argv[1], sys.argv[2]
T0, T, T1 = '2026-09-30T10:27:40.972Z', '2026-10-07T10:27:40.972Z', '2026-10-14T10:27:40.972Z'
HOURS = 168.0

def ts(s):
    return D.datetime.fromisoformat(s.replace('Z', '+00:00'))

def window(at):
    if T0 <= at < T: return 'before'
    if T <= at < T1: return 'after'
    return None

def pct(v, p):  # nearest rank: the ceil(p*n)-th of the sorted values
    v = sorted(v)
    return v[max(0, math.ceil(p * len(v)) - 1)] if v else None

def med(v):
    return S.median(v) if v else None

def write(name, header, rows):
    with open(os.path.join(OUT, name), 'w', newline='') as f:
        w = csv.writer(f); w.writerow(header); w.writerows(rows)

def iso(dt):
    return dt.strftime('%Y-%m-%dT%H:%M:%S.') + f'{dt.microsecond // 1000:03d}Z'

summary = {'windows': {'before': [T0, T], 'after': [T, T1]}}
ev = sorted(json.load(open(os.path.join(IN, 'events.json'))), key=lambda x: x['id'])

# (a)(b): per landed run, from stats --full (runs[].land_phases); ask is the
# phase of a person's answer, so "without a person's wait" = runs whose ask is 0.
rows, ab = [], {}
for w in ('before', 'after'):
    st = json.load(open(os.path.join(IN, f'stats_{w}.json')))
    runs = [r for r in st['runs'] if r['status'] == 'integrated']
    for r in runs:
        lp = r['land_phases']
        cmds = [c['command'] for c in lp['verify_commands']]
        gate = ('llvm-cov' if any('llvm-cov' in c for c in cmds) else
                'landing-it' if 'sh scripts/landing-it.sh' in cmds else 'none')
        rows.append([w, r['run_id'], r['task_id'], r['landed_at'], r.get('change'), lp['verify'],
                     lp['landing_queue'], lp['ask'], gate])
    def block(sel):
        v = [r['land_phases']['verify'] for r in sel]; q = [r['land_phases']['landing_queue'] for r in sel]
        return {'n': len(sel), 'verify': {'median': med(v), 'p75': pct(v, .75), 'p90': pct(v, .9)},
                'landing_queue': {'median': med(q), 'p75': pct(q, .75), 'p90': pct(q, .9)}}
    ab[w] = {'all': block(runs), 'no_person_wait': block([r for r in runs if r['land_phases']['ask'] == 0]),
             'stats_overall': {k: st['overall']['land_phases'][k] for k in ('verify', 'landing_queue')},
             'landing_utilization': st['landing_utilization'],
             'by_gate': {g: block([r for r in runs if (('llvm-cov' if any('llvm-cov' in c['command'] for c in r['land_phases']['verify_commands']) else 'landing-it' if any(c['command'] == 'sh scripts/landing-it.sh' for c in r['land_phases']['verify_commands']) else 'none') == g)]) for g in ('llvm-cov', 'landing-it', 'none')}}
write('runs.csv', ['window', 'run_id', 'task_id', 'landed_at', 'change', 'verify_secs', 'landing_queue_secs', 'ask_secs', 'test_gate'], rows)
summary['ab'] = ab

# The gate's own command (verification_command events).
gate_rows = []
for x in ev:
    p = x['payload']
    if x['kind'] != 'verification_command': continue
    c = p.get('command', '')
    if 'llvm-cov nextest' not in c and c != 'sh scripts/landing-it.sh': continue
    w = window(x['created_at'])
    if not w: continue
    m = re.search(r'Summary \[\s*([\d.]+)s\] (\d+) tests? run: \d+ passed(?:.*?(\d+) skipped)?', p.get('output_tail') or '')
    gate_rows.append([w, x['id'], x['created_at'], x['run_id'], c, p['duration_secs'], p['exit_code'], p.get('load_avg_mean'),
                      m.group(1) if m else '', m.group(2) if m else '', (m.group(3) or 0) if m else ''])
write('gate_commands.csv', ['window', 'event_id', 'at', 'run_id', 'command', 'duration_secs', 'exit_code', 'load_avg_mean', 'nextest_secs', 'tests_run', 'skipped'], gate_rows)
gs = {}
for w in ('before', 'after'):
    g = [r for r in gate_rows if r[0] == w]
    d = [r[5] for r in g]
    gs[w] = {'n': len(g), 'median': med(d), 'p90': pct(d, .9), 'load_median': med([r[7] for r in g if r[7] is not None])}
choice = list(csv.DictReader(open(os.path.join(IN, 'choice.csv'))))
# Full or narrowed by the replay: the count of skipped tests (ran_full) also
# reads a full run as narrowed when many known failures were left out.
full = [float(r['duration_secs']) for r in choice if r['replay_full'] == '1']
narrow = [float(r['duration_secs']) for r in choice if r['replay_full'] == '0']
reasons = {}
for r in choice:
    if r['replay_full'] == '1':
        for k in (r['reason_kinds'] or 'unknown').split('+'):
            reasons[k] = reasons.get(k, 0) + 1
gs['after_full'] = {'n': len(full), 'median': med(full), 'p90': pct(full, .9)}
gs['after_narrowed'] = {'n': len(narrow), 'median': med(narrow), 'p90': pct(narrow, .9)}
gs['after_full_reasons'] = reasons
gs['replay_agrees'] = sum(1 for r in choice if r['ran_full'] != '' and r['ran_full'] == r['replay_full'])
gs['replay_compared'] = sum(1 for r in choice if r['ran_full'] != '')
# verify (land_phases) of the landings whose integrated attempt narrowed the IT
narrowed_runs = {r['run_id'] for r in choice if r['replay_full'] == '0'}
nv = [r[5] for r in rows if r[0] == 'after' and r[1] in narrowed_runs]
gs['after_narrowed_landings_verify'] = {'n': len(nv), 'median': med(nv), 'p90': pct(nv, .9), 'values': sorted(nv)}
# per day: nextest's test count and test stage of the runs whose Summary is in
# output_tail (llvm-cov's tail ends in the coverage report when it passes, so
# before T only the failed runs have one); after T only the full runs.
daily = {}
for r in gate_rows:
    if r[9] == '': continue
    if r[0] == 'after' and r[1] not in {int(c['event_id']) for c in choice if c['replay_full'] == '1'}: continue
    daily.setdefault((r[0], r[2][:10]), []).append((int(r[9]), float(r[8]), r[6]))
gs['daily'] = {f'{w} {d}': {'n': len(v), 'tests_median': med([a[0] for a in v]), 'nextest_secs_median': med([a[1] for a in v]),
                            'failed': sum(1 for a in v if a[2] != 0)} for (w, d), v in sorted(daily.items())}
summary['gate'] = gs

# (c): landings per hour, and per hour in the busy hours of the landing slot.
# An attempt holds the slot from integration_started until the same run's
# run_integrated / integration_deferred / integration_error / integration_held /
# run_recovered, cut at the next integration_started of any run.
ends = {'run_integrated', 'integration_deferred', 'integration_error', 'integration_held', 'run_recovered'}
starts = [(ts(x['created_at']), x['run_id']) for x in ev if x['kind'] == 'integration_started']
by_run = {}
for x in ev:
    if x['kind'] in ends and x.get('run_id'):
        by_run.setdefault(x['run_id'], []).append(ts(x['created_at']))
intervals = []
for i, (s, run) in enumerate(starts):
    nxt = starts[i + 1][0] if i + 1 < len(starts) else ts(T1)
    e_ = [t for t in by_run.get(run, []) if t >= s]
    end = min(min(e_) if e_ else nxt, nxt)
    intervals.append((s, end))
landed = [ts(x['created_at']) for x in ev if x['kind'] == 'run_integrated']
hours, c_sum = [], {}
for w, a in (('before', ts(T0)), ('after', ts(T))):
    for h in range(168):
        hs, he = a + D.timedelta(hours=h), a + D.timedelta(hours=h + 1)
        busy = sum(max(0.0, (min(e_, he) - max(s, hs)).total_seconds()) for s, e_ in intervals)
        n = sum(1 for t in landed if hs <= t < he)
        hours.append([w, iso(hs), round(busy), round(busy / 3600, 3), n])
write('hours.csv', ['window', 'hour_start', 'slot_busy_secs', 'slot_utilization', 'landings'], hours)
for w in ('before', 'after'):
    hw = [h for h in hours if h[0] == w]
    busy = [h for h in hw if h[3] >= 0.8]
    active = [h for h in hw if h[4] > 0 or h[2] > 0]
    c_sum[w] = {'landings': sum(h[4] for h in hw), 'per_hour': round(sum(h[4] for h in hw) / HOURS, 2),
                'busy_hours': len(busy), 'busy_per_hour': round(sum(h[4] for h in busy) / len(busy), 2) if busy else None,
                'active_hours': len(active), 'active_per_hour': round(sum(h[4] for h in active) / len(active), 2) if active else None,
                'slot_busy_secs': sum(h[2] for h in hw), 'max_landings_in_an_hour': max(h[4] for h in hw)}
summary['c'] = c_sum

# (d): red time of main's CI. A red interval runs from the createdAt of the
# first failed run after a green one to the createdAt of the next green run
# (the CI watch's red_secs), in the order of createdAt; cancelled, skipped and
# unfinished runs are passed over. Cut to each window.
ci = {}
for f in glob.glob(os.path.join(IN, 'ci_2*.json')):
    for r in json.load(open(f)):
        ci[r['databaseId']] = r
ci = sorted(ci.values(), key=lambda r: r['createdAt'])
write('ci_main_runs.csv', ['run_id', 'created_at', 'conclusion', 'head_sha', 'url'],
      [[r['databaseId'], r['createdAt'], r['conclusion'], r['headSha'], r['url']] for r in ci])
red, start = [], None
for r in ci:
    if r['conclusion'] in ('failure', 'timed_out', 'startup_failure'):
        if start is None: start = r
    elif r['conclusion'] == 'success':
        if start is not None:
            red.append((ts(start['createdAt']), ts(r['createdAt']), start['databaseId'], r['databaseId'])); start = None
if start is not None:
    red.append((ts(start['createdAt']), None, start['databaseId'], None))
rr, d_sum = [], {}
for w, a, b in (('before', ts(T0), ts(T)), ('after', ts(T), ts(T1))):
    tot = 0.0; n = 0
    for s, e_, sid, gid in red:
        e2 = e_ or b
        cut = max(0.0, (min(e2, b) - max(s, a)).total_seconds())
        if cut > 0:
            tot += cut; n += 1
            rr.append(['gh', w, iso(s), iso(e_) if e_ else '', sid, gid or '', round(cut)])
    d_sum[w] = {'gh_red_secs': round(tot), 'gh_red_hours': round(tot / 3600, 1), 'gh_red_share': round(tot / (HOURS * 3600), 3), 'gh_red_intervals': n}
# the CI watch's events (after the watch was turned on, 2026-10-07T09:12Z)
ev_red = []
open_red = None
for x in ev:
    if x['kind'] == 'ci_turned_green':
        p = x['payload']; g = ts(x['created_at']); s = g - D.timedelta(seconds=p['red_secs'])
        ev_red.append((s, g, p['red_since']['run_id'], p['run_id'], p['red_secs']))
for w, a, b in (('before', ts(T0), ts(T)), ('after', ts(T), ts(T1))):
    tot = sum(max(0.0, (min(g, b) - max(s, a)).total_seconds()) for s, g, *_ in ev_red)
    reds = sum(1 for x in ev if x['kind'] == 'ci_turned_red' and window(x['created_at']) == w)
    for s, g, sid, gid, secs in ev_red:
        cut = max(0.0, (min(g, b) - max(s, a)).total_seconds())
        if cut > 0: rr.append(['events', w, iso(s), iso(g), sid, gid, round(cut)])
    d_sum[w].update({'events_red_secs': round(tot), 'events_turned_red': reds})
write('red_intervals.csv', ['source', 'window', 'red_from', 'green_at', 'red_run', 'green_run', 'secs_in_window'], rr)
summary['d'] = d_sum

# (e): fix tasks. After: the ci_failure findings the CI watch recorded and
# whether a proposal (the route to a task) came of them; before: the
# ci-failure issues of ci-failure.yml opened in the window (reference).
fids = {}
for x in ev:
    p = x['payload']
    if x['kind'] == 'finding_recorded' and p.get('kind') == 'ci_failure' and p.get('propose'):
        fids[p['finding_id']] = window(x['created_at'])
prop = {f: 0 for f in fids}
for x in ev:
    p = x['payload']
    if x['kind'] == 'finding_status_changed' and p.get('finding_id') in fids and p.get('to') == 'proposed':
        prop[p['finding_id']] += 1
    if x['kind'] == 'finding_planner_opened' and p.get('finding_id') in fids:
        prop[p['finding_id']] += 1
issues = json.load(open(os.path.join(IN, 'ci_issues.json')))
write('ci_failure_findings.csv', ['finding_id', 'window', 'proposed_or_planner_opened'], [[f, w, prop[f]] for f, w in sorted(fids.items())])
write('ci_failure_issues.csv', ['number', 'window', 'created_at', 'closed_at'],
      [[i['number'], window(i['createdAt'].replace('Z', '.000Z')), i['createdAt'], i['closedAt']] for i in issues if window(i['createdAt'].replace('Z', '.000Z'))])
summary['e'] = {w: {'ci_failure_findings': sum(1 for f, x in fids.items() if x == w),
                    'with_a_proposal_or_planner': sum(1 for f, x in fids.items() if x == w and prop[f]),
                    'ci_failure_issues': sum(1 for i in issues if window(i['createdAt'].replace('Z', '.000Z')) == w)} for w in ('before', 'after')}

# (f): the fixed binary put back (update_restored), and the update gate's failures.
summary['f'] = {w: {'update_restored': sum(1 for x in ev if x['kind'] == 'update_restored' and window(x['created_at']) == w),
                    'update_failed': sum(1 for x in ev if x['kind'] == 'update_failed' and window(x['created_at']) == w),
                    'update_installed': sum(1 for x in ev if x['kind'] == 'update_installed' and window(x['created_at']) == w)} for w in ('before', 'after')}

# (g) resumes after a failed landing verification (integrate's, or the landing
# recheck's), classified by the rules of the document, in this order:
#   environment: the failure's text says the sandbox or the host refused
#     (Operation not permitted, No space left, sandbox);
#   otherwise the cause the worker's next turn_finished message gives
#   (RESUME_LABELS, by the resume_started event id): main_broken (main alone
#   fails the command), existing_flaky (a race or clock in a test the diff did
#   not touch), interaction (another landing changed what the run's diff uses:
#   a signature, a helper, a migration number, a size budget both grew), own
#   (the run's diff, or no message to tell). same_main_hint marks another
#   run's failure of the same command on the same main commit.
fails = []  # (time, run, main, command, tests, text, source)
for x in ev:
    p = x['payload']
    if x['kind'] == 'integration_deferred' and p.get('code') == 'verification_failed':
        fails.append((x['created_at'], x['run_id'], p.get('main'), p.get('command'), set(p.get('failed_tests') or []),
                      json.dumps(p.get('failure') or {}) + (p.get('reason') or ''), 'integrate'))
    if x['kind'] == 'landing_recheck_failed' and p.get('code') == 'verification_failed' and p.get('action') == 'resumed':
        fails.append((x['created_at'], x['run_id'], p.get('main'), p.get('command'), set(p.get('failed_tests') or []),
                      (p.get('output_tail') or '') + (p.get('reason') or ''), 'recheck'))
RESUME_LABELS = {
    62164: 'interaction', 62525: 'existing_flaky', 74774: 'existing_flaky', 79344: 'existing_flaky',
    94322: 'interaction', 111015: 'main_broken', 117069: 'interaction', 117678: 'main_broken',
    117804: 'main_broken', 118236: 'main_broken', 119996: 'interaction', 129156: 'interaction',
    132639: 'existing_flaky', 134559: 'main_broken', 134964: 'main_broken', 139307: 'existing_flaky',
    142021: 'main_broken', 144074: 'interaction', 154292: 'interaction', 163093: 'interaction',
    178109: 'interaction', 188192: 'interaction', 188400: 'interaction'}
gr = []
for x in ev:
    if x['kind'] != 'resume_started': continue
    w = window(x['created_at'])
    if not w: continue
    r = x['payload'].get('reason') or ''
    if not (r.startswith('verification command') or ('landing recheck found that "' in r and 'exits with' in r)):
        continue
    prev = [f for f in fails if f[1] == x['run_id'] and f[0] <= x['created_at']]
    if not prev:
        gr.append([w, x['id'], x['created_at'], x['run_id'], x['task_id'], '', '', 'unmatched', '', r[:160]]); continue
    f = prev[-1]
    same = [g for g in fails if g[1] != f[1] and g[2] == f[2] and g[3] == f[3] and (not f[4] or (f[4] & g[4]))]
    if re.search(r'Operation not permitted|No space left|sandbox', f[5]):
        cls = 'environment'
    else:
        cls = RESUME_LABELS.get(x['id'], 'own')
    gr.append([w, x['id'], x['created_at'], x['run_id'], x['task_id'], f[6], f[3], cls, int(bool(same)), r[:160]])
write('verification_resumes.csv', ['window', 'event_id', 'at', 'run_id', 'task_id', 'source', 'command', 'class', 'same_main_hint', 'reason'], gr)

# (g) worker_question asks: the ask's text is not in the events, so the
# worker's turn_finished message after the ask (it states the question and why)
# stands for it; each ask's label is in LABELS with the rule of the document.
LABELS = {
    263: 'environment', 264: 'environment', 386: 'own', 393: 'environment', 394: 'own', 395: 'environment',
    437: 'own', 519: 'main_broken', 556: 'main_broken', 557: 'main_broken', 559: 'main_broken', 561: 'main_broken',
    633: 'main_broken', 688: 'environment', 737: 'own', 752: 'main_broken', 756: 'own', 764: 'own',
    800: 'waits_other_task', 801: 'waits_other_task', 802: 'waits_other_task', 803: 'waits_other_task',
    804: 'own', 805: 'waits_other_task', 809: 'waits_other_task', 811: 'waits_other_task', 812: 'waits_other_task',
    813: 'waits_other_task', 814: 'waits_other_task', 815: 'waits_other_task', 816: 'waits_other_task',
    818: 'waits_other_task', 833: 'own'}
answered = {x['payload']['ask_id']: x['created_at'] for x in ev if x['kind'] == 'ask_answered'}
wq = []
for x in ev:
    p = x['payload']
    if x['kind'] == 'ask_opened' and p.get('kind') == 'worker_question':
        w = window(x['created_at'])
        if not w: continue
        msg = next((y['payload'].get('message') or '' for y in ev if y['kind'] == 'turn_finished' and y.get('run_id') == x['run_id'] and y['id'] > x['id']), '')
        a = answered.get(p['ask_id'])
        wait = round((ts(a) - ts(x['created_at'])).total_seconds()) if a else ''
        wq.append([w, p['ask_id'], x['created_at'], x['task_id'], x['run_id'], LABELS.get(p['ask_id'], 'unlabeled'), wait, msg[:300].replace('\n', ' ')])
write('worker_questions.csv', ['window', 'ask_id', 'at', 'task_id', 'run_id', 'class', 'secs_to_answer', 'turn_message'], wq)
g = {}
for w in ('before', 'after'):
    g[w] = {'resumes': {}, 'asks': {}, 'asks_wait_secs': {}}
    for r in gr:
        if r[0] == w: g[w]['resumes'][r[7]] = g[w]['resumes'].get(r[7], 0) + 1
    for r in wq:
        if r[0] == w:
            g[w]['asks'][r[5]] = g[w]['asks'].get(r[5], 0) + 1
            if r[6] != '': g[w]['asks_wait_secs'][r[5]] = g[w]['asks_wait_secs'].get(r[5], 0) + r[6]
summary['g'] = g
# the kpi --compare fields the document cites
kpi = json.load(open(os.path.join(IN, 'kpi_compare.json')))['compare']
keep = ['land_phase.verify', 'land_phase.landing_queue', 'landings', 'slot_usage', 'revise_rate', 'first_pass_rate',
        'phase.work', 'cpu_per_landing', 'load_per_core', 'candidates', 'landing_utilization', 'verification_failed_rate']
json.dump({'split': kpi['split'], 'strata': {k: kpi['strata'][k]['all'] for k in keep},
           'confounders': [c for c in kpi['confounders'] if c['kind'] != 'supervisor_started'],
           'supervisor_started': {pos: sum(1 for c in kpi['confounders'] if c['kind'] == 'supervisor_started' and c['position'] == pos) for pos in ('before', 'after')}},
          open(os.path.join(OUT, 'kpi_compare_used.json'), 'w'), ensure_ascii=False, indent=1)
json.dump(summary, sys.stdout, ensure_ascii=False, indent=1)
