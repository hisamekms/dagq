"""Before/after measurement of goal 120 (tasks 1715-1719, 1729) for it-reduction.md (task 1720); Python stdlib.

Reads the landing verification logs under --runs-dir, the host metrics CSVs given by
--metrics and the worktree's `git log`. It does not open the queue, does not
call `dagq` and writes nothing under --runs-dir; it writes logs.csv and
excluded.csv into --out-dir and prints the aggregates as JSON lines.
The parsing of a log is the one of measure_goal119.py (task 1714).

From ec310498 (2026-10-07 19:27) the landing runs [landing_verification] of dagq.toml
(ADR-t1925-1: `cargo nextest run` of every unit test and the IT scripts/landing-it.sh picks)
in place of the coverage gate, so every log of goal 120's intervals is one of those. Only a
log whose landing-it lines say `every IT runs` is read; a narrowed one runs part of dagq::it.
The tests a log leaves out as failing on main already (`landing-it: left out, ...`) are kept
per log, and (f) does not count a test as removed or added for an interval that left it out.

  python3 docs/plans/it-reduction/measure_goal120.py \
    --runs-dir ~/.local/share/dagq/77067154921b9014/runs \
    --metrics ~/.local/share/dagq-hostmetrics/metrics.csv \
              ~/.local/share/dagq/77067154921b9014/host/metrics-2026*.csv \
    --until 2026-10-20T12:00:00+09:00 --out-dir docs/plans/it-reduction/goal-120

--until (optional) leaves out logs written after it, so a later run reads the same logs.
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

# Landings of tasks 1715-1719 and 1729 (`git log --grep 'Dagq-Task: <ID>'`).
LANDINGS = {
    '1715': '63e3bfa09198eecbeb1da79deb52973dd7e1927f',
    '1717': '8b7b696db5b1dde3097d07f011d8a1cfe540b98c',
    '1718': '26b01f9148ea600bac46cdc4170992996644b041',
    '1716': 'f7183e607bc32e2b909fa16a1d123b0d8e83d420',
    '1729': '3f0c9069359acff733d4e6ec9944f753198fbf73',
    '1719': 'f9b2ff448303409114b57507f2fb9522e349f822',
}
# Landings of goal 100's 1552-1558 (`git log --grep 'Dagq-Task: <ID>'`), listed by the interval they fall in.
GOAL100 = {
    '1557': '64c8a2d9', '1558': '2762e010', '1552': '0f0d3c76', '1554': 'c4ca3e74',
    '1553': '2ebafe76', '1556': '37f10088', '1555': 'b456c08f',
}
# goal 92's landings are not marked in the commits; the subjects with these words stand for them.
GOAL92_WORDS = re.compile(r'cmux|goal 92')
# NEXTEST_TEST_THREADS in dagq.toml: 4 at 884b3d56, 8 at 6cdf238f, 6 at 40f693f2; unchanged since
# (`git log -p -G 'NEXTEST_TEST_THREADS *=' -- dagq.toml`).
THREADS = [('884b3d56', 4), ('6cdf238f', 8), ('40f693f2', 6)]
# The commit that put [landing_verification] in dagq.toml; logs before it are the coverage gate's and are not read.
LANDING_VERIFICATION_FROM = 'ec310498'
N = 20
# A log that leaves out more tests than this as failing on main already runs a smaller dagq::it
# (from 2026-10-14 12:19 every log leaves out about 280 to 350), so before and after take only
# logs at or below it; the first N logs after the last landing whatever they leave out are after_all.
LEFT_OUT_MAX = 10

TARGETS = ['runtime_cleanup', 'runtime_landing_release', 'runtime_recheck', 'runtime_release', 'runtime_broker', 'runtime_disk',
           'runtime_review_questions', 'runtime_evidence', 'runtime_landing_answers', 'runtime_stale_receipt',
           'runtime_handoff', 'runtime_waiting', 'runtime_waiting_stages', 'runtime_heartbeat']

ANSI = re.compile(r'\x1b\[[0-9;]*[A-Za-z]')
LINE = re.compile(r'^\s*(PASS|FLKY-FL \d+/\d+|FLAKY \d+/\d+|(?:TRY \d+ )?(?:FAIL|SIGSEGV|SIGABRT|SIGKILL|SIGTERM|TIMEOUT|ABORT|LEAK-FAIL))'
                  r'\s*\[\s*([0-9.]+)s\]\s*(?:\([^)]*\)\s*)?(\S+)\s+(\S+)\s*$')
LANDING_VERIFICATION = re.compile(r'^(# dagq: \[landing_verification\]|landing-it: )', re.M)
EVERY_IT = re.compile(r'^landing-it: every IT runs:', re.M)
LEFT_OUT = re.compile(r'^landing-it: left out, fails on main already: (\S+) (\S+)\s*$', re.M)
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


def is_lib(k):
    return k[0] == 'dagq'


def per_log(rs, pred):
    """Median over the logs of one log's count and sum of the tests pred picks: (c1), (c2), (d)."""
    counts = [sum(1 for k in r['tests'] if pred(k)) for r in rs]
    sums = [sum(v for k, v in r['tests'].items() if pred(k)) for r in rs]
    return dict(n=st.median(counts), sum=round(st.median(sums), 1))


def stats(rs, cuts, mask=frozenset()):
    """Aggregates of one interval; with mask, the per-log sums and counts leave out the tests in it."""
    if not rs:
        return {'n': 0}
    med = medtests(rs)
    kept = [dict(r, tests={k: v for k, v in r['tests'].items() if k not in mask}) for r in rs]
    out = dict(
        n=len(rs), period=[datetime.fromtimestamp(rs[i]['end'], TZ).isoformat(timespec='seconds') for i in (0, -1)],
        threads=dict(Counter(r['threads'] for r in rs)),
        landed=dict(Counter(sum(r['end'] > t for t in cuts.values()) for r in rs)),
        stage=round(st.median(r['stage'] for r in rs), 1),
        stage_range=[min(r['stage'] for r in rs), max(r['stage'] for r in rs)],
        total=round(st.median(r['total'] for r in rs), 1),
        total_range=[round(min(r['total'] for r in rs), 1), round(max(r['total'] for r in rs), 1)],
        tests_run=round(st.median(r['run'] for r in rs)),
        it_per_log=per_log(rs, is_it), it=by(med, is_it),
        lib_per_log=per_log(rs, is_lib), lib=by(med, is_lib),
        masked=dict(n=len(mask), total=per_log(kept, lambda k: True), it_per_log=per_log(kept, is_it)),
        left_out=dict(Counter(len(r['left_out']) for r in rs)),
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


def diff(b, f, b_out, f_out):
    """Tests in one interval and not the other, per module: (f) of task 1720.

    A test some log of the other interval left out as failing on main already is neither removed nor added.
    """
    out = {}
    for name, keys, src in (('removed', b.keys() - f.keys() - f_out, b), ('added', f.keys() - b.keys() - b_out, f)):
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


def landings(since, until):
    """main's landings between two times: their count, goal 100's 1552-1558 and the goal 92 subjects among them."""
    log = subprocess.check_output(['git', 'log', '--first-parent', 'main', f'--since={int(since)}', f'--until={int(until)}',
                                   '--format=%h %ct %s'], text=True).splitlines()
    g100 = {v: k for k, v in GOAL100.items()}
    rows = [l.split(' ', 2) for l in log]
    return dict(n=len(rows), goal100=[f"{g100[h]} {h}" for h, _, _ in rows if h in g100],
                goal92=[f"{h} {datetime.fromtimestamp(int(t), TZ).isoformat(timespec='minutes')} {s[:60]}"
                        for h, t, s in rows if GOAL92_WORDS.search(s)])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--runs-dir', required=True, type=Path)
    ap.add_argument('--metrics', required=True, type=Path, nargs='+')
    ap.add_argument('--out-dir', required=True, type=Path)
    ap.add_argument('--until', help='ISO 8601 time; logs with a later mtime are not read')
    a = ap.parse_args()
    cuts = {k: commit_time(v) for k, v in LANDINGS.items()}
    first, last = min(cuts.values()), max(cuts.values())
    threads = [(commit_time(c), n) for c, n in THREADS]
    metrics = read_metrics(a.metrics)
    until = datetime.fromisoformat(a.until).timestamp() if a.until else None
    since = commit_time(LANDING_VERIFICATION_FROM)

    rows, excluded = [], []
    for path in sorted(a.runs_dir.glob('*/integrate-*-verify-*.log')):
        end = path.stat().st_mtime
        if end < since or (until is not None and end > until):
            continue
        text = ANSI.sub('', path.read_text(errors='replace'))
        if 'Summary [' not in text:
            continue
        rel = str(path.relative_to(a.runs_dir))
        if not LANDING_VERIFICATION.search(text):
            excluded.append(dict(log=rel, end=end, reason='coverage gate (before [landing_verification])'))
            continue
        if not EVERY_IT.search(text):
            excluded.append(dict(log=rel, end=end, reason='narrowed IT (not every IT runs)'))
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
                         left_out={(m[1], m[2]) for m in LEFT_OUT.finditer(text)},
                         it=sum(v for k, v in got['tests'].items() if is_it(k)),
                         lib=sum(v for k, v in got['tests'].items() if is_lib(k)),
                         targets=sum(v for k, v in got['tests'].items() if is_it(k) and module(k) in TARGETS),
                         load=st.mean(loads), peak=max(loads), samples=len(loads)))
    rows.sort(key=lambda r: r['end'])

    full = [r for r in rows if len(r['left_out']) <= LEFT_OUT_MAX]
    before = [r for r in full if r['end'] < first][-N:]
    middle = [r for r in rows if first < r['end'] < last]
    after = [r for r in full if r['end'] > last][:N]
    after_all = [r for r in rows if r['end'] > last][:N]
    groups = {'before': before, 'middle': middle, 'after': after, 'after_all': after_all}
    left_out = {name: set().union(*(r['left_out'] for r in rs)) for name, rs in groups.items()}
    # The tests either interval of the after_all comparison left out, for its per-log values.
    mask = frozenset(left_out['before'] | left_out['after_all'])
    for name, rs in groups.items():
        print(name, json.dumps(stats(rs, cuts, mask if name in ('before', 'after_all') else frozenset()), ensure_ascii=False))
        if rs:
            print('landings', name, json.dumps(landings(rs[0]['end'] - rs[0]['stage'], rs[-1]['end']), ensure_ascii=False))
    if after_all:
        print('landings between', json.dumps(landings(before[-1]['end'], after_all[0]['end']), ensure_ascii=False))
    print('after the last landing', len([r for r in rows if r['end'] > last]), 'logs,',
          len([r for r in full if r['end'] > last]), f'leave out at most {LEFT_OUT_MAX}')
    for name in ('after', 'after_all'):
        if groups[name]:
            print('diff', name, json.dumps(diff(medtests(before), medtests(groups[name]), left_out['before'], left_out[name]),
                                           ensure_ascii=False))

    chosen = sorted({r['path']: r for rs in groups.values() for r in rs}.values(), key=lambda r: r['end'])
    a.out_dir.mkdir(parents=True, exist_ok=True)
    with (a.out_dir / 'logs.csv').open('w') as out:
        w = csv.writer(out, lineterminator='\n')
        w.writerow(['log', 'mtime_jst', 'interval', 'threads', 'tests_run', 'left_out', 'flaky', 'fail', 'summary_seconds',
                    'tests_seconds', 'it_tests', 'it_seconds', 'lib_tests', 'lib_seconds', 'target_modules_seconds',
                    'load1_mean', 'load1_max', 'load_samples'])
        for r in chosen:
            interval = '+'.join(n for n, rs in groups.items() if r in rs)
            w.writerow([r['path'], datetime.fromtimestamp(r['end'], TZ).isoformat(timespec='seconds'), interval, r['threads'],
                        r['run'], len(r['left_out']), r['flaky'], r['failed'], r['stage'], round(r['total'], 3),
                        sum(1 for k in r['tests'] if is_it(k)), round(r['it'], 3),
                        sum(1 for k in r['tests'] if is_lib(k)), round(r['lib'], 3),
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
