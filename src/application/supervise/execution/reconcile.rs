//! What the queue service did without its answer reaching the supervisor
//! (ADR-t3102-3 decision 5): a request written to the service whose answer
//! is lost (the service stops or is replaced while it answers, the
//! connection ends) may have been done or not, and the supervisor reads
//! both as the service out of reach. Neither guessing it done nor sending
//! it again is safe: a claim done but unanswered leaves a run under this
//! supervisor's lease in no slot, which its heartbeat keeps fresh so no
//! other supervisor adopts it, and a transition sent again after it was
//! done is refused or recorded twice.
//!
//! So once the service answers again, before anything else, the
//! supervisor reconciles the runs its registration leases with its slots
//! ([`plan`]): a run in no slot is taken back into one ([`orphan`]), and a
//! slot whose step the service did not answer is judged by what that step
//! may have done ([`Doubt`]). A request that no connection took, and a
//! read, did nothing, so a step that tried no write and failed on one
//! changed nothing on the queue: a slot holding work in flight (an e2e,
//! a validation, a verification, a landing, a review's or a recovery's
//! job, [`Phase::holds_work`]) is kept as it was, and its step looks at the
//! work again, whose start and result stay this slot's. Any other slot in
//! doubt is never stepped again: its step may have changed the slot before
//! the call that went unanswered (taken the session of an `/exit` and
//! what follows it), and a write may have been done without moving the
//! run's status or its lease (a resume's end that keeps the run
//! `needs_session` under the same lease), so neither the slot nor the
//! run's status tells whether to send it again. It is rebuilt from the
//! queue instead, as the process an exec hands the runs to rebuilds them,
//! so what was recorded is read back and not recorded again, and work whose
//! end was not recorded is done again; or it is let go when the lease is
//! no longer this supervisor's.
//!
//! A request that went unanswered without failing a pass (a write whose
//! failure is only logged, such as giving a run up, or one of a landing's
//! on its own thread) reconciles at the next pass too ([`Unanswered::look`],
//! once a pass joined what its threads did): a run it left under this
//! supervisor's lease in no slot is found again.
//!
//! A step never fills a read that did not reach the service, or a write
//! left unanswered, with a settled result (unreadable, used up, failed):
//! it refuses no repair and gives no lease back on it, but fails and
//! leaves the run to this reconciliation. A provisioning cut off this way
//! does not stop the claims either. A reconciliation the service stops
//! answering in the middle keeps the leases and starts again at the next
//! pass, while the heartbeat goes on writing as it does when busy, until
//! the leases would go stale ([`HeartbeatPolicy::retries`]).

use std::collections::{BTreeMap, BTreeSet};

use super::*;

/// What the queue service may have done unanswered since the last
/// reconciliation.
#[derive(Debug, Default)]
pub(super) struct Unanswered {
    /// A pass ended on the service out of reach.
    lost: bool,
    /// The runs whose slot's step the service did not answer, with what
    /// the step may have done.
    in_doubt: BTreeMap<RunId, Doubt>,
    /// The slots a takeover rebuilt whose wait the service did not answer
    /// the read of ([`Supervisor::restore_waiting`]): read again.
    waits: BTreeSet<RunId>,
}

impl Unanswered {
    /// A pass ended on the service out of reach: the next pass reconciles.
    pub(super) fn lost(&mut self) {
        self.lost = true;
    }

    /// A request went unanswered since the last look, on any thread and
    /// swallowed or not: the next pass reconciles. Looked at once a pass
    /// ended, after its slots joined what their threads did (a landing's
    /// end), so what such a request left is there to be found.
    pub(super) fn look(&mut self) {
        if crate::application::supervisor_service::take_unanswered() {
            self.lost = true;
        }
    }

    /// The wait of `run`'s slot, rebuilt by a takeover, is read again.
    pub(super) fn restore_wait(&mut self, run: &RunId) {
        self.waits.insert(run.clone());
    }

    /// The step of `run`'s slot ended on the service out of reach, having
    /// done what `doubt` says.
    pub(super) fn doubt(&mut self, run: &RunId, doubt: Doubt) {
        self.in_doubt.insert(run.clone(), doubt);
    }
}

/// What a slot's step the service did not answer may have done on the
/// queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Doubt {
    /// Nothing: it tried no write, and the request that failed was not
    /// done ([`crate::application::supervisor_service::undone`]).
    NothingDone,
    /// A write it tried may have been done, or was done.
    MaybeDone,
}

impl Doubt {
    /// What a step that tried `writes` writes and failed with `error` may
    /// have done.
    pub(super) fn of(writes: u64, error: &anyhow::Error) -> Self {
        if writes == 0 && crate::application::supervisor_service::undone(error) {
            Self::NothingDone
        } else {
            Self::MaybeDone
        }
    }
}

/// What a reconciliation does ([`plan`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Plan {
    /// Runs leased to this supervisor in no slot: taken back ([`orphan`]).
    pub take_over: Vec<RunId>,
    /// Slots in doubt whose run is still this supervisor's: rebuilt from
    /// the queue.
    pub rebuild: Vec<RunId>,
    /// Slots in doubt whose run's lease is no longer this supervisor's.
    pub disown: Vec<RunId>,
}

/// The reconciliation of `own`, the runs this supervisor's registration
/// leases, with `slots`, the runs its slots hold, `working` those of them
/// holding work in flight ([`Phase::holds_work`]), and `in_doubt` the
/// slots whose step was not answered. A slot in doubt whose step did
/// nothing and that holds work is kept, as is a slot not in doubt, whose
/// own step tells a lease lost by itself.
pub(super) fn plan(
    own: &[RunId],
    slots: &[RunId],
    working: &[RunId],
    in_doubt: &BTreeMap<RunId, Doubt>,
) -> Plan {
    let mut plan = Plan::default();
    for run in own {
        if !slots.contains(run) {
            plan.take_over.push(run.clone());
        }
    }
    for run in slots {
        match in_doubt.get(run) {
            None => (),
            Some(Doubt::NothingDone) if working.contains(run) => (),
            Some(_) if own.contains(run) => plan.rebuild.push(run.clone()),
            Some(_) => plan.disown.push(run.clone()),
        }
    }
    plan
}

/// What becomes of a run leased to this supervisor that no slot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Orphan {
    /// A claim whose answer was lost: nothing of it is provisioned yet, so
    /// it is provisioned now, as the claim would have been.
    Provision,
    /// A provisioning cut off between its records and its processes: its
    /// worktree and wrapper may be half made, so it is given up on like
    /// any provisioning that failed, its lease given back for the triage,
    /// without stopping the claims.
    Abandon,
    /// Any other run: taken over the way an exec takes its runs over
    /// ([`Supervisor::take_over_own`]).
    TakeOver,
}

/// [`Orphan`] of a run in `status`.
pub(super) fn orphan(status: RunStatus) -> Orphan {
    match status {
        RunStatus::Claimed => Orphan::Provision,
        RunStatus::Starting => Orphan::Abandon,
        _ => Orphan::TakeOver,
    }
}

impl Supervisor<'_> {
    /// Reconcile this supervisor's leases with its slots ([`plan`]) after
    /// a pass the queue service did not answer, once it answers. The
    /// service out of reach again leaves the rest for the next pass: what
    /// was done is in a slot by then, and what was not is found again.
    /// `stopping` whether the supervisor drains: a claim found again is
    /// then given up on rather than provisioned, as is one found after a
    /// provisioning stopped the claims. A run whose lease could not be
    /// given back keeps the reconciliation for the next pass.
    pub(super) fn reconcile_unanswered(&mut self, stopping: bool) -> Result<()> {
        if !self.unanswered.lost
            && self.unanswered.in_doubt.is_empty()
            && self.unanswered.waits.is_empty()
        {
            return Ok(());
        }
        // The waits a takeover could not read, read again in their slots.
        while let Some(id) = self.unanswered.waits.pop_first() {
            let Some(mut slot) = self.claim.slots.take(&id) else {
                continue;
            };
            let restored = self.restore_waiting(&mut slot, true);
            self.claim.slots.admit(slot);
            if let Err(error) = restored {
                if crate::application::supervisor_service::unreachable(&error) {
                    self.unanswered.waits.insert(id);
                    return Err(error);
                }
                warn!(run_id = %id, error = %format_args!("{error:#}"), "run {id}: its wait could not be read again: {error:#}");
            }
        }
        let own = self.queue.runs_leased_by(self.registration.token())?;
        let owned: Vec<RunId> = own.iter().map(|run| run.id().clone()).collect();
        let working: Vec<RunId> = self
            .claim
            .slots
            .iter()
            .filter(|slot| slot.phase.holds_work())
            .map(|slot| slot.run.id().clone())
            .collect();
        let plan = plan(
            &owned,
            &self.claim.slots.runs(),
            &working,
            &self.unanswered.in_doubt,
        );
        for id in &plan.disown {
            if let Some(mut slot) = self.claim.slots.take(id) {
                self.stop_slot_job(&mut slot);
                self.disown(&slot);
            }
        }
        for id in &plan.rebuild {
            if let Some(mut slot) = self.claim.slots.take(id) {
                info!(run_id = %id, "run {id}: the queue service did not answer its slot's step; the slot is rebuilt from the queue");
                self.stop_slot_job(&mut slot);
            }
        }
        // Kept slots go on with their work; the rest are decided above.
        self.unanswered.in_doubt.clear();
        for run in own
            .iter()
            .filter(|run| plan.take_over.contains(run.id()) || plan.rebuild.contains(run.id()))
        {
            let given_up = match orphan(run.status()) {
                Orphan::Provision if self.claim.claiming && !stopping => {
                    info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {}: its claim was done unanswered; it is provisioned now", run.id(), run.task_id());
                    let load = (self.host.load_average())();
                    self.provision_claimed(run, load)?;
                    None
                }
                Orphan::Provision => Some(format!(
                    "run {} was claimed unanswered while no claim may start",
                    run.id()
                )),
                Orphan::Abandon => Some(format!(
                    "run {} provisioning was cut off when the queue service did not answer",
                    run.id()
                )),
                Orphan::TakeOver => {
                    self.take_over_own(run, None, None)?;
                    None
                }
            };
            // Given up here: one whose record the service did not answer
            // keeps its lease, and the next pass, which reads the request
            // unanswered, finds it again.
            if let Some(message) = given_up {
                warn!(run_id = %run.id(), "{message}; its lease is given back for the triage");
                self.abandon(run, message, &ReasonCode::Other.into());
            }
        }
        self.unanswered.lost = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(ids: &[&str]) -> Vec<RunId> {
        ids.iter().map(|id| RunId::new(*id).unwrap()).collect()
    }

    /// The slots in doubt `ids`, each as `doubt` says.
    fn doubt_as(ids: &[&str], doubt: Doubt) -> BTreeMap<RunId, Doubt> {
        runs(ids).into_iter().map(|run| (run, doubt)).collect()
    }

    /// The slots in doubt `ids`, whose steps may have written.
    fn doubt(ids: &[&str]) -> BTreeMap<RunId, Doubt> {
        doubt_as(ids, Doubt::MaybeDone)
    }

    /// A claim the service did, whose answer was lost: the run is this
    /// supervisor's but in no slot, and is taken back into one; a run in
    /// a slot is not taken twice.
    #[test]
    fn a_leased_run_in_no_slot_is_taken_back() {
        assert_eq!(
            plan(
                &runs(&["claimed", "slotted"]),
                &runs(&["slotted"]),
                &[],
                &doubt(&[])
            ),
            Plan {
                take_over: runs(&["claimed"]),
                ..Plan::default()
            }
        );
    }

    /// A transition whose answer was lost is not sent again, whatever the
    /// queue's status and lease say: one that moved the run (a session's
    /// end) and one that keeps its status and lease (an approved resume's
    /// end, which stays `needs_session` under the same lease) are rebuilt
    /// from the queue alike, so neither is recorded a second time.
    #[test]
    fn a_step_not_answered_is_rebuilt_never_sent_again() {
        let own = runs(&["session-ended", "resume-ended", "untouched"]);
        let slots = runs(&["session-ended", "resume-ended", "untouched"]);
        let plan = plan(
            &own,
            &slots,
            &[],
            &doubt(&["session-ended", "resume-ended"]),
        );
        assert_eq!(
            plan,
            Plan {
                rebuild: runs(&["session-ended", "resume-ended"]),
                ..Plan::default()
            }
        );
        // The slot not in doubt is stepped as before.
        assert!(!plan.rebuild.contains(&RunId::new("untouched").unwrap()));
    }

    fn event(id: i64, kind: &str, payload: serde_json::Value) -> RunEvent {
        RunEvent {
            id: crate::domain::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    /// An approved run's resume that ended resolved on its last attempt,
    /// done by the service without its answer: the run stays
    /// `needs_session` under the same lease, and the supervisor never
    /// wrote its `landing_queued`. Its slot rebuilt from these events waits
    /// for the landing slot: it is neither resumed again (which would
    /// record a second `resume_finished`, or use the resumes up and send the
    /// run to its recovery) nor given back.
    #[test]
    fn an_approved_resume_ended_unanswered_is_rebuilt_waiting_to_land() {
        let events = [
            event(1, "landing_decided", json!({"decision": "approve"})),
            event(2, "resume_started", json!({"attempt": 3})),
            event(
                3,
                "resume_finished",
                json!({"attempt": 3, "outcome": "resolved", "approved": true,
                       "status": "needs_session"}),
            ),
        ];
        let id = RunId::new("resumed").unwrap();
        let plan = plan(
            std::slice::from_ref(&id),
            std::slice::from_ref(&id),
            &[],
            &doubt(&["resumed"]),
        );
        assert_eq!(plan.rebuild, [id]);
        assert_eq!(orphan(RunStatus::NeedsSession), Orphan::TakeOver);
        let queued = super::super::handoff_slots::queued(RunStatus::NeedsSession, &events);
        assert_eq!(
            super::super::handoff_slots::takeover(RunStatus::NeedsSession, None, false, queued),
            super::super::handoff_slots::Takeover::Land
        );
        // Before the resume ended, the same run is resumed as before.
        assert_eq!(
            super::super::handoff_slots::queued(RunStatus::NeedsSession, &events[..2]),
            super::super::handoff_slots::Queued::No
        );
    }

    /// A step that gave the lease up unanswered (the run came to rest)
    /// lets the slot go, writing nothing more; a slot not in doubt is left
    /// to its own step, whatever the queue says.
    #[test]
    fn a_slot_whose_lease_went_unanswered_is_let_go_and_others_left_alone() {
        assert_eq!(
            plan(
                &runs(&["moved"]),
                &runs(&["rested", "moved", "gone"]),
                &[],
                &doubt(&["rested"])
            ),
            Plan {
                disown: runs(&["rested"]),
                ..Plan::default()
            }
        );
    }

    /// A step that tried no write and failed on a read (the lease looked
    /// at while an e2e runs) or on a request no connection took did
    /// nothing: a slot holding work in flight is kept as it was, so its
    /// e2e is not started again and its result is the slot's to record;
    /// the lease is not judged from it. One holding no work is rebuilt from
    /// the queue, as its step may have taken what it held before the call.
    /// A step that tried a write, done or not, or whose request may have
    /// been done, is rebuilt even when it holds work.
    #[test]
    fn a_step_that_did_nothing_keeps_its_work_and_one_that_may_have_written_is_rebuilt() {
        let unanswered = anyhow::Error::new(
            crate::application::supervisor_service::ServiceUnreachable::not_done("read"),
        );
        let lost = anyhow::Error::new(
            crate::application::supervisor_service::ServiceUnreachable::maybe_done("written"),
        );
        assert_eq!(Doubt::of(0, &unanswered), Doubt::NothingDone);
        assert_eq!(Doubt::of(1, &unanswered), Doubt::MaybeDone);
        assert_eq!(Doubt::of(0, &lost), Doubt::MaybeDone);
        let refused = anyhow::anyhow!("the queue service refused the request");
        assert_eq!(Doubt::of(0, &refused), Doubt::MaybeDone);

        let own = runs(&["e2e", "landed", "exiting"]);
        let slots = runs(&["e2e", "landed", "exiting", "written"]);
        // The landing gave its lease back itself; its slot reads the run
        // it left once the service answers.
        let working = runs(&["e2e", "landed", "written"]);
        let mut in_doubt = doubt_as(&["e2e", "landed", "exiting"], Doubt::NothingDone);
        in_doubt.insert(RunId::new("written").unwrap(), Doubt::MaybeDone);
        assert_eq!(
            plan(&own, &slots, &working, &in_doubt),
            Plan {
                rebuild: runs(&["exiting"]),
                disown: runs(&["written"]),
                ..Plan::default()
            }
        );
        let own = runs(&["e2e", "written"]);
        assert_eq!(
            plan(&own, &runs(&["e2e", "written"]), &working, &in_doubt),
            Plan {
                rebuild: runs(&["written"]),
                ..Plan::default()
            }
        );
    }

    /// A claim answered or not is provisioned once: only a run nothing
    /// was provisioned for is provisioned now, a half-made one is given up
    /// on, and the rest are taken over as after an exec.
    #[test]
    fn only_an_unprovisioned_claim_is_provisioned_again() {
        assert_eq!(orphan(RunStatus::Claimed), Orphan::Provision);
        assert_eq!(orphan(RunStatus::Starting), Orphan::Abandon);
        for status in [
            RunStatus::Running,
            RunStatus::Validating,
            RunStatus::NeedsSession,
            RunStatus::AwaitingIntegration,
        ] {
            assert_eq!(orphan(status), Orphan::TakeOver, "{status:?}");
        }
    }
}
