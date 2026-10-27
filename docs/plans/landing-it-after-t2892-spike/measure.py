"""(docs/plans/landing-it-after-t2892-spike.md) The landing IT selection before
and after ADR-t2892-1, from the records collect.sh gathers.

Usage: python3 measure.py DATA REPO OUT

DATA holds events_before.json, events_after.json, landings.json and ci/ (from
collect.sh); REPO is the repository (git rev-list for the CI ranges); OUT gets
  attempts.csv      every sh scripts/landing-it.sh of both windows: whether it
                    ran every IT, the reasons' kinds, the command's seconds, the
                    build and nextest seconds, the files left to CI
  new-failures.csv  the tests newly red on main's CI in the after window, and
                    for each whether a landing of its range ran it
  summary.json      the values of the document's tables

The landing-it log (verification_command.log_path) says what was chosen:
"landing-it: every IT runs:" with a reason per line, or "landing-it: narrowed
IT" with "its IT left to CI" lines; the PASS/FAIL lines name the tests run.
A newly red test (landing-it-selection.md, 値の式): per job, over the runs with a
test result in time order, a test failing in a run and not in the job's
previous run with a result (FLKY-FL rows do not count). Its range is the main
first-parent commits after the previous run's headSha up to the run's."""
import csv, json, math, os, re, subprocess, sys
from collections import Counter, defaultdict

data, repo, out = sys.argv[1:4]
T0, T, T1 = '2026-10-13T02:12:03.000Z', '2026-10-20T02:12:03.000Z', '2026-10-27T02:12:03.000Z'
CMD = 'sh scripts/landing-it.sh'
FIN = re.compile(r'^\s*Finished `test` profile .* in (?:(\d+)m )?([\d.]+)s')
SUM = re.compile(r'^\s*Summary \[\s*([\d.]+)s\] (\d+) tests? run')
TST = re.compile(r'^\s*(?:PASS|FAIL|FLAKY|FLKY-FL \S+|SLOW|TIMEOUT|SIGSEGV|SIGABRT|SIGKILL|LEAK|TRY \d+ \S+)[^\[]*\[\s*([\d.]+)s\]\s*\(\s*\d+/\d+\)\s+(\S+)\s+(\S+)')
MASS = 50


def med(xs):
    xs = sorted(xs)
    n = len(xs)
    if not n:
        return None
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def p90(xs):
    xs = sorted(xs)
    return xs[math.ceil(0.9 * len(xs)) - 1] if xs else None


def reason_kind(text):
    if text.startswith('a common file'):
        return 'common'
    if 'exceeds' in text:
        return 'threshold'
    if text.startswith('a source file not in the table'):
        return 'not_in_tree'
    if text.startswith('the table is not available') or text.startswith('the table cannot be read'):
        return 'no_table'
    if text.startswith('the table is older'):
        return 'stale'
    return 'other'


def read_log(path):
    """What the landing-it log says: full, reasons, left to CI, excluded, the
    tests run, build and nextest seconds; None when the log cannot be read."""
    if not path or not os.path.exists(path):
        return None
    r = dict(full=None, reasons=[], left=[], excluded=set(), ran=set(), build=0.0, nextest=None, tests=None)
    state = None
    with open(path, errors='replace') as f:
        for line in f:
            s = line.rstrip('\n')
            if s.startswith('landing-it: every IT runs:'):
                r['full'] = True; state = 'reasons'; continue
            if s.startswith('landing-it: narrowed IT'):
                r['full'] = False; state = None; continue
            if s.startswith('landing-it: its IT left to CI: '):
                r['left'].append(s[len('landing-it: its IT left to CI: '):]); continue
            if s.startswith('landing-it: left out, fails on main already: '):
                name = s[len('landing-it: left out, fails on main already: '):]
                r['excluded'].add(name.replace(' ', '::', 1)); continue
            if state == 'reasons' and s.startswith('landing-it:   '):
                r['reasons'].append(s[len('landing-it:   '):]); continue
            if s.startswith('landing-it:'):
                state = None; continue
            m = FIN.match(s)
            if m:
                r['build'] += int(m.group(1) or 0) * 60 + float(m.group(2)); continue
            m = SUM.match(s)
            if m:
                r['nextest'], r['tests'] = float(m.group(1)), int(m.group(2)); continue
            m = TST.match(s)
            if m:
                r['ran'].add(m.group(2) + '::' + m.group(3))
    return r


def load(name):
    return json.load(open(os.path.join(data, name)))


# 1. the landing-it attempts of both windows
events = load('events_before.json') + load('events_after.json')
attempts = []
logs = {}
it_binaries = set()
for x in sorted(events, key=lambda e: e['created_at']):
    p = x['payload']
    if x['kind'] != 'verification_command' or p.get('command') != CMD:
        continue
    win = 'before' if T0 <= x['created_at'] < T else 'after' if T <= x['created_at'] < T1 else None
    if win is None:
        continue
    r = read_log(p.get('log_path'))
    logs[x['id']] = r
    if r:
        it_binaries.update(k.split('::', 2)[0] + '::' + k.split('::', 2)[1] for k in r['ran']
                           if k.count('::') >= 2 and '::bin/' not in k and not k.startswith('dagq::bin'))
    kinds = sorted(set(reason_kind(t) for t in r['reasons'])) if r else []
    attempts.append(dict(
        window=win, event_id=x['id'], at=x['created_at'], run_id=x['run_id'], task=x['task_id'],
        attempt=p.get('attempt'), exit_code=p.get('exit_code'), duration_secs=p.get('duration_secs'),
        log='missing' if r is None else ('unknown' if r['full'] is None else 'read'),
        full='' if r is None or r['full'] is None else int(r['full']),
        reasons=' '.join(kinds),
        build_secs='' if r is None else round(r['build'], 1),
        nextest_secs='' if r is None or r['nextest'] is None else r['nextest'],
        tests_run='' if r is None or r['tests'] is None else r['tests'],
        left_broad=0 if r is None else sum('the table maps it' in s for s in r['left']),
        left_not_in_tree=0 if r is None else sum("not in the table's tree" in s for s in r['left'])))
# every verification command of the same integration attempt, for the whole verify
verify_total = defaultdict(float)
for x in events:
    p = x['payload']
    if x['kind'] == 'verification_command' and p.get('phase') == 'integration':
        verify_total[(x['run_id'], p.get('attempt'))] += p.get('duration_secs') or 0.0
for a in attempts:
    a['verify_total_secs'] = round(verify_total[(a['run_id'], a['attempt'])], 1)

with open(os.path.join(out, 'attempts.csv'), 'w', newline='') as f:
    w = csv.DictWriter(f, fieldnames=list(attempts[0]))
    w.writeheader(); w.writerows(attempts)

summary = {'windows': {'before': [T0, T], 'after': [T, T1]}}
for win in ('before', 'after'):
    rows = [a for a in attempts if a['window'] == win]
    read = [a for a in rows if a['log'] == 'read']
    full = [a for a in read if a['full'] == 1]
    narrowed = [a for a in read if a['full'] == 0]
    passed = [a for a in rows if a['exit_code'] == 0]
    s = dict(attempts=len(rows), log_missing=sum(a['log'] == 'missing' for a in rows),
             log_unknown=sum(a['log'] == 'unknown' for a in rows), read=len(read), full=len(full),
             full_share=round(len(full) / len(read), 3) if read else None,
             reasons=dict(Counter(k for a in full for k in a['reasons'].split())),
             reasons_only=dict(Counter(a['reasons'] for a in full)),
             passed=len(passed))
    for name, sel in (('all', rows), ('passed', passed), ('full', full), ('narrowed', narrowed)):
        d = [a['duration_secs'] for a in sel if a['duration_secs'] is not None]
        b = [a['build_secs'] for a in sel if a['build_secs'] != '']
        n = [a['nextest_secs'] for a in sel if a['nextest_secs'] != '']
        v = [a['verify_total_secs'] for a in sel]
        s[name] = dict(n=len(sel), duration_median=med(d), duration_p90=p90(d), build_median=med(b), build_p90=p90(b),
                       nextest_median=med(n), nextest_p90=p90(n), verify_total_median=med(v), verify_total_p90=p90(v))
    s['narrowed_left_broad_runs'] = sum(a['left_broad'] > 0 for a in narrowed)
    s['narrowed_left_not_in_tree_runs'] = sum(a['left_not_in_tree'] > 0 for a in narrowed)
    summary[win] = s

# 2. the newly red tests on main's CI in the after window
runs = {r['databaseId']: r for r in json.load(open(os.path.join(data, 'ci', 'ci-runs.json')))}
step = {}
for line in open(os.path.join(data, 'ci', 'ci-jobs.tsv')):
    rid, job, concl = line.rstrip('\n').split('\t')
    step[(int(rid), 'linux' if 'linux' in job.lower() else 'macos')] = concl
fails = defaultdict(set)
for line in open(os.path.join(data, 'ci', 'ci-failed-tests.tsv')):
    rid, job, test, kind = line.rstrip('\n').split('\t')
    if kind == 'fail':
        fails[(int(rid), job)].add(test)
landings = sorted(load('landings.json'), key=lambda e: e['created_at'])
by_commit = {e['payload']['commit']: e for e in landings}
# the last landing-it of each run before it landed
last_it = {}
for x in sorted(events, key=lambda e: e['created_at']):
    if x['kind'] == 'verification_command' and x['payload'].get('command') == CMD:
        last_it.setdefault(x['run_id'], []).append(x)


def landing_it_of(landing):
    xs = [x for x in last_it.get(landing['run_id'], []) if x['created_at'] <= landing['created_at']]
    return xs[-1] if xs else None


def rev_range(a, b):
    res = subprocess.run(['git', '-C', repo, 'rev-list', '--first-parent', '%s..%s' % (a, b)],
                         capture_output=True, text=True)
    return res.stdout.split() if res.returncode == 0 else None


def is_it(test):
    return any(test.startswith(b + '::') for b in it_binaries)


new = []
for job in ('macos', 'linux'):
    prev = None
    for r in sorted(runs.values(), key=lambda r: r['createdAt']):
        c = step.get((r['databaseId'], job))
        has = c == 'success' or (c == 'failure' and fails.get((r['databaseId'], job)))
        if not has:
            continue
        cur = fails.get((r['databaseId'], job), set())
        if prev is not None and T <= r['createdAt'] < T1:
            newly = sorted(cur - fails.get((prev['databaseId'], job), set()))
            commits = rev_range(prev['headSha'], r['headSha']) if newly else []
            for t in newly:
                new.append(dict(job=job, test=t, prev_run=prev['databaseId'], run=r['databaseId'], at=r['createdAt'],
                                newly_in_run=len(newly), commits=commits))
        prev = r
# the next result of the same job, to tell a test that goes back to green
results = defaultdict(list)
for (rid, job), c in step.items():
    if c == 'success' or (c == 'failure' and fails.get((rid, job))):
        results[job].append(runs[rid])
for job in results:
    results[job].sort(key=lambda r: r['createdAt'])

rows = []
for n in new:
    seq = results[n['job']]
    i = [r['databaseId'] for r in seq].index(n['run'])
    nxt = seq[i + 1] if i + 1 < len(seq) else None
    still = '' if nxt is None else int(n['test'] in fails.get((nxt['databaseId'], n['job']), set()))
    lands, human, per = [], [], []
    for c in n['commits'] or []:
        l = by_commit.get(c)
        if l is None:
            human.append(c[:8]); continue
        lands.append(l)
        v = landing_it_of(l)
        if v is None:
            per.append('%s:no_it' % l['task_id']); continue
        lg = logs.get(v['id']) or read_log(v['payload'].get('log_path'))
        if lg is None:
            per.append('%s:log_missing' % l['task_id'])
        elif n['test'] in lg['ran']:
            per.append('%s:ran' % l['task_id'])
        elif n['test'] in lg['excluded']:
            per.append('%s:excluded' % l['task_id'])
        elif lg['full']:
            per.append('%s:full_not_run' % l['task_id'])
        else:
            kinds = []
            if any('the table maps it' in s for s in lg['left']):
                kinds.append('broad')
            if any("not in the table's tree" in s for s in lg['left']):
                kinds.append('not_in_tree')
            per.append('%s:narrowed_not_run(%s)' % (l['task_id'], '+'.join(kinds) or 'not_reached'))
    status = [p.split(':', 1)[1] for p in per]
    if n['commits'] is None:
        verdict = 'unreadable_range'
    elif n['newly_in_run'] >= MASS:
        verdict = 'mass'
    elif not is_it(n['test']):
        verdict = 'not_it'
    elif any(s == 'ran' for s in status):
        verdict = 'ran'
    elif any(s == 'log_missing' for s in status):
        verdict = 'unreadable_log'
    elif human and not any(s.startswith('narrowed') or s == 'excluded' for s in status):
        verdict = 'human_commit'
    elif status and all(s == 'no_it' for s in status):
        verdict = 'no_it_landings'
    elif any(s.startswith('narrowed_not_run') for s in status):
        verdict = 'miss'
    elif any(s == 'excluded' for s in status):
        verdict = 'excluded_known_failure'
    else:
        verdict = 'other'
    rows.append(dict(job=n['job'], test=n['test'], prev_run=n['prev_run'], run=n['run'], at=n['at'],
                     newly_in_run=n['newly_in_run'], range_commits=len(n['commits'] or []),
                     range_landings=' '.join(str(l['task_id']) for l in lands), human_commits=' '.join(human),
                     per_landing=' '.join(per), verdict=verdict, red_at_next=still))
with open(os.path.join(out, 'new-failures.csv'), 'w', newline='') as f:
    fields = ['job', 'test', 'prev_run', 'run', 'at', 'newly_in_run', 'range_commits', 'range_landings',
              'human_commits', 'per_landing', 'verdict', 'red_at_next']
    w = csv.DictWriter(f, fieldnames=fields)
    w.writeheader(); w.writerows(rows)
summary['ci'] = dict(runs=len([r for r in runs.values() if T <= r['createdAt'] < T1]),
                     runs_with_result=dict((j, len([r for r in results[j] if T <= r['createdAt'] < T1])) for j in results),
                     newly_red=len(rows), verdicts=dict(Counter(r['verdict'] for r in rows)),
                     tests=dict(Counter(r['test'] for r in rows)))
json.dump(summary, open(os.path.join(out, 'summary.json'), 'w'), indent=1, ensure_ascii=False)
print(json.dumps(summary, indent=1, ensure_ascii=False))
