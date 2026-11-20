//! The standing inbox attention for requests and revises that need a provider.

use super::PlanningEnv;
use crate::domain::{
    EventKind, Provider,
    event_kind::{PLANNER_START_KINDS, PlannerProviderReason, PlannerStartBlocked},
    provider_switch::SwitchReason,
};
use anyhow::Result;
use serde_json::json;

#[derive(Debug, PartialEq, Eq)]
enum StartBlockedStep {
    Nothing,
    Record(PlannerStartBlocked),
    Resume,
}

fn start_blocked_step(
    current: PlannerStartBlocked,
    open: Option<&PlannerStartBlocked>,
) -> StartBlockedStep {
    if current.requests.is_empty() && current.proposals.is_empty() {
        return if open.is_some() {
            StartBlockedStep::Resume
        } else {
            StartBlockedStep::Nothing
        };
    }
    if open.is_some_and(|open| {
        current.requests == open.requests
            && current.proposals == open.proposals
            && current
                .providers
                .iter()
                .all(|reason| open.providers.contains(reason))
            && open
                .providers
                .iter()
                .all(|reason| current.providers.contains(reason))
    }) {
        StartBlockedStep::Nothing
    } else {
        StartBlockedStep::Record(current)
    }
}

pub(super) fn include_reasons(
    blocked: &mut PlannerStartBlocked,
    reasons: Vec<(Provider, SwitchReason)>,
) {
    for (provider, reason) in reasons {
        let entry = PlannerProviderReason { provider, reason };
        if !blocked.providers.contains(&entry) {
            blocked.providers.push(entry);
        }
    }
}

pub(super) fn record_start_wait(env: &PlanningEnv<'_>, blocked: PlannerStartBlocked) -> Result<()> {
    let open =
        PlannerStartBlocked::from_latest(env.queue.latest_queue_event(&PLANNER_START_KINDS)?)?;
    let (kind, payload) = match start_blocked_step(blocked, open.as_ref()) {
        StartBlockedStep::Nothing => return Ok(()),
        StartBlockedStep::Record(blocked) => (EventKind::PlannerStartBlocked, json!(blocked)),
        StartBlockedStep::Resume => (EventKind::PlannerStartResumed, json!({})),
    };
    // The next pass recomputes the wait if the queue could not take it.
    env.queue.record_queue_event_now(kind, payload)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ProposalId, RequestId};

    #[test]
    fn records_changes_to_the_provider_or_work_sets_and_closes_an_empty_wait() {
        let waits = || PlannerStartBlocked {
            providers: vec![
                PlannerProviderReason {
                    provider: Provider::Claude,
                    reason: SwitchReason::Disabled,
                },
                PlannerProviderReason {
                    provider: Provider::Codex,
                    reason: SwitchReason::ExecutableMissing,
                },
            ],
            requests: [RequestId::new(1)].into(),
            proposals: [ProposalId::new(2)].into(),
        };
        let open = waits();
        assert_eq!(
            start_blocked_step(waits(), None),
            StartBlockedStep::Record(waits())
        );
        let mut reordered = waits();
        reordered.providers.reverse();
        assert_eq!(
            start_blocked_step(reordered, Some(&open)),
            StartBlockedStep::Nothing
        );

        let mut changed_reason = waits();
        changed_reason.providers[1].reason = SwitchReason::Authentication;
        let mut lost_provider = waits();
        lost_provider.providers.pop();
        let mut added_request = waits();
        added_request.requests.insert(RequestId::new(3));
        let mut lost_proposal = waits();
        lost_proposal.proposals.clear();
        for changed in [changed_reason, lost_provider, added_request, lost_proposal] {
            assert_eq!(
                start_blocked_step(waits(), Some(&changed)),
                StartBlockedStep::Record(waits())
            );
        }
        assert_eq!(
            start_blocked_step(Default::default(), Some(&open)),
            StartBlockedStep::Resume
        );
        assert_eq!(
            start_blocked_step(Default::default(), None),
            StartBlockedStep::Nothing
        );
    }
}
