"""(docs/plans/it-slowdown-2026-10-08.md) The time per IT by day, from the logs of
the landing verifications (verification_command.log_path) that ran the IT
with nextest: the passed `cargo llvm-cov nextest` runs before the switch and
the `sh scripts/landing-it.sh` runs after it (any exit code: only PASS lines count),
both only when 500 IT or more passed (a landing-it run that ran every IT).
An IT is common when it passed in both windows. A day's value is the sum, over
the common IT that passed that day, of each IT's median that day, divided by
their number; the stable value is the same over the common IT that passed
on every day (the common set shrinks as tests are renamed). The modules are the 10 whose summed per-IT growth (median after
- median before) is the largest.
The mode split: for each landing-it run, the median over its common IT of
(its seconds / the IT's median before the switch), grouped by whether the run
ended before or after LAUNCHD_AT (the supervisor's first start in launchd mode),
also by band of the run's load_avg_mean; each run's ratio is in runs_after.
With PROCESSTYPE_AT (when the production plist got ProcessType Interactive),
the launchd runs that ended at or after it are the mode launchd_interactive.
Usage: python3 by_day.py EVENTS.json SWITCH_AT LAUNCHD_AT [PROCESSTYPE_AT] > by-day.json"""
import json, math, re, sys
from collections import defaultdict

events, switch, launchd_at = json.load(open(sys.argv[1])), sys.argv[2], sys.argv[3]
processtype_at = sys.argv[4] if len(sys.argv) > 4 else None
PASS = re.compile(r'^\s*PASS[^\[]*\[\s*([\d.]+)s\]\s*\(\s*\d+/\d+\)\s+(\S+)\s+(\S+)')
SUM = re.compile(r'^\s*Summary \[\s*([\d.]+)s\] (\d+) tests? run')


def median(v):
    v = sorted(v); n = len(v)
    return v[n // 2] if n % 2 else (v[n // 2 - 1] + v[n // 2]) / 2


def p90(v):
    v = sorted(v)
    return v[math.ceil(0.9 * len(v)) - 1]


runs = []  # (day, window, {it: secs}, load, nextest secs)
for x in events:
    p = x['payload']; cmd = p.get('command') or ''
    if cmd.startswith('cargo llvm-cov nextest'):
        if p['exit_code'] != 0:
            continue
    elif cmd != 'sh scripts/landing-it.sh':
        continue
    t = {}; summ = None
    for line in open(p['log_path'], errors='replace'):
        m = PASS.match(line)
        if m and '::' in m.group(2) and '::bin/' not in m.group(2):
            t[f'{m.group(2)} {m.group(3)}'] = float(m.group(1))
        m = SUM.match(line)
        if m:
            summ = float(m.group(1))
    if len(t) < 500:  # a narrowed landing-it run
        continue
    win = 'before' if x['created_at'] < switch else 'after'
    if win == 'before' and not cmd.startswith('cargo llvm-cov'):
        continue
    runs.append((x['created_at'][:10], win, t, p.get('load_avg_mean'), summ, x['created_at']))

per = {'before': defaultdict(list), 'after': defaultdict(list)}
for d, w, t, *_ in runs:
    for k, s in t.items():
        per[w][k].append(s)
common = sorted(k for k in per['before'] if k in per['after'])
mod = lambda k: k.split(' ')[1].split('::')[0]
growth = defaultdict(float)
for k in common:
    growth[mod(k)] += median(per['after'][k]) - median(per['before'][k])
top = [m for m, _ in sorted(growth.items(), key=lambda x: -x[1])[:10]]
cset = set(common)

days = defaultdict(lambda: defaultdict(list))
meta = defaultdict(lambda: {'runs': 0, 'load': [], 'nextest': [], 'kinds': set(), 'first': None, 'last': None})
for d, w, t, load, summ, at in runs:
    key = f'{d} {w}'
    m = meta[key]; m['runs'] += 1
    m['first'] = min(m['first'] or at, at); m['last'] = max(m['last'] or at, at)
    if load is not None:
        m['load'].append(load)
    if summ is not None:
        m['nextest'].append(summ)
    for k, s in t.items():
        if k in cset:
            days[key][k].append(s)
stable = set(cset)
for key in days:
    stable &= set(days[key])
out = {'switch_at': switch, 'common_it': len(common), 'stable_it': len(stable), 'top_modules': top, 'days': {}}
for key in sorted(days):
    meds = {k: median(v) for k, v in days[key].items()}
    row = {'runs': meta[key]['runs'], 'first': meta[key]['first'], 'last': meta[key]['last'],
           'it': len(meds), 'secs_per_it': round(sum(meds.values()) / len(meds), 3),
           'stable_secs_per_it': round(sum(median(days[key][k]) for k in stable) / len(stable), 3),
           'load_avg_mean_median': round(median(meta[key]['load']), 1) if meta[key]['load'] else None,
           'nextest_secs_median': round(median(meta[key]['nextest']), 1) if meta[key]['nextest'] else None,
           'modules': {}}
    for mname in top:
        v = [s for k, s in meds.items() if mod(k) == mname]
        row['modules'][mname] = {'it': len(v), 'secs_per_it': round(sum(v) / len(v), 3) if v else None}
    out['days'][key] = row
base = {k: median(per['before'][k]) for k in common}
split = defaultdict(lambda: defaultdict(list))
out_runs = []
for d, w, t, load, summ, at in runs:
    if w != 'after':
        continue
    r = [s / base[k] for k, s in t.items() if k in base and base[k] > 0]
    mode = 'in_cmux' if at < launchd_at else 'launchd'
    if processtype_at and at >= processtype_at:
        mode = 'launchd_interactive'
    split[mode]['ratio'].append(median(r))
    if load is not None:
        split[mode]['load'].append(load)
    split[mode]['days'].append(d)
    band = next(b for lo, b in ((16, '>=16'), (13, '13-16'), (10, '10-13'), (0, '<10')) if (load or 0) >= lo)
    split[f'{mode} load {band}']['ratio'].append(median(r))
    out_runs.append({'at': at, 'mode': mode, 'ratio': round(median(r), 3), 'load_avg_mean': load})
out['mode_split'] = {'launchd_at': launchd_at}
if processtype_at:
    out['mode_split']['processtype_at'] = processtype_at
for mode, v in sorted(split.items()):
    row = {'runs': len(v['ratio']), 'ratio_median': round(median(v['ratio']), 3), 'ratio_p90': round(p90(v['ratio']), 3)}
    if v['days']:
        row.update(first_day=min(v['days']), last_day=max(v['days']), load_avg_mean_median=round(median(v['load']), 1))
    out['mode_split'][mode] = row
out['runs_after'] = sorted(out_runs, key=lambda x: x['at'])
json.dump(out, sys.stdout, indent=1)
