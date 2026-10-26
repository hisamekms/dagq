#!/usr/bin/env python3
"""Baseline of the plans that checked a premise before their implementation
(docs/plans/spike-plan-baseline.md).

  python3 -I measure.py --cache DIR [--precedent P1 ...] [--json OUT]
  python3 -I measure.py --cache DIR --population SINCE UNTIL

Reads only `dagq events` (through fetch.py, which pages and caches the
answers under DIR); never opens the queue DB and changes nothing. Run it in
the repository (any worktree) so that `dagq` resolves the production queue.
A cache left from an earlier run is read again as it is: delete DIR to read
the queue anew.

A precedent is a goal G and the tasks S that checked the premise (the Spike
in the words of ADR-t1487-1, before the runtime knew the class). From G's
events (and the events of S's tasks outside G) it derives:

  I   the planned implementation: the tasks of G not in S, first submitted
      at or after the first submit of S (60 s of slack: the old plans
      submitted S and I in one proposal), that completed, and that a planner
      registered (no origin) or that a follow-up of a task of S registered
      (the replanning of the old plans went through the Spike's follow-ups).
  F   the other tasks of G first created in the span: the follow-ups of
      other tasks and the goal review's gaps.
  (a) plan span: the first submit of S to the last completion of I. Goal
      span: the same start to the goal's close (open: to now). Beside each,
      the span less the people's waits: the union of the intervals from
      each ask_opened of a task of the span's set (S and I; S, I and F) to
      its first ask_answered or ask_closed (an ask left open ends at the
      span's end), clipped to the span. Also: the first completion of S,
      the replanning gap (the shortest, over the I tasks submitted after a
      task of S completed, from the latest such completion to the submit; 0
      when every I task was submitted before) and how many I tasks were
      submitted together with S.
  (b) rework candidates over S, I and F: plan review revise verdicts (one
      per plan review, with all its reason codes), run review revise /
      concern verdicts with all their reason codes, failed receipts,
      worker_question asks with their topics, and tasks canceled after they
      were submitted. A candidate is a premise error when one of its codes
      is in PREMISE_CODES. Otherwise it needs a reading when its primary
      code (a run review's `primary_code`; for the others every code) is in
      READ_CODES (or it has none: a failed receipt, a cancel, an old review
      or ask without a code), and then takes the verdict of JUDGED; any
      other code names another cause.
  (c) claim waits of the first claim of each task of S (class spike) and I
      (class implementation): from when the task became claimable to the
      claim. A claim that carries `ready_at` uses it; an older one reads it
      like the runtime does (the latest of the last move to ready before the
      claim, each predecessor's completion and each goal dependency's close).
  (d) the runtime planner sessions of G or of the tasks of S and I opened in
      the plan span: count, open hours, active hours and open less active
      (the time a planner held its place without working).

`--population SINCE UNTIL` prints instead, for the window, the claim waits by
class from the `ready_at` the claims carry (as `dagq stats` counts them) and
the first claim in the window that carries one.
"""

import argparse
import datetime as dt
import json
import os
import statistics
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fetch  # noqa: E402

# key: (goal, S, why it was chosen). The rule of the choice is the plan's
# section 2.
PRECEDENTS = {
    "P1": (51, [553], "spike: の名の task。結果を読んで人の planner が goal 51 を作った"),
    "P2": (57, [812, 864], "使い捨ての repository で非対話の worker の未測定の点を確かめ、ADR と実装が依存した"),
    "P3": (73, [1061], "Codex の goal review job が読み取りだけの sandbox で動くかを確かめ、ADR と実装が依存した"),
    "P4": (65, [961, 968, 969], "spike で効果の見込みを測り、進めるかを決める（受け入れ条件が明示）"),
    "P5": (77, [1180, 3336, 3337, 3338, 3339], "Codex の対話の session の前提を測り、ADR が依存した"),
    "P6": (38, [2170, 2171, 2280], "container の runtime の前提の確認（Spike）の手順と結果を記録し、実装の前提にした"),
}

MARK = "2026-10-13T16:53:31.259Z"  # supervisor_config_changed 184490, mark 184541

# The reason codes and ask topics that name a premise found false, and the
# codes behind which a false premise may stand (they need a reading). Every
# other code names another cause. See the plan's section 1 (b).
PREMISE_CODES = {"wrong_premise", "acceptance_infeasible", "precondition_missing"}
READ_CODES = {None, "acceptance_unmet"}

# The readings of the candidates that need one: (kind, id) -> (premise
# error? (None: the record does not tell), the reading). The id is the plan
# review's, the review_finished event's, the validation_finished event's,
# the ask's or the canceled task's.
SANDBOX = "Claude の停止で Codex の worker が代行した run で、sandbox が ps・e2e を拒んだ（計画の後の環境の変化で、計画の前提の誤りではない）"
JUDGED = {
    ("review_revise", 14327): (False, "report の件数の食い違い"),
    ("receipt_failed", 51005): (False, SANDBOX),
    ("canceled_after_submit", 944): (False, "後の人の方針（goal 92 で対話の経路を廃止）で要らなくなった"),
    ("plan_review_revise", 284): (False, "accepted の決定との食い違い（plan review が着手の前に止めた）"),
    ("worker_question", 155): (None, "問いの本文も topic も記録に無い"),
    ("review_concern", 31619): (False, "実装の欠け（受け入れ条件の経路の失敗）"),
    ("review_revise", 31464): (False, "コードの配置の小さな漏れ"),
    ("review_concern", 49162): (True, "計画が置いた前提（nux の書き込み）が実際と違い、受け入れ条件 (2a) から外れた"),
    ("review_revise", 48955): (False, "文言の残り"),
    ("receipt_failed", 49538): (False, SANDBOX),
    ("receipt_failed", 49449): (False, SANDBOX),
    ("review_revise", 60199): (False, "人の決定を文書に反映していない"),
    ("receipt_failed", 43476): (True, "計画が置いた前提（固定バイナリが先行の task を含む）が claim の時点で成り立たなかった"),
    ("receipt_failed", 49940): (None, "必須の e2e が通らなかった理由が summary に無い"),
    ("receipt_failed", 50037): (False, SANDBOX),
    ("receipt_failed", 50119): (False, SANDBOX),
    ("canceled_after_submit", 1124): (False, "重複（duplicate_of 1128）"),
    ("review_concern", 60605): (False, "ディスクの逼迫で 2 周目を打ち切った（host の一時的な状態）"),
    ("review_revise", 61127): (False, "表の欠け"),
    ("review_revise", 61314): (False, "分析の誤り"),
    ("review_revise", 61361): (False, "分析の誤り"),
    ("review_revise", 61398): (False, "分析の誤り"),
    ("review_concern", 314199): (True, "task の制約（CODEX_HOME を変えない）のままでは未ログインを再現できなかった"),
    ("review_revise", 330271): (False, "測定の記録の不足"),
    ("review_revise", 328752): (False, "根拠の対応づけと参照番号の誤り"),
    ("review_revise", 341141): (False, "実装の欠け"),
    ("review_revise", 341576): (False, "design の書き写し"),
    ("review_concern", 150595): (False, "手順書の安全の欠け"),
    ("review_concern", 150917): (False, "実装の欠け（fail closed の抜け）"),
    ("review_revise", 151169): (False, "文書の照合の漏れ"),
    ("review_revise", 160402): (False, "文書の照合の漏れ"),
    ("review_revise", 172177): (False, "文書の小さな直し"),
    ("review_revise", 316908): (False, "実装の欠け"),
    ("review_revise", 186811): (False, "受け入れ条件の片付けの未達を follow_up に回した"),
    ("review_revise", 161815): (False, "文書の照合の漏れ"),
    ("review_concern", 206369): (False, "実装の欠け"),
}


def ts(s):
    return dt.datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()


def iso(t):
    if t is None:
        return None
    return dt.datetime.fromtimestamp(t, dt.timezone.utc).strftime("%Y-%m-%dT%H:%MZ")


def hours(secs):
    return None if secs is None else round(secs / 3600, 1)


NOW = dt.datetime.now(dt.timezone.utc).timestamp()


def union_len(intervals, lo, hi):
    spans = sorted((max(a, lo), min(b, hi)) for a, b in intervals if b > lo and a < hi)
    total, cur_a, cur_b = 0.0, None, None
    for a, b in spans:
        if cur_b is None or a > cur_b:
            if cur_b is not None:
                total += cur_b - cur_a
            cur_a, cur_b = a, b
        else:
            cur_b = max(cur_b, b)
    if cur_b is not None:
        total += cur_b - cur_a
    return total


class Store:
    """The events of the tasks and goals a precedent reads, by task."""

    def __init__(self, cache):
        self.cache = cache
        self.by_task = {}
        self.goal_closed = {}

    def add(self, events):
        for e in events:
            if e.get("task_id") is not None:
                self.by_task.setdefault(e["task_id"], {})[e["id"]] = e
            if e["kind"] == "goal_closed" and e.get("goal_id") is not None:
                self.goal_closed.setdefault(e["goal_id"], ts(e["created_at"]))

    def task(self, tid):
        if tid not in self.by_task:
            self.add(fetch.cached(self.cache, f"task-{tid}", ["--task", str(tid)]))
        return sorted(self.by_task.get(tid, {}).values(), key=lambda e: e["id"])

    def goal(self, gid):
        evs = fetch.cached(self.cache, f"goal-{gid}", ["--goal", str(gid)])
        self.add(evs)
        return evs

    def closed_at(self, gid):
        if gid not in self.goal_closed:
            self.goal(gid)
        return self.goal_closed.get(gid)


def first(evs, kind, pred=lambda e: True):
    return next((e for e in evs if e["kind"] == kind and pred(e)), None)


def completed_at(evs):
    e = first(evs, "task_status_changed", lambda e: e["payload"].get("to") == "completed")
    return ts(e["created_at"]) if e else None


def submitted_at(evs):
    e = first(evs, "task_submitted")
    return ts(e["created_at"]) if e else None


def created_at(evs):
    e = first(evs, "task_created")
    return ts(e["created_at"]) if e else None


def origin(evs):
    """The task's origin (None: a planner registered it) and the task whose
    follow-up it is."""
    kind, source = None, None
    for e in evs:
        p = e["payload"]
        if e["kind"] in ("attributes_baseline", "draft_origin_recorded", "draft_planner_opened",
                         "follow_up_adopted", "task_status_changed") and p.get("origin"):
            kind = kind or p["origin"]
        source = source or p.get("source_task_id")
    return kind, source


def claimable_since(store, evs, claim):
    """When the task last became claimable before `claim`: the recorded
    `ready_at`, or read like the runtime's for an older claim."""
    if claim["payload"].get("ready_at"):
        return ts(claim["payload"]["ready_at"]), "recorded"
    before = [e for e in evs if e["id"] < claim["id"]]
    ready = [e for e in before if e["kind"] == "task_status_changed" and e["payload"].get("to") == "ready"]
    if not ready:
        return None, "no_ready"
    claimed = ts(claim["created_at"])
    at = ts(ready[-1]["created_at"])
    preds, goals = set(), set()
    for e in before:
        p = e["payload"]
        if e["kind"] == "dependency_added":
            preds.add(p.get("predecessor_id"))
        elif e["kind"] == "dependency_removed":
            preds.discard(p.get("predecessor_id"))
        elif e["kind"] == "goal_dependency_added":
            goals.add(p.get("goal_id"))
        elif e["kind"] == "goal_dependency_removed":
            goals.discard(p.get("goal_id"))
    for done in [completed_at(store.task(t)) for t in filter(None, preds)] + \
                [store.closed_at(g) for g in filter(None, goals)]:
        if done is not None and done < claimed:
            at = max(at, done)
    return at, "derived"


def ask_waits(tasks, members, hi):
    asks, opened = [], {}
    for t in members:
        for e in tasks[t]:
            p = e["payload"]
            if e["kind"] == "ask_opened":
                opened[p["ask_id"]] = (ts(e["created_at"]), p.get("kind"), t)
            elif e["kind"] in ("ask_answered", "ask_closed") and p.get("ask_id") in opened:
                a, kind, task = opened.pop(p["ask_id"])
                asks.append((a, ts(e["created_at"]), kind, task, p["ask_id"]))
    for ask_id, (a, kind, task) in opened.items():
        asks.append((a, hi, kind, task, ask_id))
    return asks


def span(start, end, asks):
    hi = end if end is not None else NOW
    wait = union_len([(a, b) for a, b, *_ in asks], start, hi)
    return dict(end=iso(end), open=end is None, total_h=hours(hi - start),
                wait_h=hours(wait), without_wait_h=hours(hi - start - wait))


def rework_of(tasks, members, roles):
    out, seen = [], set()
    for t in members:
        evs = tasks[t]
        submitted = any(e["kind"] == "task_submitted" for e in evs)
        for e in evs:
            p = e["payload"]
            if e["kind"] == "plan_review_finished" and p.get("decision") == "revise":
                if p.get("plan_review_id") in seen:
                    continue
                seen.add(p.get("plan_review_id"))
                c = dict(kind="plan_review_revise", id=p.get("plan_review_id"),
                         codes=sorted({c for g in p.get("reason_codes") or [] for c in g}),
                         text=" / ".join(p.get("reasons") or []))
            elif e["kind"] == "review_finished" and p.get("verdict") not in (None, "pass"):
                c = dict(kind="review_" + p["verdict"], id=e["id"], primary=p.get("primary_code"),
                         codes=sorted({c for g in p.get("reason_codes") or [] for c in g}
                                      | ({p["primary_code"]} if p.get("primary_code") else set())),
                         text=" / ".join([p.get("summary") or ""] + (p.get("reasons") or [])))
            elif e["kind"] == "validation_finished" and (p.get("receipt") or {}).get("result") == "failed":
                c = dict(kind="receipt_failed", id=e["id"], codes=[], text=p["receipt"].get("summary") or "")
            elif e["kind"] == "ask_opened" and p.get("kind") == "worker_question":
                c = dict(kind="worker_question", id=p.get("ask_id"), codes=p.get("topics") or [],
                         text=p.get("question") or "")
            elif e["kind"] == "task_status_changed" and p.get("to") == "canceled" and submitted:
                c = dict(kind="canceled_after_submit", id=t, codes=[],
                         text=p.get("reason") or json.dumps(p, ensure_ascii=False))
            else:
                continue
            codes = set(c["codes"]) or {None}
            if codes & PREMISE_CODES:
                c["premise"], c["judged"] = True, "code"
            elif ({c["primary"]} if "primary" in c else codes) <= READ_CODES:
                j = JUDGED.get((c["kind"], c["id"]))
                c["premise"], c["judged"] = (j[0], "read") if j else (None, "unjudged")
                if j:
                    c["reading"] = j[1]
            else:
                c["premise"], c["judged"] = False, "code"
            c.update(task=t, role=roles[t], at=iso(ts(e["created_at"])), text=c["text"][:600])
            out.append(c)
    return out


def precedent(store, key):
    gid, spikes, why = PRECEDENTS[key]
    gevs = store.goal(gid)
    for s in spikes:
        store.task(s)
    tids = {e["task_id"] for e in gevs if e.get("task_id") is not None} | set(spikes)
    tasks = {t: store.task(t) for t in sorted(tids)}

    start = min(filter(None, (submitted_at(tasks[s]) for s in spikes)))
    impl, other = [], []
    for t, evs in tasks.items():
        if t in spikes:
            continue
        kind, source = origin(evs)
        sub, made = submitted_at(evs), created_at(evs)
        if (kind is None or source in spikes) and sub is not None and sub >= start - 60:
            if completed_at(evs) is not None:
                impl.append(t)
        elif made is not None and made >= start - 60:
            other.append(t)
    roles = {**{t: "S" for t in spikes}, **{t: "I" for t in impl}, **{t: "F" for t in other}}

    plan_end = max((completed_at(tasks[t]) for t in impl), default=None)
    goal_end = store.closed_at(gid)
    plan = span(start, plan_end, ask_waits(tasks, spikes + impl, plan_end or NOW))
    goal = span(start, goal_end, ask_waits(tasks, spikes + impl + other, goal_end or NOW))

    done = sorted(filter(None, (completed_at(tasks[s]) for s in spikes)))
    spike_done = done[0] if done else None
    with_spike = [t for t in impl if submitted_at(tasks[t]) <= start + 60]
    gaps = []
    for t in impl:
        sub = submitted_at(tasks[t])
        before = [d for d in done if d < sub]
        if before:
            gaps.append(sub - before[-1])
    gap = (min(gaps) if gaps else 0.0) if impl and done else None

    claims = []
    for cls, members in (("spike", spikes), ("implementation", impl)):
        for t in members:
            c = first(tasks[t], "run_claimed", lambda e: e["payload"].get("from") == "ready")
            if c is None:
                continue
            since, how = claimable_since(store, tasks[t], c)
            claims.append(dict(task=t, cls=cls, recorded_class=c["payload"].get("execution_class"),
                               claimed=iso(ts(c["created_at"])), how=how,
                               wait_h=None if since is None else hours(ts(c["created_at"]) - since),
                               after_mark=c["created_at"] > MARK))

    opened = {}
    for evs in tasks.values():
        for e in evs:
            if e["kind"] == "session_opened" and e["payload"].get("kind") == "runtime_planner":
                opened[e["id"]] = e
    hi = plan_end or NOW
    occ, seen = dict(count=0, open=0.0, active=0.0), set()
    for evs in tasks.values():
        for e in evs:
            p = e["payload"]
            if e["kind"] != "session_closed" or p.get("kind") != "runtime_planner":
                continue
            o = opened.get(p.get("opened_event_id"))
            if o is None or o["id"] in seen or not (start <= ts(o["created_at"]) <= hi):
                continue
            if gid not in (o["payload"].get("goal_ids") or []) and roles.get(o.get("task_id")) not in ("S", "I"):
                continue
            seen.add(o["id"])
            occ["count"] += 1
            occ["open"] += ts(e["created_at"]) - ts(o["created_at"])
            occ["active"] += p.get("active_secs") or 0

    rework = rework_of(tasks, spikes + impl + other, roles)
    tally = {}
    for r in rework:
        group = "S+I" if r["role"] in "SI" else "F"
        stage = "plan_review" if r["kind"] == "plan_review_revise" else "run"
        verdict = {True: "premise", False: "other", None: "unknown"}[r["premise"]]
        k = f"{group}.{stage}.{verdict}"
        tally[k] = tally.get(k, 0) + 1

    return dict(
        key=key, goal=gid, spikes=spikes, why=why, implementation=impl, others=other,
        start=iso(start), first_spike_done=iso(spike_done), plan=plan, goal_span=goal,
        replan_gap_h=hours(gap), with_spike=len(with_spike), after_spike=len(impl) - len(with_spike),
        claims_after_mark=sum(c["after_mark"] for c in claims), claims_total=len(claims),
        rework_tally=dict(sorted(tally.items())), rework=rework, claims=claims,
        planner=dict(count=occ["count"], open_h=hours(occ["open"]), active_h=hours(occ["active"]),
                     idle_h=hours(occ["open"] - occ["active"])),
    )


def population(cache, since, until):
    evs = fetch.cached(cache, f"claims-{since}-{until}",
                       ["--kind", "run_claimed", "--since", since, "--until", until])
    by, first_with = {}, None
    for e in evs:
        p = e["payload"]
        if not p.get("ready_at"):
            continue
        first_with = first_with or dict(event=e["id"], at=e["created_at"])
        cls = p.get("execution_class") or "implementation"
        by.setdefault(cls, []).append(max(0.0, ts(e["created_at"]) - ts(p["ready_at"])))
    out = dict(window=[since, until], claims=len(evs), first_with_ready_at=first_with, classes={})
    for cls, w in sorted(by.items()):
        w.sort()
        out["classes"][cls] = dict(n=len(w), median_h=hours(statistics.median(w)),
                                   p90_h=hours(w[min(len(w) - 1, int(len(w) * 0.9))]))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cache", required=True)
    ap.add_argument("--precedent", action="append")
    ap.add_argument("--population", nargs=2, metavar=("SINCE", "UNTIL"))
    ap.add_argument("--json")
    a = ap.parse_args()
    os.makedirs(a.cache, exist_ok=True)
    if a.population:
        out = population(a.cache, *a.population)
    else:
        store = Store(a.cache)
        out = [precedent(store, k) for k in (a.precedent or PRECEDENTS)]
    text = json.dumps(out, ensure_ascii=False, indent=1)
    if a.json:
        with open(a.json, "w") as f:
            f.write(text + "\n")
    print(text)


if __name__ == "__main__":
    main()
