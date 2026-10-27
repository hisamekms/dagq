"""(docs/plans/landing-it-after-t2892-spike.md) Why a narrowed landing did not
run a newly red IT, from the table that landing used and its diff.

Usage: python3 classify.py DATA REPO TABLES OUT

DATA is collect.sh's directory (events, landings), OUT/new-failures.csv is
measure.py's and gets the column it_reason, TABLES holds <run id>/it-coverage-map.json
of each table a landing-it log names ("landing-it: table .../<run id>/..."); a
table not there yet is downloaded with gh run download (the artifact is kept 30 days).
For each landing of a row that narrowed and did not run the test, over the
files of git diff --name-only --no-renames main_before..commit of its
run_integrated (the diff landing-it.sh reads, DAGQ_LANDING_BASE..HEAD) and
scripts/landing-it.py's BROAD_SHARE and is_source():
  broad(<files>)        the table maps the test to diff files, each a broad entry
  not_in_tree(<files>)  no diff file maps the test, and source files of the
                        diff are not in the tree of the table's commit (the
                        table cannot say whether they reach it)
  not_reached           no diff file maps the test and every source file is
                        in the tree: the table says the diff does not reach it
  selected(<files>)     a non-broad entry maps it (the choice should have run it)
  not_in_table          the table has no such test (landing-it runs those)
  unavailable(<why>)    the log, the table or the diff cannot be read"""
import csv, importlib.util, json, os, re, subprocess, sys

data, repo, tables, out = sys.argv[1:5]
spec = importlib.util.spec_from_file_location('landing_it', os.path.join(repo, 'scripts', 'landing-it.py'))
li = importlib.util.module_from_spec(spec)
spec.loader.exec_module(li)
CMD = 'sh scripts/landing-it.sh'
TABLE = re.compile(r'^landing-it: table .*/(\d+)/it-coverage-map\.json')

events = json.load(open(os.path.join(data, 'events_before.json'))) + json.load(open(os.path.join(data, 'events_after.json')))
landings = {str(e['task_id']): e for e in json.load(open(os.path.join(data, 'landings.json')))}
loaded, trees = {}, {}


def git(*args):
    r = subprocess.run(['git', '-C', repo, *args], capture_output=True, text=True)
    return r.stdout.split('\n') if r.returncode == 0 else None


def classify(task, test):
    l = landings.get(task)
    if l is None:
        return 'unavailable(no landing)'
    xs = [x for x in events if x['run_id'] == l['run_id'] and x['kind'] == 'verification_command'
          and x['payload'].get('command') == CMD and x['created_at'] <= l['created_at']]
    path = xs[-1]['payload'].get('log_path') if xs else None
    if not path or not os.path.exists(path):
        return 'unavailable(log)'
    ids = [m.group(1) for m in (TABLE.match(s) for s in open(path, errors='replace')) if m]
    if not ids:
        return 'unavailable(no table line)'
    tid = ids[-1]
    if tid not in loaded:
        p = os.path.join(tables, tid, 'it-coverage-map.json')
        if not os.path.exists(p):
            subprocess.run(['gh', 'run', 'download', tid, '-R', 'hisamekms/dagq', '-n', 'it-coverage-map',
                            '-D', os.path.join(tables, tid)], capture_output=True)
        loaded[tid] = json.load(open(p)) if os.path.exists(p) else None
    table = loaded[tid]
    if table is None:
        return 'unavailable(table %s)' % tid
    if table['commit'] not in trees:
        t = git('ls-tree', '-r', '--name-only', table['commit'])
        trees[table['commit']] = set(x for x in t if x) if t is not None else None
    tree = trees[table['commit']]
    diff = git('diff', '--name-only', '--no-renames', l['payload']['main_before'], l['payload']['commit'])
    if diff is None or tree is None:
        return 'unavailable(git)'
    diff = [x for x in diff if x]
    tests, files = table.get('tests', {}), table.get('files', {})
    if test not in tests:
        return 'not_in_table'
    limit = li.BROAD_SHARE * len(tests)
    mapped = [f for f in diff if test in files.get(f, [])]
    narrow = [f for f in mapped if len(files[f]) <= limit]
    if narrow:
        return 'selected(%s)' % '+'.join(narrow)
    if mapped:
        return 'broad(%s)' % '+'.join(mapped)
    outside = [f for f in diff if li.is_source(f) and f not in files and f not in tree
               and not li.is_unit_test_module(f)]
    if outside:
        return 'not_in_tree(%s)' % '+'.join(outside)
    return 'not_reached'


path = os.path.join(out, 'new-failures.csv')
rows = list(csv.DictReader(open(path)))
for r in rows:
    reasons = []
    for p in r['per_landing'].split():
        task, status = p.split(':', 1)
        if status.startswith('narrowed_not_run'):
            reasons.append('%s:%s' % (task, classify(task, r['test'])))
    r['it_reason'] = ' '.join(reasons)
with open(path, 'w', newline='') as f:
    w = csv.DictWriter(f, fieldnames=[k for k in rows[0] if k != 'it_reason'] + ['it_reason'])
    w.writeheader(); w.writerows(rows)
for r in rows:
    if r['it_reason']:
        print(r['job'], r['test'].split('::', 2)[2][:60], r['it_reason'])
