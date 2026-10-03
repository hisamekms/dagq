"""Log measurement for integration-to-unit-tests.md (task 1417); Python stdlib.

Reads the coverage gate logs under --runs-dir, the host metrics CSV given by
--metrics and the worktree's `git log`. It does not open the queue, does not
call `dagq` and writes nothing under --runs-dir; it writes logs.csv and
excluded.csv into --out-dir and prints the aggregates as JSON lines.

  python3 docs/plans/integration-to-unit-tests/measure.py \
    --runs-dir ~/.local/share/dagq/77067154921b9014/runs \
    --metrics ~/.local/share/dagq-hostmetrics/metrics.csv \
    --out-dir docs/plans/integration-to-unit-tests
"""
import argparse
import csv
import json
import re
import statistics as st
import subprocess
from collections import Counter, defaultdict
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo

TZ = ZoneInfo('Asia/Tokyo')

# Landings of tasks 1412-1416 and the commit that fixes the upper end of the sample.
LANDINGS = {
    '1414': '9d74de58ddd0457b82400dda5d853147d1db93d9',
    '1412': '7fea2d6bedaec2032cee9173022cc133622d8497',
    '1413': 'ead1223c92fac0687853db39fa31afec4e9d0721',
    '1416': '05000a7a3136ee37472a5c6ad433f53a6ea713b6',
    '1415': 'a9397a719885e10fcf44a489c87a2f03fa6cfa1a',
}
UPPER = '0045125d'  # the base of this run
# NEXTEST_TEST_THREADS in dagq.toml: 8 -> 6 at 40f693f2; unchanged since (git log -G).
THREADS_6 = '40f693f2'
REQUEST = ('2026-10-02T18:20:30+09:00', '2026-10-02T22:29:41+09:00')
N = 20

CHANGED = ['runtime_resume', 'runtime_integrate', 'runtime_review', 'runtime_review_concern',
           'runtime_job_verdicts', 'runtime_triage']
GROUPS = ['runtime_resume*', 'runtime_review*', 'runtime_handoff*', 'runtime_waiting*',
          'runtime_headless*', 'runtime_session*', 'runtime_landing*', 'runtime_stall*',
          'runtime_repair*', 'runtime_adopt*', 'plan_review*', 'lifecycle_*', 'cli_*']

# src test modules where tasks 1412-1415 put the decisions they moved out of tests/it
UNIT_PREFIXES = ('application::supervise::resume::', 'domain::resume::', 'application::supervise::deliver::',
                 'application::integrate::', 'application::prompt::', 'application::supervise::landing::',
                 'domain::concern::', 'domain::recovery::', 'application::supervise::recovery::',
                 'application::supervise::triage::')

ANSI = re.compile(r'\x1b\[[0-9;]*[A-Za-z]')
LINE = re.compile(r'^\s*(PASS|FLKY-FL \d+/\d+|FLAKY \d+/\d+|(?:TRY \d+ )?(?:FAIL|SIGSEGV|SIGABRT|SIGKILL|SIGTERM|TIMEOUT|ABORT|LEAK-FAIL))'
                  r'\s*\[\s*([0-9.]+)s\]\s*(?:\([^)]*\)\s*)?(\S+)\s+(\S+)\s*$')
SUMMARY = re.compile(r'^\s*Summary\s*\[\s*([0-9.]+)s\]\s*(\S+) tests? run: (\d+) passed(?: \(([^)]*)\))?(?:, (\d+) failed)?')


def commit_time(ref):
    return int(subprocess.check_output(['git', 'show', '-s', '--format=%ct', ref]))


def parse(text):
    """Final results of one log: (stage, run, passed, tests, flaky, fails) or a reason."""
    lines = text.splitlines()
    sums = [(i, SUMMARY.match(l)) for i, l in enumerate(lines) if SUMMARY.match(l)]
    if len(sums) != 1:
        return f'{len(sums)} Summary lines'
    at, m = sums[0]
    if '/' in m[2]:
        return f'canceled ({m[2]} tests run)'
    stage, run, passed = float(m[1]), int(m[2]), int(m[3])
    tests, kinds, fails = {}, {}, set()
    for i, l in enumerate(lines):
        r = LINE.match(l)
        if not r:
            continue
        status, sec, key = r[1], float(r[2]), (r[3], r[4])
        if status == 'PASS' or status.startswith(('FLKY-FL', 'FLAKY')):
            kind = status.split()[0]
            if key in tests and kinds[key] == kind:
                return f'{kind} twice for {key}'
            tests[key], kinds[key] = sec, kind
        elif i > at:
            fails.add(key)  # the final failure, listed again after Summary
    counts = Counter(kinds.values())
    flaky = counts['FLKY-FL'] + counts['FLAKY']
    if fails & tests.keys():
        return f'a test both picked and failed: {sorted(fails & tests.keys())[:1]}'
    if len(tests) + len(fails) != run:
        return f'picked {len(tests)} + failed {len(fails)} != {run} run'
    if counts['PASS'] != passed - counts['FLAKY']:
        return f"PASS {counts['PASS']} != passed {passed} - FLAKY {counts['FLAKY']}"
    return dict(stage=stage, run=run, passed=passed, tests=tests, kinds=kinds,
                flaky=flaky, flaky_tests={k for k, v in kinds.items() if v != 'PASS'},
                failed=len(fails), fail_tests=fails)


def module(key):
    binary, test = key
    return test.split('::')[0] if binary == 'dagq::it' else f'[{binary}]'


def medtests(rs):
    per = defaultdict(list)
    for r in rs:
        for key, sec in r['tests'].items():
            per[key].append(sec)
    return {k: st.median(v) for k, v in per.items()}


def by(med, pred):
    keys = [k for k in med if pred(k)]
    return dict(n=len(keys), sum=round(sum(med[k] for k in keys), 1))


def prefix_pred(p):
    if p.endswith('*'):
        return lambda k: k[0] == 'dagq::it' and module(k).startswith(p[:-1])
    return lambda k: k[0] == 'dagq::it' and module(k) == p


def stats(rs, cuts):
    if not rs:
        return {'n': 0}
    med = medtests(rs)
    out = dict(
        n=len(rs), period=[datetime.fromtimestamp(rs[i]['end'], TZ).isoformat(timespec='seconds') for i in (0, -1)],
        threads=dict(Counter(r['threads'] for r in rs)),
        landed=dict(Counter(sum(r['end'] > t for t in cuts.values()) for r in rs)),
        total=round(st.median(r['total'] for r in rs), 1),
        stage=round(st.median(r['stage'] for r in rs), 1),
        changed=round(st.median(r['changed'] for r in rs), 1), rest=round(st.median(r['rest'] for r in rs), 1),
        changed_share=round(st.median(r['changed'] / (r['changed'] + r['rest']) for r in rs), 4),
        stage_range=[min(r['stage'] for r in rs), max(r['stage'] for r in rs)],
        total_range=[round(min(r['total'] for r in rs), 1), round(max(r['total'] for r in rs), 1)],
        per_sum=round(sum(med.values()), 1), test_count=len(med),
        it=by(med, lambda k: k[0] == 'dagq::it'), lib=by(med, lambda k: k[0] == 'dagq'),
        load=round(st.median(r['load'] for r in rs), 2), load_range=[round(min(r['load'] for r in rs), 2), round(max(r['load'] for r in rs), 2)],
        peak=round(st.median(r['peak'] for r in rs), 2), peak_range=[min(r['peak'] for r in rs), max(r['peak'] for r in rs)],
        flaky=sum(r['flaky'] for r in rs), failed=sum(r['failed'] for r in rs),
        logs_with_flaky=sum(r['flaky'] > 0 for r in rs), logs_with_fail=sum(r['failed'] > 0 for r in rs),
    )
    out['modules'] = {m: by(med, prefix_pred(m)) for m in CHANGED + GROUPS}
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--runs-dir', required=True, type=Path)
    ap.add_argument('--metrics', required=True, type=Path)
    ap.add_argument('--out-dir', required=True, type=Path)
    a = ap.parse_args()
    cuts = {k: commit_time(v) for k, v in LANDINGS.items()}
    first, last = min(cuts.values()), max(cuts.values())
    upper, threads6 = commit_time(UPPER), commit_time(THREADS_6)
    req = [datetime.fromisoformat(t).timestamp() for t in REQUEST]
    req[1] += 1  # REQUEST is given to the second; mtimes carry the fraction

    metrics = []
    with a.metrics.open() as f:
        for r in csv.DictReader(f):
            metrics.append((datetime.fromisoformat(r['ts']).replace(tzinfo=TZ).timestamp(), float(r['load1'])))

    rows, excluded = [], []
    for path in sorted(a.runs_dir.glob('*/integrate-*-verify-*.log')):
        end = path.stat().st_mtime
        if not req[0] - 86400 <= end <= upper:
            continue
        text = ANSI.sub('', path.read_text(errors='replace'))
        if 'Summary [' not in text:
            continue
        rel = str(path.relative_to(a.runs_dir))
        got = parse(text)
        if isinstance(got, str):
            excluded.append(dict(log=rel, end=end, reason=got))
            continue
        loads = [load for ts, load in metrics if end - got['stage'] <= ts <= end]
        if not loads:
            excluded.append(dict(log=rel, end=end, reason='no metrics in the test stage'))
            continue
        assert end - got['stage'] >= threads6, rel
        changed = sum(v for k, v in got['tests'].items() if k[0] == 'dagq::it' and module(k) in CHANGED)
        rows.append(dict(got, path=rel, end=end, threads=6, total=sum(got['tests'].values()), changed=changed,
                         rest=sum(v for k, v in got['tests'].items() if k[0] == 'dagq::it') - changed,
                         load=st.mean(loads), peak=max(loads), samples=len(loads)))
    rows.sort(key=lambda r: r['end'])

    before = [r for r in rows if r['end'] < first][-N:]
    after_all = [r for r in rows if r['end'] > last]
    after = after_all[:N]
    request = [r for r in rows if req[0] <= r['end'] <= req[1]]
    middle = [r for r in rows if first < r['end'] < last]
    groups = {'request': request, 'before': before, 'middle': middle, 'after': after, 'after_all': after_all}
    for name, rs in groups.items():
        print(name, json.dumps(stats(rs, cuts), ensure_ascii=False))
    # the request's numbers were taken from PASS and FLAKY only
    req_pf = [dict(r, tests={k: v for k, v in r['tests'].items() if r['kinds'][k] != 'FLKY-FL'}) for r in request]
    for r in req_pf:
        r['total'] = sum(r['tests'].values())
        r['changed'] = sum(v for k, v in r['tests'].items() if k[0] == 'dagq::it' and module(k) in CHANGED)
        r['rest'] = sum(v for k, v in r['tests'].items() if k[0] == 'dagq::it') - r['changed']
    print('request_pass_flaky_only', json.dumps(stats(req_pf, cuts), ensure_ascii=False))
    for lo, hi in [(8, 16), (10, 20), (12, 17)]:
        for name in ('before', 'after'):
            print(f'{name}_load_{lo}_{hi}', json.dumps(stats([r for r in groups[name] if lo <= r['load'] < hi], cuts), ensure_ascii=False))

    # the last log before the first landing is the gate of that landing (task 1414) itself
    print('before_without_own_gate', json.dumps(stats(before[:-1], cuts), ensure_ascii=False))
    b, f = medtests(before), medtests(after)
    common = b.keys() & f.keys()
    for name, keys in (('removed', b.keys() - f.keys()), ('added', f.keys() - b.keys())):
        per = Counter()
        for k in keys:
            per[module(k)] += 1
        src = b if name == 'removed' else f
        print(name, len(keys), round(sum(src[k] for k in keys), 1),
              json.dumps({m: [n, round(sum(src[k] for k in keys if module(k) == m), 1)] for m, n in per.most_common(12)}))
    unit = [k for k in f.keys() - b.keys() if k[0] == 'dagq' and k[1].startswith(UNIT_PREFIXES)]
    print('added_unit_in_touched_modules', len(unit), round(sum(f[k] for k in unit), 2))
    print('common', json.dumps(dict(n=len(common), before=round(sum(b[k] for k in common), 1),
                                    after=round(sum(f[k] for k in common), 1),
                                    it=[len([k for k in common if k[0] == 'dagq::it']),
                                        round(sum(b[k] for k in common if k[0] == 'dagq::it'), 1),
                                        round(sum(f[k] for k in common if k[0] == 'dagq::it'), 1)],
                                    lib=[len([k for k in common if k[0] == 'dagq']),
                                         round(sum(b[k] for k in common if k[0] == 'dagq'), 1),
                                         round(sum(f[k] for k in common if k[0] == 'dagq'), 1)])))
    for m in CHANGED + GROUPS:
        keys = [k for k in common if prefix_pred(m)(k)]
        print('common_module', m, len(keys), round(sum(b[k] for k in keys), 1), round(sum(f[k] for k in keys), 1))
    mods = defaultdict(lambda: [0, 0.0, 0, 0.0])
    for k, v in b.items():
        mods[module(k)][0] += 1
        mods[module(k)][1] += v
    for k, v in f.items():
        mods[module(k)][2] += 1
        mods[module(k)][3] += v
    top = sorted(mods.items(), key=lambda kv: -max(kv[1][1], kv[1][3]))[:25]
    print('top_modules', json.dumps([[m, v[0], round(v[1], 1), v[2], round(v[3], 1)] for m, v in top]))
    for name in ('before', 'after'):
        flaky = Counter(module(k) for r in groups[name] for k in r['flaky_tests'])
        fails = Counter(module(k) for r in groups[name] for k in r['fail_tests'])
        print('flaky_by_module', name, json.dumps(dict(flaky)), 'fail_by_module', json.dumps(dict(fails)))

    chosen = sorted({r['path']: r for rs in (request, before, middle, after) for r in rs}.values(), key=lambda r: r['end'])
    a.out_dir.mkdir(parents=True, exist_ok=True)
    with (a.out_dir / 'logs.csv').open('w') as out:
        w = csv.writer(out, lineterminator='\n')
        w.writerow(['log', 'mtime_jst', 'interval', 'threads_inferred', 'tests', 'pass', 'flky_fl', 'flaky', 'fail',
                    'tests_seconds', 'changed_modules_seconds', 'summary_seconds', 'load1_mean', 'load1_max', 'load_samples'])
        for r in chosen:
            interval = '+'.join(n for n, rs in (('request', request), ('before', before), ('middle', middle), ('after', after)) if r in rs)
            c = Counter(r['kinds'].values())
            w.writerow([r['path'], datetime.fromtimestamp(r['end'], TZ).isoformat(timespec='seconds'), interval, r['threads'],
                        len(r['tests']), c['PASS'], c['FLKY-FL'], c['FLAKY'], r['failed'], round(r['total'], 3), round(r['changed'], 3), r['stage'],
                        round(r['load'], 3), r['peak'], r['samples']])
    with (a.out_dir / 'excluded.csv').open('w') as out:
        w = csv.writer(out, lineterminator='\n')
        w.writerow(['log', 'mtime_jst', 'reason'])
        for e in sorted(excluded, key=lambda e: e['end']):
            w.writerow([e['log'], datetime.fromtimestamp(e['end'], TZ).isoformat(timespec='seconds'), e['reason']])
    print('rows', len(rows), 'excluded', len(excluded), json.dumps(Counter(e['reason'].split(' (')[0].split(':')[0] for e in excluded)))


if __name__ == '__main__':
    main()
