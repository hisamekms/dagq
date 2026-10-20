"""Replay scripts/landing-it.py's choice (full or narrowed, and why) on each
`sh scripts/landing-it.sh` of the after window, from the git diff of the
attempt (integration_started.main .. integration_rebased.head_after of the
same run before the command) and the nightly table that was the newest one
finished before the command. The reasons landing-it.sh printed are at the
head of the verification log, outside output_tail, so they are rebuilt here.
Usage: python3 replay_choice.py EVENTS.json ITMAP_RUNS.json TABLES_DIR REPO > choice.csv
TABLES_DIR holds t<run id>/it-coverage-map.json (gh run download <id> -n it-coverage-map)."""
import csv, datetime as D, importlib.util, json, re, subprocess, sys

events, runs_path, tables_dir, repo = sys.argv[1:5]
spec = importlib.util.spec_from_file_location('landing_it', f'{repo}/scripts/landing-it.py')
li = importlib.util.module_from_spec(spec); spec.loader.exec_module(li)
T1 = '2026-10-14T10:27:40.972Z'

def git(*a):
    return subprocess.run(['git', '-C', repo, *a], capture_output=True, text=True, check=True).stdout

def ts(s):
    return D.datetime.fromisoformat(s.replace('Z', '+00:00'))

itmap = sorted((r for r in json.load(open(runs_path)) if r['conclusion'] == 'success'), key=lambda r: r['updatedAt'])
cache = {}
def table_at(at):
    done = [r for r in itmap if ts(r['updatedAt']) < at]
    r = done[-1]
    if r['databaseId'] not in cache:
        t = json.load(open(f"{tables_dir}/t{r['databaseId']}/it-coverage-map.json"))
        tree = git('ls-tree', '-r', '--name-only', t['commit']).split('\n')
        cache[r['databaseId']] = (t, tree)
    return r['databaseId'], cache[r['databaseId']]

ev = json.load(open(events))
start, rebased = {}, {}
w = csv.writer(sys.stdout)
w.writerow(['event_id', 'at', 'run_id', 'duration_secs', 'exit_code', 'tests_run', 'skipped', 'ran_full',
            'replay_full', 'reason_kinds', 'table_run', 'estimate_secs', 'limit_secs', 'diff_files'])
for x in ev:
    k, p, run = x['kind'], x['payload'], x.get('run_id')
    if k == 'integration_started':
        start[run] = p['main']; rebased.pop(run, None)
    elif k == 'integration_rebased':
        rebased[run] = p['head_after']
    elif k == 'verification_command' and p.get('command') == 'sh scripts/landing-it.sh' and x['created_at'] < T1:
        at = ts(x['created_at'])
        base, head = start.get(run), rebased.get(run)
        m = re.search(r'Summary \[\s*[\d.]+s\] (\d+) tests? run: \d+ passed(?:.*?(\d+) skipped)?', p.get('output_tail') or '')
        tests_run = int(m.group(1)) if m else ''
        skipped = int(m.group(2) or 0) if m else ''
        ran_full = '' if not m else int(skipped < 200)
        if not base or not head:
            w.writerow([x['id'], x['created_at'], run, p['duration_secs'], p['exit_code'], tests_run, skipped, ran_full, '', 'no_attempt_heads', '', '', '', '']); continue
        diff = [f for f in git('diff', '--name-only', f'{base}..{head}').split('\n') if f]
        tid, (table, tree) = table_at(at)
        reasons = li.full_reasons(diff, tree, table, None, at)
        est = lim = ''
        if not reasons:
            files, tt = table.get('files', {}), table.get('tests', {})
            sel = set()
            for path in diff:
                sel.update(k2 for k2 in files.get(path, []) if k2 in tt)
            lim = li.FULL_SHARE * li.wall([t.get('duration_secs') or 0.0 for t in tt.values()], 6)
            est = li.wall([tt[k2].get('duration_secs') or 0.0 for k2 in sel], 6)
            if est > lim:
                reasons.append('threshold')
        kinds = sorted({'common_file' if r.startswith('a common file') else
                        'source_not_in_tree' if r.startswith('a source file') else
                        'table_old' if 'older than' in r else r for r in reasons})
        w.writerow([x['id'], x['created_at'], run, p['duration_secs'], p['exit_code'], tests_run, skipped, ran_full,
                    int(bool(reasons)), '+'.join(kinds), tid, f'{est:.1f}' if est != '' else '',
                    f'{lim:.1f}' if lim != '' else '', len(diff)])
