"""Replays alternatives of the landing IT selection (docs/plans/landing-it-alternatives.md)
on the after window of the landing verification switch: each `sh scripts/landing-it.sh`
of 2026-10-07T10:27:40.972Z..2026-10-14T10:27:40.972Z, with the attempt's diff and the
nightly table newest before it (as docs/plans/landing-verify-switch/replay_choice.py does),
and the landings' misses counted as docs/plans/landing-it-selection.md counts them.

Usage: python3 replay.py DATA REPO OUT
  DATA holds what collect.sh read: events_after.json (integration_started,
  integration_rebased and verification_command of the after window),
  events_before.json (verification_command of the before window),
  landings.json (run_integrated from 2026-10-06T00:00:00Z), itmap_runs.json,
  itmap/t<run id>/it-coverage-map.json and ci/ (ci-runs.json, ci-jobs.tsv,
  ci-failed-tests.tsv, as docs/plans/landing-it-selection/collect.sh steps 3 and 6
  write them).
  OUT gets attempts.csv, new-failures.csv and summary.json.
It reads records only (the queue's events, git, CI's history, the tables): one pass."""
import csv, datetime as D, importlib.util, json, math, os, re, subprocess, sys
from collections import Counter, defaultdict

data, repo, out = sys.argv[1:4]
spec = importlib.util.spec_from_file_location('landing_it', f'{repo}/scripts/landing-it.py')
li = importlib.util.module_from_spec(spec); spec.loader.exec_module(li)
T, T1 = '2026-10-07T10:27:40.972Z', '2026-10-14T10:27:40.972Z'
THREADS = 6
MASS = 50  # landing-it-selection.md: 50 or more newly red in one run is one cause apart
VARIANTS = {
    'i_current': dict(added='full', broad=None),
    'ii_parent': dict(added='parent', broad=None),
    'ii_dir': dict(added='dir', broad=None),
    'iii_ci': dict(added='none', broad=None),
    'iv_broad50': dict(added='full', broad=0.5),
    'iv_broad25': dict(added='full', broad=0.25),
    'ii_parent+iv_broad50': dict(added='parent', broad=0.5),
    'iii_ci+iv_broad50': dict(added='none', broad=0.5),
    'iii_ci+iv_broad25': dict(added='none', broad=0.25),
    'ii_parent+iv_broad25': dict(added='parent', broad=0.25),
    'iii_ci+iv_broad10': dict(added='none', broad=0.1),
}


def git(*a):
    return subprocess.run(['git', '-C', repo, *a], capture_output=True, text=True, check=True).stdout


def ts(s):
    return D.datetime.fromisoformat(s.replace('Z', '+00:00'))


def median(v):
    v = sorted(v); n = len(v)
    return None if not n else (v[n // 2] if n % 2 else (v[n // 2 - 1] + v[n // 2]) / 2)


def p90(v):
    v = sorted(v)
    return v[math.ceil(0.9 * len(v)) - 1] if v else None


itmap = sorted((r for r in json.load(open(f'{data}/itmap_runs.json')) if r['conclusion'] == 'success'),
               key=lambda r: r['updatedAt'])
tables = {}


def table_at(at):
    r = [r for r in itmap if ts(r['updatedAt']) < at][-1]
    i = r['databaseId']
    if i not in tables:
        t = json.load(open(f'{data}/itmap/t{i}/it-coverage-map.json'))
        tree = git('ls-tree', '-r', '--name-only', t['commit']).split('\n')
        n = sum(1 for k in t['tests'])
        prefix = defaultdict(set)
        for k, v in t['tests'].items():
            # dagq::it::<module>::... -> the module's tests, for a changed test file
            parts = v['name'].split('::')
            prefix[(v['binary_id'], parts[0] if len(parts) > 1 else '')].add(k)
        tables[i] = dict(id=i, t=t, tree=tree, tree_set=set(tree), n=n, prefix=prefix,
                         full_wall=li.wall([x.get('duration_secs') or 0.0 for x in t['tests'].values()], THREADS))
    return tables[i]


def test_file_tests(tab, path):
    """The table's tests a changed test file defines (scripts/landing-it.py test_file_filter)."""
    m = re.fullmatch(r'tests/it/(.+)\.rs', path)
    if m:
        parts = m.group(1).split('/')
        if parts[-1] == 'mod':
            parts = parts[:-1]
        if not parts or parts == ['main']:
            return set()
        return tab['prefix'].get(('dagq::it', parts[0]), set())
    if path == 'tests/plugin.rs':
        return {k for k, v in tab['t']['tests'].items() if v['binary_id'] == 'dagq::plugin'}
    m = re.fullmatch(r'crates/([^/]+)/tests/([^/]+)\.rs', path)
    if m:
        b = f'{m.group(1)}::{m.group(2)}'
        return {k for k, v in tab['t']['tests'].items() if v['binary_id'] == b}
    return set()


def crate_root(path):
    m = re.match(r'(crates/[^/]+/src|src)/', path)
    return m.group(1)


def module_dir(path):
    """src/a/b.rs and src/a/b/mod.rs are the module src/a/b."""
    p = path[:-3]
    return p[:-4] if p.endswith('/mod') else p


def by_parent(tab, path):
    """The table's tests of the module that declares path's module (its parent's
    file, src/a.rs or src/a/mod.rs for src/a/b.rs), going up while the parent has
    no entry (a mod.rs of mod lines only); None at the crate root with no entry."""
    files = tab['t']['files']
    root = crate_root(path)
    mod = module_dir(path)
    while True:
        parent = os.path.dirname(mod)
        if len(parent) < len(root):
            return None
        if parent == root:
            cands = [f'{root}/lib.rs', f'{root}/main.rs']
        else:
            cands = [f'{parent}.rs', f'{parent}/mod.rs']
        hit = [c for c in cands if c in files]
        if hit:
            return set().union(*(set(files[c]) for c in hit))
        if parent == root:
            return None
        mod = parent


def by_dir(tab, path):
    """The table's tests of the files in path's directory (and the module file
    of the directory), going up while none has an entry; None past the crate root."""
    files = tab['t']['files']
    root = crate_root(path)
    d = os.path.dirname(module_dir(path)) if path.endswith('/mod.rs') else os.path.dirname(path)
    while len(d) >= len(root):
        hit = [f for f in files if os.path.dirname(f) == d or f in (f'{d}.rs', f'{d}/mod.rs')]
        if hit:
            return set().union(*(set(files[f]) for f in hit))
        d = os.path.dirname(d)
    return None


def choose(diff, tab, at, variant):
    """(full, reason kinds, selected table tests, estimate, added files) under a variant."""
    t, files, tests = tab['t'], tab['t']['files'], tab['t']['tests']
    kinds = set()
    if at - li.parse_time(t['generated_at']) > li.MAX_TABLE_AGE:
        kinds.add('table_old')
    broad = None
    if variant['broad'] is not None:
        broad = variant['broad'] * tab['n']
    sel, added = set(), []
    for path in diff:
        if any(li.glob_match(path, p) for p in li.COMMON_FILES):
            kinds.add('common_file'); continue
        if li.is_source(path) and path not in files and path not in tab['tree_set'] and not li.is_unit_test_module(path):
            added.append(path)
            how = variant['added']
            if how == 'full':
                kinds.add('source_not_in_tree')
            elif how in ('parent', 'dir'):
                got = (by_parent if how == 'parent' else by_dir)(tab, path)
                if got is None:
                    kinds.add('source_not_in_tree')
                elif broad is None or len(got) <= broad:
                    sel |= got
            continue
        entry = files.get(path, [])
        if broad is None or len(entry) <= broad:
            sel.update(k for k in entry if k in tests)
        sel |= test_file_tests(tab, path)
    reach = set()
    for path in diff:
        reach.update(k for k in files.get(path, []) if k in tests)
    left = len(reach - sel)
    est = li.wall([tests[k].get('duration_secs') or 0.0 for k in sel], THREADS)
    if not kinds and est > li.FULL_SHARE * tab['full_wall']:
        kinds.add('threshold')
    full = bool(kinds)
    return full, sorted(kinds), sel, tab['full_wall'] if full else est, (0 if full else left, len(reach))


# The attempts of the after window.
ev = json.load(open(f'{data}/events_after.json'))
start, rebased, attempts = {}, {}, []
for x in ev:
    k, p, run = x['kind'], x['payload'], x.get('run_id')
    if k == 'integration_started':
        start[run] = p['main']; rebased.pop(run, None)
    elif k == 'integration_rebased':
        rebased[run] = p['head_after']
    elif k == 'verification_command' and p.get('command') == 'sh scripts/landing-it.sh' and T <= x['created_at'] < T1:
        base, head = start.get(run), rebased.get(run)
        diff = [f for f in git('diff', '--name-only', '--no-renames', f'{base}..{head}').split('\n') if f]
        attempts.append(dict(event=x['id'], at=x['created_at'], run=run, diff=diff, duration=p['duration_secs']))
assert len(attempts) == 183, len(attempts)

rows, res = [], defaultdict(list)
for a in attempts:
    tab = table_at(ts(a['at']))
    a['table'] = tab['id']
    for name, v in VARIANTS.items():
        full, kinds, sel, est, (left, reach) = choose(a['diff'], tab, ts(a['at']), v)
        res[name].append(dict(full=full, kinds=kinds, est=est, n=len(sel), left=left, reach=reach))
    rows.append(a)

# The landings and what their verification selected.
verif = defaultdict(list)  # run -> its test commands, in order
for path in (f'{data}/events_before.json', f'{data}/events_after.json'):
    for x in json.load(open(path)):
        if x['kind'] == 'verification_command':
            verif[x['run_id']].append((x['created_at'], x['payload'].get('command') or ''))
landings = []
for x in json.load(open(f'{data}/landings.json'))['events']:
    p = x['payload']
    cmds = [c for at, c in verif.get(x['run_id'], []) if at <= x['created_at']]
    gate = 'none'
    for c in cmds:
        if c == 'sh scripts/landing-it.sh':
            gate = 'landing_it'
        elif c.startswith('cargo llvm-cov') or c.startswith('cargo test --locked') and '--test ' not in c:
            gate = 'all'
    lnd = dict(event=x['id'], at=x['created_at'], task=x['task_id'], commit=p['commit'], gate=gate)
    if gate == 'landing_it':
        diff = [f for f in git('diff', '--name-only', '--no-renames', f"{p['main_before']}..{p['commit']}").split('\n') if f]
        tab = table_at(ts(x['created_at']))
        lnd['tab'] = tab
        lnd['sel'] = {}
        for name, v in VARIANTS.items():
            full, kinds, sel, est, _ = choose(diff, tab, ts(x['created_at']), v)
            lnd['sel'][name] = None if full else sel  # None: every IT
    landings.append(lnd)
by_commit = {l['commit']: l for l in landings}


def selected(l, test, name):
    """Whether the landing's verification ran the test under a variant."""
    if l['gate'] == 'all':
        return True
    if l['gate'] == 'none':
        return False
    s = l['sel'][name]
    if s is None:
        return True
    tab = l['tab']['t']['tests']
    key = test
    if key not in tab:
        return True  # an IT not in the table always runs
    return key in s


# CI: each job's results in order, and the tests newly red (landing-it-selection.md).
runs = sorted((r for r in json.load(open(f'{data}/ci/ci-runs.json')) if r['conclusion'] != 'cancelled'),
              key=lambda r: r['createdAt'])
order = {r['databaseId']: i for i, r in enumerate(runs)}
by_id = {r['databaseId']: r for r in runs}
failed = defaultdict(set)
for line in open(f'{data}/ci/ci-failed-tests.tsv'):
    rid, job, test, kind = line.rstrip('\n').split('\t')
    if kind == 'fail':
        failed[(int(rid), job)].add(test)
results = defaultdict(list)
for line in open(f'{data}/ci/ci-jobs.tsv'):
    rid, jname, step = line.rstrip('\n').split('\t')
    rid, job = int(rid), 'linux' if 'linux' in jname.lower() else 'macos'
    if step == 'success' or (step == 'failure' and failed.get((rid, job))):
        results[job].append(rid)
new_by_run = defaultdict(dict)
for job, rids in results.items():
    rids = sorted(set(rids), key=order.get)
    for i in range(1, len(rids)):
        prev, cur = rids[i - 1], rids[i]
        for t in failed.get((cur, job), set()) - failed.get((prev, job), set()):
            p0 = new_by_run[cur].get(t)
            if p0 is None or order[prev] > order[p0]:
                new_by_run[cur][t] = prev
events = []
for cur in sorted(new_by_run, key=order.get):
    r = by_id[cur]
    if not (T <= r['createdAt'] < T1):
        continue
    new = new_by_run[cur]
    for t, prev in sorted(new.items()):
        rng = git('rev-list', '--first-parent', f"{by_id[prev]['headSha']}..{r['headSha']}").split()
        in_range = [by_commit[c] for c in rng if c in by_commit]
        direct = [c for c in rng if c not in by_commit]
        binary, _, name = t.partition('::')
        # test keys: "<binary>::<name>" in CI rows are "dagq::it::<name>"; the table's keys likewise
        is_it = t.startswith('dagq::it::') or t.startswith('dagq::plugin::') or re.match(r'dagq-[a-z-]+::[a-z_]+::', t)
        e = dict(test=t, prev_run=prev, red_run=cur, red_at=r['createdAt'], new_in_run=len(new),
                 mass=len(new) >= MASS, range_tasks=[l['task'] for l in in_range], direct=len(direct),
                 cls='it' if is_it else 'unit (always run)')
        if is_it:
            in_any_table = any(l.get('tab') and t in l['tab']['t']['tests'] for l in in_range)
            if in_range and all(l['gate'] != 'landing_it' for l in in_range):
                e['cls'] = 'it, no narrowed landing in the range'
            elif not in_any_table and any(l['gate'] == 'landing_it' for l in in_range):
                e['cls'] = 'it not in the table (always run)'
            for name in VARIANTS:
                sel = [l['task'] for l in in_range if selected(l, t, name)]
                e[name] = 'selected' if sel else ('direct_unselected' if direct else 'missed')
        events.append(e)

summary = {'attempts': len(rows), 'variants': {}}
for name in VARIANTS:
    rs = res[name]
    full = [r for r in rs if r['full']]
    narrowed = [r['est'] for r in rs if not r['full']]
    kinds = Counter(k for r in full for k in r['kinds'])
    it = [e for e in events if e['cls'] == 'it']
    summary['variants'][name] = dict(
        full=len(full), full_share=round(len(full) / len(rs), 3),
        reasons=dict(kinds),
        est_median=round(median([r['est'] for r in rs]), 1), est_p90=round(p90([r['est'] for r in rs]), 1),
        narrowed_n=len(narrowed),
        narrowed_est_median=round(median(narrowed), 1) if narrowed else None,
        narrowed_est_p90=round(p90(narrowed), 1) if narrowed else None,
        missed=sum(e[name] == 'missed' for e in it),
        missed_not_mass=sum(e[name] == 'missed' and not e['mass'] for e in it),
        missed_single_landing=sum(e[name] == 'missed' and len(e['range_tasks']) == 1 for e in it),
        missed_runs=len({e['red_run'] for e in it if e[name] == 'missed'}),
        direct_unselected=sum(e[name] == 'direct_unselected' for e in it),
        left_out_attempts=sum(1 for r in rs if r['left']),
        left_out_median=median([r['left'] for r in rs if r['left']]) if any(r['left'] for r in rs) else 0,
        left_out_share_median=round(median([r['left'] / r['reach'] for r in rs if r['left']]), 3) if any(r['left'] for r in rs) else 0,
    )
summary['new_failures'] = dict(Counter(e['cls'] for e in events))
summary['new_failures_mass_runs'] = sorted({e['red_run'] for e in events if e['mass']})
summary['new_failures_it_mass'] = sum(1 for e in events if e['cls'] == 'it' and e['mass'])
summary['landings_by_gate'] = dict(Counter(l['gate'] for l in landings if T <= l['at'] < T1))
summary['tables'] = sorted(tables)
summary['broad_files'] = {str(i): {str(b): sum(1 for f, v in tab['t']['files'].items() if li.is_source(f) and len(v) > b * tab['n'])
                                   for b in (0.1, 0.25, 0.5)} | {'source_files': sum(1 for f in tab['t']['files'] if li.is_source(f)), 'tests': tab['n']}
                          for i, tab in tables.items()}
added = Counter()
for a in rows:
    for path in a['diff']:
        tab = tables[a['table']]
        if li.is_source(path) and path not in tab['t']['files'] and path not in tab['tree_set'] and not li.is_unit_test_module(path):
            added[a['event']] += 1

with open(f'{out}/attempts.csv', 'w', newline='') as f:
    w = csv.writer(f)
    w.writerow(['event_id', 'at', 'run_id', 'table_run', 'diff_files', 'added_source_files', 'duration_secs'] +
               [f'{n}:{c}' for n in VARIANTS for c in ('full', 'reasons', 'estimate_secs', 'left_out')])
    for i, a in enumerate(rows):
        w.writerow([a['event'], a['at'], a['run'], a['table'], len(a['diff']), added[a['event']], a['duration']] +
                   [x for n in VARIANTS for x in (int(res[n][i]['full']), '+'.join(res[n][i]['kinds']),
                                                  f"{res[n][i]['est']:.1f}", res[n][i]['left'])])
with open(f'{out}/new-failures.csv', 'w', newline='') as f:
    w = csv.writer(f)
    w.writerow(['test', 'prev_run', 'red_run', 'red_at', 'new_in_run', 'mass', 'range_tasks', 'direct_commits', 'class'] + list(VARIANTS))
    for e in events:
        w.writerow([e['test'], e['prev_run'], e['red_run'], e['red_at'], e['new_in_run'], int(e['mass']),
                    ';'.join(map(str, e['range_tasks'])), e['direct'], e['cls']] + [e.get(n, '') for n in VARIANTS])
json.dump(summary, open(f'{out}/summary.json', 'w'), indent=1)
print(json.dumps(summary, indent=1))
