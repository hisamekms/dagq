#!/usr/bin/env python3
"""Synthetic boundary checks for the baseline; no queue or provider access."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('membership', Path(__file__).with_name('follow-up-membership.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def event(i, kind, task=None, payload=None, goal=None, time=None):
    return {'id': i, 'kind': kind, 'task_id': task, 'goal_id': goal,
            'run_id': None, 'payload': payload or {},
            'created_at': time or f'2026-10-01T00:00:{i:02d}Z'}


def data(events):
    return {'events': events, 'snapshot_started': '2026-10-05T00:00:00Z',
            'snapshot_ended': '2026-10-05T00:00:01Z', 'goals': [],
            'snapshots': [{'goal': {'id': 1, 'status': 'open'}, 'closed': True,
                           'tasks': [{'id': 10, 'status': 'completed'},
                                     {'id': 11, 'status': 'draft'}]}]}


class BaselineTests(unittest.TestCase):
    def test_batch_revise_resubmit_and_censor(self):
        events = [event(1, 'follow_up_registered', 5, {'task_id': 10}),
                  event(2, 'follow_up_registered', 5, {'task_id': 11}),
                  event(3, 'follow_up_registered', 5, {'task_id': None, 'skipped': 'invalid'}),
                  event(4, 'task_submitted', 10, {'proposal_id': 2}),
                  event(5, 'follow_up_adopted', 10),
                  event(6, 'plan_review_finished', 10, {'proposal_id': 2, 'verdict': 'revise'}),
                  event(7, 'task_submitted', 10, {'proposal_id': 3}),
                  event(8, 'task_status_changed', 10, {'from': 'submitted', 'to': 'ready'}),
                  event(9, 'plan_review_finished', 10, {'proposal_id': 3, 'verdict': 'pass'},
                        time='2026-10-01T00:00:08Z')]
        result = module.analyse(data(events), '2026-10-01T00:00:00Z', '2026-10-04T00:00:00Z')
        self.assertEqual(result['cohort_n'], 2)
        self.assertEqual(result['first_review'], {'n': 1, 'pass': 0, 'unreviewed': 1})
        self.assertEqual(result['approved_revise']['total'], 1)
        self.assertEqual(result['durations_seconds']['adopt_to_ready']['total'], 3)
        self.assertEqual(result['rows'][0]['review_events'], [6, 9])

    def test_verdict_after_ready_and_revise_planner_session(self):
        events = [event(1, 'follow_up_registered', 5, {'task_id': 10}),
                  event(2, 'task_submitted', 10, {'proposal_id': 2}),
                  event(3, 'plan_review_started', 10, {'proposal_id': 2, 'plan_review_id': 4}),
                  event(4, 'session_opened', 99, {'kind': 'runtime_planner',
                                                'planner_id': 8, 'proposal_id': 2}),
                  event(5, 'session_closed', 99, {'opened_event_id': 4}),
                  event(6, 'task_status_changed', 10, {'from': 'submitted', 'to': 'ready'}),
                  event(7, 'plan_review_finished', 10, {'proposal_id': 2,
                                                       'plan_review_id': 4, 'verdict': 'pass'})]
        result = module.analyse(data(events), '2026-10-01T00:00:00Z', '2026-10-04T00:00:00Z')
        self.assertEqual(result['first_review']['pass'], 1)
        self.assertEqual(result['runtime_planner_seconds']['n'], 1)

    def test_adoption_after_first_ready_is_missing_not_zero(self):
        events = [event(1, 'follow_up_registered', 5, {'task_id': 10}),
                  event(2, 'task_status_changed', 10, {'from': 'draft', 'to': 'ready'}),
                  event(3, 'follow_up_adopted', 10)]
        result = module.analyse(data(events), '2026-10-01T00:00:00Z', '2026-10-04T00:00:00Z')
        self.assertEqual(result['durations_seconds']['adopt_to_ready']['n'], 0)
        self.assertIsNone(result['rows'][0]['adopt_to_ready'])

    def test_rollback_goal_move_and_unique_bundle_session(self):
        events = [event(1, 'follow_up_registered', 5, {'task_id': 10}),
                  event(2, 'follow_up_registered', 5, {'task_id': 11}),
                  event(3, 'draft_planner_opened', 10, {'planner_id': 7}),
                  event(4, 'draft_planner_opened', 11, {'planner_id': 7}),
                  event(5, 'session_opened', 10, {'kind': 'runtime_planner', 'planner_id': 7}),
                  event(6, 'session_closed', 10, {'opened_event_id': 5}),
                  event(7, 'task_status_changed', 10, {'from': 'ready', 'to': 'completed'},
                        time='2026-10-04T01:00:00Z'),
                  event(8, 'goal_closed', goal=1, time='2026-10-04T01:00:01Z'),
                  event(9, 'task_goal_changed', 11, {'from': 1, 'to': 2},
                        time='2026-10-04T01:00:02Z')]
        source = data(events)
        source['snapshots'][0]['tasks'] = [{'id': 10, 'status': 'completed'}]
        source['snapshots'].append({'goal': {'id': 2, 'status': 'open'}, 'closed': False,
                                    'tasks': [{'id': 11, 'status': 'draft'}]})
        result = module.analyse(source, '2026-10-01T00:00:00Z', '2026-10-04T00:00:00Z')
        self.assertEqual(result['runtime_planner_seconds']['n'], 1)
        self.assertEqual(result['only_follow_up_remaining'][1], [10, 11])
        self.assertEqual(result['snapshot_race_events'], [])


if __name__ == '__main__':
    unittest.main()
