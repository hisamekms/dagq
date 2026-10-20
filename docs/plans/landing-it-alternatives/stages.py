"""(docs/plans/landing-it-alternatives.md) The stages of each landing verification's test command, read from its log
(verification_command.log_path): the build (the sum of cargo's "Finished ...
in" lines), the nextest run (its Summary), the unit and IT serial sums (the
PASS/FAIL lines' times, by binary id), and the rest (duration - build -
nextest: the coverage report for llvm-cov, the table and list for landing-it).
Usage: python3 stages.py EVENTS_BEFORE.json EVENTS_AFTER.json CHOICE.csv > stages.csv
CHOICE.csv is docs/plans/landing-verify-switch/choice.csv (whether each landing-it run ran every IT)."""
import csv, json, re, sys
before, after, choice = sys.argv[1:4]
full = {r['event_id']: r['replay_full'] for r in csv.DictReader(open(choice))}
T0, T, T1 = '2026-09-30T10:27:40.972Z', '2026-10-07T10:27:40.972Z', '2026-10-14T10:27:40.972Z'
FIN = re.compile(r'^\s*Finished `test` profile .* in (?:(\d+)m )?([\d.]+)s')
SUM = re.compile(r'^\s*Summary \[\s*([\d.]+)s\] (\d+) tests? run')
TST = re.compile(r'^\s*(?:PASS|FAIL|FLAKY|SLOW|TIMEOUT|SIGSEGV|SIGABRT|SIGKILL|LEAK)[^\[]*\[\s*([\d.]+)s\]\s*\(\s*\d+/\d+\)\s+(\S+)\s')
w = csv.writer(sys.stdout)
w.writerow(['window', 'kind', 'event_id', 'at', 'exit_code', 'duration_secs', 'build_secs', 'nextest_secs',
            'rest_secs', 'tests_run', 'unit_tests', 'unit_serial_secs', 'it_tests', 'it_serial_secs', 'load_avg_mean'])
for path, win, lo, hi in ((before, 'before', T0, T), (after, 'after', T, T1)):
    for x in json.load(open(path)):
        p = x['payload']
        if x['kind'] != 'verification_command' or not (lo <= x['created_at'] < hi):
            continue
        cmd = p.get('command') or ''
        if cmd.startswith('cargo llvm-cov nextest'):
            if p['exit_code'] != 0:
                continue
            kind = 'llvm_cov_passed'
        elif cmd == 'sh scripts/landing-it.sh':
            kind = 'landing_it_full' if full.get(str(x['id'])) == '1' else 'landing_it_narrowed'
        else:
            continue
        build = 0.0; nextest = None; n = None; us = it = 0.0; un = itn = 0
        seen = set()
        with open(p['log_path'], errors='replace') as f:
            for line in f:
                m = FIN.match(line)
                if m:
                    build += int(m.group(1) or 0) * 60 + float(m.group(2)); continue
                m = SUM.match(line)
                if m:
                    nextest, n = float(m.group(1)), int(m.group(2)); continue
                m = TST.match(line)
                if m:
                    # a retried test prints a line per try; the name after the
                    # binary id is on the rest of the line
                    secs, binary = float(m.group(1)), m.group(2)
                    if '::' in binary and '::bin/' not in binary:
                        it += secs; itn += 1
                    else:
                        us += secs; un += 1
        d = p['duration_secs']
        rest = d - build - (nextest or 0)
        w.writerow([win, kind, x['id'], x['created_at'], p['exit_code'], f'{d:.1f}', f'{build:.1f}',
                    '' if nextest is None else f'{nextest:.1f}', f'{rest:.1f}', '' if n is None else n,
                    un, f'{us:.1f}', itn, f'{it:.1f}', p.get('load_avg_mean', '')])
