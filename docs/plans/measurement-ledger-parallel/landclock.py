"""A simplified copy of src/domain/stats/landing.rs LandClock::observe that
keeps its segments (phase, start, end) instead of only the totals, from
the first validation_finished to run_integrated."""
import sys
sys.path.insert(0, __file__.rsplit('/', 1)[0]); import lib
ms = lib.F['ms']

def segments(events):
    segs = []; phase = None; since = None
    asks = {}; before_ask = None; before_held = None
    def key(p):
        v = p.get('ask_id', p.get('id'))
        return None if v is None else str(v)
    def enter(nxt, at):
        nonlocal phase, since
        segs.append((phase, since, at)); phase, since = nxt, at
    for e in events:
        at = ms(e['created_at']); k = e['kind']; p = e['payload'] or {}
        if phase is None:
            if k == 'validation_finished':
                phase, since = 'exit', at
                # the event that starts the clock is observed too
            else:
                continue
        if k == 'run_integrated':
            segs.append((phase, since, at)); return segs
        nxt = None
        if p.get('status') == 'needs_session': nxt = 'resume'
        elif k in ('review_programs_started', 'review_started', 'review_retried'): nxt = 'review'
        elif k in ('review_finished', 'validation_finished'): nxt = 'exit'
        elif k == 'review_failed': nxt = 'ask'
        elif k == 'revise_requested': nxt = 'revise'
        elif k == 'revise_unsent': nxt = 'exit'
        elif k == 'conflict_precheck': nxt = 'conflict' if p.get('requested') is True else 'exit'
        elif k == 'landing_queued': nxt = 'landing_queue'
        elif k == 'run_e2e_waiting': nxt = 'e2e_wait'
        elif k == 'run_e2e_started': nxt = 'e2e'
        elif k == 'run_e2e_finished': nxt = 'landing_queue'
        elif k in ('integration_error', 'integration_held'): nxt = 'ask'
        elif k == 'integration_started': nxt = 'rebase'
        elif k == 'integration_rebased': nxt = 'verify'
        elif k == 'ask_opened':
            if p.get('kind') in ('blocked', 'planner_question'): nxt = None
            else:
                if key(p) is not None: asks[key(p)] = p.get('kind')
                if phase == 'held':
                    if before_held != 'ask': before_ask = before_held or 'exit'
                elif phase != 'ask': before_ask = phase
                nxt = 'ask'
        elif k == 'ask_answered':
            opened = asks.pop(key(p), None) if key(p) is not None else None
            ans = p.get('kind') or opened
            if ans == 'approve_landing':
                asks.clear(); before_ask = None
                nxt = 'ask' if p.get('runtime_delivers') is False else ('review' if p.get('option') == 'retry_review' else 'landing_queue')
            elif not asks:
                nxt, before_ask = before_ask, None
        # through_hold
        is_record = k == 'run_phase_changed'
        if is_record and p.get('phase') == 'held':
            if phase != 'held': before_held = phase
            nxt = 'held'
        elif phase == 'held':
            if k == 'task_released' or is_record:
                nxt = nxt or before_held or 'exit'; before_held = None if not nxt else before_held
            else:
                if nxt: before_held = nxt
                nxt = None
        if nxt:
            if nxt not in ('ask', 'held') and k != 'ask_answered' and phase != 'held':
                asks.clear(); before_ask = None
            enter(nxt, at)
    return None
