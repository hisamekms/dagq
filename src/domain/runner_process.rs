//! The runner process's decisions (ADR-t3987-1 decisions 1 and 2), without
//! I/O. The loop in [`crate::application::runner_process`] reads its clock
//! and the stop request into [`State`], asks [`choose`] for the one move to
//! make now (an event the time or the stop request already settles, one call
//! to a port, or a wait), makes it, feeds the call's result to [`next`] and
//! applies the decision with [`settle`]. Every call is chosen again from a
//! fresh reading, so a call that took long is followed by the checks of the
//! stop request, the drain limit and the lease's end before anything else
//! is sent or started.
//!
//! - The runner never chooses work and never extends its lease by itself:
//!   a lease's end is counted on the runner's monotonic clock from when the
//!   call that the control side answered was sent, plus the remaining
//!   seconds the answer gave ([`lease_deadline_ms`]). No answer, no renewal.
//! - The runner sends no time of its own clock to the control side.
//! - A lost lease (`lease_lost`, a refusal of the run, or the counted end
//!   passing) stops the work and throws away the clone and the run's
//!   directory: changes the control side has not accepted are not carried
//!   over, and no report of that generation is sent afterwards.
//! - A stop request (SIGTERM) asks no new work and starts no assigned one;
//!   the running turn's end is reported and the lease handed back. Past the
//!   drain limit the work is stopped without a report, and the lease ends by
//!   its expiry.
//! - A refresh is sent again only when it surely rotated nothing; one whose
//!   outcome is unknown (no answer, or an answer that could not be saved)
//!   may have used the refresh credential, so the work is stopped and the
//!   process ends with the guide to enroll again.
//! - The heartbeat keeps its interval while a turn's report is retried.
//! - The hand-back is a call like the others: a refused token is taken again
//!   and the hand-back sent again, a call without an answer is retried until
//!   the lease's end, and a revocation or a version refusal ends the process
//!   with its guide.

use std::time::Duration;

/// How often a held lease's heartbeat is sent: well inside the control
/// side's 90-second lease.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
/// The wait before asking again after an empty answer.
pub const EMPTY_WAIT: Duration = Duration::from_secs(5);
/// The wait before a failed call (no answer) is tried again.
pub const RETRY_DELAY: Duration = Duration::from_secs(5);
/// The longest wait between two readings of the stop request.
pub const TICK: Duration = Duration::from_secs(1);
/// How often a waiting call asks whether to give up ([`interrupts`]).
pub const GIVE_UP_POLL: Duration = Duration::from_millis(100);

/// What a call waiting for its answer does next ([`answer_wait`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnswerWait {
    /// Take the answer that came.
    Take,
    /// Give up: the answer, if one came, is not taken.
    GiveUp,
    /// Wait for the answer no longer than this.
    Wait(Duration),
}

/// The next step of a call given `within` after `elapsed`, `give_up` read
/// now ([`interrupts`]) and whether its answer has come. It gives up once the
/// budget has passed or `give_up` holds, even with an answer in hand, so
/// nothing that came at or after a deadline is taken (a refresh's rotated
/// credential is then not saved, and its outcome is unknown); it takes an
/// answer that came before; otherwise it waits no longer than
/// [`GIVE_UP_POLL`] and the time left.
pub fn answer_wait(
    elapsed: Duration,
    within: Duration,
    give_up: bool,
    answered: bool,
) -> AnswerWait {
    if give_up || elapsed >= within {
        AnswerWait::GiveUp
    } else if answered {
        AnswerWait::Take
    } else {
        AnswerWait::Wait(GIVE_UP_POLL.min(within - elapsed))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Holding no lease; asking for work.
    Waiting,
    /// Holding a lease; the work runs.
    Running,
    /// Holding a lease after a stop request; waiting on the turn's end.
    Draining,
    /// The work is stopped and thrown away; handing the lease back, then
    /// ending with the given ending, or asking for work again with none.
    Releasing(Option<Ending>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A call that changes nothing here (a token taken, no turn ended yet).
    Idle,
    /// The poll was answered with no work.
    Empty,
    /// The poll was answered with an assignment.
    Assigned,
    /// The assigned work started.
    Started,
    /// A heartbeat was accepted.
    Renewed,
    /// The work's turn ended; its report waits to be sent.
    TurnEnded,
    /// The turn's report was accepted; `finished` when the work has no
    /// more turns.
    ReportAccepted {
        finished: bool,
    },
    /// The hand-back was accepted.
    Released,
    /// The work could not start, failed outside a turn, or its report was
    /// refused for what it carried.
    WorkFailed,
    /// A stop came before the assigned work started.
    Unstarted,
    StopRequested,
    /// The drain limit passed before the turn ended.
    DrainLimit,
    /// `lease_lost`, a refusal of the run, or the counted end passed.
    LeaseLost,
    /// The short token was refused; take a new one.
    Unauthenticated,
    /// The refresh credential was refused (revoked): enroll again.
    Revoked,
    /// A refresh got no answer, or its answer could not be saved: the
    /// control side may have used the refresh credential, and sending it
    /// again would revoke the runner. It is not sent again: enroll again.
    RefreshUnknown,
    /// The control side answers outside this binary's API versions.
    VersionRefused,
    /// A call got no answer.
    TransportFailed,
}

/// How the process ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// Stopped on request, the lease handed back (or never held).
    Stopped,
    /// The drain limit passed; the work was stopped without a report.
    DrainTimedOut,
    /// The runner's credential was revoked.
    EnrollAgain,
    /// The control side's API versions exclude this binary.
    VersionOutside,
}

impl Ending {
    /// The line the process leaves for the person who runs it.
    pub fn guide(self) -> &'static str {
        match self {
            Ending::Stopped => "runner stopped",
            Ending::DrainTimedOut => {
                "runner stopped: the turn did not end within the drain limit; its lease ends by expiry"
            }
            Ending::EnrollAgain => {
                "runner credential refused; obtain a new register-code and run dagq runner enroll again"
            }
            Ending::VersionOutside => {
                "the control side does not answer this dagq's API version; install a compatible dagq binary"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing more to do now.
    Continue,
    /// Ask again after [`EMPTY_WAIT`].
    WaitThenPoll,
    /// Try the failed call again after [`RETRY_DELAY`].
    RetryLater,
    /// Drop the short token and take a new one.
    Refresh,
    /// Stop the work and throw the work place away; the lease stays to be
    /// handed back.
    GiveUp,
    /// Hand back a lease whose work never started: there is nothing to stop,
    /// so its hand-back gets one call's budget ([`CALL_TIMEOUT`]) even past
    /// the drain limit, rather than leaving the lease to its expiry.
    GiveUpUnstarted,
    /// Stop the work, throw the work place away and forget the lease:
    /// nothing more of its generation is sent.
    Abandon,
    /// [`Action::Abandon`], then end.
    AbandonAndExit(Ending),
    /// Forget the lease (handed back or lost).
    Forget,
    /// End; any lease left is forgotten.
    Exit(Ending),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub phase: Phase,
    pub action: Action,
}

const fn decide(phase: Phase, action: Action) -> Decision {
    Decision { phase, action }
}

/// The next move after an event. Events that do not belong to the phase
/// change nothing.
pub fn next(phase: Phase, event: Event) -> Decision {
    use Action::*;
    use Event as E;
    use Phase::*;
    match (phase, event) {
        (_, E::TransportFailed) => decide(phase, RetryLater),
        (_, E::Unauthenticated) => decide(phase, Refresh),
        (Waiting | Releasing(_), E::Revoked | E::RefreshUnknown) => {
            decide(Waiting, Exit(Ending::EnrollAgain))
        }
        (Waiting | Releasing(_), E::VersionRefused) => {
            decide(Waiting, Exit(Ending::VersionOutside))
        }
        (_, E::Revoked | E::RefreshUnknown) => decide(Waiting, AbandonAndExit(Ending::EnrollAgain)),
        (_, E::VersionRefused) => decide(Waiting, AbandonAndExit(Ending::VersionOutside)),

        (Waiting, E::Empty) => decide(Waiting, WaitThenPoll),
        (Waiting, E::Assigned) => decide(Running, Continue),
        (Waiting, E::StopRequested) => decide(Waiting, Exit(Ending::Stopped)),
        (Waiting, _) => decide(Waiting, Continue),

        (Running, E::ReportAccepted { finished: true }) | (Running, E::WorkFailed) => {
            decide(Releasing(None), GiveUp)
        }
        (Running, E::StopRequested) => decide(Draining, Continue),
        (Running, E::LeaseLost) => decide(Waiting, Abandon),
        (Running, _) => decide(Running, Continue),

        (Draining, E::ReportAccepted { .. } | E::WorkFailed) => {
            decide(Releasing(Some(Ending::Stopped)), GiveUp)
        }
        (Draining, E::Unstarted) => decide(Releasing(Some(Ending::Stopped)), GiveUpUnstarted),
        (Draining, E::DrainLimit) => decide(Waiting, AbandonAndExit(Ending::DrainTimedOut)),
        (Draining, E::LeaseLost) => decide(Waiting, AbandonAndExit(Ending::Stopped)),
        (Draining, _) => decide(Draining, Continue),

        (Releasing(Some(ending)), E::Released | E::LeaseLost | E::DrainLimit) => {
            decide(Waiting, Exit(ending))
        }
        (Releasing(None), E::Released | E::LeaseLost) => decide(Waiting, Forget),
        (Releasing(None), E::StopRequested) => decide(Releasing(Some(Ending::Stopped)), Continue),
        (Releasing(_), _) => decide(phase, Continue),
    }
}

/// A held lease's times on the runner's monotonic clock and what is still
/// owed on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease {
    /// The counted end ([`lease_deadline_ms`]).
    pub deadline_ms: u64,
    pub next_heartbeat_ms: u64,
    /// Before this, a report or hand-back that got no answer is not sent
    /// again (a heartbeat without an answer puts off its next one instead).
    pub retry_ms: u64,
    /// When the work is next asked whether its turn ended.
    pub next_check_ms: u64,
    /// Assigned, not started yet.
    pub unstarted: bool,
    /// A turn ended and its report is not accepted yet.
    pub report_pending: bool,
}

impl Lease {
    /// A lease granted by the poll sent at `sent_ms`, answered at `now_ms`.
    pub fn granted(sent_ms: u64, remaining_seconds: i64, now_ms: u64) -> Self {
        Self {
            deadline_ms: lease_deadline_ms(sent_ms, remaining_seconds),
            next_heartbeat_ms: now_ms.saturating_add(millis(HEARTBEAT_INTERVAL)),
            retry_ms: 0,
            next_check_ms: 0,
            unstarted: true,
            report_pending: false,
        }
    }

    /// An accepted heartbeat or report sent at `sent_ms`, answered at
    /// `now_ms`; without remaining seconds the end does not move.
    pub fn renew(&mut self, sent_ms: u64, remaining_seconds: Option<i64>, now_ms: u64) {
        if let Some(remaining) = remaining_seconds {
            self.deadline_ms = lease_deadline_ms(sent_ms, remaining);
        }
        self.next_heartbeat_ms = now_ms.saturating_add(millis(HEARTBEAT_INTERVAL));
    }
}

/// What the loop knows: its phase, the readings it took, and its lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub phase: Phase,
    /// When the stop request came, on the loop's monotonic clock (read
    /// from the stop port, which records the time it came, not when the
    /// loop read it). The drain's end counts from it.
    pub stop_requested_at: Option<u64>,
    pub drain_deadline_ms: Option<u64>,
    pub has_token: bool,
    /// Before this, a refresh that surely rotated nothing (its credential
    /// was not sent, or the control side answered that nothing was
    /// committed) is not tried again. A refresh whose outcome is unknown is
    /// never sent again ([`Event::RefreshUnknown`]).
    pub token_retry_ms: u64,
    /// Before this, no poll is sent (after an empty or failed one).
    pub poll_after_ms: u64,
    pub lease: Option<Lease>,
}

impl State {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Waiting,
            stop_requested_at: None,
            drain_deadline_ms: None,
            has_token: false,
            token_retry_ms: 0,
            poll_after_ms: 0,
            lease: None,
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

/// The calls the loop makes, one per move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    Refresh,
    Poll,
    Start,
    Heartbeat,
    Report,
    Release,
    CheckTurn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// The readings already settle an event; no call is made.
    Observe(Event),
    Call(Call),
    /// Nothing is due; read again after this.
    Wait(Duration),
}

fn wait_until(now_ms: u64, times: &[u64]) -> Choice {
    let earliest = times
        .iter()
        .copied()
        .filter(|&at| at > now_ms)
        .min()
        .unwrap_or(u64::MAX);
    Choice::Wait(Duration::from_millis(
        earliest.saturating_sub(now_ms).min(millis(TICK)),
    ))
}

/// `call` once `at_ms` has come; until then a wait that also wakes at the
/// lease's end and the drain's end, so neither is overshot.
fn due(now_ms: u64, at_ms: u64, call: Call, state: &State) -> Choice {
    if now_ms >= at_ms {
        Choice::Call(call)
    } else {
        let lease = state.lease.map_or(u64::MAX, |lease| lease.deadline_ms);
        let drain = state.drain_deadline_ms.unwrap_or(u64::MAX);
        wait_until(now_ms, &[at_ms, lease, drain])
    }
}

/// The one move to make at `now_ms`. In order: a stop request, the drain
/// limit (which also bounds the hand-back after a drain) and the lease's
/// end; then a token; then, by phase, a poll, the
/// hand-back, the start of the assigned work, the pending report, the
/// heartbeat and the check of the turn's end.
pub fn choose(now_ms: u64, state: &State) -> Choice {
    use Phase::*;
    let stop_counts = matches!(state.phase, Waiting | Running | Releasing(None));
    if state.stop_requested_at.is_some() && stop_counts {
        return Choice::Observe(Event::StopRequested);
    }
    if state.phase == Draining && state.lease.is_some_and(|lease| lease.unstarted) {
        return Choice::Observe(Event::Unstarted);
    }
    let drains = matches!(state.phase, Draining | Releasing(Some(_)));
    if drains && state.drain_deadline_ms.is_some_and(|end| now_ms >= end) {
        return Choice::Observe(Event::DrainLimit);
    }
    if state
        .lease
        .is_some_and(|lease| lease_ended(lease.deadline_ms, now_ms))
    {
        return Choice::Observe(Event::LeaseLost);
    }
    if !state.has_token {
        // A refresh cut at a deadline ends the runner; one that could not
        // get MIN_CALL before the work's deadline waits for that deadline
        // instead, and is sent after it with its whole budget.
        if let Some(end) = work_deadline_ms(state)
            && end.saturating_sub(now_ms) < millis(MIN_CALL)
        {
            return wait_until(now_ms, &[end]);
        }
        return due(now_ms, state.token_retry_ms, Call::Refresh, state);
    }
    let lease = match (state.phase, state.lease) {
        (Waiting, _) | (_, None) => {
            return due(now_ms, state.poll_after_ms, Call::Poll, state);
        }
        (_, Some(lease)) => lease,
    };
    if let Releasing(_) = state.phase {
        return due(now_ms, lease.retry_ms, Call::Release, state);
    }
    if lease.unstarted {
        return if state.phase == Draining {
            Choice::Observe(Event::Unstarted)
        } else {
            Choice::Call(Call::Start)
        };
    }
    // The heartbeat keeps its interval while a report waits to be sent again.
    let heartbeat_ms = lease.next_heartbeat_ms;
    if now_ms >= heartbeat_ms {
        return Choice::Call(Call::Heartbeat);
    }
    if lease.report_pending && now_ms >= lease.retry_ms {
        return Choice::Call(Call::Report);
    }
    let drain = state.drain_deadline_ms.unwrap_or(u64::MAX);
    if lease.report_pending {
        return wait_until(
            now_ms,
            &[lease.retry_ms, heartbeat_ms, lease.deadline_ms, drain],
        );
    }
    if now_ms >= lease.next_check_ms {
        return Choice::Call(Call::CheckTurn);
    }
    wait_until(
        now_ms,
        &[heartbeat_ms, lease.next_check_ms, lease.deadline_ms, drain],
    )
}

/// The longest a call waits for its answer.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// The least a call other than the heartbeat is given when the next
/// heartbeat is nearer.
pub const MIN_CALL: Duration = Duration::from_secs(5);

/// How long `call`, chosen at `now_ms`, may wait for its answer; the port
/// returns by then (an answer not come by then is one that never came). The
/// loop makes one call at a time, so this is what keeps the deadlines while
/// a call waits: no call outlasts the lease's end or the drain's end (where
/// the work must be stopped), and a call other than the heartbeat and the
/// refresh returns by the next heartbeat (or [`MIN_CALL`] from now, if
/// later), so the heartbeat keeps its interval. Never more than
/// [`CALL_TIMEOUT`]. A cut refresh has an unknown outcome and ends the
/// runner, so a refresh is cut only by [`work_deadline_ms`] (not by the
/// heartbeat, which needs its token anyway, nor by the end of a lease whose
/// work is already stopped), and [`choose`] does not start one closer than
/// [`MIN_CALL`] to that deadline.
pub fn call_budget(now_ms: u64, state: &State, call: Call) -> Duration {
    let mut until = now_ms.saturating_add(millis(CALL_TIMEOUT));
    if call == Call::Refresh {
        if let Some(end) = work_deadline_ms(state) {
            until = until.min(end);
        }
        return Duration::from_millis(until.saturating_sub(now_ms));
    }
    if let Some(lease) = state.lease {
        until = until.min(lease.deadline_ms);
    }
    if matches!(state.phase, Phase::Draining | Phase::Releasing(Some(_)))
        && let Some(end) = state.drain_deadline_ms
    {
        until = until.min(end);
    }
    if let (Some(lease), Phase::Running | Phase::Draining) = (state.lease, state.phase)
        && !matches!(call, Call::Heartbeat | Call::Refresh)
    {
        let soft = lease
            .next_heartbeat_ms
            .max(now_ms.saturating_add(millis(MIN_CALL)));
        until = until.min(soft);
    }
    Duration::from_millis(until.saturating_sub(now_ms))
}

/// The time by which the process must act whatever it waits on: the
/// lease's end while its work runs (the work is stopped then), and the
/// drain's end while a stop drains a turn or hands the lease back (the
/// process ends then).
pub fn work_deadline_ms(state: &State) -> Option<u64> {
    let lease = match (state.phase, state.lease) {
        (Phase::Running | Phase::Draining, Some(lease)) => Some(lease.deadline_ms),
        _ => None,
    };
    let drain = match state.phase {
        Phase::Draining | Phase::Releasing(Some(_)) => state.drain_deadline_ms,
        _ => None,
    };
    match (lease, drain) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Whether a call still waiting at `now_ms` must give up now so the loop
/// can act: the work's deadline ([`work_deadline_ms`]) has come, or, while
/// a lease is held, a stop request has reached its drain limit. The caller
/// reads the stop request afresh into `state` while the call waits, so a
/// stop that comes during a call is kept to its limit too. Holding no lease
/// (waiting for work) there is nothing to stop: a waiting poll or refresh
/// is left to its budget, so an assignment it brings is handed back and a
/// refresh is not cut into an unknown outcome.
pub fn interrupts(now_ms: u64, state: &State, drain_limit: Duration) -> bool {
    let holds = state.lease.is_some() && state.phase != Phase::Waiting;
    // Once the drain has begun its end is in the state (and in the work's
    // deadline); before that, a stop read afresh counts from its arrival.
    let stop_end = match (state.drain_deadline_ms, state.stop_requested_at) {
        (None, Some(at)) if holds => Some(drain_deadline_ms(at, drain_limit)),
        _ => None,
    };
    work_deadline_ms(state)
        .into_iter()
        .chain(stop_end)
        .any(|end| now_ms >= end)
}

/// Apply a decision to the state at `now_ms`. `failed` is the call whose
/// result led to it, so a retry is put off for that call only.
pub fn settle(
    state: &mut State,
    decision: Decision,
    failed: Option<Call>,
    now_ms: u64,
    drain_limit: Duration,
) {
    // The drain's end counts from when the stop request came (which may
    // be during a call, before the loop read it), whether a turn runs
    // (Draining) or the lease is being handed back.
    if state.drain_deadline_ms.is_none()
        && matches!(decision.phase, Phase::Draining | Phase::Releasing(Some(_)))
    {
        let requested = state.stop_requested_at.unwrap_or(now_ms).min(now_ms);
        state.drain_deadline_ms = Some(drain_deadline_ms(requested, drain_limit));
    }
    state.phase = decision.phase;
    let retry = now_ms.saturating_add(millis(RETRY_DELAY));
    match decision.action {
        Action::Continue => {}
        Action::WaitThenPoll => state.poll_after_ms = now_ms.saturating_add(millis(EMPTY_WAIT)),
        Action::RetryLater => match (failed, state.lease.as_mut()) {
            (Some(Call::Refresh), _) => state.token_retry_ms = retry,
            (Some(Call::Poll) | None, _) | (_, None) => state.poll_after_ms = retry,
            (Some(Call::Heartbeat), Some(lease)) => lease.next_heartbeat_ms = retry,
            (Some(_), Some(lease)) => lease.retry_ms = retry,
        },
        Action::Refresh => state.has_token = false,
        Action::GiveUp | Action::GiveUpUnstarted => {
            if decision.action == Action::GiveUpUnstarted {
                let window = now_ms.saturating_add(millis(CALL_TIMEOUT));
                state.drain_deadline_ms = state.drain_deadline_ms.max(Some(window));
            }
            if let Some(lease) = state.lease.as_mut() {
                lease.unstarted = false;
                lease.report_pending = false;
                lease.retry_ms = 0;
            }
        }
        Action::Abandon | Action::AbandonAndExit(_) | Action::Forget | Action::Exit(_) => {
            state.lease = None;
        }
    }
}

/// The lease's end on the runner's monotonic clock: from when the answered
/// call was sent (not when its answer came), plus the remaining seconds the
/// control side answered. The runner's own estimate never lengthens it.
pub fn lease_deadline_ms(sent_ms: u64, remaining_seconds: i64) -> u64 {
    sent_ms.saturating_add(u64::try_from(remaining_seconds.max(0)).unwrap_or(0) * 1000)
}

/// Whether the counted lease end has passed.
pub fn lease_ended(deadline_ms: u64, now_ms: u64) -> bool {
    now_ms >= deadline_ms
}

/// The drain's end: from the stop request plus the configured limit.
pub fn drain_deadline_ms(requested_ms: u64, limit: Duration) -> u64 {
    requested_ms.saturating_add(millis(limit))
}

pub fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::*;

    #[test]
    fn waiting_polls_again_on_empty_and_holds_an_assignment() {
        assert_eq!(
            next(Phase::Waiting, Event::Empty),
            decide(Phase::Waiting, WaitThenPoll)
        );
        assert_eq!(
            next(Phase::Waiting, Event::Assigned),
            decide(Phase::Running, Continue)
        );
        assert_eq!(
            next(Phase::Waiting, Event::StopRequested),
            decide(Phase::Waiting, Exit(Ending::Stopped))
        );
        for stray in [
            Event::Renewed,
            Event::TurnEnded,
            Event::LeaseLost,
            Event::DrainLimit,
        ] {
            assert_eq!(
                next(Phase::Waiting, stray),
                decide(Phase::Waiting, Continue)
            );
        }
    }

    #[test]
    fn a_failed_call_is_retried_and_a_refused_token_is_refreshed_in_every_phase() {
        for phase in [
            Phase::Waiting,
            Phase::Running,
            Phase::Draining,
            Phase::Releasing(None),
            Phase::Releasing(Some(Ending::Stopped)),
        ] {
            assert_eq!(
                next(phase, Event::TransportFailed),
                decide(phase, RetryLater)
            );
            assert_eq!(next(phase, Event::Unauthenticated), decide(phase, Refresh));
        }
    }

    #[test]
    fn running_reports_each_turn_and_hands_back_when_the_work_is_done() {
        assert_eq!(
            next(Phase::Running, Event::TurnEnded),
            decide(Phase::Running, Continue)
        );
        assert_eq!(
            next(Phase::Running, Event::ReportAccepted { finished: false }),
            decide(Phase::Running, Continue)
        );
        assert_eq!(
            next(Phase::Running, Event::ReportAccepted { finished: true }),
            decide(Phase::Releasing(None), GiveUp)
        );
        assert_eq!(
            next(Phase::Running, Event::WorkFailed),
            decide(Phase::Releasing(None), GiveUp)
        );
        assert_eq!(
            next(Phase::Releasing(None), Event::Released),
            decide(Phase::Waiting, Forget)
        );
        assert_eq!(
            next(Phase::Releasing(None), Event::StopRequested),
            decide(Phase::Releasing(Some(Ending::Stopped)), Continue)
        );
    }

    #[test]
    fn a_stop_drains_to_the_turn_end_or_abandons_past_the_limit() {
        assert_eq!(
            next(Phase::Running, Event::StopRequested),
            decide(Phase::Draining, Continue)
        );
        for event in [
            Event::ReportAccepted { finished: false },
            Event::ReportAccepted { finished: true },
            Event::WorkFailed,
        ] {
            assert_eq!(
                next(Phase::Draining, event),
                decide(Phase::Releasing(Some(Ending::Stopped)), GiveUp)
            );
        }
        assert_eq!(
            next(Phase::Draining, Event::Unstarted),
            decide(Phase::Releasing(Some(Ending::Stopped)), GiveUpUnstarted)
        );
        assert_eq!(
            next(Phase::Releasing(Some(Ending::Stopped)), Event::Released),
            decide(Phase::Waiting, Exit(Ending::Stopped))
        );
        assert_eq!(
            next(Phase::Draining, Event::DrainLimit),
            decide(Phase::Waiting, AbandonAndExit(Ending::DrainTimedOut))
        );
        assert_eq!(
            next(Phase::Releasing(Some(Ending::Stopped)), Event::DrainLimit),
            decide(Phase::Waiting, Exit(Ending::Stopped))
        );
    }

    #[test]
    fn a_lost_lease_abandons_the_work_and_asks_again() {
        assert_eq!(
            next(Phase::Running, Event::LeaseLost),
            decide(Phase::Waiting, Abandon)
        );
        assert_eq!(
            next(Phase::Draining, Event::LeaseLost),
            decide(Phase::Waiting, AbandonAndExit(Ending::Stopped))
        );
        assert_eq!(
            next(Phase::Releasing(None), Event::LeaseLost),
            decide(Phase::Waiting, Forget)
        );
    }

    #[test]
    fn revocation_and_version_refusal_end_after_stopping_the_work() {
        for phase in [Phase::Waiting, Phase::Releasing(None)] {
            assert_eq!(
                next(phase, Event::Revoked),
                decide(Phase::Waiting, Exit(Ending::EnrollAgain))
            );
            assert_eq!(
                next(phase, Event::VersionRefused),
                decide(Phase::Waiting, Exit(Ending::VersionOutside))
            );
        }
        for phase in [Phase::Running, Phase::Draining] {
            assert_eq!(
                next(phase, Event::Revoked),
                decide(Phase::Waiting, AbandonAndExit(Ending::EnrollAgain))
            );
            assert_eq!(
                next(phase, Event::VersionRefused),
                decide(Phase::Waiting, AbandonAndExit(Ending::VersionOutside))
            );
        }
        for phase in [Phase::Waiting, Phase::Releasing(None)] {
            assert_eq!(
                next(phase, Event::RefreshUnknown),
                decide(Phase::Waiting, Exit(Ending::EnrollAgain))
            );
        }
        for phase in [Phase::Running, Phase::Draining] {
            assert_eq!(
                next(phase, Event::RefreshUnknown),
                decide(Phase::Waiting, AbandonAndExit(Ending::EnrollAgain))
            );
        }
        assert!(Ending::EnrollAgain.guide().contains("dagq runner enroll"));
        assert!(Ending::VersionOutside.guide().contains("API version"));
    }

    #[test]
    fn the_lease_end_counts_from_the_answered_call_and_never_from_a_guess() {
        assert_eq!(lease_deadline_ms(1_000, 90), 91_000);
        assert_eq!(lease_deadline_ms(1_000, -5), 1_000);
        let mut lease = Lease::granted(1_000, 90, 3_000);
        assert_eq!(lease.next_heartbeat_ms, 23_000);
        lease.renew(20_000, None, 21_000);
        assert_eq!(lease.deadline_ms, 91_000, "no answer, no renewal");
        lease.renew(20_000, Some(90), 21_000);
        assert_eq!(lease.deadline_ms, 110_000);
        assert_eq!(drain_deadline_ms(5, Duration::from_secs(2)), 2_005);
    }

    fn held(phase: Phase) -> State {
        State {
            phase,
            has_token: true,
            lease: Some(Lease {
                deadline_ms: 90_000,
                next_heartbeat_ms: 20_000,
                retry_ms: 0,
                next_check_ms: 0,
                unstarted: false,
                report_pending: false,
            }),
            ..State::new()
        }
    }

    #[test]
    fn the_stop_request_the_drain_limit_and_the_lease_end_come_before_any_call() {
        let mut state = held(Phase::Running);
        state.stop_requested_at = Some(0);
        assert_eq!(choose(0, &state), Choice::Observe(Event::StopRequested));
        let mut waiting = State {
            has_token: false,
            stop_requested_at: Some(0),
            ..State::new()
        };
        assert_eq!(choose(0, &waiting), Choice::Observe(Event::StopRequested));
        waiting.stop_requested_at = None;
        assert_eq!(choose(0, &waiting), Choice::Call(Call::Refresh));

        let mut draining = held(Phase::Draining);
        draining.stop_requested_at = Some(0);
        draining.drain_deadline_ms = Some(40_000);
        draining.lease.as_mut().unwrap().report_pending = true;
        draining.lease.as_mut().unwrap().next_heartbeat_ms = 60_000;
        assert_eq!(choose(39_999, &draining), Choice::Call(Call::Report));
        assert_eq!(
            choose(40_000, &draining),
            Choice::Observe(Event::DrainLimit)
        );
        draining.has_token = false;
        assert_eq!(
            choose(40_000, &draining),
            Choice::Observe(Event::DrainLimit)
        );

        let mut releasing = held(Phase::Releasing(Some(Ending::Stopped)));
        releasing.stop_requested_at = Some(0);
        releasing.drain_deadline_ms = Some(40_000);
        assert_eq!(choose(39_999, &releasing), Choice::Call(Call::Release));
        assert_eq!(
            choose(40_000, &releasing),
            Choice::Observe(Event::DrainLimit)
        );

        let state = held(Phase::Running);
        assert_eq!(choose(89_999, &state), Choice::Call(Call::Heartbeat));
        assert_eq!(choose(90_000, &state), Choice::Observe(Event::LeaseLost));
    }

    #[test]
    fn an_assigned_work_starts_only_while_running_and_inside_its_lease() {
        let mut state = held(Phase::Running);
        state.lease.as_mut().unwrap().unstarted = true;
        assert_eq!(choose(89_999, &state), Choice::Call(Call::Start));
        assert_eq!(choose(90_000, &state), Choice::Observe(Event::LeaseLost));
        state.phase = Phase::Draining;
        assert_eq!(choose(1, &state), Choice::Observe(Event::Unstarted));
    }

    #[test]
    fn each_call_is_due_at_its_time_and_not_a_millisecond_before() {
        let tick = millis(TICK);
        let mut state = State {
            has_token: false,
            token_retry_ms: 5_000,
            ..State::new()
        };
        assert_eq!(
            choose(4_999, &state),
            Choice::Wait(Duration::from_millis(1))
        );
        assert_eq!(choose(5_000, &state), Choice::Call(Call::Refresh));
        state.has_token = true;
        state.poll_after_ms = 5_000;
        assert_eq!(
            choose(4_999, &state),
            Choice::Wait(Duration::from_millis(1))
        );
        assert_eq!(choose(5_000, &state), Choice::Call(Call::Poll));
        assert_eq!(choose(0, &state), Choice::Wait(TICK));

        let mut state = held(Phase::Running);
        state.lease.as_mut().unwrap().next_check_ms = 10_000;
        assert_eq!(
            choose(9_999, &state),
            Choice::Wait(Duration::from_millis(1))
        );
        assert_eq!(choose(10_000, &state), Choice::Call(Call::CheckTurn));
        state.lease.as_mut().unwrap().next_check_ms = 30_000;
        assert_eq!(
            choose(19_999, &state),
            Choice::Wait(Duration::from_millis(1))
        );
        assert_eq!(choose(20_000, &state), Choice::Call(Call::Heartbeat));
        state.lease.as_mut().unwrap().retry_ms = 25_000;
        assert_eq!(
            choose(20_000, &state),
            Choice::Call(Call::Heartbeat),
            "a report's or hand-back's retry does not put off the heartbeat"
        );
        state.lease.as_mut().unwrap().next_heartbeat_ms = 40_000;
        state.lease.as_mut().unwrap().report_pending = true;
        assert_eq!(
            choose(24_999, &state),
            Choice::Wait(Duration::from_millis(1))
        );
        assert_eq!(choose(25_000, &state), Choice::Call(Call::Report));
        state.phase = Phase::Releasing(None);
        assert_eq!(
            choose(24_999, &state),
            Choice::Wait(Duration::from_millis(1))
        );
        assert_eq!(choose(25_000, &state), Choice::Call(Call::Release));
        assert_eq!(choose(25_000 - tick - 5, &state), Choice::Wait(TICK));
    }

    #[test]
    fn the_heartbeat_keeps_its_interval_while_a_report_is_retried() {
        let mut state = held(Phase::Running);
        let lease = state.lease.as_mut().unwrap();
        lease.report_pending = true;
        lease.retry_ms = 25_000;
        assert_eq!(
            choose(19_999, &state),
            Choice::Wait(Duration::from_millis(1))
        );
        assert_eq!(choose(20_000, &state), Choice::Call(Call::Heartbeat));
        state.lease.as_mut().unwrap().next_heartbeat_ms = 40_000;
        assert_eq!(
            choose(24_999, &state),
            Choice::Wait(Duration::from_millis(1))
        );
        assert_eq!(choose(25_000, &state), Choice::Call(Call::Report));
    }

    #[test]
    fn a_due_heartbeat_goes_before_a_due_report() {
        let mut state = held(Phase::Running);
        let lease = state.lease.as_mut().unwrap();
        lease.report_pending = true;
        lease.retry_ms = 10_000;
        assert_eq!(choose(20_000, &state), Choice::Call(Call::Heartbeat));
        assert_eq!(choose(19_999, &state), Choice::Call(Call::Report));
    }

    #[test]
    fn no_call_waits_past_the_lease_end_the_drain_end_or_the_next_heartbeat() {
        let ms = Duration::from_millis;
        let mut state = held(Phase::Running);
        // Lease end 90 s, next heartbeat 20 s.
        assert_eq!(call_budget(0, &state, Call::Heartbeat), CALL_TIMEOUT);
        assert_eq!(call_budget(60_000, &state, Call::Heartbeat), ms(30_000));
        assert_eq!(call_budget(60_001, &state, Call::Heartbeat), ms(29_999));
        assert_eq!(call_budget(89_999, &state, Call::Refresh), ms(1));
        assert_eq!(call_budget(0, &state, Call::Report), ms(20_000));
        assert_eq!(call_budget(14_999, &state, Call::Report), ms(5_001));
        assert_eq!(call_budget(15_000, &state, Call::Report), MIN_CALL);
        assert_eq!(call_budget(19_000, &state, Call::Report), MIN_CALL);
        assert_eq!(call_budget(19_000, &state, Call::Refresh), CALL_TIMEOUT);
        assert_eq!(
            call_budget(87_000, &state, Call::Report),
            ms(3_000),
            "the lease end is hard"
        );

        state.phase = Phase::Draining;
        state.drain_deadline_ms = Some(40_000);
        assert_eq!(call_budget(25_000, &state, Call::Heartbeat), ms(15_000));
        assert_eq!(call_budget(39_999, &state, Call::Refresh), ms(1));
        assert_eq!(call_budget(40_000, &state, Call::Refresh), ms(0));

        state.phase = Phase::Releasing(Some(Ending::Stopped));
        assert_eq!(call_budget(30_000, &state, Call::Release), ms(10_000));
        state.phase = Phase::Releasing(None);
        assert_eq!(call_budget(30_000, &state, Call::Release), CALL_TIMEOUT);
        assert_eq!(call_budget(0, &State::new(), Call::Poll), CALL_TIMEOUT);
    }

    #[test]
    fn a_refresh_is_cut_only_by_the_work_s_deadline_and_not_started_close_to_it() {
        let ms = Duration::from_millis;
        let mut state = held(Phase::Running);
        state.has_token = false;
        // Lease end 90 s: a refresh needs MIN_CALL before it.
        assert_eq!(choose(85_000, &state), Choice::Call(Call::Refresh));
        assert_eq!(call_budget(85_000, &state, Call::Refresh), MIN_CALL);
        assert_eq!(choose(85_001, &state), Choice::Wait(TICK));
        assert_eq!(choose(89_999, &state), Choice::Wait(ms(1)));
        assert_eq!(choose(90_000, &state), Choice::Observe(Event::LeaseLost));

        state.phase = Phase::Draining;
        state.drain_deadline_ms = Some(40_000);
        assert_eq!(choose(35_000, &state), Choice::Call(Call::Refresh));
        assert_eq!(call_budget(35_000, &state, Call::Refresh), MIN_CALL);
        assert_eq!(choose(35_001, &state), Choice::Wait(TICK));

        // The work is stopped: the lease's end no longer cuts a refresh.
        state.phase = Phase::Releasing(None);
        assert_eq!(choose(88_000, &state), Choice::Call(Call::Refresh));
        assert_eq!(call_budget(88_000, &state, Call::Refresh), CALL_TIMEOUT);
        assert_eq!(call_budget(88_000, &state, Call::Release), ms(2_000));
        state.phase = Phase::Releasing(Some(Ending::Stopped));
        assert_eq!(call_budget(30_000, &state, Call::Refresh), ms(10_000));
        assert_eq!(work_deadline_ms(&State::new()), None);
    }

    #[test]
    fn the_drain_counts_from_the_stop_s_arrival_and_cuts_a_waiting_call_at_its_end() {
        let limit = Duration::from_secs(5);
        let mut state = held(Phase::Running);
        assert!(!interrupts(50_000, &state, limit));
        // The stop came at 21 s, during a call the loop sent at 20 s.
        state.stop_requested_at = Some(21_000);
        assert!(!interrupts(25_999, &state, limit));
        assert!(interrupts(26_000, &state, limit));
        // Read after the call came back at 30 s: the drain still ends at 26 s.
        let stop = next(Phase::Running, Event::StopRequested);
        settle(&mut state, stop, Some(Call::Heartbeat), 30_000, limit);
        assert_eq!(state.drain_deadline_ms, Some(26_000));
        assert_eq!(choose(30_000, &state), Choice::Observe(Event::DrainLimit));
        // The lease's end cuts a waiting call while the work runs.
        let state = held(Phase::Running);
        assert!(!interrupts(89_999, &state, limit));
        assert!(interrupts(90_000, &state, limit));
        assert!(!interrupts(90_000, &held(Phase::Releasing(None)), limit));
        let mut handing_back = held(Phase::Releasing(None));
        handing_back.stop_requested_at = Some(21_000);
        assert!(interrupts(26_000, &handing_back, limit));
        // Holding no lease, a stop does not cut a waiting poll or refresh.
        let waiting = State {
            stop_requested_at: Some(21_000),
            ..State::new()
        };
        assert!(!interrupts(60_000, &waiting, limit));
    }

    #[test]
    fn an_assignment_that_came_past_the_drain_limit_is_still_handed_back() {
        let limit = Duration::from_secs(5);
        let mut state = held(Phase::Draining);
        state.stop_requested_at = Some(1_000);
        state.drain_deadline_ms = Some(6_000);
        state.lease.as_mut().unwrap().unstarted = true;
        assert_eq!(choose(8_000, &state), Choice::Observe(Event::Unstarted));
        let hand_back = next(Phase::Draining, Event::Unstarted);
        settle(&mut state, hand_back, None, 8_000, limit);
        assert_eq!(state.drain_deadline_ms, Some(8_000 + millis(CALL_TIMEOUT)));
        assert_eq!(choose(8_000, &state), Choice::Call(Call::Release));
        assert!(!interrupts(8_000, &state, limit), "the hand-back may wait");
        assert!(!interrupts(37_999, &state, limit));
        assert!(interrupts(38_000, &state, limit));
        // A started work keeps its drain limit.
        let mut state = held(Phase::Draining);
        state.drain_deadline_ms = Some(6_000);
        assert_eq!(choose(8_000, &state), Choice::Observe(Event::DrainLimit));
    }

    #[test]
    fn an_answer_is_taken_only_before_the_budget_and_while_no_deadline_holds() {
        let ms = Duration::from_millis;
        let within = ms(1_000);
        assert_eq!(answer_wait(ms(999), within, false, true), AnswerWait::Take);
        assert_eq!(
            answer_wait(ms(1_000), within, false, true),
            AnswerWait::GiveUp
        );
        assert_eq!(
            answer_wait(ms(1_050), within, false, true),
            AnswerWait::GiveUp
        );
        assert_eq!(answer_wait(ms(50), within, true, true), AnswerWait::GiveUp);
        assert_eq!(
            answer_wait(ms(0), within, false, false),
            AnswerWait::Wait(GIVE_UP_POLL)
        );
        assert_eq!(
            answer_wait(ms(950), within, false, false),
            AnswerWait::Wait(ms(50))
        );
        assert_eq!(
            answer_wait(ms(999), within, false, false),
            AnswerWait::Wait(ms(1))
        );
        assert_eq!(
            answer_wait(ms(1_000), within, false, false),
            AnswerWait::GiveUp
        );
        assert_eq!(
            answer_wait(ms(0), ms(1), false, false),
            AnswerWait::Wait(ms(1))
        );
        assert_eq!(answer_wait(ms(0), ms(0), false, true), AnswerWait::GiveUp);
    }

    #[test]
    fn a_wait_for_a_retry_wakes_at_the_drain_end_and_the_lease_end() {
        let ms = Duration::from_millis;
        let mut state = held(Phase::Releasing(Some(Ending::Stopped)));
        state.lease.as_mut().unwrap().retry_ms = 29_000;
        state.drain_deadline_ms = Some(26_500);
        assert_eq!(choose(26_000, &state), Choice::Wait(ms(500)));
        assert_eq!(choose(26_500, &state), Choice::Observe(Event::DrainLimit));
        let mut state = held(Phase::Releasing(None));
        state.lease.as_mut().unwrap().retry_ms = 95_000;
        assert_eq!(choose(89_600, &state), Choice::Wait(ms(400)));
        assert_eq!(choose(90_000, &state), Choice::Observe(Event::LeaseLost));
    }

    #[test]
    fn a_stop_while_handing_back_starts_the_drain_clock_too() {
        let mut state = held(Phase::Releasing(None));
        let stop = next(Phase::Releasing(None), Event::StopRequested);
        settle(&mut state, stop, None, 7_000, Duration::from_secs(30));
        assert_eq!(state.phase, Phase::Releasing(Some(Ending::Stopped)));
        assert_eq!(state.drain_deadline_ms, Some(37_000));
        state.lease.as_mut().unwrap().retry_ms = 36_000;
        assert_eq!(choose(36_999, &state), Choice::Call(Call::Release));
        assert_eq!(choose(37_000, &state), Choice::Observe(Event::DrainLimit));
    }

    #[test]
    fn settling_puts_off_only_the_failed_call_and_starts_the_drain_clock() {
        let mut state = held(Phase::Running);
        let retry = next(Phase::Running, Event::TransportFailed);
        settle(&mut state, retry, Some(Call::Heartbeat), 1_000, TICK);
        assert_eq!(state.lease.unwrap().next_heartbeat_ms, 6_000);
        assert_eq!(state.lease.unwrap().retry_ms, 0, "a report is not put off");
        settle(&mut state, retry, Some(Call::Report), 1_500, TICK);
        assert_eq!(state.lease.unwrap().retry_ms, 6_500);
        settle(&mut state, retry, Some(Call::Refresh), 2_000, TICK);
        assert_eq!(state.token_retry_ms, 7_000);
        let stop = next(Phase::Running, Event::StopRequested);
        settle(&mut state, stop, None, 3_000, Duration::from_secs(30));
        assert_eq!(state.drain_deadline_ms, Some(33_000));
        let again = next(Phase::Draining, Event::StopRequested);
        settle(&mut state, again, None, 9_000, Duration::from_secs(30));
        assert_eq!(state.drain_deadline_ms, Some(33_000));
        let lost = next(Phase::Draining, Event::LeaseLost);
        settle(&mut state, lost, None, 10_000, TICK);
        assert_eq!(state.lease, None);
        let mut waiting = State::new();
        settle(
            &mut waiting,
            next(Phase::Waiting, Event::Empty),
            Some(Call::Poll),
            1,
            TICK,
        );
        assert_eq!(waiting.poll_after_ms, 1 + millis(EMPTY_WAIT));
    }
}
