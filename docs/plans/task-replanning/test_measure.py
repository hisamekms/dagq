#!/usr/bin/env python3
"""Unit tests of measure.py's task-level aggregation of replaced tasks.

  python3 -I docs/plans/task-replanning/test_measure.py

They build the events of a few tasks by hand (no queue is read) and check
that a replacement tree is added up the same whatever order the
`--replaced` arguments come in: every task of the tree keeps its runs and
sessions in the root's row, and the root ends only when every leaf has.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import measure  # noqa: E402

CLAIMS_FROM, CLAIMS_TO, EVENTS_TO = "2026-11-01", "2026-11-04", "2026-11-06"


class Queue:
    """Events of tasks that each have one headless run."""

    def __init__(self, earlier=()):
        self.events, self.sessions, self.next_id = [], [], 1
        self.earlier = set(earlier)

    def add(self, kind, at, task, run=None, **payload):
        self.events.append(dict(id=self.next_id, kind=kind, created_at=at, task_id=task,
                                run_id=run, payload=payload))
        self.next_id += 1

    def task(self, task, claim, end_status=None, end_at=None, cost=1.0, tokens=100):
        run = f"run-{task}"
        self.add("run_claimed", claim, task, run, worker_mode="headless", provider="claude")
        self.add("turn_started", claim, task, run, what="the task's prompt")
        self.add("turn_finished", claim, task, run, cost_usd=cost, tokens={"output": tokens})
        self.sessions.append(dict(id=self.next_id, kind="session_closed", created_at=claim,
                                  task_id=task, run_id=run,
                                  payload=dict(kind="worker",
                                               tokens={"output": tokens, "cost_usd": cost})))
        self.next_id += 1
        if end_status:
            self.add("task_status_changed", end_at, task, None, **{"from": "in_progress",
                                                                    "to": end_status})

    def rows(self, specs):
        replaced = measure.parse_replaced(specs)
        runs = measure.classify(self.events, CLAIMS_FROM, CLAIMS_TO, replaced)
        return measure.task_level(runs, self.events, self.sessions, self.earlier, EVENTS_TO,
                                  replaced)

    def session(self, task, at, cost=1.0, tokens=100):
        self.sessions.append(dict(id=self.next_id, kind="session_closed", created_at=at,
                                  task_id=task, run_id=f"run-{task}",
                                  payload=dict(kind="review",
                                               tokens={"output": tokens, "cost_usd": cost})))
        self.next_id += 1


def chain(leaf_status):
    """A (claimed in the window) replaced by B, B replaced by C (both
    claimed in the grace period after the window)."""
    q = Queue()
    q.task(1, "2026-11-02T00:00:00Z", "replaced", "2026-11-02T06:00:00Z", cost=1.0, tokens=10)
    q.task(2, "2026-11-04T01:00:00Z", "replaced", "2026-11-04T06:00:00Z", cost=2.0, tokens=20)
    q.task(3, "2026-11-05T01:00:00Z", leaf_status, leaf_status and "2026-11-05T09:00:00Z",
           cost=4.0, tokens=40)
    return q


class ReplacementTree(unittest.TestCase):
    def check_chain_completed(self, specs):
        rows = chain("completed").rows(specs)
        self.assertEqual(sorted(rows), [1])
        a = rows[1]
        self.assertEqual(sorted(r["task"] for r in a["runs"]), [1, 2, 3])
        self.assertEqual(sum(r["started"] for r in a["runs"]), 3)
        self.assertEqual(a["end"], ("replaced", "2026-11-05T09:00:00Z"))
        self.assertEqual(a["comp"]["worker"]["sessions"], 3)
        self.assertEqual(a["comp"]["worker"]["output_tokens"], 70)
        self.assertEqual(a["comp"]["worker"]["cost_usd_milli"], 7000)

    def test_chain_in_order_of_the_tree(self):
        self.check_chain_completed(["1:2", "2:3"])

    def test_chain_in_reverse_order(self):
        self.check_chain_completed(["2:3", "1:2"])

    def test_a_canceled_leaf_marks_the_root_in_either_order(self):
        for specs in (["1:2", "2:3"], ["2:3", "1:2"]):
            a = chain("canceled").rows(specs)[1]
            self.assertEqual(a["end"], ("replaced_child_canceled", "2026-11-05T09:00:00Z"))
            self.assertEqual(len(a["runs"]), 3)

    def test_an_unfinished_leaf_keeps_the_root_open_with_every_run(self):
        for specs in (["1:2", "2:3"], ["2:3", "1:2"]):
            a = chain(None).rows(specs)[1]
            self.assertNotIn("end", a)
            self.assertEqual(len(a["runs"]), 3)
            self.assertEqual(a["comp"]["worker"]["cost_usd_milli"], 7000)

    def test_fan_out_ends_at_the_latest_leaf(self):
        q = Queue()
        q.task(1, "2026-11-02T00:00:00Z", "replaced", "2026-11-02T06:00:00Z")
        q.task(2, "2026-11-02T07:00:00Z", "replaced", "2026-11-02T08:00:00Z")
        q.task(3, "2026-11-02T07:00:00Z", "completed", "2026-11-03T00:00:00Z")
        q.task(4, "2026-11-02T09:00:00Z", "completed", "2026-11-02T10:00:00Z")
        for specs in (["1:2,3", "2:4"], ["2:4", "1:2,3"]):
            rows = q.rows(specs)
            self.assertEqual(sorted(rows), [1])
            self.assertEqual(rows[1]["end"], ("replaced", "2026-11-03T00:00:00Z"))
            self.assertEqual(len(rows[1]["runs"]), 4)

    def test_a_child_with_no_run_or_an_empty_list_stays_open(self):
        q = Queue()
        q.task(1, "2026-11-02T00:00:00Z", "replaced", "2026-11-02T06:00:00Z")
        self.assertNotIn("end", q.rows(["1:9"])[1])
        self.assertNotIn("end", q.rows(["1:"])[1])

    def test_a_cycle_is_reported_and_never_ends(self):
        q = Queue()
        q.task(1, "2026-11-02T00:00:00Z", "replaced", "2026-11-02T06:00:00Z")
        q.task(2, "2026-11-02T07:00:00Z", "replaced", "2026-11-02T08:00:00Z")
        for specs in (["1:2", "2:1"], ["2:1", "1:2"]):
            rows = q.rows(specs)
            self.assertEqual(sorted(rows), [1])
            self.assertEqual(rows[1].get("unresolved"), measure.CYCLE)
            self.assertNotIn("end", rows[1])
            self.assertEqual(len(rows[1]["runs"]), 2)

    def test_a_tree_rooted_after_the_window_adds_no_row(self):
        q = Queue()
        q.task(1, "2026-11-02T00:00:00Z", "completed", "2026-11-02T06:00:00Z")
        q.task(10, "2026-11-04T02:00:00Z", "replaced", "2026-11-04T03:00:00Z")
        q.task(11, "2026-11-04T04:00:00Z", "replaced", "2026-11-04T05:00:00Z")
        q.task(12, "2026-11-04T06:00:00Z", "completed", "2026-11-05T00:00:00Z")
        for specs in ([], ["10:11"], ["10:11", "11:12"], ["11:12", "10:11"]):
            self.assertEqual(sorted(q.rows(specs)), [1])

    def test_a_tree_rooted_before_the_window_is_one_excluded_row(self):
        q = Queue(earlier={20})
        q.task(21, "2026-11-02T00:00:00Z", "replaced", "2026-11-02T06:00:00Z")
        q.task(22, "2026-11-02T07:00:00Z", "completed", "2026-11-03T00:00:00Z")
        for specs in (["20:21", "21:22"], ["21:22", "20:21"]):
            rows = q.rows(specs)
            self.assertEqual(sorted(rows), [21])
            self.assertTrue(rows[21]["excluded"])
            self.assertEqual(len(rows[21]["runs"]), 2)

    def test_a_cycle_whose_smallest_task_never_ran_is_one_row(self):
        q = Queue()
        q.task(2, "2026-11-02T00:00:00Z", "replaced", "2026-11-02T06:00:00Z")
        q.task(3, "2026-11-02T07:00:00Z", "replaced", "2026-11-02T08:00:00Z")
        for specs in (["1:2", "2:3", "3:1"], ["3:1", "2:3", "1:2"]):
            rows = q.rows(specs)
            self.assertEqual(sorted(rows), [2])
            self.assertEqual(rows[2].get("unresolved"), measure.CYCLE)
            self.assertEqual(len(rows[2]["runs"]), 2)

    def test_an_intermediate_task_keeps_its_sessions_until_its_leaves_end(self):
        for specs in (["1:2", "2:3"], ["2:3", "1:2"]):
            q = chain("completed")
            q.session(2, "2026-11-04T07:00:00Z", cost=8.0, tokens=80)
            q.session(2, "2026-11-05T10:00:00Z", cost=16.0, tokens=160)
            a = q.rows(specs)[1]
            self.assertEqual(a["comp"]["review_job"]["sessions"], 1)
            self.assertEqual(a["comp"]["review_job"]["cost_usd_milli"], 8000)

    def test_a_child_of_two_sources_is_refused(self):
        with self.assertRaises(ValueError):
            measure.replacement_roots({1: [3], 2: [3]})

    def test_a_source_given_twice_is_refused(self):
        with self.assertRaises(ValueError):
            measure.parse_replaced(["1:2", "1:3"])


if __name__ == "__main__":
    unittest.main()
