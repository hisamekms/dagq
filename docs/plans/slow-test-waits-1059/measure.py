"""Read-only log measurement for slow-test-waits.md chapter 8; Python stdlib."""
import csv
import re
import statistics as st
import subprocess
from collections import Counter, defaultdict
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo

TZ = ZoneInfo('Asia/Tokyo')
ROOT = Path.home() / '.local/share/dagq/77067154921b9014/runs'
CUTOFF = datetime.fromisoformat('2026-09-30T14:22:41+09:00').timestamp()

def commit_time(ref):
    return int(subprocess.check_output(['git', 'show', '-s', '--format=%ct', ref]))

CUTS = {k: commit_time(v) for k, v in {
    'threads6': '40f693f', 'F2': '621f651', 'F1': '752e5b5',
    'F4': '3a3d961', '1060': '61fa487', 'F3': 'f8baf2c',
    'F5': '202bc6e', 'G2': '0889e120',
}.items()}
pat = re.compile(r'^\s*(PASS|FLAKY \d+/\d+)\s*\[\s*([0-9.]+)s\]\s*(?:\(\s*\d+/\d+\)\s*)?(\S+)\s+(\S+)', re.M)
summary = re.compile(r'Summary\s*\[\s*([0-9.]+)s\]')
metrics = []
with (Path.home() / '.local/share/dagq-hostmetrics/metrics.csv').open() as f:
    for r in csv.DictReader(f):
        metrics.append((datetime.fromisoformat(r['ts']).replace(tzinfo=TZ).timestamp(), float(r['load1'])))
rows = []
for path in ROOT.glob('*/integrate-*-verify-*.log'):
    end = path.stat().st_mtime
    if not CUTS['F2'] - 86400 * 2 < end <= CUTOFF:
        continue
    text = re.sub(r'\x1b\[[0-9;]*m', '', path.read_text(errors='replace'))
    sums = summary.findall(text)
    if not sums:
        continue
    assert len(sums) == 1, path
    stage = float(sums[0])
    records = pat.findall(text)
    tests = {(binary, test): float(sec) for _, sec, binary, test in records}
    summary_line = next(line for line in text.splitlines() if summary.search(line))
    passed = int(re.search(r'(\d+) passed', summary_line)[1])
    failed_match = re.search(r'(\d+) failed', summary_line)
    failed = int(failed_match[1]) if failed_match else 0
    assert len(records) == len(tests) == passed, path
    assert not end-stage < CUTS['threads6'] < end, path
    loads = [load for ts, load in metrics if end-stage <= ts <= end]
    assert loads, path
    rows.append(dict(path=str(path.relative_to(ROOT)), end=end, stage=stage,
                     threads=6 if end-stage >= CUTS['threads6'] else 8,
                     total=sum(tests.values()), tests=tests,
                     load=st.mean(loads), peak=max(loads), samples=len(loads),
                     flaky=len(re.findall(r'^\s*FLAKY ', text, re.M)),
                     failed=failed))
rows.sort(key=lambda r:r['end'])
before = [r for r in rows if r['end'] < CUTS['F2']][-32:]
after_all = [r for r in rows if r['end'] > CUTS['F3']]
after = after_all[:32]
groups = {'before': before, 'after': after,
          'F2-F1': [r for r in rows if CUTS['F2'] < r['end'] < CUTS['F1']],
          'F1-F4': [r for r in rows if CUTS['F1'] < r['end'] < CUTS['F4']],
          'F4-1060': [r for r in rows if CUTS['F4'] < r['end'] < CUTS['1060']],
          '1060-F3': [r for r in rows if CUTS['1060'] < r['end'] < CUTS['F3']],
          'all_fixes': [r for r in after_all if r['end'] > CUTS['F5']],
          'after_pre_G2': [r for r in after if r['end'] < CUTS['G2']]}

def medtests(rs):
    per = defaultdict(list)
    for r in rs:
        for key, sec in r['tests'].items():
            per[key].append(sec)
    return {k: st.median(v) for k, v in per.items()}

def stats(rs):
    if not rs:
        return {'n': 0}
    return dict(n=len(rs), period=[datetime.fromtimestamp(rs[i]['end'], TZ).isoformat() for i in (0,-1)],
                threads=dict(Counter(r['threads'] for r in rs)),
                f5=[sum(r['end'] < CUTS['F5'] for r in rs),sum(r['end'] > CUTS['F5'] for r in rs)],
                total=st.median(r['total'] for r in rs), stage=st.median(r['stage'] for r in rs),
                normalized=st.median(r['total']/6 for r in rs),
                stage6=st.median(r['stage']*r['threads']/6 for r in rs),
                stage_range=[min(r['stage'] for r in rs),max(r['stage'] for r in rs)],
                per_sum=sum(medtests(rs).values()), test_count=len(medtests(rs)),
                load=st.median(r['load'] for r in rs), load_range=[min(r['load'] for r in rs),max(r['load'] for r in rs)],
                peak=st.median(r['peak'] for r in rs), peak_range=[min(r['peak'] for r in rs),max(r['peak'] for r in rs)])

if __name__ == '__main__':
    import json
    for name, rs in groups.items():
        print(name, json.dumps(stats(rs)))
    for name, rs in [('before6', [r for r in before if r['threads']==6]), ('before8', [r for r in before if r['threads']==8])]:
        print(name, json.dumps(stats(rs)))
    for name, rs in [('before_load',before), ('after_load',after)]:
        print(name, json.dumps(stats([r for r in rs if 10 <= r['load'] < 20])))
    for name, rs in [('before_matched6', before), ('after_matched6', after)]:
        print(name, json.dumps(stats([r for r in rs if 10 <= r['load'] < 20 and r['threads'] == 6])))
    for name, rs in [('before_passed_only', before), ('after_passed_only', after)]:
        print(name, json.dumps(stats([r for r in rs if r['failed'] == 0])))
    print('before_without_F2_gate', json.dumps(stats(before[:-1])))
    b, a = medtests(before), medtests(after)
    common = b.keys() & a.keys()
    print('common', len(common), sum(b[k] for k in common), sum(a[k] for k in common))
    for prefix in ['plan_review', 'lifecycle_', 'runtime_claim', 'queue_', 'runtime_stall', 'runtime_repair', 'runtime_waiting_stages', 'runtime_resume', 'runtime_review', 'runtime_session', 'runtime_adopt', 'runtime_open_turn', 'runtime_handoff', 'cli_version', 'lifecycle_install', 'cli_']:
        keys = [k for k in common if k[0]=='dagq::it' and k[1].split('::')[0].startswith(prefix)]
        print(prefix,len(keys),round(sum(b[k] for k in keys),3),round(sum(a[k] for k in keys),3))
    chosen = sorted({r['path']:r for rs in groups.values() for r in rs}.values(), key=lambda r:r['end'])
    with Path(__file__).with_name('logs.csv').open('w') as f:
        w=csv.writer(f, lineterminator='\n')
        w.writerow(['log','mtime_jst','threads_inferred','tests','pass_flaky_seconds','summary_seconds','load1_mean','load1_max','load_samples','flaky','fail'])
        for r in chosen:
            w.writerow([r['path'],datetime.fromtimestamp(r['end'], TZ).isoformat(),r['threads'],len(r['tests']),round(r['total'],3),r['stage'],round(r['load'],6),r['peak'],r['samples'],r['flaky'],r['failed']])
