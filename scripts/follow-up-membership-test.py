#!/usr/bin/env python3
"""Synthetic boundary checks for the baseline; no queue or provider access."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('membership', Path(__file__).with_name('follow-up-membership.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def event(i, kind, task=None, payload=None, goal=None, time=None, actor=None):
    return {'id': i, 'kind': kind, 'task_id': task, 'goal_id': goal,
            'run_id': None, 'payload': payload or {}, 'actor': {'id': actor},
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

    def test_membership_pass_tokens_and_correction(self):
        tokens = {'input': 1, 'cache_read': 2, 'cache_creation': 3, 'output': 4}
        judgement = {'id': 1, 'classification': 'required', 'destination_goal_id': 1,
                     'source_goal_id': 1, 'corrects': None}
        events = [event(1, 'follow_up_registered', 5, {'task_id': 10}),
                  event(2, 'draft_planner_opened', 10, {'planner_id': 7, 'members': [10]}),
                  event(3, 'follow_up_judged', 10, judgement),
                  event(4, 'follow_up_judged', None, judgement, goal=1),
                  event(5, 'turn_finished', None, {'planner_id': 7, 'provider': 'claude',
                                                    'tokens': tokens, 'tokens_source': 'model_usage'}),
                  event(6, 'turn_finished', None, {'planner_id': 7, 'provider': 'claude', 'tokens': tokens}),
                  event(7, 'task_submitted', 10, {'proposal_id': 2}),
                  event(8, 'plan_review_started', 10, {'proposal_id': 2, 'plan_review_id': 4,
                                                       'launch': {'provider': 'codex'}}),
                  event(9, 'plan_review_finished', 10, {'proposal_id': 2, 'plan_review_id': 4,
                                                        'verdict': 'revise', 'reasons': ['所属先を直す'],
                                                        'tokens': tokens, 'tokens_source': 'token_usage_record'}),
                  event(10, 'plan_revise_sent', 10, {'proposal_id': 2, 'planner_id': 8}),
                  event(11, 'turn_finished', None, {'planner_id': 8, 'provider': 'claude',
                                                     'tokens': tokens, 'tokens_source': 'model_usage'}),
                  event(12, 'task_status_changed', 10, {'from': 'submitted', 'to': 'ready'}),
                  event(13, 'follow_up_judged', 10, {'id': 2, 'classification': 'required',
                                                      'destination_goal_id': 3, 'corrects': 1}),
                  event(14, 'follow_up_judged', 10, {'id': 3, 'classification': 'required',
                                                      'destination_goal_id': 3, 'corrects': 2})]
        result = module.analyse(data(events), '2026-10-01T00:00:00Z', '2026-10-04T00:00:00Z')['evaluation']
        self.assertEqual(result['judged_before_submit'], 1)
        self.assertEqual(result['classification_first_pass'], {'n': 1, 'pass': 0})
        self.assertEqual(result['first_revise_naming_membership'], [9])
        self.assertEqual(result['first_pass_strata']['plan_review_provider'], {'codex': {'n': 1, 'pass': 0}})
        # The second draft-planner turn has no tokens_source: unmeasured, not 0.
        self.assertEqual(result['tokens']['pre_submit_planners']['executions'], 2)
        self.assertEqual(result['tokens']['pre_submit_planners']['measured'], 1)
        self.assertEqual(result['tokens']['pre_submit_planners']['total'], 10)
        # Never adopted: none of its work is adoption-to-approval work.
        unadopted = result['tokens']['by_boundary']['unapproved_or_unadopted']
        self.assertEqual(unadopted['planners']['total'], 20)
        self.assertEqual(unadopted['plan_reviews']['by_provider']['codex']['total'], 10)
        self.assertEqual(result['tokens']['by_boundary']['adopt_to_ready']['planners']['executions'], 0)
        # Only the correction that changes the destination makes the task wrong.
        corrections = result['corrections_after_ready']
        self.assertEqual([c['classification_or_destination_changed'] for c in corrections['events']],
                         [True, False])
        self.assertEqual(corrections['wrong_tasks'], [10])
        self.assertEqual(corrections['observed_30_days'], 0)

    def test_live_planner_revise_turn_and_taken_over_stop(self):
        tokens = {'input': 1, 'cache_read': 0, 'cache_creation': 0, 'output': 0}
        measured = {'tokens': tokens, 'tokens_source': 'model_usage'}
        events = [event(1, 'follow_up_registered', 5, {'task_id': 10}),
                  event(2, 'draft_planner_opened', 10, {'planner_id': 7, 'members': [10]}),
                  event(3, 'turn_started', None, {'planner_id': 7, 'turn': 1}),
                  event(4, 'follow_up_judged', 10, {'id': 1, 'source_goal_id': None, 'corrects': None}),
                  event(5, 'task_submitted', 10, {'proposal_id': 2}, actor='planner:7'),
                  event(6, 'turn_finished', None, {'planner_id': 7, 'turn': 1, **measured}),
                  event(7, 'plan_review_finished', 10, {'proposal_id': 2, 'verdict': 'revise',
                                                        'session_id': 's', **measured}),
                  event(8, 'headless_job_stopped', None, {'kind': 'plan_review', 'proposal_id': 2,
                                                          'session_id': 's'}),
                  event(9, 'plan_revise_sent', 10, {'proposal_id': 2, 'planner_id': 7, 'opened': False}),
                  event(10, 'turn_started', None, {'planner_id': 7, 'turn': 2}),
                  event(11, 'turn_finished', None, {'planner_id': 7, 'turn': 2, **measured}),
                  event(12, 'task_status_changed', 10, {'from': 'submitted', 'to': 'ready'}),
                  event(13, 'follow_up_judged', 10, {'id': 2, 'corrects': 99})]
        result = module.analyse(data(events), '2026-10-01T00:00:00Z', '2026-10-04T00:00:00Z')['evaluation']
        # The submitting turn is pre-submit even though it ends after the submit.
        self.assertEqual(result['tokens']['pre_submit_planners']['total'], 1)
        unadopted = result['tokens']['by_boundary']['unapproved_or_unadopted']
        self.assertEqual(unadopted['planners']['total'], 2)
        self.assertEqual(unadopted['plan_reviews']['executions'], 1)
        # No source goal: left out of (1) and listed.
        self.assertEqual(result['judged_without_source_goal'], [10])
        self.assertEqual(result['classification_first_pass'], {'n': 0, 'pass': 0})
        # A correction of a judgement that cannot be found is unknown, not a change.
        corrections = result['corrections_after_ready']
        self.assertIsNone(corrections['events'][0]['classification_or_destination_changed'])
        self.assertEqual(corrections['wrong_tasks'], [])

    def test_tokens_split_at_adoption_and_first_ready(self):
        def used(n, **payload):
            return {**payload, 'tokens': {'input': n}, 'tokens_source': 'model_usage'}
        events = [event(1, 'follow_up_registered', 5, {'task_id': 10}),
                  event(2, 'draft_planner_opened', 10, {'planner_id': 7, 'members': [10]}),
                  event(3, 'turn_started', None, {'planner_id': 7, 'turn': 1}),
                  event(4, 'follow_up_adopted', 10),
                  event(5, 'task_submitted', 10, {'proposal_id': 2}, actor='planner:7'),
                  event(6, 'turn_finished', None, used(1, planner_id=7, turn=1)),
                  event(7, 'plan_review_started', 10, {'proposal_id': 2, 'plan_review_id': 4}),
                  event(8, 'plan_review_finished', 10, used(10, proposal_id=2, plan_review_id=4,
                                                            verdict='revise')),
                  event(9, 'plan_revise_sent', 10, {'proposal_id': 2, 'planner_id': 8}),
                  event(10, 'turn_started', None, {'planner_id': 8, 'turn': 1}),
                  event(11, 'turn_finished', None, used(100, planner_id=8, turn=1)),
                  event(12, 'task_submitted', 10, {'proposal_id': 2}, actor='planner:8'),
                  event(13, 'plan_review_started', 10, {'proposal_id': 2, 'plan_review_id': 5}),
                  event(14, 'task_status_changed', 10, {'from': 'submitted', 'to': 'ready'}),
                  event(15, 'plan_review_finished', 10, used(1000, proposal_id=2, plan_review_id=5,
                                                             verdict='pass')),
                  event(16, 'turn_started', None, {'planner_id': 8, 'turn': 2}),
                  event(17, 'turn_finished', None, used(10000, planner_id=8, turn=2)),
                  event(18, 'follow_up_registered', 5, {'task_id': 11}),
                  event(19, 'draft_planner_opened', 11, {'planner_id': 9, 'members': [11]}),
                  event(20, 'turn_started', None, {'planner_id': 9, 'turn': 1}),
                  event(21, 'turn_finished', None, used(5, planner_id=9, turn=1))]
        tokens = module.analyse(data(events), '2026-10-01T00:00:00Z',
                                '2026-10-04T00:00:00Z')['evaluation']['tokens']
        split = tokens['by_boundary']
        # Inside: the revise turn, and both reviews (the approving one ends just after ready).
        self.assertEqual(split['adopt_to_ready']['planners']['total'], 100)
        self.assertEqual(split['adopt_to_ready']['plan_reviews']['total'], 1010)
        # The turn that adopts is shown whole as crossing the boundary, not split.
        self.assertEqual(split['crossing_adoption']['planners']['total'], 1)
        self.assertEqual(split['after_ready']['planners']['total'], 10000)
        self.assertEqual(split['unapproved_or_unadopted']['planners']['total'], 5)
        self.assertEqual(tokens['approved_with_adoption'], 1)


if __name__ == '__main__':
    unittest.main()
