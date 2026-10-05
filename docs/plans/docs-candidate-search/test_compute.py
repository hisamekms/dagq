"""Pure fixtures for the measurement's route and censoring rules; no queue access."""
import unittest
import compute as m


def event(i, kind, attempt=1, **payload):
    return {'id': i, 'kind': kind, 'created_at': '2026-10-05T00:00:%02dZ' % i,
            'payload': dict(attempt=attempt, **payload)}


class MeasurementRules(unittest.TestCase):
    def test_concern_sendback_route_overrides_verdict(self):
        review = event(1,'review_finished',verdict='concern',route={'destination':'send_back'})
        self.assertEqual(m.route(review,[review]),'send_back')

    def test_ask_route_overrides_revise(self):
        review = event(1,'review_finished',verdict='revise',route={'destination':'ask'})
        self.assertEqual(m.route(review,[review,event(2,'revise_requested')]),'ask')

    def test_legacy_concern_needs_request_before_next_review(self):
        review = event(1,'review_finished',verdict='concern')
        self.assertEqual(m.route(review,[review,event(2,'revise_requested')]),'send_back')
        self.assertEqual(m.route(review,[review,event(2,'review_started',2),event(3,'revise_requested')]),'ask')

    def test_all_reason_codes_include_secondary_drift(self):
        review = event(1,'review_finished',primary_code='code_defect',reason_codes=[['code_defect'],['docs_drift']])
        self.assertTrue(m.docs(review))

    def test_cancel_then_resend_uses_resend_clock(self):
        review = event(1,'review_finished')
        evs = [review,event(2,'revise_requested'),event(3,'revise_unsent'),event(4,'revise_requested'),event(6,'review_started',2),event(8,'review_finished',2,duration_secs=2)]
        cycle = m.cycle(review,evs)
        self.assertEqual((cycle['revise'],cycle['rereview'],cycle['total']),(2,2,4))
        self.assertEqual(cycle['errors'],['cancelled'])

    def test_cancel_without_resend_has_no_time(self):
        review = event(1,'review_finished')
        cycle = m.cycle(review,[review,event(2,'revise_requested'),event(3,'revise_unsent')])
        self.assertIsNone(cycle['total'])
        self.assertEqual(cycle['errors'],['cancelled'])

    def test_unsent_of_another_attempt_does_not_cancel(self):
        review = event(1,'review_finished')
        cycle = m.cycle(review,[review,event(2,'revise_requested'),event(3,'revise_unsent',2),event(4,'review_started',2),event(6,'review_finished',2,duration_secs=2)])
        self.assertEqual(cycle['total'],4)
        self.assertEqual(cycle['errors'],[])

    def test_no_request_and_unfinished_revise_are_distinct(self):
        review = event(1,'review_finished')
        self.assertEqual(m.cycle(review,[review])['errors'],['not_sent'])
        part = m.cycle(review,[review,event(2,'revise_requested')])
        self.assertEqual(part['errors'],['revise_unfinished'])
        self.assertIsNone(part['revise'])

    def test_failed_review_keeps_revise_only(self):
        review = event(1,'review_finished')
        part = m.cycle(review,[review,event(2,'revise_requested'),event(4,'review_started',2),event(6,'review_failed',2,duration_secs=2)])
        self.assertEqual(part['revise'],2)
        self.assertIsNone(part['rereview'])
        self.assertIsNone(part['total'])

    def test_missing_duration_and_bad_clock_keep_other_side(self):
        review = event(1,'review_finished')
        req = event(2,'revise_requested')
        start = event(4,'review_started',2)
        end = event(6,'review_finished',2,duration_secs='2')
        self.assertEqual(m.cycle(review,[review,req,start,end])['errors'],['rereview_missing_duration'])
        req['created_at']='bad'
        end['payload']['duration_secs']=2
        part=m.cycle(review,[review,req,start,end])
        self.assertIsNone(part['revise'])
        self.assertEqual(part['rereview'],2)

    def test_waits_only_subtract_overlap_and_open_wait_is_unfinished(self):
        self.assertEqual(m.c.overlap([(0,3000),(8000,15000)],2000,10000),3000)
        self.assertIsNone(m.c.overlap([(8000,None)],2000,10000))
        self.assertEqual(m.c.overlap([(12000,None)],2000,10000),0)

    def test_initial_receipt_skips_post_review_final_receipt(self):
        early = event(1,'validation_finished',receipt={'summary':'early'})
        initial = event(2,'validation_finished',receipt={'summary':'initial'})
        review = event(3,'review_finished')
        final = event(4,'validation_finished',receipt={'summary':'final'})
        evs=[early,initial,review,final]
        self.assertIs(m.c.validation_receipt_before(evs,review),initial)
        initial['payload']['receipt']['summary']=''
        self.assertEqual(m.c.validation_receipt_before(evs,review)['payload']['receipt']['summary'],'')

    def test_missing_summary_is_not_in_check_denominator(self):
        row = {key:0 for key in ('drift_any','drift_primary','drift_reference','human','route_absent','summary_missing','check_absent','searched','work_missing','wait_unfinished','waiting_deferred')}
        row['summary_missing']=1
        row.update({key:None for key in ('work','work_excl_wait','revise','rereview','total','primary_revise','primary_rereview','primary_total')})
        result=m.aggregate([row])
        self.assertEqual(result['summary_n'],0)
        self.assertIsNone(result['check_absent_rate'])

if __name__=='__main__':
    unittest.main()
