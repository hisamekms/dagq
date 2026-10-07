"""Before/after measurement of goal 119 (tasks 1709-1713) for it-reduction.md (task 1714); Python stdlib.

Reads the coverage gate logs under --runs-dir, the host metrics CSVs given by
--metrics and the worktree's `git log`. It does not open the queue, does not
call `dagq` and writes nothing under --runs-dir; it writes logs.csv and
excluded.csv into --out-dir and prints the aggregates as JSON lines.
The parsing of a log is the one of docs/plans/integration-to-unit-tests/measure.py (task 1417).

--metrics takes one or more CSVs: the host sampler's (columns ts, load1; JST
without an offset) and the queue's daily host/metrics-*.csv (columns unix, load1).
The first ends on 2026-10-03, so the queue's files are needed for the later logs.

  python3 docs/plans/it-reduction/measure_goal119.py \
    --runs-dir ~/.local/share/dagq/77067154921b9014/runs \
    --metrics ~/.local/share/dagq-hostmetrics/metrics.csv \
              ~/.local/share/dagq/77067154921b9014/host/metrics-2026*.csv \
    --until 2026-10-08T06:30:00+09:00 --out-dir docs/plans/it-reduction/goal-119

--until (optional) leaves out logs written after it, so a later run reads the same logs.
A log of [landing_verification] (dagq.toml from ec310498, ADR-t1925-1; its header line or the
`landing-it:` lines of scripts/landing-it.sh) is left out: it runs `cargo nextest run` without
the coverage instrumentation, so its seconds are not the coverage gate's.
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

# Landings of tasks 1709-1713 (`git log --grep 'Dagq-Task: <ID>'`).
LANDINGS = {
    '1709': '6e53bc4dc8643fc81c0ab2091b368c6c7f176d2e',
    '1712': 'e70840a90ad29812c855146c54be13f18649b173',
    '1713': '04d8bb4e527a81a749fb0dcf0efe02215d7bdb4c',
    '1711': '824619bff7cc9f6bc467a079471de9838fecb66b',
    '1710': 'cf96001dbf5fc23f07a754918541b912542067f5',
}
# NEXTEST_TEST_THREADS in dagq.toml: 4 at 884b3d56, 8 at 6cdf238f, 6 at 40f693f2; unchanged since
# (`git log -p -G 'NEXTEST_TEST_THREADS *=' -- dagq.toml`).
THREADS = [('884b3d56', 4), ('6cdf238f', 8), ('40f693f2', 6)]
N = 20

TARGETS = ['cli_stats', 'installed_plugin', 'runtime_claim', 'runtime_heartbeat', 'cli_authorization', 'cli_actor',
           'plan_review', 'plan_review_concern', 'plan_review_codex', 'plan_review_reasons', 'plan_review_prompt_bytes',
           'planner_headless_turns', 'request_planner', 'planner_headless_stats', 'review_subagents', 'goal_review',
           'goal_review_codex', 'runtime_codex', 'runtime_provider_switch']

ANSI = re.compile(r'\x1b\[[0-9;]*[A-Za-z]')
LINE = re.compile(r'^\s*(PASS|FLKY-FL \d+/\d+|FLAKY \d+/\d+|(?:TRY \d+ )?(?:FAIL|SIGSEGV|SIGABRT|SIGKILL|SIGTERM|TIMEOUT|ABORT|LEAK-FAIL))'
                  r'\s*\[\s*([0-9.]+)s\]\s*(?:\([^)]*\)\s*)?(\S+)\s+(\S+)\s*$')
LANDING_VERIFICATION = re.compile(r'^(# dagq: \[landing_verification\]|landing-it: )', re.M)
SUMMARY = re.compile(r'^\s*Summary\s*\[\s*([0-9.]+)s\]\s*(\S+) tests? run: (\d+) passed(?: \(([^)]*)\))?(?:, (\d+) failed)?')


def commit_time(ref):
    return int(subprocess.check_output(['git', 'show', '-s', '--format=%ct', ref]))


def parse(text):
    """Final results of one log, or the reason it is left out.

    As task 1417's measure.py, except for the final failure: the gate's nextest lists it
    as `FAIL` or as the last `TRY n FAIL`, before or after Summary, so a test is a final
    failure when it has a failing line and no PASS, FLKY-FL or FLAKY line.
    """
    lines = text.splitlines()
    sums = [m for m in map(SUMMARY.match, lines) if m]
    if len(sums) != 1:
        return f'{len(sums)} Summary lines'
    m = sums[0]
    if '/' in m[2]:
        return f'canceled ({m[2]} tests run)'
    stage, run, passed = float(m[1]), int(m[2]), int(m[3])
    tests, kinds, failing = {}, {}, set()
    for l in lines:
        r = LINE.match(l)
        if not r:
            continue
        status, sec, key = r[1], float(r[2]), (r[3], r[4])
        if status == 'PASS' or status.startswith(('FLKY-FL', 'FLAKY')):
            kind = status.split()[0]
            if key in tests and kinds[key] == kind:
                return f'{kind} twice for {key}'
            tests[key], kinds[key] = sec, kind
        else:
            failing.add(key)
    fails = failing - tests.keys()
    counts = Counter(kinds.values())
    if len(tests) + len(fails) != run:
        return f'picked {len(tests)} + failed {len(fails)} != {run} run'
    if counts['PASS'] != passed - counts['FLAKY']:
        return f"PASS {counts['PASS']} != passed {passed} - FLAKY {counts['FLAKY']}"
    return dict(stage=stage, run=run, tests=tests, kinds=kinds, failed=len(fails),
                flaky=counts['FLKY-FL'] + counts['FLAKY'])


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


def is_it(k):
    return k[0] == 'dagq::it'


def stats(rs, cuts):
    if not rs:
        return {'n': 0}
    med = medtests(rs)
    out = dict(
        n=len(rs), period=[datetime.fromtimestamp(rs[i]['end'], TZ).isoformat(timespec='seconds') for i in (0, -1)],
        threads=dict(Counter(r['threads'] for r in rs)),
        landed=dict(Counter(sum(r['end'] > t for t in cuts.values()) for r in rs)),
        stage=round(st.median(r['stage'] for r in rs), 1),
        stage_range=[min(r['stage'] for r in rs), max(r['stage'] for r in rs)],
        total=round(st.median(r['total'] for r in rs), 1),
        total_range=[round(min(r['total'] for r in rs), 1), round(max(r['total'] for r in rs), 1)],
        tests_run=round(st.median(r['run'] for r in rs)),
        it=by(med, is_it), lib=by(med, lambda k: k[0] == 'dagq'),
        it_per_log=round(st.median(r['it'] for r in rs), 1),
        targets=by(med, lambda k: is_it(k) and module(k) in TARGETS),
        load=round(st.median(r['load'] for r in rs), 2), load_range=[round(min(r['load'] for r in rs), 2), round(max(r['load'] for r in rs), 2)],
        peak=round(st.median(r['peak'] for r in rs), 2), peak_range=[min(r['peak'] for r in rs), max(r['peak'] for r in rs)],
        flaky=sum(r['flaky'] for r in rs), failed=sum(r['failed'] for r in rs),
    )
    out['modules'] = {m: by(med, lambda k, m=m: is_it(k) and module(k) == m) for m in TARGETS}
    return out


def read_metrics(paths):
    out = []
    for p in paths:
        with p.open() as f:
            for r in csv.DictReader(f):
                if r.get('unix') == 'unix' or r.get('ts') == 'ts':
                    continue  # a header written again when the sampler restarts
                if 'unix' in r:
                    out.append((float(r['unix']), float(r['load1'])))
                else:
                    out.append((datetime.fromisoformat(r['ts']).replace(tzinfo=TZ).timestamp(), float(r['load1'])))
    out.sort()
    return out


def diff(b, f):
    """Tests in one interval and not the other, per module: (f) of task 1714."""
    out = {}
    for name, keys, src in (('removed', b.keys() - f.keys(), b), ('added', f.keys() - b.keys(), f)):
        per = defaultdict(lambda: [0, 0.0])
        for k in keys:
            per[module(k)][0] += 1
            per[module(k)][1] += src[k]
        out[name] = dict(n=len(keys), sum=round(sum(src[k] for k in keys), 1),
                         it=[len([k for k in keys if is_it(k)]), round(sum(src[k] for k in keys if is_it(k)), 1)],
                         modules={m: [v[0], round(v[1], 1)] for m, v in sorted(per.items(), key=lambda kv: -kv[1][1])})
        for part, pred in (('targets', lambda k: is_it(k) and module(k) in TARGETS),
                           ('other_it', lambda k: is_it(k) and module(k) not in TARGETS),
                           ('not_it', lambda k: not is_it(k))):
            ks = [k for k in keys if pred(k)]
            out[name][part] = [len(ks), round(sum(src[k] for k in ks), 1)]
    common = b.keys() & f.keys()
    out['common'] = {m: [len(ks), round(sum(b[k] for k in ks), 1), round(sum(f[k] for k in ks), 1)]
                     for m, ks in (('it', [k for k in common if is_it(k)]),
                                   ('targets', [k for k in common if is_it(k) and module(k) in TARGETS]),
                                   ('other_it', [k for k in common if is_it(k) and module(k) not in TARGETS]))}
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--runs-dir', required=True, type=Path)
    ap.add_argument('--metrics', required=True, type=Path, nargs='+')
    ap.add_argument('--out-dir', required=True, type=Path)
    ap.add_argument('--until', help='ISO 8601 time; logs with a later mtime are not read')
    a = ap.parse_args()
    cuts = {k: commit_time(v) for k, v in LANDINGS.items()}
    order = sorted(cuts.values())
    first, fourth, last = order[0], order[-2], order[-1]
    threads = [(commit_time(c), n) for c, n in THREADS]
    metrics = read_metrics(a.metrics)
    until = datetime.fromisoformat(a.until).timestamp() if a.until else None

    rows, excluded = [], []
    for path in sorted(a.runs_dir.glob('*/integrate-*-verify-*.log')):
        end = path.stat().st_mtime
        if end < first - 7 * 86400 or (until is not None and end > until):
            continue
        text = ANSI.sub('', path.read_text(errors='replace'))
        if 'Summary [' not in text:
            continue
        rel = str(path.relative_to(a.runs_dir))
        if LANDING_VERIFICATION.search(text):
            excluded.append(dict(log=rel, end=end, reason='landing_verification (not the coverage gate)'))
            continue
        got = parse(text)
        if isinstance(got, str):
            excluded.append(dict(log=rel, end=end, reason=got))
            continue
        loads = [load for ts, load in metrics if end - got['stage'] <= ts <= end]
        if not loads:
            excluded.append(dict(log=rel, end=end, reason='no metrics in the test stage'))
            continue
        start = end - got['stage']
        n_threads = [n for t, n in threads if t <= start][-1]
        rows.append(dict(got, path=rel, end=end, threads=n_threads, total=sum(got['tests'].values()),
                         it=sum(v for k, v in got['tests'].items() if is_it(k)),
                         targets=sum(v for k, v in got['tests'].items() if is_it(k) and module(k) in TARGETS),
                         load=st.mean(loads), peak=max(loads), samples=len(loads)))
    rows.sort(key=lambda r: r['end'])

    before = [r for r in rows if r['end'] < first][-N:]
    middle = [r for r in rows if first < r['end'] < last]
    after4 = [r for r in rows if fourth < r['end'] < last][:N]  # 1709, 1711, 1712, 1713 landed; 1710 not yet
    after = [r for r in rows if r['end'] > last][:N]
    groups = {'before': before, 'middle': middle, 'after4': after4, 'after': after}
    for name, rs in groups.items():
        print(name, json.dumps(stats(rs, cuts), ensure_ascii=False))
    b = medtests(before)
    for name in ('after4', 'after'):
        if groups[name]:
            print('diff', name, json.dumps(diff(b, medtests(groups[name])), ensure_ascii=False))

    chosen = sorted({r['path']: r for rs in groups.values() for r in rs}.values(), key=lambda r: r['end'])
    a.out_dir.mkdir(parents=True, exist_ok=True)
    with (a.out_dir / 'logs.csv').open('w') as out:
        w = csv.writer(out, lineterminator='\n')
        w.writerow(['log', 'mtime_jst', 'interval', 'threads', 'tests_run', 'flaky', 'fail', 'summary_seconds',
                    'tests_seconds', 'it_seconds', 'target_modules_seconds', 'load1_mean', 'load1_max', 'load_samples'])
        for r in chosen:
            interval = '+'.join(n for n, rs in groups.items() if r in rs)
            w.writerow([r['path'], datetime.fromtimestamp(r['end'], TZ).isoformat(timespec='seconds'), interval, r['threads'],
                        r['run'], r['flaky'], r['failed'], r['stage'], round(r['total'], 3), round(r['it'], 3),
                        round(r['targets'], 3), round(r['load'], 3), r['peak'], r['samples']])
    with (a.out_dir / 'excluded.csv').open('w') as out:
        w = csv.writer(out, lineterminator='\n')
        w.writerow(['log', 'mtime_jst', 'reason'])
        for e in sorted(excluded, key=lambda e: e['end']):
            w.writerow([e['log'], datetime.fromtimestamp(e['end'], TZ).isoformat(timespec='seconds'), e['reason']])
    print('rows', len(rows), 'excluded', len(excluded),
          json.dumps(Counter(e['reason'].split(' (')[0].split(':')[0] for e in excluded)))


if __name__ == '__main__':
    main()
