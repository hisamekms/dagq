#!/usr/bin/env python3
"""Count the before and after of goal 100's structure split
(docs/plans/architecture-boundaries.md).

Usage (from anywhere in the repository):

  python3 scripts/architecture-boundaries.py structure <commit>
      (A) The structure of <commit>'s src: one "metric<TAB>value" line per
      metric, then the QueueOpener candidates outside src/application/ports
      as CSV rows (file:line, counted or the reason it is not, signature or
      field, name, type).
  python3 scripts/architecture-boundaries.py ci-runs <base> <runs.json> <landing>...
      (B) Pick the CI runs: <runs.json> is `gh run list --workflow ci.yml
      --branch main --event push --json databaseId,headSha,createdAt,
      conclusion,status`. Prints "interval,run_id,head,created_at,
      conclusion" for the
      runs before <base> (head an ancestor of <base> containing a722911f)
      and after (head containing every <landing>), newest first before and
      oldest first after.
  python3 scripts/architecture-boundaries.py ci-log <run_id> <log>
      (B) One run: reads `gh run view <run_id> --log` saved in <log> and
      prints, for each module group, the CSV row "run_id,group,tests,
      passed,seconds,excluded" (excluded names the non-PASS statuses that
      take the group out of the sample), then a row "run_id,#summary,
      tests,passed,seconds,runner_image".
  python3 scripts/architecture-boundaries.py ci-table <ci-modules.csv>
      (B) The table of the document from the kept CSV.
  python3 scripts/architecture-boundaries.py churn <since> <until> <set>
      (C) "<files> <src> <ratio>": the unique commits that changed the
      files of <set> (before or after) and src in [since, until).

Each count is the rule written under its table in the document; the
rules live there and the code follows them.
"""

import os
from statistics import median
import re
import subprocess
import sys
import tempfile


def git(*args):
    return subprocess.run(
        ["git", *args], check=True, capture_output=True
    ).stdout


def archive(commit, into):
    data = git("archive", commit, "src")
    subprocess.run(["tar", "-x", "-C", into], input=data, check=True)
    return os.path.join(into, "src")


def rust_files(root):
    for dirpath, _, files in os.walk(root):
        for name in sorted(files):
            if name.endswith(".rs"):
                yield os.path.join(dirpath, name)


def lines_of(path):
    with open(path, encoding="utf-8") as f:
        return f.read().split("\n")[:-1]


def wc(path):
    with open(path, "rb") as f:
        return f.read().count(b"\n")


# --- #[cfg(test)] ---------------------------------------------------------


def test_files(src):
    """Files that are whole #[cfg(test)] modules (`#[cfg(test)] mod x;`)."""
    out = set()
    for path in rust_files(src):
        lines = lines_of(path)
        for i, line in enumerate(lines):
            m = re.match(r"\s*(pub(\([^)]*\))?\s+)?mod\s+(\w+)\s*;", line)
            if not m or i == 0 or "#[cfg(test)]" not in lines[i - 1]:
                continue
            here = os.path.dirname(path)
            base = os.path.basename(path)
            if base not in ("mod.rs", "lib.rs", "main.rs"):
                here = os.path.join(here, base[:-3])
            for cand in (
                os.path.join(here, m.group(3) + ".rs"),
                os.path.join(here, m.group(3), "mod.rs"),
            ):
                out.add(os.path.normpath(cand))
    return out


def test_lines(path, lines, whole):
    """Line indexes inside an inline `#[cfg(test)] mod x {` at column 0
    (rustfmt's layout: it ends at the next `}` at column 0)."""
    def under(w):
        top = os.path.dirname(w) if os.path.basename(w) == "mod.rs" else w[:-3]
        return os.path.normpath(path).startswith(top + os.sep)

    if os.path.normpath(path) in whole or any(under(w) for w in whole):
        return set(range(len(lines)))
    out = set()
    i = 0
    while i < len(lines):
        if lines[i].strip() == "#[cfg(test)]" and i + 1 < len(lines):
            j = i + 1
            while j < len(lines) and lines[j].startswith("#["):
                j += 1
            if j < len(lines) and re.match(r"(pub(\([^)]*\))?\s+)?mod\s+\w+\s*\{", lines[j]):
                k = j + 1
                while k < len(lines) and lines[k] != "}":
                    k += 1
                out.update(range(i, k + 1))
                i = k
        i += 1
    return out


# --- declarations ----------------------------------------------------------

FN = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?((const|async|unsafe)\s+)*fn\s+(\w+)")
STRUCT = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?(struct|enum)\s+(\w+)")


def signature_end(lines, start):
    for k in range(start, len(lines)):
        s = lines[k].rstrip()
        if s.endswith("{") or s.endswith(";"):
            return k
    return len(lines) - 1


def fn_signatures(lines):
    """(start, end, name, text) of each fn signature."""
    out = []
    for i, line in enumerate(lines):
        m = FN.match(line)
        if m and not line.lstrip().startswith("//"):
            end = signature_end(lines, i)
            out.append((i, end, m.group(5), " ".join(l.strip() for l in lines[i : end + 1])))
    return out


def enclosing(lines, i):
    """What line i declares: ("signature", fn name, text, the fn's line),
    ("field", "Struct.field", type, i), or ("other", "", line, _)."""
    for k in range(i, -1, -1):
        line = lines[k]
        m = FN.match(line)
        if m:
            end = signature_end(lines, k)
            if i <= end:
                return "signature", m.group(5), " ".join(l.strip() for l in lines[k : end + 1]), k
            return "other", m.group(5), lines[i].strip(), k
        m = STRUCT.match(line)
        if m:
            name = m.group(4)
            if k == i:
                t = re.match(r".*?\((.*)\)\s*;", line)
                if t:  # a tuple struct: struct X(T);
                    return "field", name + ".0", t.group(1).strip(), i
            close = k
            while close < len(lines) and not re.match(r"^\s*\}", lines[close]):
                close += 1
            if i > close:
                return "other", name, lines[i].strip(), k
            f = re.match(r"\s*(pub(\([^)]*\))?\s+)?(\w+)\s*:\s*(.*)", lines[i])
            if not f:
                return "other", name, lines[i].strip(), k
            text = f.group(4)
            j = i
            while not text.rstrip().endswith(",") and j + 1 < close:
                j += 1
                text += " " + lines[j].strip()
            return "field", name + "." + f.group(3), text.rstrip(",").strip(), i
        if re.match(r"^(impl|trait|mod)\b", line):
            return "other", "", lines[i].strip(), k
    return "other", "", lines[i].strip(), 0


def opener_args(text):
    """The type argument of each QueueOpener in text ("" for none)."""
    out = []
    for m in re.finditer(r"\bQueueOpener\b", text):
        rest = text[m.end() :]
        if not rest.startswith("<"):
            out.append("")
            continue
        depth = 0
        for n, ch in enumerate(rest):
            depth += {"<": 1, ">": -1}.get(ch, 0)
            if depth == 0:
                out.append(rest[1:n].strip())
                break
    return out


def whole_queue(arg):
    a = re.sub(r"\s+", " ", arg)
    a = re.sub(r"^dyn ", "", a)
    a = re.sub(r"\s*\+\s*(Send|Sync|'\w+)", "", a).strip()
    a = re.sub(r"^(crate::|super::)*(application::)?", "", a)
    return a in ("", "Queue")


def opener_candidates(src):
    """Every line of `grep -rn QueueOpener src` outside
    src/application/ports, classified."""
    whole = test_files(src)
    rows = []
    ports = os.path.join(src, "application", "ports")
    for path in rust_files(src):
        if path == ports + ".rs" or path.startswith(ports + os.sep):
            continue
        lines = lines_of(path)
        tests = None
        rel = os.path.relpath(path, os.path.dirname(src))
        in_use = False
        for i, line in enumerate(lines):
            # Inside a `use ...{` that goes on over lines.
            was_use = in_use or re.match(r"\s*(pub(\([^)]*\))?\s+)?use\b", line)
            in_use = bool(was_use) and not line.rstrip().endswith(";")
            if "QueueOpener" not in line:
                continue
            if tests is None:
                tests = test_lines(path, lines, whole)
            s = line.strip()
            where = f"{rel}:{i + 1}"
            if i in tests:
                rows.append((where, "excluded:test", "", "", s, where))
            elif s.startswith("//"):
                rows.append((where, "excluded:comment", "", "", s, where))
            elif was_use:
                rows.append((where, "excluded:use", "", "", s, where))
            elif re.match(r"impl\b.*\bQueueOpener\b.*\bfor\b", s):
                rows.append((where, "excluded:impl", "", "", s, where))
            else:
                kind, name, text, at = enclosing(lines, i)
                decl = f"{rel}:{at + 1}"
                args = opener_args(text)
                if kind == "other":
                    result = "excluded:body"
                elif any(whole_queue(a) for a in args):
                    result = "counted"
                elif any(re.fullmatch(r"[A-Z]", a) for a in args):
                    result = "excluded:generic"
                else:
                    result = "excluded:narrow"
                rows.append((where, result, kind, name, text, decl))
    return rows


# --- (A) -------------------------------------------------------------------


def trait_methods(src, trait, seen=None):
    """(own fns, fns with the supertraits') of `pub trait <trait>`."""
    seen = seen if seen is not None else set()
    seen.add(trait)
    for path in rust_files(os.path.join(src, "application")):
        lines = lines_of(path)
        for i, line in enumerate(lines):
            m = re.match(r"pub trait " + trait + r"\b(\s*:\s*([^{]*))?", line)
            if not m:
                continue
            own = 0
            if not line.rstrip().endswith("{}"):
                for l in lines[i + 1 :]:
                    if l == "}":
                        break
                    if re.match(r"    fn ", l):
                        own += 1
            total = own
            for sup in re.findall(r"\b([A-Z]\w*)\b", m.group(2) or ""):
                if sup not in seen and sup not in ("Send", "Sync"):
                    total += trait_methods(src, sup, seen)[1]
            return own, total
    return 0, 0


def module_lines(src, rels):
    sizes = []
    for rel in rels:
        p = os.path.join(src, rel)
        if os.path.isfile(p):
            sizes.append((rel, wc(p)))
        elif os.path.isdir(p):
            for f in rust_files(p):
                sizes.append((os.path.relpath(f, src), wc(f)))
    return sum(s for _, s in sizes), max((s for _, s in sizes), default=0), len(sizes)


def structure(commit):
    with tempfile.TemporaryDirectory() as tmp:
        src = archive(commit, tmp)
        out = []
        sup = os.path.join(src, "application", "supervise", "mod.rs")
        out.append(("supervise/mod.rs lines", wc(sup)))
        for label, rels in (
            ("ports", ["application/ports.rs", "application/ports"]),
            ("compose", ["compose.rs", "compose"]),
            ("queue_reads", ["application/queue_reads.rs", "application/queue_reads"]),
        ):
            total, most, n = module_lines(src, rels)
            out.append((f"{label} lines total", total))
            out.append((f"{label} lines max", most))
            out.append((f"{label} files", n))
        for rel in (
            "application/supervise/inbox_nudge.rs",
            "infrastructure/runtime_store/run_log.rs",
            "application/recording.rs",
            "application/lifecycle.rs",
        ):
            out.append((f"{rel} lines", wc(os.path.join(src, rel))))
        own, total = trait_methods(src, "RunLog")
        out.append(("RunLog own methods", own))
        out.append(("RunLog methods with supertraits", total))
        sv = os.path.join(src, "application", "supervise")
        impl = [
            p
            for p in rust_files(sv)
            if os.path.basename(p) != "mod.rs" or os.path.dirname(p) != sv
            if any(re.match(r"^impl(<[^>]*>)? Supervisor\b", l) for l in lines_of(p))
        ]
        out.append(("impl Supervisor submodules", len(impl)))
        n, inside = 0, False
        for l in lines_of(sup):
            if re.match(r"^struct Supervisor<.a> \{", l):
                inside = True
                continue
            if inside and l.startswith("}"):
                break
            if inside and re.match(r"^    [a-z_][a-z_0-9]*:", l):
                n += 1
        out.append(("struct Supervisor fields", n))
        whole = test_files(src)
        dyn_q = re.compile(r"(Box<\s*dyn\s+Queue|&\s*(mut\s+)?dyn\s+Queue|impl\s+Queue)(?![\w])")
        all_sigs = prod_sigs = 0
        ports = os.path.join(src, "application", "ports")
        for path in rust_files(src):
            if path == ports + ".rs" or path.startswith(ports + os.sep):
                continue
            lines = lines_of(path)
            tests = test_lines(path, lines, whole)
            for start, _, _, text in fn_signatures(lines):
                if dyn_q.search(text):
                    all_sigs += 1
                    prod_sigs += start not in tests
        out.append(("dyn Queue signatures outside ports", all_sigs))
        out.append(("dyn Queue signatures outside ports, not test", prod_sigs))
        rows = opener_candidates(src)
        counted = [r for r in rows if r[1] == "counted"]
        # One per fn and one per field: the key is the file and the name.
        decls = {(r[5], r[2]) for r in counted}
        out.append(("whole QueueOpener signatures", sum(k == "signature" for _, k in decls)))
        out.append(("whole QueueOpener fields", sum(k == "field" for _, k in decls)))
        app = os.path.join(src, "application")
        inst = inst_prod = 0
        for path in rust_files(app):
            lines = lines_of(path)
            tests = test_lines(path, lines, whole)
            for i, l in enumerate(lines):
                c = l.count("Instant::now")
                inst += c
                inst_prod += c if i not in tests else 0
        out.append(("application Instant::now", inst))
        out.append(("application Instant::now, not test", inst_prod))
        key = re.compile(r'payload\s*\[\s*"|payload\.get\(\s*"')
        for label, rels in (
            ("supervise", ["application/supervise"]),
            ("domain/run", ["domain/run.rs", "domain/run"]),
        ):
            c = 0
            for rel in rels:
                p = os.path.join(src, rel)
                files = [p] if os.path.isfile(p) else list(rust_files(p)) if os.path.isdir(p) else []
                for f in files:
                    c += sum(len(key.findall(l)) for l in lines_of(f))
            out.append((f"{label} payload string keys", c))
        for k, v in out:
            print(f"{k}\t{v}")
        print("file:line,result,kind,name,type,declared_at")
        for r in rows:
            print(",".join('"' + str(x).replace('"', '""') + '"' for x in r))


# --- (B) -------------------------------------------------------------------

# Module groups (docs/plans/architecture-boundaries.md, (B)): the IT
# modules (tests/it/<name>.rs) the description and the receipts name, and the
# unit test paths before the split. A test counts in every group whose path
# it starts with, so a test that moved into a split module keeps its group.
IT_GROUPS = [
    "cli_planner_request", "cli_read", "draft_bundles", "draft_revisit",
    "finding_planner", "goal_review", "goal_review_codex", "inbox_nudge",
    "lifecycle_down", "lifecycle_handoff_stop", "lifecycle_install",
    "lifecycle_install_watch", "lifecycle_replace", "observer_codex",
    "plan_review", "plan_review_codex", "plan_review_concern",
    "plan_review_expected_files", "plan_review_launch", "plan_review_turns",
    "planner_close", "planner_headless", "planner_headless_stats",
    "planner_headless_turns", "planner_slots", "queue_service",
    "queue_service_reads", "recovery_codex", "request_planner",
    "review_subagents", "runtime_adopt", "runtime_ask", "runtime_candidates",
    "runtime_ci_watch", "runtime_claim", "runtime_claim_defer",
    "runtime_claim_hold", "runtime_claim_spacing", "runtime_cleanup",
    "runtime_codex", "runtime_codex_ask", "runtime_codex_work", "runtime_disk",
    "runtime_e2e", "runtime_forecast", "runtime_handoff",
    "runtime_headless_jobs", "runtime_headless_reopen",
    "runtime_headless_stall", "runtime_heartbeat", "runtime_host_metrics",
    "runtime_integrate", "runtime_landing_answers", "runtime_light_slots",
    "runtime_observer", "runtime_provider_switch", "runtime_push",
    "runtime_queue_hold", "runtime_queue_hold_detect",
    "runtime_queue_hold_shared", "runtime_recheck", "runtime_release",
    "runtime_report", "runtime_resume", "runtime_review",
    "runtime_review_questions", "runtime_sccache", "runtime_session",
    "runtime_slot_limits", "runtime_stall", "runtime_stall_end",
    "runtime_stall_recovery", "runtime_sweep", "runtime_throughput_review",
    "runtime_triage", "runtime_waiting", "runtime_waiting_stages",
    "supervisor_without_cmux", "throughput_review_codex",
]
LIB_GROUPS = [
    "application::supervise",
    "application::supervise::inbox_nudge",
    "application::ports",
    "compose",
    "application::queue_reads",
    "application::recording",
    "application::lifecycle",
    "infrastructure::runtime_store::run_log",
    "application::install",
    "application::update",
    "application::health",
    "application::ci_watch",
    "application::kpi",
    "application::review",
    "application::integrate",
    "infrastructure::clock",
    "domain::run::payload",
    "domain::run_phase",
    "infrastructure::runtime_store",
]

JOB = "fmt / test / clippy / coverage\t"
ANSI = re.compile(r"\x1b\[[0-9;]*m")
# A nextest result line. The time field is empty for a test that did not
# run (`SKIP [         ]`), and the progress field may be missing, so both
# are optional: a SKIP still names its test and takes its group out.
LINE = re.compile(
    r"^\s*(TRY \d+ )?(PASS|FAIL|FLAKY|SKIP|TIMEOUT|CANCEL|SIGSEGV|SIGABRT|SIGBUS|SIGILL|SIGTERM|SIGKILL"
    r"|ABORT|LEAK|LEAK-FAIL|FLKY-FL)"
    r"\s+\[\s*(?:([0-9.]+)s)?\s*\]\s+(?:\([^)]*\)\s+)?(\S+)\s+(\S+)"
)


def groups_of(binary, name):
    if binary == "dagq::it":
        return ["it::" + g for g in IT_GROUPS if name.startswith(g + "::")]
    if binary == "dagq":
        return [g for g in LIB_GROUPS if name.startswith(g + "::")]
    return []


def ci_log(run_id, log):
    """The macOS job's lines of `gh run view --log` (any other job's lines
    are skipped). A test counts when its only line is a plain PASS; a TRY
    line or any other status puts its group out of the run's sample."""
    image = version = ""
    groups = {}
    summary = None
    with open(log, encoding="utf-8", errors="replace") as f:
        for raw in f:
            if not raw.startswith(JOB):
                continue
            # job, step, then "<timestamp> <text>"
            text = ANSI.sub("", raw.rstrip("\n").split("\t", 2)[2])
            text = text.split(" ", 1)[1] if " " in text else ""
            if not image:
                m = re.match(r"Image: (\S+)", text)
                if m:
                    image = m.group(1)
                    continue
            elif not version:
                m = re.match(r"Version: (\S+)", text)
                if m:
                    version = m.group(1)
                    continue
            m = re.search(r"Summary \[\s*([0-9.]+)s\]\s+(\d+) tests? run: (\d+) passed", text)
            if m:
                summary = (float(m.group(1)), int(m.group(2)), int(m.group(3)))
                continue
            m = LINE.match(text)
            if not m:
                continue
            tried, status, secs, binary, name = m.groups()
            for g in groups_of(binary, name):
                row = groups.setdefault(g, {"tests": set(), "passed": 0, "seconds": 0.0, "bad": set()})
                row["tests"].add(name)
                if status == "PASS" and not tried and secs is not None:
                    row["passed"] += 1
                    row["seconds"] += float(secs)
                elif tried:
                    row["bad"].add("FLAKY" if status == "PASS" else "TRY-" + status)
                else:
                    row["bad"].add(status)
    image = f"{image}/{version}" if version else image
    for g, row in sorted(groups.items()):
        bad = "|".join(sorted(row["bad"]))
        print(f"{run_id},{g},{len(row['tests'])},{row['passed']},{row['seconds']:.3f},{bad}")
    if summary:
        print(f"{run_id},#summary,{summary[1]},{summary[2]},{summary[0]:.3f},{image}")
    else:
        print(f"{run_id},#summary,0,0,,{image}")


def ci_runs(base, runs_json, landings):
    import json

    def anc(a, b):
        return subprocess.run(["git", "merge-base", "--is-ancestor", a, b], capture_output=True).returncode == 0

    runs = json.load(open(runs_json))
    pre, post = [], []
    for r in runs:
        h = r["headSha"]
        if subprocess.run(["git", "cat-file", "-e", h + "^{commit}"], capture_output=True).returncode:
            continue
        if anc(h, base) and anc("a722911f", h):
            pre.append(r)
        elif all(anc(l, h) for l in landings):
            post.append(r)
    pre.sort(key=lambda r: r["createdAt"], reverse=True)
    post.sort(key=lambda r: r["createdAt"])
    for name, rs in (("before", pre), ("after", post)):
        for r in rs:
            print(f"{name},{r['databaseId']},{r['headSha']},{r['createdAt']},{r['conclusion']}")


def ci_table(samples):
    """The (B) table from the CSV the doc keeps (ci-modules.csv): per group
    and interval, the runs used and taken out, the test count range, the
    median of the PASS seconds, the Summary median, the dates and images,
    then the difference and the reduction (1 - after / before)."""
    import csv

    rows = list(csv.DictReader(open(samples)))
    runs = {}
    for r in rows:
        if r["group"] == "#summary":
            runs.setdefault(r["interval"], []).append(r)
    head = {}
    for iv, rs in runs.items():
        at = sorted(r["created_at"] for r in rs)
        imgs = sorted({r["excluded_or_image"] for r in rs})
        head[iv] = (len(rs), f"{at[0]}〜{at[-1]}", median(float(r["seconds"]) for r in rs), ", ".join(imgs))
    groups = sorted(set(LIB_GROUPS) | {"it::" + g for g in IT_GROUPS})
    print("| module | 区間 | run の本数（使った / 外した / test の無い） | test の数 | test 時間の合計の中央値（秒） | Summary の壁時計の中央値（秒） | 期間 | runner の image | 差（秒） | 短縮率 |")
    print("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
    for g in groups:
        med = {}
        for iv in ("before", "after"):
            n, period, summ, img = head[iv]
            mine = [r for r in rows if r["interval"] == iv and r["group"] == g]
            used = [r for r in mine if not r["excluded_or_image"]]
            out = len(mine) - len(used)
            counts = sorted(int(r["tests"]) for r in mine)
            span = "-" if not counts else str(counts[0]) if counts[0] == counts[-1] else f"{counts[0]}〜{counts[-1]}"
            if used:
                med[iv] = median(float(r["seconds"]) for r in used)
                value = f"{med[iv]:.3f}"
            else:
                value = "欠測"
            cells = [g, iv, f"{len(used)} / {out} / {n - len(mine)}", span, value, f"{summ:.1f}", period, img]
            if iv == "after":
                if len(med) == 2:
                    d = med["after"] - med["before"]
                    rate = "-" if med["before"] == 0 else f"{1 - med['after'] / med['before']:.1%}"
                    cells += [f"{d:+.3f}", rate]
                else:
                    cells += ["-", "-"]
            else:
                cells += ["", ""]
            print("| " + " | ".join(cells) + " |")


# --- (C) -------------------------------------------------------------------

# The files of (C)'s ratio: the four files before the split and the files
# task 2212 and 2232 changed (the same paths in both periods), then for the
# after period the modules the split created (ports/, compose/,
# queue_reads/, supervise/contexts.rs, stages.rs, handoff_slots.rs and
# port_fakes.rs).
BEFORE = [
    "src/application/ports.rs",
    "src/compose.rs",
    "src/application/supervise/mod.rs",
    "src/application/queue_reads.rs",
    "src/application/ports/execution.rs",
    "src/application/ports/host.rs",
    "src/application/supervise/inbox_nudge.rs",
    "src/infrastructure/runtime_store/run_log.rs",
    "src/application/recording.rs",
    "src/application/lifecycle.rs",
]
AFTER = BEFORE + [
    "src/application/ports",
    "src/compose",
    "src/application/queue_reads",
    "src/application/supervise/contexts.rs",
    "src/application/supervise/stages.rs",
    "src/application/supervise/handoff_slots.rs",
    "src/application/port_fakes.rs",
]


def churn(since, until, which):
    paths = BEFORE if which == "before" else AFTER

    def count(*p):
        out = git("log", "main", f"--since={since}", f"--until={until}", "--format=%H", "--", *p)
        return len(set(out.split()))

    files, src = count(*paths), count("src")
    print(f"{files} {src} {files / src if src else 0:.3f}")


def main(argv):
    if len(argv) >= 2 and argv[0] == "structure":
        structure(argv[1])
    elif len(argv) >= 3 and argv[0] == "ci-runs":
        ci_runs(argv[1], argv[2], argv[3:])
    elif len(argv) == 3 and argv[0] == "ci-log":
        ci_log(argv[1], argv[2])
    elif len(argv) == 2 and argv[0] == "ci-table":
        ci_table(argv[1])
    elif len(argv) == 4 and argv[0] == "churn":
        churn(argv[1], argv[2], argv[3])
    else:
        print(__doc__, file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
