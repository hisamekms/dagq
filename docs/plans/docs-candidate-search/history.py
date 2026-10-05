#!/usr/bin/env python3
"""Git-only concurrent-change inventory, plus definition tree and prompt identities."""
import argparse
import hashlib
import json
import re
import subprocess
from pathlib import Path
import compute as m


def git(*args):
    return subprocess.run(['git',*args],capture_output=True,text=True,check=True).stdout


def main():
    p=argparse.ArgumentParser()
    p.add_argument('snapshot',type=Path)
    p.add_argument('--out',type=Path,default=m.HERE/'out')
    a=p.parse_args()
    meta=json.loads((a.out/'meta.json').read_text())
    events=m.c.load_jsonl(a.snapshot/'normalized/events.jsonl')
    lands={e['payload'].get('commit'):e for e in events if e['kind']=='run_integrated'}
    import csv
    rows=list(csv.DictReader((a.out/'runs.csv').open()))
    since=min(r['claimed_at'] for r in rows)
    # Include any prompt/review implementation or agent definition, not just filenames containing docs.
    paths=['src/application/prompt.rs','src/application/review.rs','src/application/supervise/jobs.rs','src/domain/review_subagents.rs','src/domain/review_reason.rs','.dagq/review-agents','dagq.toml']
    log=git('log','--first-parent','--format=%H%x09%cI%x09%s','--since='+since,'--until='+meta['cutoff'],'HEAD','--',*paths)
    history=[]
    for line in log.splitlines():
        commit,at,title=line.split('\t',2)
        e=lands.get(commit)
        history.append({'commit':commit,'committed_at':at,'landing_at':e['created_at'] if e else '', 'landing_event':e['id'] if e else '', 'task_id':e['task_id'] if e else '', 'title':title})
    m.write_csv(a.out/'concurrent-changes.csv',history)
    changes=[]
    for line in git('log','--first-parent','--reverse','--format=%H%x09%cI%x09%s','HEAD','--','.dagq/review-agents').splitlines():
        commit,at,title=line.split('\t',2)
        e=lands.get(commit)
        changes.append({'commit':commit,'committed_at':at,'landing_at':e['created_at'] if e else '', 'tree':git('rev-parse',commit+':.dagq/review-agents').strip(),'title':title})
    m.write_csv(a.out/'definition-changes.csv',changes)
    prompt_hashes={}
    for commit, holds in meta['ancestry'].items():
        if commit == 'null' or holds['1429'] is not True or holds['1460'] is not True:
            continue
        text=git('show',commit+':src/application/prompt.rs')
        const=re.search(r'pub const REVIEW_DOCS_CHECK: &str = .*?;\n',text,re.S).group(0)
        prompt_hashes[commit]=hashlib.sha256(const.encode()).hexdigest()
    (a.out/'review-docs-check.json').write_text(json.dumps(prompt_hashes,indent=2)+'\n')

if __name__=='__main__':
    main()
