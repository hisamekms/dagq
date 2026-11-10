//! The team version runner's process (ADR-t3987-1 decisions 1 and 2): asks
//! the control side for work, keeps the assigned run's lease alive with
//! heartbeats while the work runs, reports each turn's end, hands the lease
//! back, drains on a stop request and cleans up when the lease is lost. What
//! to do and when is [`crate::domain::runner_process`]'s ([`choose`], [`next`],
//! [`settle`]); this loop reads its clock and the stop request, makes the one
//! call chosen, turns the answer into an event and does the decision's side
//! effects. Each call is followed by a fresh reading before the next one,
//! and waits for its answer no longer than [`call_budget`] gives it, so the
//! lease's end, the drain's end and the heartbeat's interval hold while a
//! call waits; the work runs in the background ([`RunnerWork`]).
//!
//! - Calls to the control side ([`RunnerControl`]) carry the short token as
//!   the bearer and params of run ids and generations only: no time of the
//!   runner's clock ([`RunnerTimer`]) and no credential. The token and the
//!   refresh credential never reach [`RunnerOutput`].
//! - The short token comes from [`super::runner_enroll::refresh_within`]; a refused
//!   token is taken again, a refused refresh (revocation) stops the work and
//!   ends with the guide to enroll again (ADR-t3985-1 decision 4). A refresh
//!   is sent again only after a failure that surely rotated nothing
//!   ([`NothingRotated`]); after any other failure the refresh credential
//!   may already be used, so it is never resent and the process ends the
//!   same way.
//! - The entry command and the signal handler are not here: they wire real
//!   adapters to [`run`].

use std::time::Duration;

use serde_json::{Value, json};

use super::ports::{
    RunnerAssignment, RunnerControl, RunnerOutput, RunnerStopRequest, RunnerTimer, RunnerTurnEnd,
    RunnerWork,
};
use super::runner_enroll::{
    ENROLL_AGAIN, NothingRotated, RunnerCredentialStore, RunnerExchange, refresh_within,
};
use crate::domain::queue_service::{ServiceErrorCode, ServiceResponse, UseCase, understands};
use crate::domain::runner_process::{
    Action, Call, Choice, Ending, Event, Lease, State, TICK, call_budget, choose, interrupts,
    millis, next, settle,
};

pub struct RunnerPorts<'a> {
    pub control: &'a dyn RunnerControl,
    pub exchange: &'a dyn RunnerExchange,
    pub credentials: &'a dyn RunnerCredentialStore,
    pub work: &'a mut dyn RunnerWork,
    pub timer: &'a dyn RunnerTimer,
    pub stop: &'a dyn RunnerStopRequest,
    pub output: &'a dyn RunnerOutput,
}

/// The assignment the lease in [`State`] is of, and the turn's end its
/// pending report carries.
struct Held {
    assignment: RunnerAssignment,
    /// Whether the work's start was called: only then is it stopped and
    /// thrown away (a start that failed half way included).
    started: bool,
    end: Option<RunnerTurnEnd>,
}

struct Process<'p, 'a> {
    ports: &'p mut RunnerPorts<'a>,
    url: String,
    token: Option<String>,
    state: State,
    held: Option<Held>,
    /// How long the call being made may wait for its answer.
    budget: Duration,
    drain_limit: Duration,
}

/// An answer's refusal as an event, or its result.
fn classify(response: ServiceResponse) -> Result<Value, Event> {
    if !understands(&response) {
        return Err(Event::VersionRefused);
    }
    if let Some(error) = response.error {
        return Err(match error.code {
            ServiceErrorCode::ApiVersionMismatch => Event::VersionRefused,
            ServiceErrorCode::Unauthenticated => Event::Unauthenticated,
            ServiceErrorCode::LeaseLost | ServiceErrorCode::AuthorizationDenied => Event::LeaseLost,
            // Nothing was committed; the call may be sent again.
            ServiceErrorCode::NotLeader => Event::TransportFailed,
            // Any other refusal answered the call: what it carried was not accepted.
            _ => Event::WorkFailed,
        });
    }
    response
        .result
        .filter(|_| response.ok)
        .ok_or(Event::TransportFailed)
}

fn assignment(value: &Value) -> Option<(RunnerAssignment, i64)> {
    let work = value.get("assignment")?;
    let run = work.get("run")?;
    Some((
        RunnerAssignment {
            run_id: run.get("id")?.as_str()?.to_owned(),
            generation: work.get("generation")?.as_i64()?,
            run: run.clone(),
        },
        work.get("remaining_seconds")?.as_i64()?,
    ))
}

impl Process<'_, '_> {
    fn now(&self) -> u64 {
        self.ports.timer.now_ms()
    }

    /// Whether the waiting call must give up now: the state as the call
    /// began, with the stop request and the clock read afresh.
    fn deadline_check(&self) -> impl Fn() -> bool + '_ {
        let (timer, stop, state, limit) = (
            self.ports.timer,
            self.ports.stop,
            self.state,
            self.drain_limit,
        );
        move || {
            let mut now = state;
            now.stop_requested_at = stop.requested_at();
            interrupts(timer.now_ms(), &now, limit)
        }
    }

    fn call(&mut self, use_case: UseCase, params: Value) -> Result<(Value, u64), Event> {
        let token = self.token.as_deref().ok_or(Event::Unauthenticated)?;
        let sent = self.now();
        let give_up = self.deadline_check();
        let response = self
            .ports
            .control
            .call(&self.url, token, use_case, params, self.budget, &give_up)
            .map_err(|_| Event::TransportFailed)?;
        classify(response).map(|value| (value, sent))
    }

    fn lease_params(&self) -> Value {
        self.held.as_ref().map_or_else(
            || json!({}),
            |h| json!({"run_id": h.assignment.run_id, "generation": h.assignment.generation}),
        )
    }

    /// Renew the lease from an accepted answer to the call sent at `sent`.
    fn renewed(&mut self, value: &Value, sent: u64) {
        let now = self.now();
        let remaining = value.get("remaining_seconds").and_then(Value::as_i64);
        if let Some(lease) = self.state.lease.as_mut() {
            lease.renew(sent, remaining, now);
        }
    }

    fn take_token(&mut self) -> Event {
        let refreshed = refresh_within(
            self.ports.exchange,
            self.ports.credentials,
            self.budget,
            &self.deadline_check(),
        );
        match refreshed {
            Ok(session) => {
                self.token = Some(session.token);
                self.state.has_token = true;
                Event::Idle
            }
            Err(error) if error.is::<super::login::ApiVersionMismatch>() => Event::VersionRefused,
            Err(error) if error.to_string() == ENROLL_AGAIN => Event::Revoked,
            Err(error) if error.is::<NothingRotated>() => {
                self.ports
                    .output
                    .line("runner: could not renew the runner token; retrying");
                Event::TransportFailed
            }
            // No answer, an answer that could not be read or saved, or a
            // refusal that may follow a committed rotation: the refresh
            // credential may be used, and sending it again revokes the runner.
            Err(_) => {
                self.ports.output.line(
                    "runner: the token renewal's outcome is unknown; its refresh credential is not sent again",
                );
                Event::RefreshUnknown
            }
        }
    }

    fn poll(&mut self) -> Event {
        let (value, sent) = match self.call(UseCase::RunnerPoll, json!({})) {
            Ok(answer) => answer,
            // A refused poll holds no lease to lose; ask again later.
            Err(Event::LeaseLost | Event::WorkFailed) => return Event::TransportFailed,
            Err(event) => return event,
        };
        if value.get("assignment").is_none_or(Value::is_null) {
            return Event::Empty;
        }
        let Some((work, remaining)) = assignment(&value) else {
            self.ports
                .output
                .line("runner: the control side's assignment could not be read; asking again");
            return Event::TransportFailed;
        };
        self.state.lease = Some(Lease::granted(sent, remaining, self.now()));
        self.held = Some(Held {
            assignment: work,
            started: false,
            end: None,
        });
        Event::Assigned
    }

    fn start(&mut self) -> Event {
        let Some(held) = self.held.as_ref() else {
            return Event::WorkFailed;
        };
        self.ports.output.line(&format!(
            "runner: assigned run {} generation {}",
            held.assignment.run_id, held.assignment.generation
        ));
        let started = self.ports.work.start(&held.assignment);
        if let Some(held) = self.held.as_mut() {
            held.started = true;
        }
        if started.is_err() {
            return Event::WorkFailed;
        }
        if let Some(lease) = self.state.lease.as_mut() {
            lease.unstarted = false;
        }
        Event::Started
    }

    fn heartbeat(&mut self) -> Event {
        match self.call(UseCase::RunnerHeartbeat, self.lease_params()) {
            Ok((value, sent)) => {
                self.renewed(&value, sent);
                Event::Renewed
            }
            // A heartbeat refused otherwise is tried again; the lease's
            // counted end still bounds it.
            Err(Event::WorkFailed) => Event::TransportFailed,
            Err(event) => event,
        }
    }

    fn report(&mut self) -> Event {
        let Some((held, end)) = self
            .held
            .as_ref()
            .and_then(|h| h.end.as_ref().map(|end| (h, end)))
        else {
            return Event::WorkFailed;
        };
        let finished = end.finished;
        let params = json!({
            "run_id": held.assignment.run_id,
            "generation": held.assignment.generation,
            "head": end.head,
            "state": end.state,
            "receipt": end.receipt,
            "exit_code": end.exit_code,
        });
        match self.call(UseCase::RunnerReport, params) {
            Ok((value, sent)) => {
                self.renewed(&value, sent);
                if let Some(lease) = self.state.lease.as_mut() {
                    lease.report_pending = false;
                }
                if let Some(held) = self.held.as_mut() {
                    held.end = None;
                }
                Event::ReportAccepted { finished }
            }
            // A report refused otherwise (WorkFailed) is not accepted; the
            // work is given up and the lease handed back.
            Err(event) => event,
        }
    }

    fn release(&mut self) -> Event {
        match self.call(UseCase::RunnerRelease, self.lease_params()) {
            Ok(_) => Event::Released,
            // A hand-back refused otherwise is tried again until the
            // lease's end.
            Err(Event::WorkFailed) => Event::TransportFailed,
            Err(event) => event,
        }
    }

    fn check_turn(&mut self) -> Event {
        let now = self.now();
        match self.ports.work.turn_end() {
            Ok(Some(end)) => {
                if let Some(held) = self.held.as_mut() {
                    held.end = Some(end);
                }
                if let Some(lease) = self.state.lease.as_mut() {
                    lease.report_pending = true;
                }
                Event::TurnEnded
            }
            Ok(None) => {
                if let Some(lease) = self.state.lease.as_mut() {
                    lease.next_check_ms = now.saturating_add(millis(TICK));
                }
                Event::Idle
            }
            Err(_) => Event::WorkFailed,
        }
    }

    fn perform(&mut self, call: Call) -> Event {
        match call {
            Call::Refresh => self.take_token(),
            Call::Poll => self.poll(),
            Call::Start => self.start(),
            Call::Heartbeat => self.heartbeat(),
            Call::Report => self.report(),
            Call::Release => self.release(),
            Call::CheckTurn => self.check_turn(),
        }
    }

    /// Stop the work and throw its place away.
    fn give_up(&mut self) {
        if let Some(held) = self.held.as_ref().filter(|h| h.started) {
            self.ports.work.stop();
            self.ports.work.discard();
            self.ports.output.line(&format!(
                "runner: stopped run {} generation {}; its work place was discarded",
                held.assignment.run_id, held.assignment.generation
            ));
        }
    }

    fn end(&mut self, ending: Ending) -> Ending {
        self.ports.output.line(ending.guide());
        ending
    }

    /// Decide on `event`, settle the state and do the decision's side
    /// effects; the ending when the process ends.
    fn handle(
        &mut self,
        event: Event,
        failed: Option<Call>,
        drain_limit: Duration,
    ) -> Option<Ending> {
        let decision = next(self.state.phase, event);
        let now = self.now();
        if decision.action == Action::RetryLater
            && self.held.is_none()
            && failed == Some(Call::Poll)
        {
            self.ports
                .output
                .line("runner: the control side did not answer; asking again later");
        }
        let ending = match decision.action {
            Action::GiveUp | Action::GiveUpUnstarted | Action::Abandon => {
                self.give_up();
                None
            }
            Action::AbandonAndExit(ending) => {
                self.give_up();
                Some(ending)
            }
            Action::Exit(ending) => Some(ending),
            _ => None,
        };
        settle(&mut self.state, decision, failed, now, drain_limit);
        if self.state.lease.is_none() {
            self.held = None;
        }
        if decision.action == Action::Refresh {
            self.token = None;
        }
        ending.map(|ending| self.end(ending))
    }
}

/// Run the runner until a stop request, a revocation or a version refusal
/// ends it. `drain_limit` bounds how long a stop waits on the running turn.
pub fn run(mut ports: RunnerPorts<'_>, drain_limit: Duration) -> Ending {
    let url = match ports.credentials.load() {
        Ok(credential) => credential.control_url,
        Err(_) => {
            ports.output.line(Ending::EnrollAgain.guide());
            return Ending::EnrollAgain;
        }
    };
    let mut process = Process {
        ports: &mut ports,
        url,
        token: None,
        state: State::new(),
        held: None,
        budget: Duration::ZERO,
        drain_limit,
    };
    loop {
        process.state.stop_requested_at = process.ports.stop.requested_at();
        let now = process.now();
        let (event, failed) = match choose(now, &process.state) {
            Choice::Wait(duration) => {
                process.ports.timer.sleep(duration);
                continue;
            }
            Choice::Observe(event) => (event, None),
            Choice::Call(call) => {
                process.budget = call_budget(now, &process.state, call);
                (process.perform(call), Some(call))
            }
        };
        if let Some(ending) = process.handle(event, failed, drain_limit) {
            return ending;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::runner_enroll::Credential;
    use crate::domain::run_clone::RunState;
    use crate::domain::runner_process::{
        CALL_TIMEOUT, EMPTY_WAIT, HEARTBEAT_INTERVAL, RETRY_DELAY,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::{HashMap, VecDeque};
    use std::rc::Rc;

    const REFRESH: &str = "refresh-secret-1";

    type Clock = Rc<Cell<u64>>;

    /// The clock and the time it started at.
    struct Timer(Clock, u64);
    impl Timer {
        fn new(clock: &Clock) -> Self {
            Self(clock.clone(), clock.get())
        }
    }
    impl RunnerTimer for Timer {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }
        fn sleep(&self, duration: Duration) {
            let now = self.0.get() + millis(duration);
            assert!(now - self.1 < 4 * 3_600_000, "the runner loop did not end");
            self.0.set(now);
        }
    }

    /// Asks to stop from `at` on.
    struct Stop(Clock, Option<u64>);
    impl RunnerStopRequest for Stop {
        fn requested_at(&self) -> Option<u64> {
            self.1.filter(|&at| self.0.get() >= at)
        }
    }

    #[derive(Default)]
    struct Output(RefCell<Vec<String>>);
    impl RunnerOutput for Output {
        fn line(&self, text: &str) {
            self.0.borrow_mut().push(text.into());
        }
    }

    /// The bearer, the use case, the params, when it was sent and its budget.
    type Sent = (String, UseCase, Value, u64, Duration);
    /// An answer with how long its call takes on the fake clock.
    type Scripted = (u64, anyhow::Result<ServiceResponse>);

    /// Wait for an answer that comes `took_ms` after the call, on the fake
    /// clock, step by step as the HTTPS adapter does
    /// ([`crate::domain::runner_process::answer_wait`]).
    fn waited<T>(
        clock: &Clock,
        took_ms: u64,
        within: Duration,
        give_up: &dyn Fn() -> bool,
        answer: anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        use crate::domain::runner_process::{AnswerWait, answer_wait};
        let start = clock.get();
        loop {
            let elapsed = clock.get() - start;
            match answer_wait(
                Duration::from_millis(elapsed),
                within,
                give_up(),
                elapsed >= took_ms,
            ) {
                AnswerWait::Take => return answer,
                AnswerWait::GiveUp => anyhow::bail!("timed out"),
                AnswerWait::Wait(at_most) => {
                    clock.set(start + (elapsed + millis(at_most)).min(took_ms));
                }
            }
        }
    }

    /// Answers per use case in order; when one runs out, poll is empty,
    /// heartbeat and report renew 90 seconds and release is accepted.
    struct Control {
        clock: Clock,
        calls: RefCell<Vec<Sent>>,
        /// Each answer with how long its call takes on the fake clock.
        script: RefCell<HashMap<&'static str, VecDeque<Scripted>>>,
    }
    impl Control {
        fn new(clock: &Clock) -> Self {
            Self {
                clock: clock.clone(),
                calls: RefCell::default(),
                script: RefCell::default(),
            }
        }
        fn answer(self, use_case: UseCase, answer: anyhow::Result<ServiceResponse>) -> Self {
            self.answer_after(use_case, 0, answer)
        }
        /// An answer that comes `took_ms` after its call was sent.
        fn answer_after(
            self,
            use_case: UseCase,
            took_ms: u64,
            answer: anyhow::Result<ServiceResponse>,
        ) -> Self {
            self.script
                .borrow_mut()
                .entry(use_case.as_str())
                .or_default()
                .push_back((took_ms, answer));
            self
        }
        fn of(&self, use_case: UseCase) -> Vec<Sent> {
            self.calls
                .borrow()
                .iter()
                .filter(|c| c.1 == use_case)
                .cloned()
                .collect()
        }
    }
    impl RunnerControl for Control {
        fn call(
            &self,
            url: &str,
            bearer: &str,
            use_case: UseCase,
            params: Value,
            within: Duration,
            give_up: &dyn Fn() -> bool,
        ) -> anyhow::Result<ServiceResponse> {
            assert_eq!(url, "https://control.example");
            self.calls.borrow_mut().push((
                bearer.into(),
                use_case,
                params,
                self.clock.get(),
                within,
            ));
            let scripted = self
                .script
                .borrow_mut()
                .get_mut(use_case.as_str())
                .and_then(VecDeque::pop_front);
            let (took, answer) = scripted.unwrap_or_else(|| {
                (
                    0,
                    Ok(match use_case {
                        UseCase::RunnerPoll => {
                            ServiceResponse::success(json!({"assignment": null}))
                        }
                        UseCase::RunnerRelease => {
                            ServiceResponse::success(json!({"released": true}))
                        }
                        _ => ServiceResponse::success(json!({"remaining_seconds": 90})),
                    }),
                )
            });
            waited(&self.clock, took, within, give_up, answer)
        }
    }

    struct Store(RefCell<Credential>);
    impl RunnerCredentialStore for Store {
        fn load(&self) -> anyhow::Result<Credential> {
            let c = self.0.borrow();
            Ok(Credential {
                control_url: c.control_url.clone(),
                runner_id: c.runner_id.clone(),
                refresh: c.refresh.clone(),
            })
        }
        fn save(&self, value: &Credential) -> anyhow::Result<()> {
            *self.0.borrow_mut() = Credential {
                control_url: value.control_url.clone(),
                runner_id: value.runner_id.clone(),
                refresh: value.refresh.clone(),
            };
            Ok(())
        }
    }
    fn store() -> Store {
        Store(RefCell::new(Credential {
            control_url: "https://control.example".into(),
            runner_id: "runner-1".into(),
            refresh: REFRESH.into(),
        }))
    }

    /// Hands out token-1, token-2, ...; refused (revoked) once `revoke_after`
    /// tokens were handed out.
    struct Exchange {
        issued: Cell<u32>,
        revoke_after: Option<u32>,
    }
    impl RunnerExchange for Exchange {
        fn exchange(
            &self,
            _: &str,
            use_case: UseCase,
            params: Value,
        ) -> anyhow::Result<ServiceResponse> {
            assert_eq!(use_case, UseCase::RunnerRefresh);
            assert!(
                params["refresh"]
                    .as_str()
                    .unwrap()
                    .starts_with("refresh-secret")
            );
            if self.revoke_after.is_some_and(|n| self.issued.get() >= n) {
                return Ok(ServiceResponse::failure(
                    ServiceErrorCode::Unauthenticated,
                    "revoked",
                ));
            }
            let n = self.issued.get() + 1;
            self.issued.set(n);
            Ok(ServiceResponse::success(json!({
                "runner_id": "runner-1",
                "refresh": format!("refresh-secret-{}", n + 1),
                "token": format!("token-{n}"),
                "expires_at": 1,
            })))
        }
    }

    #[derive(Debug, PartialEq)]
    enum Did {
        Start(String, i64),
        Stop,
        Discard,
    }

    /// Ends turns `turns_ms` after its start, the last one finished.
    struct Work {
        clock: Clock,
        turns_ms: Vec<u64>,
        started_ms: Option<u64>,
        ended: usize,
        did: Vec<Did>,
        /// When each stop came.
        stops_ms: Vec<u64>,
    }
    impl Work {
        fn new(clock: &Clock, turns_ms: &[u64]) -> Self {
            Self {
                clock: clock.clone(),
                turns_ms: turns_ms.to_vec(),
                started_ms: None,
                ended: 0,
                did: Vec::new(),
                stops_ms: Vec::new(),
            }
        }
    }
    impl RunnerWork for Work {
        fn start(&mut self, assignment: &RunnerAssignment) -> anyhow::Result<()> {
            self.did
                .push(Did::Start(assignment.run_id.clone(), assignment.generation));
            self.started_ms = Some(self.clock.get());
            self.ended = 0;
            Ok(())
        }
        fn turn_end(&mut self) -> anyhow::Result<Option<RunnerTurnEnd>> {
            let Some(start) = self.started_ms else {
                return Ok(None);
            };
            let Some(at) = self.turns_ms.get(self.ended) else {
                return Ok(None);
            };
            if self.clock.get() < start + at {
                return Ok(None);
            }
            self.ended += 1;
            let finished = self.ended == self.turns_ms.len();
            Ok(Some(RunnerTurnEnd {
                head: "b".repeat(40),
                state: RunState {
                    branch: Some("dagq/run-1/3".into()),
                    head: "b".repeat(40),
                    status: String::new(),
                    rebase_in_progress: false,
                    turn_ended: Some(true),
                    generation: None,
                },
                receipt: finished.then(|| json!({"result": "succeeded"})),
                exit_code: finished.then_some(0),
                finished,
            }))
        }
        fn stop(&mut self) {
            self.started_ms = None;
            self.stops_ms.push(self.clock.get());
            self.did.push(Did::Stop);
        }
        fn discard(&mut self) {
            self.started_ms = None;
            self.did.push(Did::Discard);
        }
    }

    fn assigned(run: &str, generation: i64, remaining: i64) -> anyhow::Result<ServiceResponse> {
        Ok(ServiceResponse::success(json!({"assignment": {
            "run": {"id": run, "status": "starting"},
            "generation": generation,
            "remaining_seconds": remaining,
        }})))
    }
    fn refused(code: ServiceErrorCode) -> anyhow::Result<ServiceResponse> {
        Ok(ServiceResponse::failure(code, "refused"))
    }
    fn no_answer() -> anyhow::Result<ServiceResponse> {
        Err(anyhow::anyhow!("connection reset"))
    }

    struct Outcome {
        ending: Ending,
        did: Vec<Did>,
        lines: Vec<String>,
    }

    fn drive(
        clock: &Clock,
        control: &Control,
        work: &mut Work,
        exchange: &Exchange,
        stop_at: Option<u64>,
        drain_limit: Duration,
    ) -> Outcome {
        let output = Output::default();
        let store = store();
        let stop = Stop(clock.clone(), stop_at);
        let timer = Timer::new(clock);
        let ending = run(
            RunnerPorts {
                control,
                exchange,
                credentials: &store,
                work,
                timer: &timer,
                stop: &stop,
                output: &output,
            },
            drain_limit,
        );
        Outcome {
            ending,
            did: std::mem::take(&mut work.did),
            lines: output.0.into_inner(),
        }
    }
    fn exchange() -> Exchange {
        Exchange {
            issued: Cell::new(0),
            revoke_after: None,
        }
    }
    const LIMIT: Duration = Duration::from_secs(600);

    #[test]
    fn empty_answers_wait_and_ask_again_without_starting_work() {
        let clock = Clock::default();
        let control = Control::new(&clock);
        let mut work = Work::new(&clock, &[]);
        let out = drive(
            &clock,
            &control,
            &mut work,
            &exchange(),
            Some(30_000),
            LIMIT,
        );
        assert_eq!(out.ending, Ending::Stopped);
        assert!(out.did.is_empty(), "{:?}", out.did);
        let polls = control.of(UseCase::RunnerPoll);
        assert!(polls.len() >= 5, "{polls:?}");
        for pair in polls.windows(2) {
            assert_eq!(pair[1].3 - pair[0].3, millis(EMPTY_WAIT));
        }
        assert!(control.of(UseCase::RunnerHeartbeat).is_empty());
    }

    #[test]
    fn an_assignment_starts_the_work_heartbeats_each_interval_and_reports_and_releases() {
        let clock = Clock::default();
        let control = Control::new(&clock).answer(UseCase::RunnerPoll, assigned("run-1", 3, 90));
        let mut work = Work::new(&clock, &[50_000, 110_000]);
        let out = drive(
            &clock,
            &control,
            &mut work,
            &exchange(),
            Some(200_000),
            LIMIT,
        );
        assert_eq!(out.ending, Ending::Stopped);
        assert_eq!(
            out.did,
            vec![Did::Start("run-1".into(), 3), Did::Stop, Did::Discard],
            "a finished run is released and its clone thrown away"
        );
        let beats = control.of(UseCase::RunnerHeartbeat);
        assert!(beats.len() >= 4, "{beats:?}");
        for pair in beats.windows(2) {
            assert!(pair[1].3 - pair[0].3 >= millis(HEARTBEAT_INTERVAL));
        }
        let renewing: Vec<u64> = control
            .calls
            .borrow()
            .iter()
            .filter(|c| matches!(c.1, UseCase::RunnerHeartbeat | UseCase::RunnerReport))
            .map(|c| c.3)
            .collect();
        for pair in renewing.windows(2) {
            assert!(pair[1] - pair[0] <= 21_000, "{renewing:?}");
        }
        let reports = control.of(UseCase::RunnerReport);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].2["generation"], 3);
        assert_eq!(reports[1].2["exit_code"], 0);
        assert_eq!(control.of(UseCase::RunnerRelease).len(), 1);
        assert!(
            control.of(UseCase::RunnerPoll).len() > 1,
            "after the release the runner asks again"
        );
    }

    #[test]
    fn calls_without_answers_are_retried_and_never_lengthen_the_lease() {
        let clock = Clock::default();
        let mut control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, no_answer())
            .answer(UseCase::RunnerPoll, assigned("run-1", 1, 90));
        for _ in 0..40 {
            control = control.answer(UseCase::RunnerHeartbeat, no_answer());
        }
        let mut work = Work::new(&clock, &[3_600_000]);
        let out = drive(
            &clock,
            &control,
            &mut work,
            &exchange(),
            Some(400_000),
            LIMIT,
        );
        let polls = control.of(UseCase::RunnerPoll);
        assert_eq!(polls[1].3 - polls[0].3, millis(RETRY_DELAY));
        let beats = control.of(UseCase::RunnerHeartbeat);
        assert!(beats.len() >= 2);
        for pair in beats.windows(2) {
            assert_eq!(pair[1].3 - pair[0].3, millis(RETRY_DELAY));
        }
        // The lease ends 90 seconds after the poll that granted it was sent,
        // however many heartbeats went unanswered.
        let granted = polls[1].3;
        assert!(beats.iter().all(|b| b.3 < granted + 90_000), "{beats:?}");
        assert_eq!(
            out.did,
            vec![Did::Start("run-1".into(), 1), Did::Stop, Did::Discard]
        );
        assert!(control.of(UseCase::RunnerReport).is_empty());
        assert!(control.of(UseCase::RunnerRelease).is_empty());
        assert_eq!(out.ending, Ending::Stopped);
    }

    #[test]
    fn a_stop_asks_nothing_new_reports_the_running_turn_and_hands_the_lease_back() {
        let clock = Clock::default();
        let control = Control::new(&clock).answer(UseCase::RunnerPoll, assigned("run-1", 2, 90));
        let mut work = Work::new(&clock, &[60_000, 300_000]);
        let out = drive(
            &clock,
            &control,
            &mut work,
            &exchange(),
            Some(10_000),
            LIMIT,
        );
        assert_eq!(out.ending, Ending::Stopped);
        assert_eq!(control.of(UseCase::RunnerPoll).len(), 1);
        let reports = control.of(UseCase::RunnerReport);
        assert_eq!(reports.len(), 1, "only the running turn's end");
        assert!(reports[0].3 >= 60_000);
        let release = control.of(UseCase::RunnerRelease);
        assert_eq!(release.len(), 1);
        assert!(release[0].3 >= reports[0].3);
        assert_eq!(
            out.did,
            vec![Did::Start("run-1".into(), 2), Did::Stop, Did::Discard]
        );
    }

    #[test]
    fn past_the_drain_limit_the_work_is_stopped_without_a_report() {
        let clock = Clock::default();
        let control = Control::new(&clock).answer(UseCase::RunnerPoll, assigned("run-1", 2, 90));
        let mut work = Work::new(&clock, &[600_000]);
        let out = drive(
            &clock,
            &control,
            &mut work,
            &exchange(),
            Some(10_000),
            Duration::from_secs(30),
        );
        assert_eq!(out.ending, Ending::DrainTimedOut);
        assert!(clock.get() >= 40_000 && clock.get() < 45_000);
        assert!(control.of(UseCase::RunnerReport).is_empty());
        assert!(control.of(UseCase::RunnerRelease).is_empty());
        assert_eq!(
            out.did,
            vec![Did::Start("run-1".into(), 2), Did::Stop, Did::Discard]
        );
        assert_eq!(out.lines.last().unwrap(), Ending::DrainTimedOut.guide());
    }

    #[test]
    fn a_lost_lease_discards_the_work_sends_nothing_more_and_asks_again() {
        for refusal in [
            ServiceErrorCode::LeaseLost,
            ServiceErrorCode::AuthorizationDenied,
        ] {
            let clock = Clock::default();
            let control = Control::new(&clock)
                .answer(UseCase::RunnerPoll, assigned("run-1", 4, 90))
                .answer(UseCase::RunnerHeartbeat, refused(refusal));
            let mut work = Work::new(&clock, &[25_000]);
            let out = drive(
                &clock,
                &control,
                &mut work,
                &exchange(),
                Some(120_000),
                LIMIT,
            );
            assert_eq!(
                out.did,
                vec![Did::Start("run-1".into(), 4), Did::Stop, Did::Discard]
            );
            let lost_at = control.of(UseCase::RunnerHeartbeat)[0].3;
            let after = control
                .calls
                .borrow()
                .iter()
                .filter(|c| c.3 > lost_at && c.2.get("run_id").is_some())
                .count();
            assert_eq!(after, 0, "no report of generation 4 after the loss");
            assert_eq!(control.of(UseCase::RunnerHeartbeat).len(), 1);
            assert!(control.of(UseCase::RunnerReport).is_empty());
            assert!(
                control
                    .of(UseCase::RunnerPoll)
                    .iter()
                    .any(|p| p.3 > lost_at),
                "back to asking"
            );
            assert_eq!(out.ending, Ending::Stopped);
        }
    }

    #[test]
    fn a_refused_report_is_a_lost_lease_too() {
        let clock = Clock::default();
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, assigned("run-1", 4, 90))
            .answer(UseCase::RunnerReport, refused(ServiceErrorCode::LeaseLost));
        let mut work = Work::new(&clock, &[5_000, 10_000]);
        let out = drive(
            &clock,
            &control,
            &mut work,
            &exchange(),
            Some(60_000),
            LIMIT,
        );
        assert_eq!(control.of(UseCase::RunnerReport).len(), 1);
        assert!(control.of(UseCase::RunnerRelease).is_empty());
        assert_eq!(
            out.did,
            vec![Did::Start("run-1".into(), 4), Did::Stop, Did::Discard]
        );
    }

    #[test]
    fn a_report_refused_otherwise_gives_the_work_up_and_hands_the_lease_back() {
        let clock = Clock::default();
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, assigned("run-1", 4, 90))
            .answer(UseCase::RunnerReport, refused(ServiceErrorCode::Failed));
        let mut work = Work::new(&clock, &[5_000, 10_000]);
        let out = drive(
            &clock,
            &control,
            &mut work,
            &exchange(),
            Some(60_000),
            LIMIT,
        );
        assert_eq!(control.of(UseCase::RunnerReport).len(), 1, "not resent");
        let release = control.of(UseCase::RunnerRelease);
        assert_eq!(release.len(), 1);
        assert_eq!(release[0].3, control.of(UseCase::RunnerReport)[0].3);
        assert_eq!(
            out.did,
            vec![Did::Start("run-1".into(), 4), Did::Stop, Did::Discard]
        );
    }

    /// A refresh exchange that fails its first calls as `fail` says.
    struct Flaky {
        tries: RefCell<Vec<u64>>,
        clock: Clock,
        fail: fn(usize) -> Option<anyhow::Result<ServiceResponse>>,
    }
    impl RunnerExchange for Flaky {
        fn exchange(&self, _: &str, _: UseCase, _: Value) -> anyhow::Result<ServiceResponse> {
            self.tries.borrow_mut().push(self.clock.get());
            let n = self.tries.borrow().len();
            if let Some(answer) = (self.fail)(n) {
                return answer;
            }
            Ok(ServiceResponse::success(json!({
                "runner_id": "runner-1",
                "refresh": format!("refresh-secret-{}", n + 1),
                "token": format!("token-{n}"),
                "expires_at": 1,
            })))
        }
    }

    #[test]
    fn a_refresh_that_rotated_nothing_is_tried_again_only_after_the_delay() {
        let clock = Clock::default();
        let control = Control::new(&clock).answer(UseCase::RunnerPoll, assigned("run-1", 1, 90));
        let exchange = Flaky {
            tries: RefCell::default(),
            clock: clock.clone(),
            fail: |n| (n < 3).then(|| refused(ServiceErrorCode::NotLeader)),
        };
        let output = Output::default();
        let mut work = Work::new(&clock, &[600_000]);
        let ending = run(
            RunnerPorts {
                control: &control,
                exchange: &exchange,
                credentials: &store(),
                work: &mut work,
                timer: &Timer::new(&clock),
                stop: &Stop(clock.clone(), Some(30_000)),
                output: &output,
            },
            Duration::from_secs(1),
        );
        let tries = exchange.tries.borrow().clone();
        assert_eq!(tries.len(), 3, "{tries:?}");
        for pair in tries.windows(2) {
            assert!(pair[1] - pair[0] >= millis(RETRY_DELAY), "{tries:?}");
        }
        assert_eq!(control.of(UseCase::RunnerPoll)[0].0, "token-3");
        assert_eq!(ending, Ending::DrainTimedOut);
    }

    #[test]
    fn a_refresh_with_an_unknown_outcome_is_never_sent_again() {
        // The second refresh (after a refused heartbeat) gets no answer, or
        // its rotated credential cannot be saved: either way the old refresh
        // credential may be used already.
        /// Fails every save after the first when `.2`.
        struct FailingSave(Store, Cell<u32>, bool);
        impl RunnerCredentialStore for FailingSave {
            fn load(&self) -> anyhow::Result<Credential> {
                self.0.load()
            }
            fn save(&self, value: &Credential) -> anyhow::Result<()> {
                self.1.set(self.1.get() + 1);
                anyhow::ensure!(!self.2 || self.1.get() < 2, "disk full");
                self.0.save(value)
            }
        }
        for lost_answer in [true, false] {
            let clock = Clock::default();
            let control = Control::new(&clock)
                .answer(UseCase::RunnerPoll, assigned("run-1", 1, 90))
                .answer(
                    UseCase::RunnerHeartbeat,
                    refused(ServiceErrorCode::Unauthenticated),
                );
            let exchange = Flaky {
                tries: RefCell::default(),
                clock: clock.clone(),
                fail: if lost_answer {
                    |n| (n == 2).then(no_answer)
                } else {
                    |_| None
                },
            };
            let credentials = FailingSave(store(), Cell::new(0), !lost_answer);
            let output = Output::default();
            let mut work = Work::new(&clock, &[600_000]);
            let ending = run(
                RunnerPorts {
                    control: &control,
                    exchange: &exchange,
                    credentials: &credentials,
                    work: &mut work,
                    timer: &Timer::new(&clock),
                    stop: &Stop(clock.clone(), None),
                    output: &output,
                },
                LIMIT,
            );
            assert_eq!(ending, Ending::EnrollAgain);
            assert_eq!(exchange.tries.borrow().len(), 2, "never sent again");
            assert_eq!(
                work.did,
                vec![Did::Start("run-1".into(), 1), Did::Stop, Did::Discard]
            );
            assert!(control.of(UseCase::RunnerReport).is_empty());
            let lines = output.0.into_inner();
            assert!(lines.iter().any(|l| l.contains("outcome is unknown")));
            assert!(lines.last().unwrap().contains("dagq runner enroll"));
        }
    }

    #[test]
    fn the_heartbeat_goes_on_while_a_report_is_retried() {
        let clock = Clock::default();
        let mut control =
            Control::new(&clock).answer(UseCase::RunnerPoll, assigned("run-1", 1, 90));
        for _ in 0..30 {
            control = control.answer(UseCase::RunnerReport, no_answer());
        }
        let mut work = Work::new(&clock, &[5_000]);
        let out = drive(
            &clock,
            &control,
            &mut work,
            &exchange(),
            Some(300_000),
            LIMIT,
        );
        let reports = control.of(UseCase::RunnerReport);
        assert_eq!(reports.len(), 31, "retried until accepted");
        let last_report = reports.last().unwrap().3;
        assert!(last_report > 150_000, "past the first lease's 90 seconds");
        let beats: Vec<u64> = control
            .of(UseCase::RunnerHeartbeat)
            .iter()
            .map(|c| c.3)
            .filter(|&at| at <= last_report)
            .collect();
        assert!(beats.len() >= 7, "{beats:?}");
        for pair in beats.windows(2) {
            assert!(pair[1] - pair[0] <= 21_000, "{beats:?}");
        }
        assert_eq!(
            out.did,
            vec![Did::Start("run-1".into(), 1), Did::Stop, Did::Discard],
            "the lease was not lost: the finished run was handed back"
        );
        assert_eq!(control.of(UseCase::RunnerRelease).len(), 1);
        assert_eq!(out.ending, Ending::Stopped);
    }

    #[test]
    fn a_stop_while_a_hand_back_is_retried_ends_at_the_drain_limit() {
        let clock = Clock::default();
        let mut control =
            Control::new(&clock).answer(UseCase::RunnerPoll, assigned("run-1", 1, 90));
        for _ in 0..40 {
            control = control.answer(UseCase::RunnerRelease, no_answer());
        }
        let mut work = Work::new(&clock, &[5_000]);
        let out = drive(
            &clock,
            &control,
            &mut work,
            &exchange(),
            Some(20_000),
            Duration::from_secs(30),
        );
        assert_eq!(out.ending, Ending::Stopped);
        assert!(
            clock.get() >= 50_000 && clock.get() < 51_000,
            "{}",
            clock.get()
        );
        let release = control.of(UseCase::RunnerRelease);
        assert!(release.len() > 2);
        assert!(release.iter().all(|c| c.3 < 50_000), "{release:?}");
    }

    #[test]
    fn an_assignment_answered_after_its_lease_end_is_not_started() {
        let clock = Clock::default();
        // The answer comes 25 s after the poll, within the poll's budget,
        // but the lease it grants ended 20 s after the poll was sent.
        let control = Control::new(&clock).answer_after(
            UseCase::RunnerPoll,
            25_000,
            assigned("run-1", 1, 20),
        );
        let mut work = Work::new(&clock, &[1_000]);
        let out = drive(
            &clock,
            &control,
            &mut work,
            &exchange(),
            Some(130_000),
            LIMIT,
        );
        assert!(out.did.is_empty(), "{:?}", out.did);
        assert!(control.of(UseCase::RunnerHeartbeat).is_empty());
        assert!(control.of(UseCase::RunnerRelease).is_empty());
        assert!(control.of(UseCase::RunnerPoll).len() > 1, "back to asking");
        assert_eq!(out.ending, Ending::Stopped);
    }

    #[test]
    fn a_stop_that_comes_during_a_call_is_read_before_the_next_one() {
        // The first refresh takes 5 seconds; the stop comes 3 seconds in.
        struct Slow(Clock);
        impl RunnerExchange for Slow {
            fn exchange(&self, _: &str, _: UseCase, _: Value) -> anyhow::Result<ServiceResponse> {
                self.0.set(self.0.get() + 5_000);
                Ok(ServiceResponse::success(json!({
                    "runner_id": "runner-1",
                    "refresh": "refresh-secret-9",
                    "token": "token-9",
                    "expires_at": 1,
                })))
            }
        }
        let clock = Clock::default();
        let control = Control::new(&clock);
        let mut work = Work::new(&clock, &[]);
        let output = Output::default();
        let ending = run(
            RunnerPorts {
                control: &control,
                exchange: &Slow(clock.clone()),
                credentials: &store(),
                work: &mut work,
                timer: &Timer::new(&clock),
                stop: &Stop(clock.clone(), Some(3_000)),
                output: &output,
            },
            LIMIT,
        );
        assert_eq!(ending, Ending::Stopped);
        assert!(control.calls.borrow().is_empty(), "no poll after the stop");
    }

    #[test]
    fn a_call_that_would_end_past_the_drain_limit_is_cut_there_and_the_work_stopped() {
        // Stop at 10 s, drain limit 30 s: the drain ends at 40 s. The
        // heartbeat at 20 s is refused, and the refresh after it would take
        // 25 s: it is given until 40 s, and the work stops at 40 s.
        struct Slow(Clock, Cell<u32>, RefCell<Vec<Duration>>);
        impl RunnerExchange for Slow {
            fn exchange(&self, _: &str, _: UseCase, _: Value) -> anyhow::Result<ServiceResponse> {
                unreachable!("the runner bounds every exchange")
            }
            fn exchange_within(
                &self,
                _: &str,
                _: UseCase,
                _: Value,
                within: Duration,
                give_up: &dyn Fn() -> bool,
            ) -> anyhow::Result<ServiceResponse> {
                let n = self.1.get() + 1;
                self.1.set(n);
                self.2.borrow_mut().push(within);
                let took = if n == 2 { 25_000 } else { 0 };
                let answer = Ok(ServiceResponse::success(json!({
                    "runner_id": "runner-1",
                    "refresh": format!("refresh-secret-{n}"),
                    "token": format!("token-{n}"),
                    "expires_at": 1,
                })));
                waited(&self.0, took, within, give_up, answer)
            }
        }
        let clock = Clock::default();
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, assigned("run-1", 2, 90))
            .answer(
                UseCase::RunnerHeartbeat,
                refused(ServiceErrorCode::Unauthenticated),
            );
        let mut work = Work::new(&clock, &[42_000]);
        let output = Output::default();
        let exchange = Slow(clock.clone(), Cell::new(0), RefCell::default());
        let ending = run(
            RunnerPorts {
                control: &control,
                exchange: &exchange,
                credentials: &store(),
                work: &mut work,
                timer: &Timer::new(&clock),
                stop: &Stop(clock.clone(), Some(10_000)),
                output: &output,
            },
            Duration::from_secs(30),
        );
        assert_eq!(exchange.2.borrow()[1], Duration::from_secs(20));
        assert_eq!(work.stops_ms, vec![40_000], "stopped at the drain's end");
        // The cut refresh may have used the credential: enroll again.
        assert_eq!(ending, Ending::EnrollAgain);
        assert_eq!(exchange.1.get(), 2);
        assert!(control.of(UseCase::RunnerReport).is_empty());
        assert!(control.of(UseCase::RunnerRelease).is_empty());
        assert_eq!(
            work.did,
            vec![Did::Start("run-1".into(), 2), Did::Stop, Did::Discard]
        );
    }

    #[test]
    fn a_refresh_after_the_work_stopped_is_not_cut_at_the_lease_end() {
        // The report at 5 s leaves 8 s of lease; the hand-back's answer, a
        // refused token, comes at 10 s; the refresh then takes 4 s, past the
        // lease's end at 13 s. The work is already stopped, so it is given
        // its whole budget and the runner goes on.
        struct Slow(Clock, RefCell<Vec<Duration>>);
        impl RunnerExchange for Slow {
            fn exchange(&self, _: &str, _: UseCase, _: Value) -> anyhow::Result<ServiceResponse> {
                unreachable!("the runner bounds every exchange")
            }
            fn exchange_within(
                &self,
                _: &str,
                _: UseCase,
                _: Value,
                within: Duration,
                give_up: &dyn Fn() -> bool,
            ) -> anyhow::Result<ServiceResponse> {
                self.1.borrow_mut().push(within);
                let n = self.1.borrow().len();
                let took = if n == 2 { 4_000 } else { 0 };
                let answer = Ok(ServiceResponse::success(json!({
                    "runner_id": "runner-1",
                    "refresh": format!("refresh-secret-{}", n + 1),
                    "token": format!("token-{n}"),
                    "expires_at": 1,
                })));
                waited(&self.0, took, within, give_up, answer)
            }
        }
        let clock = Clock::default();
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, assigned("run-1", 1, 90))
            .answer(
                UseCase::RunnerReport,
                Ok(ServiceResponse::success(json!({"remaining_seconds": 8}))),
            )
            .answer_after(
                UseCase::RunnerRelease,
                5_000,
                refused(ServiceErrorCode::Unauthenticated),
            );
        let exchange = Slow(clock.clone(), RefCell::default());
        let output = Output::default();
        let mut work = Work::new(&clock, &[5_000]);
        let ending = run(
            RunnerPorts {
                control: &control,
                exchange: &exchange,
                credentials: &store(),
                work: &mut work,
                timer: &Timer::new(&clock),
                stop: &Stop(clock.clone(), Some(30_000)),
                output: &output,
            },
            LIMIT,
        );
        assert_eq!(exchange.1.borrow().len(), 2);
        assert_eq!(exchange.1.borrow()[1], CALL_TIMEOUT);
        assert_eq!(ending, Ending::Stopped, "not ended for enrolling again");
        assert_eq!(
            control.of(UseCase::RunnerRelease).len(),
            1,
            "the lease ended"
        );
        assert!(
            control
                .of(UseCase::RunnerPoll)
                .iter()
                .any(|p| p.0 == "token-2" && p.3 >= 14_000),
            "asks again with the new token"
        );
    }

    #[test]
    fn a_stop_that_comes_while_a_call_waits_stops_the_work_at_its_drain_limit() {
        // The heartbeat at 20 s would wait 30 s; the stop comes at 21 s with
        // a drain limit of 5 s, and the turn would end at 40 s. The work
        // stops at 26 s without a report or a hand-back.
        let clock = Clock::default();
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, assigned("run-1", 1, 90))
            .answer_after(UseCase::RunnerHeartbeat, 30_000, no_answer());
        let mut work = Work::new(&clock, &[40_000]);
        let output = Output::default();
        let ending = run(
            RunnerPorts {
                control: &control,
                exchange: &exchange(),
                credentials: &store(),
                work: &mut work,
                timer: &Timer::new(&clock),
                stop: &Stop(clock.clone(), Some(21_000)),
                output: &output,
            },
            Duration::from_secs(5),
        );
        assert_eq!(ending, Ending::DrainTimedOut);
        assert_eq!(work.stops_ms, vec![26_000]);
        let beats = control.of(UseCase::RunnerHeartbeat);
        assert_eq!(beats.len(), 1);
        assert_eq!((beats[0].3, beats[0].4), (20_000, CALL_TIMEOUT));
        assert!(control.of(UseCase::RunnerReport).is_empty());
        assert!(control.of(UseCase::RunnerRelease).is_empty());
        assert_eq!(
            work.did,
            vec![Did::Start("run-1".into(), 1), Did::Stop, Did::Discard]
        );
        assert_eq!(
            output.0.into_inner().last().unwrap(),
            Ending::DrainTimedOut.guide()
        );
    }

    #[test]
    fn a_stop_that_comes_while_a_refresh_waits_stops_the_work_at_its_drain_limit() {
        // The token is refused at 20 s; the refresh would wait 30 s; the stop
        // comes at 22 s with a drain limit of 5 s. The refresh gives up at
        // 27 s, its outcome unknown, and the work stops then.
        struct Slow(Clock, Cell<u32>);
        impl RunnerExchange for Slow {
            fn exchange(&self, _: &str, _: UseCase, _: Value) -> anyhow::Result<ServiceResponse> {
                unreachable!("the runner bounds every exchange")
            }
            fn exchange_within(
                &self,
                _: &str,
                _: UseCase,
                _: Value,
                within: Duration,
                give_up: &dyn Fn() -> bool,
            ) -> anyhow::Result<ServiceResponse> {
                self.1.set(self.1.get() + 1);
                let took = if self.1.get() == 2 { 30_000 } else { 0 };
                let answer = Ok(ServiceResponse::success(json!({
                    "runner_id": "runner-1",
                    "refresh": format!("refresh-secret-{}", self.1.get() + 1),
                    "token": format!("token-{}", self.1.get()),
                    "expires_at": 1,
                })));
                waited(&self.0, took, within, give_up, answer)
            }
        }
        let clock = Clock::default();
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, assigned("run-1", 1, 90))
            .answer(
                UseCase::RunnerHeartbeat,
                refused(ServiceErrorCode::Unauthenticated),
            );
        let mut work = Work::new(&clock, &[40_000]);
        let output = Output::default();
        let ending = run(
            RunnerPorts {
                control: &control,
                exchange: &Slow(clock.clone(), Cell::new(0)),
                credentials: &store(),
                work: &mut work,
                timer: &Timer::new(&clock),
                stop: &Stop(clock.clone(), Some(22_000)),
                output: &output,
            },
            Duration::from_secs(5),
        );
        assert_eq!(work.stops_ms, vec![27_000]);
        assert_eq!(
            ending,
            Ending::EnrollAgain,
            "the cut refresh may have used it"
        );
        assert!(control.of(UseCase::RunnerReport).is_empty());
    }

    /// A refresh exchange whose `slow`th call would wait `took_ms`, giving
    /// up at its budget or when asked to.
    struct SlowRefresh {
        clock: Clock,
        calls: Cell<u32>,
        slow: u32,
        took_ms: u64,
    }
    impl RunnerExchange for SlowRefresh {
        fn exchange(&self, _: &str, _: UseCase, _: Value) -> anyhow::Result<ServiceResponse> {
            unreachable!("the runner bounds every exchange")
        }
        fn exchange_within(
            &self,
            _: &str,
            _: UseCase,
            _: Value,
            within: Duration,
            give_up: &dyn Fn() -> bool,
        ) -> anyhow::Result<ServiceResponse> {
            let n = self.calls.get() + 1;
            self.calls.set(n);
            let took = if n == self.slow { self.took_ms } else { 0 };
            let answer = Ok(ServiceResponse::success(json!({
                "runner_id": "runner-1",
                "refresh": format!("refresh-secret-{}", n + 1),
                "token": format!("token-{n}"),
                "expires_at": 1,
            })));
            waited(&self.clock, took, within, give_up, answer)
        }
    }

    #[test]
    fn holding_no_lease_a_stop_does_not_cut_a_waiting_refresh() {
        // The first refresh takes 8 s; the stop comes at 1 s with a drain
        // limit of 5 s. No work is held, so the refresh is not cut (a cut
        // one would need enrolling again), and the runner then stops.
        let clock = Clock::default();
        let control = Control::new(&clock);
        let exchange = SlowRefresh {
            clock: clock.clone(),
            calls: Cell::new(0),
            slow: 1,
            took_ms: 8_000,
        };
        let mut work = Work::new(&clock, &[]);
        let output = Output::default();
        let ending = run(
            RunnerPorts {
                control: &control,
                exchange: &exchange,
                credentials: &store(),
                work: &mut work,
                timer: &Timer::new(&clock),
                stop: &Stop(clock.clone(), Some(1_000)),
                output: &output,
            },
            Duration::from_secs(5),
        );
        assert_eq!(ending, Ending::Stopped);
        assert_eq!(clock.get(), 8_000);
        assert_eq!(exchange.calls.get(), 1);
        assert!(control.calls.borrow().is_empty(), "no poll after the stop");
    }

    #[test]
    fn an_assignment_a_poll_brings_after_the_stop_is_handed_back_unstarted() {
        // The poll waits 8 s and brings an assignment; the stop came at 1 s
        // with a drain limit of 5 s. The work never starts, and its lease
        // is handed back rather than left to its expiry.
        let clock = Clock::default();
        let control = Control::new(&clock)
            .answer_after(UseCase::RunnerPoll, 8_000, assigned("run-1", 1, 90))
            .answer_after(
                UseCase::RunnerRelease,
                2_000,
                Ok(ServiceResponse::success(json!({"released": true}))),
            );
        let mut work = Work::new(&clock, &[1_000]);
        let output = Output::default();
        let ending = run(
            RunnerPorts {
                control: &control,
                exchange: &exchange(),
                credentials: &store(),
                work: &mut work,
                timer: &Timer::new(&clock),
                stop: &Stop(clock.clone(), Some(1_000)),
                output: &output,
            },
            Duration::from_secs(5),
        );
        assert_eq!(ending, Ending::Stopped);
        assert!(work.did.is_empty(), "{:?}", work.did);
        let release = control.of(UseCase::RunnerRelease);
        assert_eq!(release.len(), 1, "answered after 2 s, not cut");
        assert_eq!(release[0].2, json!({"run_id": "run-1", "generation": 1}));
        assert_eq!(clock.get(), 10_000);
        assert!(control.of(UseCase::RunnerHeartbeat).is_empty());
    }

    #[test]
    fn a_hanging_heartbeat_is_cut_at_the_lease_end_and_the_work_stopped_there() {
        let clock = Clock::default();
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, assigned("run-1", 1, 40))
            .answer_after(UseCase::RunnerHeartbeat, 600_000, no_answer());
        let mut work = Work::new(&clock, &[600_000]);
        let output = Output::default();
        let ending = run(
            RunnerPorts {
                control: &control,
                exchange: &exchange(),
                credentials: &store(),
                work: &mut work,
                timer: &Timer::new(&clock),
                stop: &Stop(clock.clone(), Some(100_000)),
                output: &output,
            },
            LIMIT,
        );
        let beats = control.of(UseCase::RunnerHeartbeat);
        assert_eq!(beats.len(), 1);
        assert_eq!((beats[0].3, beats[0].4), (20_000, Duration::from_secs(20)));
        assert_eq!(work.stops_ms, vec![40_000], "stopped at the lease's end");
        assert!(control.of(UseCase::RunnerReport).is_empty());
        assert!(control.of(UseCase::RunnerRelease).is_empty());
        assert_eq!(ending, Ending::Stopped);
    }

    #[test]
    fn a_hanging_report_is_cut_at_the_next_heartbeat_which_keeps_its_interval() {
        let clock = Clock::default();
        let mut control =
            Control::new(&clock).answer(UseCase::RunnerPoll, assigned("run-1", 1, 90));
        for _ in 0..4 {
            control = control.answer_after(UseCase::RunnerReport, 600_000, no_answer());
        }
        let mut work = Work::new(&clock, &[5_000]);
        let out = drive(
            &clock,
            &control,
            &mut work,
            &exchange(),
            Some(300_000),
            LIMIT,
        );
        let reports = control.of(UseCase::RunnerReport);
        assert_eq!(reports.len(), 5, "cut four times, then accepted");
        let last_report = reports.last().unwrap().3;
        let beats: Vec<u64> = control
            .of(UseCase::RunnerHeartbeat)
            .iter()
            .map(|c| c.3)
            .filter(|&at| at <= last_report)
            .collect();
        assert!(beats.len() >= 3, "{beats:?}");
        let mut renewing = vec![0];
        renewing.extend(&beats);
        for pair in renewing.windows(2) {
            assert!(pair[1] - pair[0] <= 21_000, "{renewing:?}");
        }
        for report in &reports[..4] {
            assert!(report.4 <= Duration::from_secs(20), "{report:?}");
            assert!(
                beats.iter().any(|&b| b >= report.3 + millis(report.4)),
                "a heartbeat follows the cut report"
            );
        }
        assert_eq!(
            out.did,
            vec![Did::Start("run-1".into(), 1), Did::Stop, Did::Discard],
            "the lease held: the finished run was handed back"
        );
        assert_eq!(out.ending, Ending::Stopped);
    }

    #[test]
    fn a_refused_hand_back_takes_a_token_again_and_is_sent_again() {
        let clock = Clock::default();
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, assigned("run-1", 2, 90))
            .answer(
                UseCase::RunnerRelease,
                refused(ServiceErrorCode::Unauthenticated),
            )
            .answer(UseCase::RunnerRelease, no_answer());
        let exchange = exchange();
        let mut work = Work::new(&clock, &[5_000]);
        let out = drive(&clock, &control, &mut work, &exchange, Some(1_000), LIMIT);
        assert_eq!(out.ending, Ending::Stopped);
        let release = control.of(UseCase::RunnerRelease);
        assert_eq!(release.len(), 3, "{release:?}");
        assert_eq!(release[0].0, "token-1");
        assert_eq!(release[1].0, "token-2");
        assert_eq!(release[2].3 - release[1].3, millis(RETRY_DELAY));
        assert_eq!(exchange.issued.get(), 2);
    }

    #[test]
    fn a_hand_back_refused_by_version_or_revocation_ends_with_its_guide() {
        let clock = Clock::default();
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, assigned("run-1", 2, 90))
            .answer(
                UseCase::RunnerRelease,
                refused(ServiceErrorCode::ApiVersionMismatch),
            );
        let mut work = Work::new(&clock, &[5_000]);
        let out = drive(&clock, &control, &mut work, &exchange(), Some(1_000), LIMIT);
        assert_eq!(out.ending, Ending::VersionOutside);
        assert_eq!(out.lines.last().unwrap(), Ending::VersionOutside.guide());

        let clock = Clock::default();
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, assigned("run-1", 2, 90))
            .answer(
                UseCase::RunnerRelease,
                refused(ServiceErrorCode::Unauthenticated),
            );
        let exchange = Exchange {
            issued: Cell::new(0),
            revoke_after: Some(1),
        };
        let mut work = Work::new(&clock, &[5_000]);
        let out = drive(&clock, &control, &mut work, &exchange, Some(1_000), LIMIT);
        assert_eq!(out.ending, Ending::EnrollAgain);
        assert_eq!(control.of(UseCase::RunnerRelease).len(), 1);
        assert!(out.lines.last().unwrap().contains("dagq runner enroll"));
    }

    #[test]
    fn a_refused_token_is_taken_again_and_the_work_goes_on() {
        let clock = Clock::default();
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, assigned("run-1", 1, 90))
            .answer(
                UseCase::RunnerHeartbeat,
                refused(ServiceErrorCode::Unauthenticated),
            );
        let exchange = exchange();
        let mut work = Work::new(&clock, &[70_000]);
        let out = drive(&clock, &control, &mut work, &exchange, Some(100_000), LIMIT);
        assert_eq!(exchange.issued.get(), 2);
        let beats = control.of(UseCase::RunnerHeartbeat);
        assert_eq!(beats[0].0, "token-1");
        assert_eq!(beats[1].0, "token-2");
        assert_eq!(control.of(UseCase::RunnerReport)[0].0, "token-2");
        assert_eq!(
            out.did,
            vec![Did::Start("run-1".into(), 1), Did::Stop, Did::Discard]
        );
        assert_eq!(out.ending, Ending::Stopped);
    }

    #[test]
    fn revocation_stops_the_work_and_ends_with_the_enroll_guide() {
        let clock = Clock::default();
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, assigned("run-1", 1, 90))
            .answer(
                UseCase::RunnerHeartbeat,
                refused(ServiceErrorCode::Unauthenticated),
            );
        let exchange = Exchange {
            issued: Cell::new(0),
            revoke_after: Some(1),
        };
        let mut work = Work::new(&clock, &[600_000]);
        let out = drive(&clock, &control, &mut work, &exchange, None, LIMIT);
        assert_eq!(out.ending, Ending::EnrollAgain);
        assert_eq!(
            out.did,
            vec![Did::Start("run-1".into(), 1), Did::Stop, Did::Discard]
        );
        assert!(out.lines.last().unwrap().contains("dagq runner enroll"));
    }

    #[test]
    fn a_version_refusal_ends_with_the_install_guide() {
        let clock = Clock::default();
        let control = Control::new(&clock).answer(
            UseCase::RunnerPoll,
            refused(ServiceErrorCode::ApiVersionMismatch),
        );
        let mut work = Work::new(&clock, &[]);
        let out = drive(&clock, &control, &mut work, &exchange(), None, LIMIT);
        assert_eq!(out.ending, Ending::VersionOutside);
        assert!(out.did.is_empty());
        assert_eq!(out.lines.last().unwrap(), Ending::VersionOutside.guide());

        let clock = Clock::default();
        let mut outside = ServiceResponse::success(json!({"assignment": null}));
        outside.min_api_version = u32::MAX;
        outside.api_version = u32::MAX;
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, assigned("run-1", 1, 90))
            .answer(UseCase::RunnerHeartbeat, Ok(outside));
        let mut work = Work::new(&clock, &[600_000]);
        let out = drive(&clock, &control, &mut work, &exchange(), None, LIMIT);
        assert_eq!(out.ending, Ending::VersionOutside);
        assert_eq!(
            out.did,
            vec![Did::Start("run-1".into(), 1), Did::Stop, Did::Discard]
        );
    }

    #[test]
    fn no_call_carries_runner_time_or_a_credential_and_no_line_shows_one() {
        let clock = Clock::default();
        clock.set(1_700_000_000_000);
        let control = Control::new(&clock)
            .answer(UseCase::RunnerPoll, no_answer())
            .answer(UseCase::RunnerPoll, assigned("run-1", 1, 90))
            .answer(
                UseCase::RunnerHeartbeat,
                refused(ServiceErrorCode::Unauthenticated),
            );
        let mut work = Work::new(&clock, &[30_000, 70_000]);
        let out = drive(
            &clock,
            &control,
            &mut work,
            &exchange(),
            Some(1_700_000_200_000),
            LIMIT,
        );
        let allowed = [
            "run_id",
            "generation",
            "head",
            "state",
            "receipt",
            "exit_code",
        ];
        let calls = control.calls.borrow();
        assert!(calls.len() > 5);
        for (bearer, use_case, params, _, _) in calls.iter() {
            assert!(bearer.starts_with("token-"));
            let object = params.as_object().unwrap();
            if *use_case == UseCase::RunnerPoll {
                assert!(object.is_empty());
            }
            for key in object.keys() {
                assert!(allowed.contains(&key.as_str()), "{key} in {use_case:?}");
            }
            let text = params.to_string();
            assert!(!text.contains("token-") && !text.contains("refresh-secret"));
            assert!(!text.contains("1700000"), "no runner clock time: {text}");
        }
        assert!(!out.lines.is_empty());
        for line in &out.lines {
            assert!(
                !line.contains("token-") && !line.contains("refresh-secret"),
                "{line}"
            );
        }
    }

    #[test]
    fn a_runner_without_a_credential_ends_with_the_enroll_guide() {
        struct Missing;
        impl RunnerCredentialStore for Missing {
            fn load(&self) -> anyhow::Result<Credential> {
                anyhow::bail!("no runner credential")
            }
            fn save(&self, _: &Credential) -> anyhow::Result<()> {
                unreachable!()
            }
        }
        let clock = Clock::default();
        let control = Control::new(&clock);
        let mut work = Work::new(&clock, &[]);
        let output = Output::default();
        let ending = run(
            RunnerPorts {
                control: &control,
                exchange: &exchange(),
                credentials: &Missing,
                work: &mut work,
                timer: &Timer::new(&clock),
                stop: &Stop(clock.clone(), None),
                output: &output,
            },
            LIMIT,
        );
        assert_eq!(ending, Ending::EnrollAgain);
        assert!(control.calls.borrow().is_empty());
    }
}
