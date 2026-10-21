"""Read every event of each task (its runs' included) with the fixed
binary's read-only `dagq events --full --all --task N`, page by page.

usage: python3 -I fetch.py <tasks.txt> <out_dir>
"""
import json, os, subprocess, sys

ids, out = sys.argv[1], sys.argv[2]
os.makedirs(out, exist_ok=True)
for tid in open(ids).read().split():
    after, rows = 0, []
    while True:
        page = json.loads(subprocess.run(
            ['dagq', 'events', '--full', '--all', '--task', tid, '--limit', '1000',
             '--after', str(after)], check=True, capture_output=True, text=True).stdout)
        rows += page['events']
        if len(page['events']) < 1000:
            break
        after = page['cursor']
    with open(os.path.join(out, tid + '.jsonl'), 'w') as f:
        for e in rows:
            f.write(json.dumps(e, ensure_ascii=False) + '\n')
