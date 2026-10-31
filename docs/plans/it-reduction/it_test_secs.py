"""The time of each test of tests/it on the production landing verification (it-reduction.md, "2026-11 の分類"); Python stdlib.

Reads tests/it of the worktree (the cwd's git work tree) for the tests and the landing
verification logs under --runs-dir for their times. It does not open the queue, does not
call `dagq` and writes nothing under --runs-dir; it writes --out (a TSV with the columns
module, test, secs, secs_n, secs_window) and prints the log counts as JSON lines.

  python3 -I docs/plans/it-reduction/it_test_secs.py \
    --runs-dir ~/.local/share/dagq/77067154921b9014/runs \
    --until 2026-11-01T12:00:00+09:00 \
    --out docs/plans/it-reduction/it-test-secs-2026-11.tsv

A row is one line of tests/it that `grep -E '^[[:space:]]*#\\[test\\]'` matches, so the rows
are as many as that grep counts (a #[cfg] pair of one name is two rows). module is the
file's module path (tests/it/runtime_support/cleanup.rs is runtime_support::cleanup),
test the mods inside it and the fn, so that `dagq::it <module>::<test>` is nextest's name.

A log is an integrate-*-verify-*.log whose first line is `# dagq: [landing_verification]`
and whose first 4 KiB have a `landing-it: ` line (the logs measure_goal237.py reads). after
is a log whose mtime is from the landing of 3770 (AFTER_FROM) to --until, before one from
--before-since to the landing of 3759 (BEFORE_UNTIL, not included). The value of a test in
one log is the seconds of its last `PASS [ N s]` or `FAIL [ N s]` line naming
`dagq::it <module>::<test>` (of a retried test, the line with the highest `TRY n`);
TIMEOUT and SIGKILL lines are left out (their time is the limit, not the test's). secs is
the median over the after logs that have the test (secs_window after), or else over the
before logs (before); secs_n is the number of logs used. A test in neither is empty.
"""
import argparse
import bisect
import json
import re
import statistics as st
import subprocess
from collections import defaultdict
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo

TZ = ZoneInfo('Asia/Tokyo')
# The landing of 3770, the last of goal 237 (c77abb98): after starts here.
AFTER_FROM = '2026-10-31T20:05:30+09:00'
# The landing of 3759, the first of goal 237 (bd10d148): before ends here.
BEFORE_UNTIL = '2026-10-31T14:02:20+09:00'
BEFORE_SINCE = '2026-10-27T00:00:00+09:00'

ANSI = re.compile(r'\x1b\[[0-9;]*[A-Za-z]')
RESULT = re.compile(r'^\s*(?:TRY (\d+) )?(?:PASS|FAIL) \[\s*([0-9.]+)s\]\s*(?:\([^)]*\)\s*)?dagq::it (\S+)\s*$')
TEST_LINE = re.compile(r'^[ \t]*#\[test\]')

# The tokenizer and the mod walk of scripts/check-it-added-tests.sh (names as nextest gives them).
TOKEN = re.compile(r'''
  (?P<lc>//[^\n]*)
| (?P<bc>/\*)
| (?P<raw>(?<![A-Za-z0-9_])(?:br|cr|r)(?P<h>\#*)")
| (?P<str>(?<![A-Za-z0-9_])[bc]?"(?:[^"\\]|\\.)*")
| (?P<chr>'(?:[^'\\\n]|\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f]{1,6}\}|.))')
| (?P<p>[{};()\[\]])
''', re.X | re.S)
BLOCK = re.compile(r"/\*|\*/")
MOD = re.compile(r"(?<![A-Za-z0-9_])mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*$")
TEST_ATTR = re.compile(r"#\s*\[\s*test\s*\]")
FN = re.compile(r"(?<![A-Za-z0-9_])fn\s+([A-Za-z_][A-Za-z0-9_]*)")
PATH_MOD = re.compile(r'#\[path\s*=\s*"([^"]+)"\s*\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;')


def fns_of(text):
    """[(line of the fn, [mods around it], fn name)] of the #[test] fns of a file's text."""
    nl = [m.start() for m in re.finditer("\n", text)]
    out, stack, buf, nest, pos, seg, n = [], [], [], 0, 0, 0, len(text)
    while pos < n:
        m = TOKEN.search(text, pos)
        if not m:
            break
        buf.append(text[pos:m.start()])
        kind, end = m.lastgroup, m.end()
        if kind == "bc":
            depth, k = 1, end
            while depth:
                b = BLOCK.search(text, k)
                if not b:
                    k = n
                    break
                depth += 1 if b.group() == "/*" else -1
                k = b.end()
            end = k
        elif kind == "raw":
            close = '"' + m.group("h")
            k = text.find(close, end)
            end = n if k < 0 else k + len(close)
        if kind != "p":
            buf.append(" " * (end - m.start()))
            pos = end
            continue
        c, pos = m.group(), end
        if c in "([":
            nest += 1
            buf.append(c)
            continue
        if c in ")]":
            nest = max(0, nest - 1)
            buf.append(c)
            continue
        if c == ";" and nest:
            buf.append(c)
            continue
        s = "".join(buf)
        if c == "{":
            mm = MOD.search(s)
            fm = FN.search(s) if TEST_ATTR.search(s) else None
            if mm:
                stack.append(("m", mm.group(1)))
            elif fm:
                line = bisect.bisect_left(nl, seg + fm.start()) + 1
                out.append((line, [f[1] for f in stack if f[0] == "m"], fm.group(1)))
                stack.append(("o",))
            else:
                stack.append(("o",))
        elif c == "}" and stack:
            stack.pop()
        buf, nest, seg = [], 0, end
    return out


def module_of(path, mods):
    if path in mods:
        return mods[path]
    p = path[len("tests/it/"):][:-len(".rs")]
    if p.endswith("/mod"):
        p = p[:-len("/mod")]
    return "" if p in ("main", "mod") else p.replace("/", "::")


def path_mods(main_text):
    res = {}
    for p, name in PATH_MOD.findall(main_text):
        parts = []
        for x in p.split("/"):
            if x in ("", "."):
                continue
            if x == "..":
                if not parts:
                    parts = None
                    break
                parts.pop()
                continue
            parts.append(x)
        if parts:
            res["tests/it/" + "/".join(parts)] = name
    return res


def tests_of_tree():
    """[(module, test, path, line of #[test])], one per line the grep counts, in path and line order."""
    files = subprocess.check_output(['git', 'ls-files', '--cached', '--others', '--exclude-standard', 'tests/it'],
                                    text=True).split()
    files = sorted(p for p in files if p.endswith('.rs') and Path(p).is_file())
    main = Path('tests/it/main.rs')
    mods = path_mods(main.read_text() if main.is_file() else '')
    rows = []
    for p in files:
        text = Path(p).read_text(errors='replace')
        fns = fns_of(text)
        fn_lines = [f[0] for f in fns]
        mod = module_of(p, mods)
        for i, line in enumerate(text.split('\n'), 1):
            if not TEST_LINE.match(line):
                continue
            k = bisect.bisect_left(fn_lines, i)
            if k == len(fns):
                raise SystemExit('%s:%d: no #[test] fn after the attribute' % (p, i))
            _, inner, name = fns[k]
            full = '::'.join(([mod] if mod else []) + inner + [name])
            first, rest = full.split('::', 1) if not mod else (mod, '::'.join(inner + [name]))
            rows.append((first, rest, p, i))
    return rows


def read_log(path):
    """{full name: seconds of its last attempt} of one log."""
    best = {}
    with open(path, errors='replace') as f:
        for raw in f:
            m = RESULT.match(ANSI.sub('', raw))
            if not m:
                continue
            tr, secs, name = int(m.group(1) or 0), float(m.group(2)), m.group(3)
            if name not in best or tr >= best[name][0]:
                best[name] = (tr, secs)
    return {k: v[1] for k, v in best.items()}


def select_logs(runs_dir, since, until):
    res = []
    for p in Path(runs_dir).expanduser().glob('*/integrate-*-verify-*.log'):
        mt = p.stat().st_mtime
        if not since <= mt < until:
            continue
        with open(p, errors='replace') as f:
            head = f.read(4096)
        if head.startswith('# dagq: [landing_verification]') and 'landing-it: ' in head:
            res.append((mt, p))
    return [p for _, p in sorted(res)]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--runs-dir', required=True)
    ap.add_argument('--until', required=True, help='the end of the after interval (when the logs were read)')
    ap.add_argument('--before-since', default=BEFORE_SINCE)
    ap.add_argument('--out', required=True)
    a = ap.parse_args()
    ts = lambda s: datetime.fromisoformat(s).timestamp()
    windows = {
        'after': select_logs(a.runs_dir, ts(AFTER_FROM), ts(a.until)),
        'before': select_logs(a.runs_dir, ts(a.before_since), ts(BEFORE_UNTIL)),
    }
    values = {}
    for w, logs in windows.items():
        per = defaultdict(list)
        for p in logs:
            for name, s in read_log(p).items():
                per[name].append(s)
        values[w] = per
        mt = [datetime.fromtimestamp(p.stat().st_mtime, TZ).isoformat(timespec='seconds') for p in logs]
        print(json.dumps({'window': w, 'logs': len(logs), 'mtime': [mt[0], mt[-1]] if mt else None,
                          'tests_seen': len(per)}))
    rows = tests_of_tree()
    counts = defaultdict(int)
    with open(a.out, 'w') as f:
        f.write('module\ttest\tsecs\tsecs_n\tsecs_window\n')
        for mod, test, _, _ in rows:
            full = mod + '::' + test
            secs, n, w = '', 0, ''
            for name in ('after', 'before'):
                xs = values[name].get(full)
                if xs:
                    secs, n, w = '%.3f' % st.median(xs), len(xs), name
                    break
            counts[w or 'none'] += 1
            f.write('%s\t%s\t%s\t%s\t%s\n' % (mod, test, secs, n if n else '', w))
    print(json.dumps({'rows': len(rows), 'by_window': dict(counts)}))


if __name__ == '__main__':
    main()
