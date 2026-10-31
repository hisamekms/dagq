"""Before/after measurement of goal 237 (tasks 3759, 3761, 3763-3770) for it-reduction.md (task 3771); Python stdlib.

Reads the landing verification logs under --runs-dir (the integrate-*-verify-*.log whose
first lines are scripts/landing-it.sh's), the host metrics CSVs given by --metrics, the
worktree's `git log` and tests/it of the worktree. It does not open the queue, does not
call `dagq` and writes nothing under --runs-dir; it writes logs.csv into --out-dir and
prints the aggregates as JSON lines.

  python3 -I docs/plans/it-reduction/measure_goal237.py \
    --runs-dir ~/.local/share/dagq/77067154921b9014/runs \
    --metrics ~/.local/share/dagq/77067154921b9014/host/metrics-2026*.csv \
    --since 2026-10-27T00:00:00+09:00 --until 2026-10-31T20:30:00+09:00 \
    --out-dir docs/plans/it-reduction/goal-237

before is a log written from --since up to the landing of 3759 (the first of goal 237),
middle up to the landing of 3770 (the last), after the rest up to --until.
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
# The landings of goal 237 (`git log --grep 'Dagq-Task: <ID>'`); before ends at the first, after starts at the last.
FIRST = 'bd10d148bd490c6ff49cbe93c0fcba2590b01594'  # 3759
LAST = 'c77abb980164a793ee3ff47342f720b06aacbb2c'  # 3770
# The 8 modules goal 237 moves (runtime_handoff and runtime_cleanup with the sibling file each task changed).
TARGETS = {
    'runtime_replan_park': ['runtime_replan_park'],
    'runtime_resume': ['runtime_resume'],
    'runtime_recheck': ['runtime_recheck'],
    'runtime_handoff': ['runtime_handoff', 'runtime_handoff_race'],
    'runtime_cleanup': ['runtime_cleanup', 'runtime_cleanup_incremental'],
    'runtime_integrate': ['runtime_integrate'],
    'runtime_headless': ['runtime_headless'],
    'review_subagents': ['review_subagents'],
}
ANSI = re.compile(r'\x1b\[[0-9;]*[A-Za-z]')
PASS = re.compile(r'^\s*PASS \[\s*([0-9.]+)s\]\s*(?:\([^)]*\)\s*)?dagq::it (\S+)\s*$')
SUMMARY = re.compile(r'^\s*Summary \[\s*([0-9.]+)s\]\s*(\d+)(?:/(\d+))? tests? run')
BUILD = re.compile(r'^\s*Finished `test` profile .* in (?:(\d+)m )?([0-9.]+)s\s*$')
NOT_RUN = re.compile(r'(\d+)/(\d+) tests were not run due to signal')
SIGNAL = re.compile(r'tests were not run due to signal|test aborted with signal 15')


def commit_time(ref):
    return int(subprocess.check_output(['git', 'show', '-s', '--format=%ct', ref]))


def reason_kind(r):
    """The kind of one `landing-it:   <reason>` line, with a common file split by where it is."""
    if r.startswith('a common file: '):
        p = r[len('a common file: '):]
        if p.startswith('tests/common/'):
            return 'a common file: tests/common/**'
        return 'a common file: ' + p
    if r.startswith("the narrowed IT's estimated wall time"):
        return "the narrowed IT's estimated wall time exceeds the share of every IT's"
    return r.split(':', 1)[0]


def read_log(path):
    text = ANSI.sub('', path.read_text(errors='replace'))
    lines = text.splitlines()
    every, reasons = False, []
    for i, l in enumerate(lines):
        if l.startswith('landing-it: every IT runs:'):
            every = True
            for r in lines[i + 1:]:
                if not r.startswith('landing-it:   '):
                    break
                reasons.append(r[len('landing-it:   '):])
            break
    if 'no table to hand over' in text and not every:
        every, reasons = True, ['no table to hand over']
    m = None
    for l in lines:
        m = SUMMARY.match(l) or m
    build_s = 0.0
    for l in lines:
        b = BUILD.match(l)
        if b:
            build_s += int(b.group(1) or 0) * 60 + float(b.group(2))
    it_n, it_s, pass_s = 0, 0.0, 0.0
    for l in lines:
        q = re.match(r'^\s*PASS \[\s*([0-9.]+)s\]', l)
        if q:
            pass_s += float(q.group(1))
    mods = defaultdict(lambda: [0, 0.0])
    every_mod = defaultdict(lambda: [0, 0.0])
    for l in lines:
        p = PASS.match(l)
        if p:
            s, name = float(p.group(1)), p.group(2)
            it_n += 1
            it_s += s
            mod = name.split('::', 1)[0]
            every_mod[mod][0] += 1
            every_mod[mod][1] += s
            for t, files in TARGETS.items():
                if mod in files:
                    mods[t][0] += 1
                    mods[t][1] += s
    return {
        'every': every,
        'reasons': reasons,
        'summary_s': float(m.group(1)) if m else None,
        'build_s': build_s,
        'not_run': [int(x) for x in NOT_RUN.search(text).groups()] if NOT_RUN.search(text) else None,
        'timed_out': bool(SIGNAL.search(text)),
        'failed': 'error: test run failed' in text and not SIGNAL.search(text),
        'it_n': it_n,
        'it_s': it_s,
        'pass_s': pass_s,
        'mods': mods,
        'every_mod': every_mod,
    }


def load_series(paths):
    rows = []
    for p in paths:
        with open(p) as f:
            for r in csv.DictReader(f):
                try:
                    rows.append((int(r['unix']), float(r['load1'])))
                except (KeyError, ValueError):
                    pass
    rows.sort()
    return rows


def load_during(series, start, end):
    xs = [l for t, l in series if start <= t <= end]
    return (round(st.mean(xs), 2), max(xs)) if xs else (None, None)


def pct(xs, q):
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * q
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return round(xs[lo] + (xs[hi] - xs[lo]) * (k - lo), 1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--runs-dir', required=True)
    ap.add_argument('--metrics', nargs='*', default=[])
    ap.add_argument('--since', required=True)
    ap.add_argument('--until', required=True)
    ap.add_argument('--out-dir', required=True)
    a = ap.parse_args()
    since = datetime.fromisoformat(a.since).timestamp()
    until = datetime.fromisoformat(a.until).timestamp()
    first, last = commit_time(FIRST), commit_time(LAST)
    series = load_series(a.metrics)
    logs = []
    for p in Path(a.runs_dir).expanduser().glob('*/integrate-*-verify-*.log'):
        mt = p.stat().st_mtime
        if not since <= mt <= until:
            continue
        with open(p, errors='replace') as f:
            head = f.read(4096)
        if not head.startswith('# dagq: [landing_verification]') or 'landing-it: ' not in head:
            continue
        r = read_log(p)
        r['path'] = '%s/%s' % (p.parent.name, p.name)
        r['mtime'] = mt
        r['interval'] = 'before' if mt < first else ('middle' if mt < last else 'after')
        start = mt - (r['summary_s'] or 0)
        r['load_mean'], r['load_max'] = load_during(series, start, mt)
        logs.append(r)
    logs.sort(key=lambda r: r['mtime'])
    out = Path(a.out_dir)
    out.mkdir(parents=True, exist_ok=True)
    with open(out / 'logs.csv', 'w', newline='') as f:
        w = csv.writer(f)
        w.writerow(['path', 'mtime', 'interval', 'every_it', 'reasons', 'build_s', 'summary_s', 'timed_out', 'failed',
                    'it_pass', 'it_pass_s', 'load1_mean', 'load1_max'] +
                   ['%s_%s' % (t, k) for t in TARGETS for k in ('n', 's')])
        for r in logs:
            w.writerow([r['path'], datetime.fromtimestamp(r['mtime'], TZ).isoformat(timespec='seconds'), r['interval'],
                        int(r['every']), ' | '.join(reason_kind(x) for x in r['reasons']), round(r['build_s'], 1), r['summary_s'],
                        int(r['timed_out']), int(r['failed']), r['it_n'], round(r['it_s'], 1),
                        r['load_mean'], r['load_max']] +
                       [x for t in TARGETS for x in (r['mods'][t][0], round(r['mods'][t][1], 1))])
    for name in ('before', 'middle', 'after'):
        xs = [r for r in logs if r['interval'] == name]
        if not xs:
            print(json.dumps({'interval': name, 'logs': 0}))
            continue
        every = [r for r in xs if r['every']]
        kinds = Counter(k for r in every for k in sorted({reason_kind(x) for x in r['reasons']}))
        done = [r for r in xs if r['summary_s'] is not None and not r['timed_out']]
        full_done = [r for r in every if r['summary_s'] is not None and not r['timed_out']]
        summ = [r['summary_s'] for r in done]
        summ_full = [r['summary_s'] for r in full_done]
        loads = [r['load_mean'] for r in xs if r['load_mean'] is not None]
        stretched = [(r['build_s'] + r['summary_s']) * r['not_run'][1] / (r['not_run'][1] - r['not_run'][0])
                     for r in xs if r['timed_out'] and r['not_run'] and r['summary_s']]
        res = {
            'interval': name,
            'logs': len(xs),
            'mtime': [datetime.fromtimestamp(xs[0]['mtime'], TZ).isoformat(timespec='seconds'),
                      datetime.fromtimestamp(xs[-1]['mtime'], TZ).isoformat(timespec='seconds')],
            'every_it': len(every),
            'every_it_share': round(len(every) / len(xs), 3),
            'reason_kinds': dict(kinds),
            'timed_out': sum(r['timed_out'] for r in xs),
            'timed_out_every_it': sum(r['timed_out'] for r in every),
            'no_summary': sum(r['summary_s'] is None for r in xs),
            'failed_tests': sum(r['failed'] for r in xs),
            'summary_median_p90_all_done': [pct(summ, .5), pct(summ, .9), len(summ)],
            'summary_median_p90_every_it_done': [pct(summ_full, .5), pct(summ_full, .9), max(summ_full, default=None),
                                                 len(summ_full)],
            'build_plus_summary_median_p90_every_it_done': [pct([r['build_s'] + r['summary_s'] for r in full_done], .5),
                                                            pct([r['build_s'] + r['summary_s'] for r in full_done], .9),
                                                            max((r['build_s'] + r['summary_s'] for r in full_done), default=None)],
            'build_plus_summary_timed_out': sorted(round(r['build_s'] + (r['summary_s'] or 0), 1)
                                                   for r in xs if r['timed_out']),
            # A timed-out log stretched by the share of its tests it ran: (build + Summary) * Y / (Y - X).
            'stretched_timed_out_median_max': [pct(stretched, .5), max(stretched, default=None)],
            'non_it_pass_s_median_every_it_done': pct([r['pass_s'] - r['it_s'] for r in full_done], .5),
            'build_median_every_it_done': pct([r['build_s'] for r in full_done], .5),
            'build_median_all': pct([r['build_s'] for r in xs], .5),
            'it_n_median_every_it_done': pct([r['it_n'] for r in full_done], .5),
            'it_s_median_every_it_done': pct([r['it_s'] for r in full_done], .5),
            'it_n_median_narrowed_done': pct([r['it_n'] for r in done if not r['every']], .5),
            'it_s_median_narrowed_done': pct([r['it_s'] for r in done if not r['every']], .5),
            'load1_mean_median_range': [pct(loads, .5), min(loads, default=None), max(loads, default=None)],
            'targets_every_it_done': {t: [pct([r['mods'][t][0] for r in full_done], .5),
                                          pct([r['mods'][t][1] for r in full_done], .5)] for t in TARGETS},
        }
        names = {m for r in full_done for m in r['every_mod']}
        ranked = sorted(((pct([r['every_mod'][m][1] for r in full_done], .5),
                          pct([r['every_mod'][m][0] for r in full_done], .5), m) for m in names), reverse=True)
        res['modules_by_median_s_every_it_done'] = [[m, n, s_] for s_, n, m in ranked[:20]]
        print(json.dumps(res, ensure_ascii=False))


if __name__ == '__main__':
    main()
