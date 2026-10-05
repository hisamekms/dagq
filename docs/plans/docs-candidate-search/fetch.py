#!/usr/bin/env python3
"""Reuse acceptance-check fetch unchanged; record all paging calls, including empty pages."""
import importlib.util
import json
import os
import sys
from pathlib import Path

spec = importlib.util.spec_from_file_location('acceptance_fetch', Path(__file__).resolve().parents[1] / 'acceptance-check/fetch.py')
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
original = f.dagq_json
calls = []
def counted(args):
    result = original(args)
    if args[0] == 'events':
        calls.append({'args': args, 'count': len(result['events'])})
    return result
f.dagq_json = counted
f.main()
out = sys.argv[sys.argv.index('--out') + 1]
with open(os.path.join(out, 'pages.json'), 'w') as stream:
    json.dump(calls, stream, indent=2)
