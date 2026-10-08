#!/usr/bin/env python3
"""goal 92 の前後の数字を、記録済みの log・event・repository から数える。

読むだけ: --runs-dir の下の log（integrate-*-verify-*.log・e2e-*.log）、
--events-dir の下の `dagq events --full` / `dagq stats --full` の出力を保存した JSON、
--repo の git の object（`git show` / `git ls-tree`）。queue・runs の下には書かず、
dagq を呼ばない。書くのは --out-dir の CSV だけ。手順は ../cmux-inbox-only.md。

    python3 docs/plans/cmux-inbox-only/measure.py \
        --runs-dir ~/.local/share/dagq/77067154921b9014/runs \
        --events-dir "$TMPDIR/cmux-events" --repo . \
        --out-dir docs/plans/cmux-inbox-only
"""

import argparse
import csv
import glob
import json
import os
import re
import statistics
import subprocess
import sys

# 前の基準: goal 92 の登録（2026-10-02T15:25Z）の直前に着地し、関門が最初の試行で全て通った 3 本。
BEFORE_GATE_RUNS = [
    "4ea3a630-3fbb-43e1-8e03-b97274756187",  # task 1389
    "baeccaaa-dbf7-4d3c-b44b-443060b5207b",  # task 648
    "e6fceb15-7afe-43c8-bf11-e6b8c4fe12d2",  # task 1329
]
GOAL_REGISTERED = "2026-10-02T15:25:00Z"
# task 1436 と task 1443 の着地（run_integrated の created_at）と、この task の run の claim。
LANDED_1436 = "2026-10-03T08:26:24.042Z"
LANDED_1443 = "2026-10-08T10:09:15.224Z"
CLAIMED = "2026-10-08T16:56:50.125Z"
# 前は goal 92 の最初の task（1436）の着地の直前の main、後はこの run の base。
BEFORE_COMMIT = "151a49556925aa7ac19c07b3c5aed97389bd422c"
AFTER_COMMIT = "63e3bfa0"
SAMPLE = 10

RESULT = re.compile(
    r"^\s+(PASS|FAIL|FLKY-FL \d+/\d+|FLAKY \d+/\d+|TRY \d+ (?:PASS|FAIL)|SIGSEGV|SIGABRT|TIMEOUT|LEAK|LEAK-FAIL)"
    r"\s+\[\s*([\d.]+)s\]\s+\([^)]*\)\s+(\S+)\s+(\S+)"
)
SUMMARY = re.compile(r"^\s+Summary \[\s*([\d.]+)s\] (\d+) tests? run: (.*)$")
E2E_LINE = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)")
# goal 92 の description の「対話に固有」のキーワード（画面・ダイアログ・/exit・Enter の送り直し・
# answer_prompt・stuck_exit）を test の本文に当てる。概算の分類で、本文が名指すかだけを見る。
INTERACTIVE = re.compile(
    r"screen|dialog|/exit\b|exits?_sent|answer_prompt|stuck_exit|interactive|resend|\benter\b",
    re.IGNORECASE,
)
TOP_ITEM = re.compile(
    r"^(?:pub(?:\([^)]*\))? )?(?:fn|struct|enum|impl|mod|const|static|type|trait|macro_rules!)\b"
)
FAKE_IMPL = re.compile(r"impl\s+(?:[\w:]+::)?WorkspaceBackend\s+for\s+(\w+)")


def die(message):
    sys.exit(f"measure.py: {message}")


def git(repo, *args):
    return subprocess.run(
        ["git", "-C", repo, *args], check=True, capture_output=True, text=True
    ).stdout


def pages(events_dir, name):
    found = sorted(
        glob.glob(os.path.join(events_dir, f"{name}-page*.json")),
        key=lambda p: int(re.search(r"page(\d+)\.json$", p).group(1)),
    )
    if not found:
        die(f"no pages {name}-page*.json in {events_dir}")
    events = []
    for path in found:
        with open(path) as f:
            events.extend(json.load(f)["events"])
    return len(found), events


def stats_runs(events_dir, name):
    with open(os.path.join(events_dir, name)) as f:
        return {r["run_id"]: r for r in json.load(f)["runs"]}


def describe(values):
    if not values:
        return {"n": 0}
    return {
        "n": len(values),
        "mean": round(statistics.mean(values), 1),
        "median": round(statistics.median(values), 1),
        "min": round(min(values), 1),
        "max": round(max(values), 1),
    }


def read_gate_log(path):
    """1 本の log の nextest の Summary と test ごとの結果の行（Summary より前）を読む。"""
    results, summary, mode = [], None, "llvm-cov"
    with open(path, errors="replace") as f:
        for line in f:
            if line.startswith("landing-it: every IT runs"):
                mode = "landing-it (every IT)"
            elif line.startswith("landing-it: narrowed IT"):
                mode = "landing-it (selected)"
            s = SUMMARY.match(line)
            if s:
                summary = (float(s.group(1)), int(s.group(2)), s.group(3).strip())
                break
            m = RESULT.match(line)
            if m:
                results.append((m.group(1), float(m.group(2)), m.group(3), m.group(4)))
    return summary, results, mode


def first_gate_log(runs_dir, run_id):
    """integrate の最初の試行の log のうち nextest の Summary があるもの。"""
    logs = sorted(
        glob.glob(os.path.join(runs_dir, run_id, "integrate-1-verify-*.log")),
        key=lambda p: int(re.search(r"verify-(\d+)\.log$", p).group(1)),
    )
    for path in logs:
        summary, results, mode = read_gate_log(path)
        if summary:
            if mode == "llvm-cov":
                with open(path, errors="replace") as f:
                    head = f.read(4096)
                if "landing-it:" in head:
                    mode = "landing-it (selected)"
            return path, summary, results, mode
    return None, None, None, None


def module_path(path):
    rel = path[len("tests/it/"):-len(".rs")]
    parts = rel.split("/")
    if parts[-1] == "mod":
        parts = parts[:-1]
    return "::".join(parts)


def chunks(text):
    """ファイルを桁 0 の item ごとに分け、(種類, 名前, 本文) を返す。#[test] は 'test'。"""
    lines = text.split("\n")
    starts = [i for i, l in enumerate(lines) if TOP_ITEM.match(l)]
    out = []
    for n, start in enumerate(starts):
        end = starts[n + 1] if n + 1 < len(starts) else len(lines)
        attrs = start
        while attrs > 0 and lines[attrs - 1].startswith(("#[", "//")):
            attrs -= 1
        is_test = any(l.strip() == "#[test]" for l in lines[attrs:start])
        head = lines[start]
        m = re.match(
            r"^(?:pub(?:\([^)]*\))? )?(fn|struct|enum|impl|mod|const|static|type|trait|macro_rules!)\s*(\w*)",
            head,
        )
        kind, name = m.group(1), m.group(2)
        out.append(("test" if is_test and kind == "fn" else kind, name, "\n".join(lines[start:end])))
    return out


def classify_tests(repo, commit):
    """tests/it の #[test] ごとに、対話のキーワードと偽の cmux（WorkspaceBackend の test 実装）を使うかを返す。

    偽の cmux を使う: 本文が test 実装の型か、それを本文・型に持つ support の fn・struct
    （tests/common と tests/it/*support* の下は全ファイルから、それ以外は同じファイルから見える）を名指す。
    """
    files = [
        p
        for p in git(repo, "ls-tree", "-r", "--name-only", commit, "tests").split()
        if p.endswith(".rs") and (p.startswith("tests/it/") or p.startswith("tests/common/"))
    ]
    texts = {p: git(repo, "show", f"{commit}:{p}") for p in files}
    fakes = sorted({m for t in texts.values() for m in FAKE_IMPL.findall(t)})
    support = lambda p: p.startswith("tests/common/") or "support" in p
    items = {p: chunks(t) for p, t in texts.items()}

    def mentions(body, name, kind):
        body = re.sub(r"(?m)^\s*//.*$", "", body)
        if kind == "fn":
            # メソッド（`.up(`）と別の path の同名（`x::up(`）は数えない。support の module からの path は数える。
            return re.search(rf"(?<![.\w:]){re.escape(name)}\s*(\(|::<)|(?:common|lifecycle|runtime_support)::{re.escape(name)}\s*\(", body)
        return re.search(rf"\b{re.escape(name)}\b", body)

    tainted = {(None, f, "type") for f in fakes}
    changed = True
    while changed:
        changed = False
        for path, chs in items.items():
            visible = [(n, k) for (p, n, k) in tainted if p is None or p == path]
            for kind, name, body in chs:
                if kind not in ("fn", "struct", "enum") or not name:
                    continue
                key = (None if support(path) else path, name, "fn" if kind == "fn" else "type")
                if key in tainted:
                    continue
                own = body.split("\n", 1)[1] if kind == "fn" else body
                if any(mentions(own, n, k) for n, k in visible if n != name):
                    tainted.add(key)
                    changed = True
    tests = {}
    for path, chs in items.items():
        if not path.startswith("tests/it/"):
            continue
        visible = [(n, k) for (p, n, k) in tainted if p is None or p == path]
        for kind, name, body in chs:
            if kind != "test":
                continue
            fake = any(mentions(body, n, k) for n, k in visible)
            direct = any(mentions(body, f, "type") for f in fakes)
            interactive = bool(INTERACTIVE.search(body))
            tests[f"{module_path(path)}::{name}"] = {
                "file": path,
                "fake": fake,
                "direct": direct,
                "interactive": interactive,
                "class": "interactive" if interactive else ("fake_cmux" if fake else "other"),
            }
    return fakes, tests


def e2e_test_counts(repo, commit):
    files = [
        p
        for p in git(repo, "ls-tree", "-r", "--name-only", commit, "tests/e2e.rs", "tests/e2e").split()
        if p.endswith(".rs")
    ]
    out = {}
    for p in files:
        text = git(repo, "show", f"{commit}:{p}")
        out[p] = {
            "test": len(re.findall(r"^\s*#\[test\]", text, re.M)),
            "ignore": len(re.findall(r"^\s*#\[ignore", text, re.M)),
        }
    return out


def e2e_log_path(runs_dir, event):
    """payload の log を --runs-dir の下の <run id>/e2e-<attempt>.log として読む。"""
    return os.path.join(runs_dir, event["run_id"], os.path.basename(event["payload"]["log"]))


def e2e_log_counts(path):
    if not os.path.exists(path):
        return None
    counts = {"ok": 0, "FAILED": 0, "ignored": 0}
    with open(path, errors="replace") as f:
        for line in f:
            m = E2E_LINE.match(line)
            if m:
                counts[m.group(2)] += 1
    return counts


def rel(runs_dir, path):
    return os.path.relpath(path, runs_dir)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--runs-dir", required=True)
    parser.add_argument("--events-dir", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--out-dir", required=True)
    args = parser.parse_args()
    runs_dir = os.path.realpath(os.path.expanduser(args.runs_dir))
    if not os.path.isdir(runs_dir):
        die(f"--runs-dir {runs_dir} is not a readable directory")
    report = {"pages": {}}

    # --- 着地（run_integrated）と stats ---
    integrated = {}
    for name in ("before-integrated", "after-integrated"):
        n, events = pages(args.events_dir, name)
        report["pages"][name] = {"pages": n, "events": len(events)}
        integrated[name] = events
    stats = {**stats_runs(args.events_dir, "before-stats.json"), **stats_runs(args.events_dir, "after-stats.json")}

    classes = {}
    for label, commit in (("before", BEFORE_COMMIT), ("after", AFTER_COMMIT)):
        fakes, tests = classify_tests(args.repo, commit)
        classes[label] = tests
        report[f"it_{label}"] = {
            "commit": commit,
            "fakes": fakes,
            "tests": len(tests),
            "fake_cmux_tests": sum(t["fake"] for t in tests.values()),
            "fake_cmux_direct_tests": sum(t["direct"] for t in tests.values()),
            "fake_cmux_files": len({t["file"] for t in tests.values() if t["fake"]}),
            "interactive_keyword_tests": sum(t["interactive"] for t in tests.values()),
        }
        with open(os.path.join(args.out_dir, f"it-tests-{label}.csv"), "w", newline="") as f:
            w = csv.writer(f)
            w.writerow(["test", "file", "fake_cmux", "fake_cmux_direct", "interactive_keyword", "class"])
            for name in sorted(tests):
                t = tests[name]
                w.writerow([name, t["file"], int(t["fake"]), int(t["direct"]), int(t["interactive"]), t["class"]])

    # --- 関門の log の選び方 ---
    selected, excluded = [], []
    landed = {e["run_id"]: e for e in integrated["before-integrated"] + integrated["after-integrated"]}
    for run_id in BEFORE_GATE_RUNS:
        path, summary, results, mode = first_gate_log(runs_dir, run_id)
        if not summary:
            die(f"before log of {run_id} is missing")
        selected.append(("before", run_id, path, summary, results, mode))
    for e in integrated["after-integrated"]:
        run_id = e["run_id"]
        if len([s for s in selected if s[0] == "after"]) >= SAMPLE:
            excluded.append((run_id, e["task_id"], e["created_at"], "10 件が揃った後"))
            continue
        path, summary, results, mode = first_gate_log(runs_dir, run_id)
        if not summary:
            excluded.append((run_id, e["task_id"], e["created_at"], "nextest の段が無い（関門の test を流さない task）"))
        elif re.search(r" (failed|timed out)\b", summary[2]):
            excluded.append((run_id, e["task_id"], e["created_at"], f"最初の試行で落ちた: {summary[2]}"))
        elif mode != "landing-it (every IT)":
            excluded.append((run_id, e["task_id"], e["created_at"], f"IT を絞った関門（{summary[1]} tests run）"))
        else:
            selected.append(("after", run_id, path, summary, results, mode))

    gate_rows, module_rows = [], {}
    # 結果の行の秒の和は全ての試行の秒で、summary の test_secs_sum も同じ行から求める。
    for period, run_id, path, summary, results, mode in selected:
        st = stats.get(run_id, {})
        tests = classes[period]
        by_class = {"lib・bin・crates": 0.0, "it: interactive": 0.0, "it: fake_cmux": 0.0, "it: other": 0.0, "it: unknown": 0.0, "plugin": 0.0}
        counts = {k: 0 for k in by_class}
        flaky = 0
        for status, secs, binary, name in results:
            # 再試行の test は試行ごとの TRY の行の秒を足し、最後の試行と同じ秒の FLKY の行は数えない。
            if status.startswith(("FLKY", "FLAKY")):
                flaky += 1
                continue
            if binary == "dagq::it":
                t = tests.get(name)
                key = f"it: {t['class']}" if t else "it: unknown"
                module = name.split("::")[0]
                row = module_rows.setdefault((period, module), {"secs": 0.0, "results": 0, "logs": set()})
                row["secs"] += secs
                row["results"] += 1
                row["logs"].add(run_id)
            elif binary == "dagq::plugin":
                key = "plugin"
            else:
                key = "lib・bin・crates"
            by_class[key] += secs
            counts[key] += 1
        gate_rows.append(
            {
                "period": period,
                "run_id": run_id,
                "task_id": (landed.get(run_id) or {}).get("task_id") or st.get("task_id"),
                "landed_at": (landed.get(run_id) or {}).get("created_at") or st.get("landed_at"),
                "log": rel(runs_dir, path),
                "mode": mode,
                "tests_run": summary[1],
                "summary_secs": summary[0],
                "test_secs_sum": round(sum(r[1] for r in results if not r[0].startswith(("FLKY", "FLAKY"))), 1),
                "flaky": flaky,
                "land_phases_verify": (st.get("land_phases") or {}).get("verify"),
                "verify_load_mean": ((st.get("load") or {}).get("verify") or {}).get("mean"),
                **{f"secs[{k}]": round(v, 1) for k, v in by_class.items()},
                **{f"n[{k}]": v for k, v in counts.items()},
            }
        )
    with open(os.path.join(args.out_dir, "gate-logs.csv"), "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(gate_rows[0]))
        w.writeheader()
        w.writerows(gate_rows)
    with open(os.path.join(args.out_dir, "gate-excluded.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["run_id", "task_id", "landed_at", "reason"])
        w.writerows(excluded)
    with open(os.path.join(args.out_dir, "gate-modules.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["period", "module", "logs", "mean_secs_per_log", "mean_results_per_log"])
        for (period, module), row in sorted(module_rows.items()):
            n = len([g for g in gate_rows if g["period"] == period])
            w.writerow([period, module, len(row["logs"]), round(row["secs"] / n, 1), round(row["results"] / n, 1)])

    for period in ("before", "after"):
        rows = [g for g in gate_rows if g["period"] == period]
        report[f"gate_{period}"] = {
            "logs": len(rows),
            "tests_run": describe([g["tests_run"] for g in rows]),
            "test_secs_sum": describe([g["test_secs_sum"] for g in rows]),
            "summary_secs": describe([g["summary_secs"] for g in rows]),
            "land_phases_verify": describe([g["land_phases_verify"] for g in rows if g["land_phases_verify"] is not None]),
            "verify_load_mean": describe([g["verify_load_mean"] for g in rows if g["verify_load_mean"] is not None]),
            "by_class": {
                k[5:-1]: describe([g[k] for g in rows])
                for k in rows[0]
                if k.startswith("secs[")
            },
            "count_by_class": {
                k[2:-1]: describe([g[k] for g in rows])
                for k in rows[0]
                if k.startswith("n[")
            },
        }

    # --- e2e ---
    e2e_rows = []
    windows = {
        "before": (None, GOAL_REGISTERED),
        "after": (LANDED_1436, CLAIMED),
        "after-1443": (LANDED_1443, CLAIMED),
    }
    for name in ("before-e2e", "after-e2e"):
        n, events = pages(args.events_dir, name)
        report["pages"][name] = {"pages": n, "events": len(events)}
        timed = [e for e in events if e["payload"].get("outcome") in ("passed", "failed")]
        if name == "before-e2e":
            sample = timed[-SAMPLE:]
            start = sample[0]["created_at"]
            periods = {"before": (start, GOAL_REGISTERED)}
        else:
            sample = timed[:SAMPLE]
            periods = {"after": (LANDED_1436, CLAIMED), "after-1443": (LANDED_1443, CLAIMED)}
        sample_ids = {e["id"] for e in sample}
        later = [e for e in timed if e["created_at"] >= LANDED_1443][:SAMPLE] if name == "after-e2e" else []
        later_ids = {e["id"] for e in later}
        for period, (start, end) in periods.items():
            in_period = [e for e in events if start <= e["created_at"] < end]
            outcomes = {}
            for e in in_period:
                o = e["payload"].get("outcome")
                outcomes[o] = outcomes.get(o, 0) + 1
            ids = sample_ids if period in ("before", "after") else later_ids
            picked = [e for e in in_period if e["id"] in ids]
            report[f"e2e_{period}"] = {
                "period": [start, end],
                "events_in_period": len(in_period),
                "outcomes": {o: {"count": c, "share": round(c / len(in_period), 3)} for o, c in sorted(outcomes.items())},
                "sample": len(picked),
                "secs": describe([e["payload"]["secs"] for e in picked]),
                "lock_wait_secs": describe([e["payload"].get("lock_wait_secs") or 0 for e in picked]),
            }
            executed = []
            for e in picked:
                c = e2e_log_counts(e2e_log_path(runs_dir, e))
                if c:
                    executed.append(c["ok"] + c["FAILED"])
            report[f"e2e_{period}"]["executed_tests"] = describe(executed)
        for e in events:
            p = e["payload"]
            c = e2e_log_counts(e2e_log_path(runs_dir, e))
            e2e_rows.append(
                [
                    name.split("-")[0],
                    e["created_at"],
                    e["run_id"],
                    p.get("attempt"),
                    p.get("outcome"),
                    p.get("secs"),
                    p.get("lock_wait_secs"),
                    int(e["id"] in sample_ids),
                    int(e["id"] in later_ids),
                    "" if c is None else c["ok"] + c["FAILED"],
                    "" if c is None else c["FAILED"],
                    rel(runs_dir, e2e_log_path(runs_dir, e)),
                ]
            )
    with open(os.path.join(args.out_dir, "e2e-events.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["side", "created_at", "run_id", "attempt", "outcome", "secs", "lock_wait_secs", "sample", "sample_after_1443", "executed", "failed", "log"])
        w.writerows(e2e_rows)
    report["e2e_tests_before"] = e2e_test_counts(args.repo, BEFORE_COMMIT)
    report["e2e_tests_after"] = e2e_test_counts(args.repo, AFTER_COMMIT)
    json.dump(report, sys.stdout, ensure_ascii=False, indent=1)
    print()


if __name__ == "__main__":
    main()
