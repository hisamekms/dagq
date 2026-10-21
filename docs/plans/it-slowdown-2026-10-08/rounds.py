"""(docs/plans/it-slowdown-2026-10-08.md) The reproduction's table: for each
setting, from the nextest logs of its rounds (LOGDIR/<prefix><round>-<setting>.log),
the nextest wall time of each round and, over the PASS lines of every round
pooled, the median and p90 of the seconds per test, also by module.
Usage: python3 rounds.py LOGDIR PREFIX SETTING... > json"""
import glob, json, math, os, re, sys
from collections import defaultdict

PASS = re.compile(r'^\s*PASS[^\[]*\[\s*([\d.]+)s\]\s*\(\s*\d+/\d+\)\s+(\S+)\s+(\S+)')
SUM = re.compile(r'^\s*Summary \[\s*([\d.]+)s\] (\d+) tests? run')


def median(v):
    v = sorted(v); n = len(v)
    return v[n // 2] if n % 2 else (v[n // 2 - 1] + v[n // 2]) / 2


def p90(v):
    v = sorted(v)
    return v[math.ceil(0.9 * len(v)) - 1]


logdir, prefix, settings = sys.argv[1], sys.argv[2], sys.argv[3:]
out = {}
for s in settings:
    walls, secs, by_mod = [], [], defaultdict(list)
    for path in sorted(glob.glob(os.path.join(logdir, f'{prefix}[0-9]-{s}.log'))):
        for line in open(path, errors='replace'):
            m = PASS.match(line)
            if m:
                secs.append(float(m.group(1)))
                by_mod[m.group(3).split('::')[0]].append(float(m.group(1)))
            m = SUM.match(line)
            if m:
                walls.append(float(m.group(1)))
    out[s] = {'rounds': len(walls), 'wall_secs': walls, 'tests': len(secs),
              'secs_per_test_median': round(median(secs), 3), 'secs_per_test_p90': round(p90(secs), 3),
              'modules': {k: {'median': round(median(v), 3), 'p90': round(p90(v), 3)} for k, v in sorted(by_mod.items())}}
json.dump(out, sys.stdout, indent=1)
