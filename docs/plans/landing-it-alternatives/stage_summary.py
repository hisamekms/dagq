"""Sums up stages.csv (docs/plans/landing-it-alternatives.md, 「全部流した run の段」):
per kind of command the median and p90 of each stage, the time per test, and,
for the IT both windows ran, each test's median time before (the passed
llvm-cov runs) and after (the full landing-it runs), from the PASS lines of the logs.
Usage: python3 stage_summary.py stages.csv EVENTS_BEFORE.json EVENTS_AFTER.json > stage-summary.json"""
import csv, json, math, re, sys
from collections import defaultdict


def median(v):
    v = sorted(v); n = len(v)
    return None if not n else (v[n // 2] if n % 2 else (v[n // 2 - 1] + v[n // 2]) / 2)


def p90(v):
    v = sorted(v)
    return v[math.ceil(0.9 * len(v)) - 1] if v else None


rows = list(csv.DictReader(open(sys.argv[1])))
out = {'kinds': {}, 'it_by_day': {}, 'common_it': {}}
groups = defaultdict(list)
for r in rows:
    groups[r['kind']].append(r)
cols = ['duration_secs', 'build_secs', 'nextest_secs', 'rest_secs', 'tests_run', 'unit_tests', 'unit_serial_secs',
        'it_tests', 'it_serial_secs', 'load_avg_mean']
for k, rs in groups.items():
    d = {'n': len(rs)}
    for c in cols:
        v = [float(r[c]) for r in rs if r[c] != '']
        d[c] = {'median': round(median(v), 1), 'p90': round(p90(v), 1)}
    for a, b in (('it_serial_secs', 'it_tests'), ('unit_serial_secs', 'unit_tests')):
        d[f'{a}_per_test_median'] = round(median([float(r[a]) / int(r[b]) for r in rs if int(r[b])]), 3)
    out['kinds'][k] = d
by_day = defaultdict(list)
for r in rows:
    if r['kind'] in ('llvm_cov_passed', 'landing_it_full') and int(r['it_tests']):
        by_day[f"{r['window']} {r['at'][:10]}"].append(float(r['it_serial_secs']) / int(r['it_tests']))
out['it_by_day'] = {k: {'n': len(v), 'it_secs_per_test_median': round(median(v), 2)} for k, v in sorted(by_day.items())}

kind = {r['event_id']: (r['window'], r['kind']) for r in rows}
TST = re.compile(r'^\s*PASS[^\[]*\[\s*([\d.]+)s\]\s*\(\s*\d+/\d+\)\s+(\S+)\s+(\S+)')
times = {'before': defaultdict(list), 'after': defaultdict(list)}
for path in sys.argv[2:4]:
    for x in json.load(open(path)):
        w = kind.get(str(x['id']))
        if not w or w[1] not in ('llvm_cov_passed', 'landing_it_full'):
            continue
        for line in open(x['payload']['log_path'], errors='replace'):
            m = TST.match(line)
            if m and '::' in m.group(2) and '::bin/' not in m.group(2):
                times[w[0]][f'{m.group(2)} {m.group(3)}'].append(float(m.group(1)))
both = [k for k in times['before'] if k in times['after']]
b = sum(median(times['before'][k]) for k in both)
a = sum(median(times['after'][k]) for k in both)
growth = defaultdict(float)
for k in both:
    growth[k.split(' ')[1].split('::')[0]] += median(times['after'][k]) - median(times['before'][k])
out['common_it'] = {
    'tests': len(both), 'sum_of_medians_before': round(b, 1), 'sum_of_medians_after': round(a, 1),
    'ratio': round(a / b, 3),
    'per_test_ratio_median': round(median([median(times['after'][k]) / median(times['before'][k]) for k in both
                                           if median(times['before'][k]) > 0]), 3),
    'modules_grown_most': [[m, round(v, 1)] for m, v in sorted(growth.items(), key=lambda x: -x[1])[:10]],
}
json.dump(out, sys.stdout, indent=1)
