//! Starting a run's worker over in a new session when its context grew
//! large (ADR-t2080-1): a send-back (review's revise or `send_back`), a
//! `needs_session` resume or a triage's resume of a worker whose previous
//! turn's `peak_context` is above the threshold of `[fresh_session]` goes
//! on in a session that carries no conversation, in the same worktree,
//! branch and run, on the same provider, with a handoff prompt as its
//! first request. Only that `peak_context` and the threshold decide
//! (decision 4): no classification of the send-back (a review's reason
//! code, say) does (ADR-t947-1 decision 3). The new session is a boundary
//! like a provider's switch (ADR-t813-2 decision 4), recorded as
//! `session_renewed`.

use serde_json::{Value, json};

use super::{Provider, RunEvent, event_kind, provider_switch, turn};

/// `[fresh_session]` of `dagq.toml` (ADR-t2080-1 decision 3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FreshSessionConfig {
    /// `peak_context_above`, in tokens: a send-back or resume of a run's
    /// worker starts a new session when the worker's previous turn's
    /// `peak_context` (the largest input of one call of its model, cached
    /// input included, in tokens) is above it; at or below it, the session
    /// goes on. A whole number above 0. `None` (no key, no table) never
    /// starts one.
    pub peak_context_above: Option<u64>,
}

impl FreshSessionConfig {
    /// The keys of `[fresh_session]`.
    pub const KEYS: [&'static str; 1] = ["peak_context_above"];
}

/// Whether a send-back or resume of a run's worker goes on in its session
/// or in a new one ([`next_session`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextSession {
    /// In the session the worker has (or the one it reopens).
    Same,
    /// In a new session: the previous turn's `peak_context` was above
    /// `threshold`.
    Fresh { peak_context: u64, threshold: u64 },
}

/// Where a worker whose previous turn's `peak_context` was `peak_context`
/// goes on, given `[fresh_session] peak_context_above` (`threshold`): in a
/// new session only when both are known and the peak is above the
/// threshold. A peak not measured (`None`) or no threshold keeps the
/// session.
pub fn next_session(peak_context: Option<i64>, threshold: Option<u64>) -> NextSession {
    let (Some(peak), Some(threshold)) = (
        peak_context.and_then(|peak| u64::try_from(peak).ok()),
        threshold,
    ) else {
        return NextSession::Same;
    };
    if peak > threshold {
        NextSession::Fresh {
            peak_context: peak,
            threshold,
        }
    } else {
        NextSession::Same
    }
}

/// The `peak_context` of the run's worker's previous turn: that of the
/// last `turn_finished` of its current session
/// ([`provider_switch::since_session_start`]); `None` when that session
/// finished no turn yet (a session just renewed or switched is not judged
/// by the turns of the one it left) or the turn's was not measured
/// (`null`).
pub fn previous_peak_context(events: &[RunEvent]) -> Option<i64> {
    provider_switch::since_session_start(events)
        .iter()
        .rfind(|e| e.kind == event_kind::TURN_FINISHED)
        .and_then(|e| e.payload["peak_context"].as_i64())
}

/// The run's new sessions started for a large context, as recorded.
pub fn renewals(events: &[RunEvent]) -> usize {
    events
        .iter()
        .filter(|e| e.kind == event_kind::SESSION_RENEWED)
        .count()
}

/// The payload of the `session_renewed` that starts the next session of
/// run `run_id`'s worker on `provider`, its events so far being `events`,
/// because the previous turn's `peak_context` was above `threshold`: the
/// turn the boundary follows (`after_turn`, the last finished; `None`
/// before any), why (`peak_context` and `threshold`), the provider it
/// stays on, the name of the new session (`session`,
/// [`turn::session_name`] after one more start) and which renewal of the
/// run it is (`count`).
pub fn renewed_payload(
    run_id: &str,
    provider: Provider,
    events: &[RunEvent],
    (peak_context, threshold): (u64, u64),
) -> Value {
    let after_turn = events
        .iter()
        .rfind(|e| e.kind == event_kind::TURN_FINISHED)
        .and_then(|e| e.payload["turn"].as_u64());
    json!({
        "after_turn": after_turn,
        "peak_context": peak_context,
        "threshold": threshold,
        "provider": provider,
        "session": turn::session_name(run_id, provider_switch::session_starts(events) + 1),
        "count": renewals(events) + 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;

    fn event(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    /// Only a peak above the threshold starts a new session: at it, or
    /// below, the session goes on.
    #[test]
    fn a_new_session_starts_only_above_the_threshold() {
        let threshold = Some(150_000);
        assert_eq!(
            next_session(Some(150_001), threshold),
            NextSession::Fresh {
                peak_context: 150_001,
                threshold: 150_000
            }
        );
        assert_eq!(next_session(Some(150_000), threshold), NextSession::Same);
        assert_eq!(next_session(Some(149_999), threshold), NextSession::Same);
        assert_eq!(next_session(Some(0), threshold), NextSession::Same);
    }

    /// A peak not measured, a negative one, or no threshold keeps the
    /// session.
    #[test]
    fn an_unmeasured_peak_or_no_threshold_keeps_the_session() {
        assert_eq!(next_session(None, Some(150_000)), NextSession::Same);
        assert_eq!(next_session(Some(-1), Some(150_000)), NextSession::Same);
        assert_eq!(next_session(Some(900_000), None), NextSession::Same);
        assert_eq!(next_session(None, None), NextSession::Same);
        assert_eq!(FreshSessionConfig::default().peak_context_above, None);
    }

    /// The previous turn is the last `turn_finished`: its peak, even when
    /// an earlier turn's was larger, and none when it was not measured.
    #[test]
    fn the_previous_turns_peak_is_the_last_finished_turns() {
        assert_eq!(previous_peak_context(&[]), None);
        let mut events = vec![
            event(
                event_kind::TURN_FINISHED,
                json!({"turn": 1, "peak_context": 400_000}),
            ),
            event(event_kind::TURN_STARTED, json!({"turn": 2})),
        ];
        assert_eq!(previous_peak_context(&events), Some(400_000));
        events.push(event(
            event_kind::TURN_FINISHED,
            json!({"turn": 2, "peak_context": 90_000}),
        ));
        assert_eq!(previous_peak_context(&events), Some(90_000));
        events.push(event(
            event_kind::TURN_FINISHED,
            json!({"turn": 3, "peak_context": null, "context_reason": "no_stream"}),
        ));
        assert_eq!(previous_peak_context(&events), None);
        // A new session (renewed or switched) has no previous turn of its
        // own until one finishes: the large peak of the one it left does
        // not renew it again.
        events.push(event(
            event_kind::TURN_FINISHED,
            json!({"turn": 4, "peak_context": 500_000}),
        ));
        for boundary in [event_kind::SESSION_RENEWED, event_kind::PROVIDER_SWITCHED] {
            let mut renewed = events.clone();
            renewed.push(event(boundary, json!({})));
            assert_eq!(previous_peak_context(&renewed), None, "{boundary}");
            assert_eq!(
                next_session(previous_peak_context(&renewed), Some(150_000)),
                NextSession::Same
            );
            renewed.push(event(
                event_kind::TURN_FINISHED,
                json!({"turn": 5, "peak_context": 160_000}),
            ));
            assert_eq!(previous_peak_context(&renewed), Some(160_000));
        }
    }

    /// The record names the boundary (the last finished turn), why, the
    /// provider kept and the new session, whose name differs from every
    /// earlier one of the run; renewals are counted from the records.
    #[test]
    fn a_renewal_records_its_boundary_reason_and_session() {
        let mut events = vec![
            event(event_kind::TURN_STARTED, json!({"turn": 1})),
            event(
                event_kind::TURN_FINISHED,
                json!({"turn": 1, "peak_context": 200_000}),
            ),
        ];
        let first = renewed_payload("run-1", Provider::Codex, &events, (200_000, 150_000));
        let name = turn::session_name("run-1", 1);
        assert_ne!(name, "run-1");
        assert_eq!(
            first,
            json!({"after_turn": 1, "peak_context": 200_000, "threshold": 150_000,
                "provider": "codex", "session": name, "count": 1})
        );
        events.push(event(event_kind::SESSION_RENEWED, first));
        events.push(event(event_kind::PROVIDER_SWITCHED, json!({"turn": 2})));
        events.push(event(
            event_kind::TURN_FINISHED,
            json!({"turn": 3, "peak_context": 160_000}),
        ));
        let second = renewed_payload("run-1", Provider::Claude, &events, (160_000, 150_000));
        assert_eq!(second["after_turn"], 3);
        assert_eq!(second["provider"], "claude");
        assert_eq!(second["count"], 2);
        assert_eq!(second["session"], turn::session_name("run-1", 3));
        assert_ne!(second["session"], name);
        assert_eq!(renewals(&events), 1);
        assert_eq!(
            renewed_payload("run-1", Provider::Claude, &[], (1, 0))["after_turn"],
            json!(null)
        );
    }
}
