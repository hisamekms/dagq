"""The screen of docs/plans/ledger-queue-rows-readers-spike.md.

For each kind of src/domain/event_kind.rs: the reader functions (stats,
kpi, report and the ledger's window and folds; tests left out) that name
it, by its variant (`EventKind::X`), its constant, its string, or a
constant set of kinds that holds it. A set is the constant of the module
the reference names (its path, its `use`, or the file's own), never
another module's constant of the same name; a reference that names no
single one is counted apart (`ambiguous`), not as a read. A name inside
the definition of a set is not a read: the references to the set are.

For each kind of queue row (`subject.kind`): the rows of the intervals
(their records' event kinds named), the landing slot's (made of a run's
events), and the kinds of one event each by the order of
src/domain/ledger/owner.rs (`queue_wide` always; else a run's; the
attribute records of no task; else a session's, `of_session`'s kinds
left out; then a task's, else the queue's: the kinds a goal alone carries
(`task_store::goal_event`'s calls), `EventKind::is_queue`'s of no run and
no task, and every other kind with the route it would take), one row of
the table the plan carries: how it becomes one, the row's form and the function that
makes it, the readers, what they use it for and what read could stand in
for it, whether its writer puts a `supervisor` in its payload (a guess
from the code near the writer: the supervisors' lives read that of any
kind) and the verdict, with what is missing for one not compared.

Run from the repository's root:

    python3 -I docs/plans/ledger-queue-rows-readers-spike/screen.py

It writes screen.tsv (every kind) and queue-kinds.md.txt (the table of the
queue kinds, in Markdown, that the plan's section carries) beside itself.
"""

import glob
import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))

READERS = [
    "src/domain/stats.rs", "src/domain/stats/**/*.rs",
    "src/domain/kpi.rs", "src/domain/kpi/**/*.rs",
    "src/domain/ledger/window.rs", "src/domain/ledger/attributes.rs",
    "src/domain/ledger/attributes/**/*.rs", "src/domain/ledger/findings.rs",
    "src/domain/ledger/findings/**/*.rs", "src/domain/ledger/post_landing.rs",
    "src/domain/ledger/post_landing/**/*.rs", "src/domain/ledger/read.rs",
    "src/domain/supervisor_life.rs", "src/domain/live_alerts.rs",
    "src/domain/live_alerts/**/*.rs", "src/domain/pre_claim.rs",
    "src/domain/pre_claim/**/*.rs", "src/domain/claim_hold.rs",
    "src/application/stats.rs", "src/application/stats/**/*.rs",
    "src/application/kpi.rs", "src/application/report.rs",
    "src/application/report/**/*.rs", "src/domain/forecast.rs",
    "src/domain/forecast/**/*.rs",
]

# What the readers of each file use a kind for, and what read could stand
# in for the queue rows of it: by the reader's file, the first that matches.
USES = [
    ("domain/supervisor_life.rs", "一生の証拠と時刻（`Lives`・`heartbeats`）",
     "範囲より前に始まった一生を読むので全履歴。supervisor ごとの範囲の前の最後の証拠の点の読み（`LedgerStore::last_event_naming_supervisor` の形）は試していない"),
    ("domain/stats/main_log.rs", "main の履歴の fold の値", "全履歴の fold"),
    ("domain/ledger/post_landing.rs", "着地の後の revert・CI の fold の値", "全履歴の fold"),
    ("domain/ledger/attributes.rs", "属性の基準からの fold の値", "基準からの全履歴"),
    ("domain/ledger/findings.rs", "finding の基準からの fold", "基準からの全履歴"),
    ("domain/live_alerts", "今の判断の fold（窓の終わりの状態）", "stream の基準からの全履歴"),
    ("domain/pre_claim", "待ちの区間", "対象外（区間の行の頭を読む）"),
    ("domain/claim_hold.rs", "待ちの区間", "対象外（区間の行の頭を読む）"),
    ("domain/stats/updates.rs", "窓の `(after, upto]` の件数・値", "窓の範囲の kind の読み（窓に比例）"),
    ("domain/kpi/window.rs", "kpi の期間の値（関数ごと）", "関数ごとに未確認（期間の中だけか、期間より前を読むか）"),
    ("domain/kpi", "kpi の値（関数ごと）", "関数ごとに未確認"),
    ("domain/ledger/window.rs", "窓の作り（範囲より前から運ぶ kind の集合など）", "範囲より前を運ぶので全履歴か、決まった読みかは未確認"),
    ("domain/forecast", "予測の入力", "未確認"),
    ("domain/stats", "stats の窓の値（関数ごと）", "関数ごとに未確認（窓の中だけか、窓より前を読むか）"),
    ("domain/marks.rs", "mark の値（kpi・report の marks。`marks_with_earlier` は範囲より前の mark も読む）", "範囲より前の mark を読むので、範囲の読みでは足りない（範囲の前の mark の読みの量は未確認）"),
    ("domain/", "読み手が呼ぶ domain の module の関数（関数ごと）", "関数ごとに未確認"),
    ("application/", "stats・kpi・report の組み立て", "関数ごとに未確認"),
]

# The kinds the spike's test compared, and what it found
# (src/application/ports/observation/queue_rows_spike.rs).
TESTED = {
    "authorization_denied": "holds（比べた。窓の端の点の読みだけで全値が同じ）",
    "telemetry_token_issued": "holds（比べた。窓の端の点の読みだけで全値が同じ）",
    "telemetry_token_revoked": "holds（比べた。窓の端の点の読みだけで全値が同じ）",
    "update_started": "unresolved（比べた。payload の supervisor を持つ一生の証拠。開始済みの supervisor の最後の証拠が範囲の前の update だと、範囲の読みを補っても kpi --since の比べ（前の期間の slot_usage）が変わる。比べの期間を含む範囲の読みでは比べていない）",
    "update_installed": "unresolved（比べた。payload の supervisor を持つ一生の証拠。開始済みの supervisor の最後の証拠が範囲の前の update だと、範囲の読みを補っても kpi --since の比べ（前の期間の slot_usage）が変わる。比べの期間を含む範囲の読みでは比べていない）",
    "main_commits_recorded": "unresolved（比べた。範囲の読みでこの入力の値は同じ。範囲より前の記録が範囲の値に効く入力を試していない）",
    "main_observed": "unresolved（比べた。範囲の読みでこの入力の値は同じ。範囲より前の記録が範囲の値に効く入力を試していない）",
    "queue_service_running": "unresolved（比べた。この入力では除いても値が変わらない。前の入力（s9 のその日の最後の証拠）では窓の端の読みだけで kpi が変わり、範囲の読みで同じだった。範囲の前の最後の証拠になる入力は比べていない）",
    "supervisor_alive": "unresolved（比べた。範囲の読みを補っても kpi --since の比べ（前の期間・7 日の基準）だけが変わる。比べの期間を含む範囲の読みでは比べていない）",
    "supervisor_started": "unresolved（比べた。範囲の読みを補っても kpi --since の比べ（前の期間・7 日の基準）だけが変わる。比べの期間を含む範囲の読みでは比べていない）",
    "candidates_sampled": "unresolved（比べた。窓の端の読みだけでは kpi・report の candidates が変わる。範囲の読みを補うと違うのは kpi --since の比べ（前の期間の candidates の previous 1.0 → null）だけ。比べの期間を含む範囲の読みでは比べていない）",
    "finding": "does_not_hold（比べた。範囲の読みを補っても kpi --since の findings_open が変わる。範囲の前の record からの fold が要る）",
    "finding_recorded": "does_not_hold（行の kind `finding` を比べた。範囲の読みを補っても kpi --since の findings_open が変わる。範囲の前の record からの fold が要る）",
    "finding_updated": "does_not_hold（行の kind `finding` を比べた。範囲の読みを補っても kpi --since の findings_open が変わる。範囲の前の record からの fold が要る）",
    "finding_status_changed": "does_not_hold（行の kind `finding` を比べた。範囲の読みを補っても kpi --since の findings_open が変わる。範囲の前の record からの fold が要る）",
    "finding_baseline": "does_not_hold（行の kind `finding` を比べた。範囲の読みを補っても kpi --since の findings_open が変わる。範囲の前の record からの fold が要る）",
}


def no_comments(text):
    return "\n".join(l if not l.strip().startswith("//") else "" for l in text.split("\n"))


def no_tests(text):
    head = re.split(r"#\[cfg\(test\)\]\nmod tests \{", text)[0]
    return no_comments(head)


def rel(path):
    return path.replace("src/", "", 1)


pairs = re.findall(r'^\s+(\w+) => "([a-z_0-9]+)",', open("src/domain/event_kind.rs").read(), re.M)
by_variant = dict(pairs)
files = sorted(f for f in glob.glob("src/**/*.rs", recursive=True) if "test" not in os.path.basename(f) and "/tests/" not in f)
code = {f: no_tests(open(f).read()) for f in files}

SET_DEF = re.compile(
    r"const (\w+): (?:&\[EventKind\]|\[EventKind; *\w+\]|&\[RecordedKind\]|&\[&str\]|\[&str; *\w+\]) *= *&?\[(.*?)\];",
    re.S)
defs = {}  # (file, name) -> members
for f in files:
    # The whole file: a set may stand after the file's test module.
    for m in SET_DEF.finditer(no_comments(open(f).read())):
        members = set(re.findall(r"EventKind::(\w+)", m.group(2)))
        members |= {v for v, s in pairs if re.search(r"\b%s\b" % s.upper(), m.group(2))}
        defs[(f, m.group(1))] = members
names = {name for _, name in defs}
# What is no read: a `use`, and a constant's definition (a set's or a
# kind's name).
def_spans = {}  # file -> [(start, end)]
for f, text in code.items():
    # A constant's type may hold a `;` (`[EventKind; 8]`): its item ends at
    # the `;` after its value.
    for m in re.finditer(
            r"\buse\s+[^;]*;|\b(?:const|static)\s+\w+\s*:[^=]*=\s*&?\[.*?\];|\b(?:const|static)\s+\w+\s*:[^=]*=[^;]*;",
            text, re.S):
        def_spans.setdefault(f, []).append((m.start(), m.end()))


def module_files(module):
    """The files a module's name stands for."""
    if module == "domain":
        return ["src/domain/mod.rs"]
    if module == "application":
        return ["src/application/mod.rs"]
    return [f for f in files if os.path.splitext(os.path.basename(f))[0] == module
            or f.endswith("/%s/mod.rs" % module)]


def use_module(text, name, pos):
    """The module a `use` of the file imports `name` from, if one does."""
    for m in re.finditer(r"\buse\s+[^;]*;", text, re.S):
        stmt = m.group(0)
        for n in re.finditer(r"\b%s\b" % name, stmt):
            i, depth = n.start(), 0
            while i > 0:
                i -= 1
                c = stmt[i]
                if c == "}":
                    depth += 1
                elif c == "{":
                    if depth == 0:
                        before = re.search(r"(\w+)::\s*$", stmt[:i])
                        return before.group(1) if before else None
                    depth -= 1
            direct = re.search(r"(\w+)::%s\b" % name, stmt)
            if direct:
                return direct.group(1)
    return None


def resolve(f, qual, name, pos):
    """The definition a reference to `name` in `f` stands for, or None."""
    candidates = [file for (file, n) in defs if n == name]
    if qual:
        module = qual.rstrip(":").split("::")[-1]
        if module in ("self",):
            hit = [c for c in candidates if c == f]
        elif module == "super":
            parent = os.path.dirname(f) + ".rs"
            hit = [c for c in candidates if c == parent or c == os.path.dirname(f) + "/mod.rs"]
        else:
            hit = [c for c in candidates if c in module_files(module)]
        return hit[0] if len(hit) == 1 else None
    if f in candidates:
        return f
    module = use_module(code[f], name, pos)
    if module:
        hit = [c for c in candidates if c in module_files(module)]
        if len(hit) == 1:
            return hit[0]
    return candidates[0] if len(candidates) == 1 else None


def in_def(f, pos):
    return any(a <= pos < b for a, b in def_spans.get(f, []))


fn_spans = {}


def spans_of_fns(text):
    """Each `fn` with a body, its name and the span from its name to the
    brace that closes its body (strings and chars left out of the count)."""
    out = []
    for m in re.finditer(r"\bfn (\w+)", text):
        i = m.end()
        # The body's brace, unless a `;` (a declaration) comes first.
        while i < len(text) and text[i] not in "{;":
            i += 1
        if i >= len(text) or text[i] == ";":
            continue
        depth, j, quote = 0, i, None
        while j < len(text):
            c = text[j]
            if quote:
                if c == "\\":
                    j += 2
                    continue
                if c == quote:
                    quote = None
            elif c == '"':
                quote = c
            elif c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        out.append((m.start(), j, m.group(1)))
    return out


def enclosing_fn(text, pos, f=None):
    """The innermost `fn` whose body holds `pos`; `(module)` outside any."""
    key = f or id(text)
    if key not in fn_spans:
        fn_spans[key] = spans_of_fns(text)
    inside = [(a, b, n) for a, b, n in fn_spans[key] if a <= pos <= b]
    return max(inside)[2] if inside else "(module)"


core = sorted({f for p in READERS for f in glob.glob(p, recursive=True)} & set(code))
# One level more: the domain modules the readers name (`crate::domain::x`,
# `domain::{x, ...}`, `super::x` from a reader in src/domain), whose
# functions the readers call (`marks::marks_with_earlier` of `kpi` and
# `report`, for one). Not event_kind (the kinds), ledger (its builder and
# store are no readers; its readers are listed above) or mod.rs.
named = set()
for f in core:
    text = code[f]
    named |= set(re.findall(r"crate::domain::(\w+)", text))
    for group in re.findall(r"(?:crate::)?domain::\{([^;]*?)\}\s*;", text, re.S):
        named |= set(re.findall(r"(?:^|[{,]\s*)(\w+)(?=::|\s*[,}]|\s*$)", group))
    if f.startswith("src/domain/") and f.count("/") == 2:
        named |= set(re.findall(r"\bsuper::(\w+)::", text))
named -= {"event_kind", "ledger", "mod", "self", "super", "crate"}
deps = sorted({g for m in named for g in
               glob.glob("src/domain/%s.rs" % m) + glob.glob("src/domain/%s/**/*.rs" % m, recursive=True)}
              & set(code) - set(core))
readers = sorted(set(core) | set(deps))
reads = {v: set() for v, _ in pairs}  # variant -> {(file, fn, via)}
ambiguous = {}
REF = re.compile(r"((?:\w+::)*)\b([A-Z][A-Z0-9_]*[A-Z0-9])\b")
for f in readers:
    text = code[f]
    for m in REF.finditer(text):
        qual, name = m.group(1), m.group(2)
        if in_def(f, m.start()):
            continue
        fn = enclosing_fn(text, m.start(), f)
        # Outside any function: an item or a macro's table (a string enum
        # of another type of the same text), not a read.
        if fn == "(module)":
            continue
        if name in names:
            target = resolve(f, qual, name, m.start())
            if target is None:
                ambiguous.setdefault(name, set()).add(rel(f))
                continue
            for v in defs[(target, name)]:
                reads[v].add((rel(f), fn, "%s::%s" % (rel(target), name)))
        for v, s in pairs:
            if name == s.upper():
                reads[v].add((rel(f), fn, ""))
    for v, s in pairs:
        for m in re.finditer(r"EventKind::%s\b|\"%s\"" % (v, s), text):
            fn = enclosing_fn(text, m.start(), f)
            if not in_def(f, m.start()) and fn != "(module)":
                reads[v].add((rel(f), fn, ""))

# The kinds that become queue rows.
owner = open("src/domain/ledger/owner.rs").read()
wide_fn = re.search(r"fn queue_wide.*?\n\}", owner, re.S).group(0)
wide = set(re.findall(r"EventKind::(\w+)", wide_fn))
for qual, name in re.findall(r"((?:\w+::)*)([A-Z][A-Z0-9_]+)\)", wide_fn):
    target = resolve("src/domain/ledger/owner.rs", qual, name, 0)
    wide |= defs.get((target, name), set())
attributes = defs[("src/domain/ledger/attributes.rs", "KINDS")] | {"AttributesBaseline"}
kinds_rs = code["src/domain/event_kind.rs"]
is_queue = set(re.findall(r"\| (\w+)", re.search(r"pub const fn is_queue.*?\n    \}", kinds_rs, re.S).group(0)))
first = re.search(r"matches!\(\s*self,\s*(\w+)", re.search(r"pub const fn is_queue.*?\n    \}", kinds_rs, re.S).group(0))
is_queue.add(first.group(1))

waits = {"SlotsFullStarted", "SlotsFullEnded", "ClaimDeferred", "ClaimDeferralEnded"}
waits |= defs[("src/domain/claim_hold.rs", "CLAIM_HOLD_KINDS")] | defs[("src/domain/claim_hold.rs", "LANDING_HOLD_KINDS")]
streams = defs[("src/domain/live_alerts.rs", "RECORD_KINDS")]
findings = defs[("src/domain/ledger/findings.rs", "KINDS")]

# Whether the writer puts a `supervisor` in the payload: the lines near a
# use of the variant outside the readers and the tests, or a call there of
# a function that sets it.
setters = set()
for f, text in code.items():
    for m in re.finditer(r"fn (\w+)\([^)]*\)[^{;]*\{", text):
        body = text[m.end():m.end() + 600]
        if re.search(r'\["supervisor"\]\s*=', body):
            setters.add(m.group(1))
writer_supervisor = {}
for v, _ in pairs:
    found = False
    for f, text in code.items():
        if f in readers or f == "src/domain/event_kind.rs":
            continue
        lines = text.split("\n")
        for i, line in enumerate(lines):
            if re.search(r"EventKind::%s\b" % v, line):
                near = "\n".join(lines[max(0, i - 3):i + 16])
                if '"supervisor"' in near or any(re.search(r"\b%s\(" % s, near) for s in setters):
                    found = True
                    break
        if found:
            break
    writer_supervisor[v] = found


def use_of(rows):
    uses, stands = [], []
    for f, _, _ in sorted(rows):
        for prefix, use, stand in USES:
            if f.startswith(prefix):
                if use not in uses:
                    uses.append(use)
                if stand not in stands:
                    stands.append(stand)
                break
    return uses, stands


with open(os.path.join(HERE, "screen.tsv"), "w") as out:
    for v, s in pairs:
        rows = reads[v]
        state = "read" if rows else "unnamed"
        out.write("\t".join([
            s, state,
            ",".join(sorted({"%s::%s" % (f, fn) for f, fn, _ in rows})),
            ",".join(sorted({via for _, _, via in rows if via})),
        ]) + "\n")

# The session's kinds: `owner::of_session` takes an event of one of them as
# a session's before it asks whether it is the queue's (a run's first).
own = open("src/domain/ledger/owner.rs").read()
session_fn = re.search(r"fn of_session.*?\n\}", own, re.S).group(0)
session_kinds = set(re.findall(r"EventKind::(\w+)", session_fn))

# The kinds written with a goal alone (no run, no task): the calls of
# `task_store::goal_event`/`goal_event_id` (and `sqlite::goal_event`, which
# calls it), each kind named in the call; a call whose kind is a variable
# is listed by its place.
goal_path = {}  # variant -> {file:line}
goal_variable = []
for f, text in code.items():
    for m in re.finditer(r"\bgoal_event(?:_id)?\(", text):
        before = text[max(0, m.start() - 4):m.start()]
        if before.endswith("fn "):
            continue
        depth, j = 0, m.end() - 1
        while j < len(text):
            if text[j] == "(":
                depth += 1
            elif text[j] == ")":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        call = text[m.start():j]
        line = text.count("\n", 0, m.start()) + 1
        named = re.findall(r"EventKind::(\w+)", call)
        if not named:
            goal_variable.append("%s:%d" % (rel(f), line))
        for v in named:
            goal_path.setdefault(v, set()).add("%s:%d" % (rel(f), line))

# The rows' kinds (`subject.kind`) of the intervals, each with the event
# kinds that are its records, and the landing slot's, made of a run's events.
ROW_KINDS = [
    ("slots_full", {"SlotsFullStarted", "SlotsFullEnded"}, "`QueueBook::interval`"),
    ("claim_hold", defs[("src/domain/claim_hold.rs", "CLAIM_HOLD_KINDS")], "`QueueBook::interval`"),
    ("claim_deferral", {"ClaimDeferred", "ClaimDeferralEnded"}, "`QueueBook::interval`"),
    ("landing_hold", defs[("src/domain/claim_hold.rs", "LANDING_HOLD_KINDS")], "`QueueBook::interval`"),
    ("live_alert_stream", streams - {"LiveAlertStarted", "LiveAlertEnded"}, "`QueueBook::stream`"),
    ("live_alert", {"LiveAlertStarted", "LiveAlertEnded"}, "`QueueBook::alert`"),
    ("finding", findings, "`QueueBook::finding`"),
]
interval_records = set().union(*(members for _, members, _ in ROW_KINDS))

rows_route = {}
lines = [
    "| 行の kind（`subject.kind`） | 行になる経路（`owner::owner`） | 区間か単発か・作る関数 | 読む読み手（file::関数） | 使い方 | 代える読みの範囲と量 | 書く所の payload の supervisor（推定） | 判定と不足する読み |",
    "|---|---|---|---|---|---|---|---|",
]
count = {}


def add_row(kind, route, form, fns, uses, stands, sup, verdict):
    head = verdict.split("（")[0]
    count[head] = count.get(head, 0) + 1
    lines.append("| `%s` | %s | %s | %s | %s | %s | %s | %s |" % (
        kind, route, form, "、".join("`%s`" % f for f in fns) or "無し",
        "。".join(uses), "。".join(stands) or "未確認", sup, verdict))


for kind, members, maker in ROW_KINDS:
    rows = set().union(*(reads[v] for v in members))
    uses, stands = use_of(rows)
    add_row(kind, "`queue_wide`（いつも）",
            "区間・%s。records は %s" % (maker, "・".join("`%s`" % by_variant[v] for v in sorted(members))),
            sorted({"%s::%s" % (f, fn) for f, fn, _ in rows}) + ["domain/ledger/queue.rs::QueueBook::from_heads"],
            uses or ["区間の行の頭（時刻・detail）"], stands, "—",
            TESTED.get(kind, "対象外（区間。行の頭を読む読み手がある）"))
add_row("landing_slot", "run の event から（`QueueBook::attempt`、`stats::landing_utilization::Attempts`）",
        "区間（`integrate` の試み 1 つ）・`QueueBook::attempt`・`Throughs`。records は持たず、その run の行の records",
        ["domain/stats/landing_utilization.rs::spans", "domain/ledger/queue.rs::QueueBook::from_heads"],
        ["着地の枠の使用の区間（stats・kpi の `landing_utilization`）"], ["対象外（区間）"], "—",
        "対象外（区間。行の頭と through を `QueueBook::from_heads` が読む）")

for v, s in pairs:
    if v in interval_records:
        continue
    goal = goal_path.get(v)
    by_goal = ("goal だけを持つ（`task_store::goal_event` の経路: %s）とき" % "、".join(sorted(goal))) if goal else ""
    if v in wide:
        route = "`queue_wide`（いつも）"
    elif v in session_kinds:
        continue  # a session's row whenever it has no run
    elif v in attributes:
        route = "属性の record（task を持たないとき）" + ("。" + by_goal if goal else "")
    elif goal:
        route = by_goal + "。run・task を持てばその行、payload に session の id（`planner_id`・`plan_review_id`・`goal_review_id`）があれば session の行"
    elif v in is_queue:
        route = "`is_queue` の kind で run・task を持たず、session の id（`planner_id`・`plan_review_id`・`goal_review_id`）も持たないとき"
    else:
        route = "run・task・session の id を持たずに書かれたときだけ（その書く経路があるかは screen で確かめていない。`owner::owner` の最後の枝）"
    rows_route[v] = route
    form = "単発・`QueueBook::add` の `!taken` の枝"
    rows = reads[v]
    fns = sorted({"%s::%s" % (f, fn) for f, fn, _ in rows})
    uses, stands = use_of(rows)
    sup = writer_supervisor[v]
    if not rows:
        uses = ["値を読む読み手は screen に無い。窓の端（最初・最新・cursor の event、時刻から読む窓の下限）にだけ効く"]
        stands = ["窓の端の点の読み（窓 1 つにつき数件。履歴に比例しない）"]
    if sup:
        uses.append("payload の `supervisor` で一生の証拠（`supervisor_life::Lives::add`）")
    if s in TESTED:
        verdict = TESTED[s]
    else:
        missing = ["形 B の比べ（kind を除き、窓の端の点の読みと読み手の範囲の kind の読みを補う）を通していない"]
        if sup:
            missing.append("一生の証拠として、範囲の前の最後の証拠が範囲と kpi の比べの期間の一生を決める入力の比べが要る")
        if not rows and not sup:
            missing.append("payload を読む読み手が screen の外に無いことを確かめていない")
        if rows:
            missing.append("読み手の関数が範囲より前（kpi の比べの期間を含む）の event を読むかを関数ごとに確かめる要がある")
        verdict = "unresolved（" + "。".join(missing) + "）"
    add_row(s, route, form, fns, uses, stands, "有り" if sup else "無し", verdict)

with open(os.path.join(HERE, "queue-kinds.md.txt"), "w") as out:
    out.write("\n".join(lines) + "\n")
print("kinds", len(pairs), "read", sum(1 for v, _ in pairs if reads[v]),
      "unnamed", sum(1 for v, _ in pairs if not reads[v]),
      "queue row kinds", len(lines) - 2, count,
      "goal path", len(goal_path), "goal calls with a variable kind", goal_variable,
      "routes", {k: sum(1 for x in rows_route.values() if x.startswith(k)) for k in ["`queue_wide`", "属性", "goal だけ", "`is_queue`", "run・task・session の id を持たず"]},
      "session kinds left out", sorted(by_variant[v] for v in session_kinds),
      "reader files", len(core), "+ dependencies", len(deps), [rel(d) for d in deps],
      "ambiguous refs", {k: sorted(v) for k, v in ambiguous.items()})
