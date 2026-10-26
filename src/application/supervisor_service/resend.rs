//! The records of results the supervisor's side sends again while the
//! queue service does not answer (ADR-t3102-3 decision 5): a side effect
//! already done (`main` moved, a push, a job's process that ended) has its
//! record kept and sent again until the service takes it, under the
//! identity its first sending gave it, and the queue takes the same record
//! once however often and however late it comes. A record still not taken
//! when its caller ends is kept beside the queue ([`HeldRecords`]), its
//! identity with it, and sent by the next caller that sends what is held.
//! Nothing else is sent again: a claim or a job's start that meets no
//! service starts nothing. A run's event its caller makes before what it
//! starts, or one a pass finds against the queue's history, is sent by
//! [`ServiceCaller::call_now`] and never kept: its caller goes on only once
//! the queue took it, or the next pass finds it again.

use super::{ServiceCaller, unreachable};
use crate::application::{Generators, QueueBusy};
use crate::domain::LeaseToken;
use crate::domain::queue_service::UseCase;
use crate::domain::{
    broker_relay, claim_hold, container_runtime, marks, queue_broker, recheck, run_env,
};
use anyhow::Result;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The supervisor's operations that record a result and that the queue
/// takes once however often they are sent: an event (by its record's
/// identity, [`super::SupervisorParams::record`]), the landing (by its commit),
/// the follow-ups (by their keys), the asks a landing closes (closed
/// once), and a job command's finding (its same evidence is no new
/// occurrence).
pub const RESENT_OPS: [&str; 9] = [
    "record_runtime_event",
    "record_queue_event",
    "finish_integration",
    "register_follow_ups",
    "close_approve_landing_asks",
    "close_run_asks",
    "close_blocked_asks",
    "close_ended_landing_asks",
    "record_finding",
];

/// The kinds of events whose callers record again themselves what they
/// could not record, so they are not sent again here and not written
/// twice: the outage's file keeps `queue_service_down` (ADR-t3102-3
/// decision 1), and each other kind is a change a pass finds against the
/// latest of its kinds on the queue, no side effect's result, which the
/// next pass finds again while the queue lacks it. Held here, it would be
/// missing from that read and found and held again.
pub const OWN_RETRY_KINDS: [&str; 17] = [
    "queue_service_down",
    run_env::RUN_ENV_PROGRAM_KINDS[0],
    run_env::RUN_ENV_PROGRAM_KINDS[1],
    marks::RUN_ENV_CHANGED,
    container_runtime::CONTAINER_RUNTIME_KINDS[0],
    container_runtime::CONTAINER_RUNTIME_KINDS[1],
    claim_hold::CLAIMS.kinds()[0],
    claim_hold::CLAIMS.kinds()[1],
    claim_hold::LANDINGS.kinds()[0],
    claim_hold::LANDINGS.kinds()[1],
    recheck::LANDING_RECHECK_MAIN_KINDS[0],
    recheck::LANDING_RECHECK_MAIN_KINDS[1],
    recheck::TARGET_KINDS[0],
    recheck::TARGET_KINDS[1],
    queue_broker::QUEUE_BROKER_DOWN,
    broker_relay::QUEUE_RELAY_DOWN,
    broker_relay::QUEUE_RELAY_RUNNING,
];

/// The ones of [`RESENT_OPS`] the queue takes once by their record's
/// identity: an event, which nothing in it tells from another alike.
pub const MATCHED_OPS: [&str; 2] = ["record_runtime_event", "record_queue_event"];

/// The supervisor's operation that ends a job's row: not sent again by
/// [`Resending`] (a takeover reads whether it ended the row), but kept for
/// the next supervisor when the supervisor ends with the end unwritten.
pub const JOB_END_OP: &str = "end_headless_job";

/// Whether a kept record of `params` may be sent: a result's record
/// ([`RESENT_OPS`]) or a job's end ([`JOB_END_OP`]). What is kept is a file
/// beside the queue, so nothing else in it is sent.
pub fn kept_op(params: &Value) -> bool {
    params["op"]
        .as_str()
        .is_some_and(|op| RESENT_OPS.contains(&op) || op == JOB_END_OP)
}

/// Whether `error` leaves a record not taken, to be sent again: the
/// service did not answer, or its queue stayed busy past its wait.
pub fn not_taken(error: &anyhow::Error) -> bool {
    unreachable(error) || QueueBusy::is(error)
}

/// A result's record the service did not take, kept to be sent under its
/// identity until the queue takes it once (the context of the error the
/// sending met): the result is recorded as far as its caller goes. A
/// caller that records again on a later pass what failed treats it as
/// recorded ([`kept`]), as recording it again would be another result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub record: String,
}

impl std::fmt::Display for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the record {} is kept and sent once the queue service takes it",
            self.record
        )
    }
}

/// Whether `error` is a result's record kept to be sent ([`Kept`]).
pub fn kept(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Kept>().is_some()
}

/// `error` of the sending of `params`, kept as [`Kept`] when `kept`.
fn kept_as(error: anyhow::Error, params: &Value, kept: bool) -> anyhow::Error {
    match params["record"].as_str().filter(|_| kept) {
        Some(record) => error.context(Kept {
            record: record.to_owned(),
        }),
        None => error,
    }
}

/// A record kept while the service did not take it: its key in its
/// store, its use case and its params (with its identity).
#[derive(Debug, Clone, PartialEq)]
pub struct KeptRecord {
    pub key: String,
    pub use_case: UseCase,
    pub params: Value,
}

/// Where the records not taken are kept until they are: in this process
/// only ([`InMemory`]), or beside the queue, so the next supervisor (after
/// an exec or a restart) and the supervisor of a job command that gave up
/// send them.
pub trait HeldRecords: Send + Sync {
    /// Keep `params` of `use_case` after those kept: its key.
    fn keep(&self, use_case: UseCase, params: &Value) -> Result<String>;
    /// The records kept, oldest first.
    fn kept(&self) -> Result<Vec<KeptRecord>>;
    /// Let go of the record `key`, sent or refused.
    fn forget(&self, key: &str) -> Result<()>;
}

/// The records kept in this process only.
#[derive(Default)]
pub struct InMemory {
    next: std::sync::atomic::AtomicU64,
    records: Mutex<VecDeque<KeptRecord>>,
}

impl HeldRecords for InMemory {
    fn keep(&self, use_case: UseCase, params: &Value) -> Result<String> {
        let key = self
            .next
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .to_string();
        self.lock().push_back(KeptRecord {
            key: key.clone(),
            use_case,
            params: params.clone(),
        });
        Ok(key)
    }
    fn kept(&self) -> Result<Vec<KeptRecord>> {
        Ok(self.lock().iter().cloned().collect())
    }
    fn forget(&self, key: &str) -> Result<()> {
        self.lock().retain(|record| record.key != key);
        Ok(())
    }
}

impl InMemory {
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<KeptRecord>> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// How a caller sends again a record the service did not answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resend {
    /// Kept, and sent before anything else at the next
    /// [`ServiceCaller::send_held`]: the supervisor's loop, which goes on
    /// to look at the service (and start it again) between its passes.
    /// While some are kept, a later one sends them first and is kept
    /// behind those still not taken ([`Behind`]), so none is written ahead
    /// of one sent before it.
    Hold,
    /// Sent again here, waiting between, until the service answers or
    /// [`Waits::limit`] passed, then kept: a landing's integrator (with no
    /// limit) and a job command, which have nothing else to do meanwhile.
    Wait,
}

/// Why the records a [`Resend::Hold`] caller kept wait to be sent, which
/// a later record that could not send them waits behind, answered as its
/// own sending would have been then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behind {
    /// The queue was busy past its wait.
    Busy,
    /// The service did not answer.
    Unanswered,
}

impl Behind {
    fn of(error: &anyhow::Error) -> Self {
        if unreachable(error) {
            Self::Unanswered
        } else {
            Self::Busy
        }
    }

    /// The error of a record kept behind, which was not sent.
    fn error(self) -> anyhow::Error {
        const WHY: &str = "the records kept before it are not taken yet";
        match self {
            Self::Busy => anyhow::anyhow!(WHY).context(QueueBusy),
            Self::Unanswered => super::ServiceUnreachable::not_done(WHY).into(),
        }
    }
}

/// The waits of [`Resend::Wait`]: the first, doubled up to `longest`, and
/// how long it sends again at most.
#[derive(Debug, Clone, Copy)]
pub struct Waits {
    pub first: Duration,
    pub longest: Duration,
    pub limit: Duration,
}

impl Waits {
    /// The production's: from 200 ms to 5 s, for 30 minutes, past a
    /// service's replacement and the supervisor's starts of it.
    pub const DEFAULT: Self = Self {
        first: Duration::from_millis(200),
        longest: Duration::from_secs(5),
        limit: Duration::from_secs(30 * 60),
    };

    /// A landing's integrator's: its records follow `main` moved, so they
    /// are sent again for as long as it takes.
    pub const LANDING: Self = Self {
        limit: Duration::MAX,
        ..Self::DEFAULT
    };

    /// What a caller does after it sent a record again `tries` times,
    /// `elapsed` after its first sending: wait `first` doubled at each try
    /// up to `longest` and send again, until `limit` passed; then keep it.
    pub fn next(&self, elapsed: Duration, tries: u32) -> Again {
        if elapsed >= self.limit {
            return Again::Keep;
        }
        let doubled = self
            .first
            .saturating_mul(2_u32.saturating_pow(tries.min(31)));
        Again::SendAfter(doubled.min(self.longest))
    }
}

/// What [`Waits::next`] says to do with a record not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Again {
    /// Wait this long, then send it again.
    SendAfter(Duration),
    /// Stop sending it here and keep it.
    Keep,
}

/// Whether `params` of `use_case` are a record of a result sent again
/// while the service does not answer ([`RESENT_OPS`], but for an event of
/// [`OWN_RETRY_KINDS`]).
pub fn resent(use_case: UseCase, params: &Value) -> bool {
    let own_retry = params["op"] == "record_queue_event"
        && params["args"][0]
            .as_str()
            .is_some_and(|kind| OWN_RETRY_KINDS.contains(&kind));
    use_case == UseCase::Supervisor
        && !own_retry
        && params["op"]
            .as_str()
            .is_some_and(|op| RESENT_OPS.contains(&op))
}

/// `params` as the result's record `record`
/// ([`super::SupervisorParams::record`]); a record given one keeps it.
pub fn with_record(mut params: Value, record: String) -> Value {
    if params["record"].is_null() {
        params["record"] = Value::from(record);
    }
    params
}

/// A caller that sends again the records of results ([`resent`]) its
/// `inner` caller did not get taken ([`not_taken`]), as `mode` says, and
/// keeps them in `held` until they are.
pub struct Resending {
    inner: Arc<dyn ServiceCaller>,
    mode: Resend,
    waits: Waits,
    /// How long a record was sent ([`Waits::next`]), and the identity of
    /// each result's record at its first sending.
    generators: Generators,
    /// How a caller that waits waits between its sendings.
    pause: Arc<dyn Fn(Duration) + Send + Sync>,
    held: Arc<dyn HeldRecords>,
    /// Whether, and why, records this caller kept or found kept still
    /// wait to be sent ([`Resend::Hold`]).
    behind: Mutex<Option<Behind>>,
    /// One sending of the kept records at a time, so a later record
    /// decides after it.
    sending: Mutex<()>,
}

impl Resending {
    pub fn new(inner: Arc<dyn ServiceCaller>, mode: Resend, generators: Generators) -> Self {
        Self::with_waits(inner, mode, generators, Waits::DEFAULT)
    }

    pub fn with_waits(
        inner: Arc<dyn ServiceCaller>,
        mode: Resend,
        generators: Generators,
        waits: Waits,
    ) -> Self {
        Self {
            inner,
            mode,
            waits,
            generators,
            pause: Arc::new(std::thread::sleep),
            held: Arc::new(InMemory::default()),
            behind: Mutex::new(None),
            sending: Mutex::new(()),
        }
    }

    /// This caller with its waits between sendings made by `pause` (a
    /// test's clock it moves on) rather than a sleep.
    #[must_use]
    pub fn pausing_with(mut self, pause: Arc<dyn Fn(Duration) + Send + Sync>) -> Self {
        self.pause = pause;
        self
    }

    /// This caller with its records kept in `held` rather than in this
    /// process only.
    #[must_use]
    pub fn kept_in(mut self, held: Arc<dyn HeldRecords>) -> Self {
        self.held = held;
        self
    }

    /// Keep `params` of `use_case`, logging a keeping that failed: the
    /// record is then lost. Its key, kept.
    fn hold(&self, use_case: UseCase, params: &Value) -> Option<String> {
        match self.held.keep(use_case, params) {
            Ok(key) => Some(key),
            Err(error) => {
                tracing::warn!(
                    op = params["op"].as_str(),
                    "the record not taken could not be kept, and is lost: {error:#}"
                );
                None
            }
        }
    }

    /// Why the records kept wait to be sent, when they do and this caller
    /// holds them ([`Resend::Hold`]).
    fn behind(&self) -> Option<Behind> {
        let behind = *self
            .behind
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        behind.filter(|_| self.mode == Resend::Hold)
    }

    fn set_behind(&self, behind: Option<Behind>) {
        *self
            .behind
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = behind;
    }

    /// Send the kept records, oldest first, while no other sending of them
    /// runs ([`ServiceCaller::send_held`]).
    fn send_kept(&self) -> Result<bool> {
        let _sending = self
            .sending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let kept = match self.held.kept() {
            Ok(kept) => kept,
            Err(error) => {
                tracing::warn!(
                    "the records kept for the queue service could not be read, and none is sent: {error:#}"
                );
                self.set_behind(None);
                return Ok(false);
            }
        };
        for record in kept {
            if kept_op(&record.params) {
                match self.inner.call(record.use_case, record.params.clone()) {
                    Err(error) if unreachable(&error) => {
                        self.set_behind(Some(Behind::Unanswered));
                        return Err(error);
                    }
                    Err(error) if QueueBusy::is(&error) => {
                        self.set_behind(Some(Behind::Busy));
                        return Ok(false);
                    }
                    Err(error) => {
                        tracing::warn!(
                            op = record.params["op"].as_str(),
                            "a held record was refused when sent again: {error:#}"
                        );
                    }
                    Ok(_) => {}
                }
            } else {
                tracing::warn!(
                    op = record.params["op"].as_str(),
                    "a kept record that is no result's record is let go unsent"
                );
            }
            self.let_go(&record.params, &record.key);
        }
        self.set_behind(None);
        Ok(true)
    }

    /// Send `params`, a record that is not kept
    /// ([`ServiceCaller::call_now`]), once: never ahead of the records kept
    /// before it, which are sent first, so while the queue does not take
    /// those it is not sent and is answered as their sending was; an error
    /// goes to its caller, nothing kept.
    fn send_unheld(&self, use_case: UseCase, params: Value) -> Result<Value> {
        if self.behind().is_some() {
            // What the sending met is in `behind`, for this record too.
            let _ = self.send_kept();
        }
        if let Some(behind) = self.behind() {
            tracing::warn!(
                op = params["op"].as_str(),
                "the record is not sent ahead of those kept before it, which the queue service has not taken"
            );
            return Err(behind.error());
        }
        self.inner.call(use_case, params)
    }

    /// Let go of the kept record `key` of `params`, taken or refused.
    fn let_go(&self, params: &Value, key: &str) {
        if let Err(error) = self.held.forget(key) {
            tracing::warn!(
                op = params["op"].as_str(),
                "a held record sent could not be let go, and is sent again: {error:#}"
            );
        }
    }

    /// Send `params` again as [`Waits::next`] says until the service takes
    /// them, kept meanwhile when `keep`, so a caller stopped while it waits
    /// leaves them for the supervisor to send: the answer, or the error
    /// `unanswered` last met, still kept (a record sent by
    /// [`ServiceCaller::call_now`] is not kept, and its caller does not go
    /// on).
    fn wait_for(
        &self,
        use_case: UseCase,
        params: &Value,
        mut unanswered: anyhow::Error,
        keep: bool,
    ) -> Result<Value> {
        let kept = keep.then(|| self.hold(use_case, params)).flatten();
        let clock = &self.generators.clock;
        let started = clock.monotonic();
        let mut tries = 0;
        while let Again::SendAfter(wait) = self.waits.next(clock.since(started), tries) {
            tracing::warn!(
                op = params["op"].as_str(),
                "the record is sent again once the queue service takes it: {unanswered:#}"
            );
            (self.pause)(wait);
            tries = tries.saturating_add(1);
            match self.inner.call(use_case, params.clone()) {
                Err(error) if not_taken(&error) => unanswered = error,
                answered => {
                    if let Some(key) = &kept {
                        self.let_go(params, key);
                    }
                    return answered;
                }
            }
        }
        let unanswered = unanswered.context(format!(
            "the queue service did not take the record for {:?}; it is kept for the supervisor to send",
            self.waits.limit
        ));
        Err(kept_as(unanswered, params, kept.is_some()))
    }
}

impl ServiceCaller for Resending {
    /// Each call of a result's record is a result of its own: its first
    /// sending gives it its identity, which every later sending of it
    /// keeps. One not taken and kept is answered as [`Kept`], left to be
    /// sent from here. While records kept before it wait, they are sent
    /// first; one is kept behind those still not taken, unsent, and
    /// answered as their sending was (the queue busy, or the service out of
    /// reach), so the queue takes them in the order they were recorded and
    /// a caller meets an error only where its own sending would have. One
    /// that cannot be kept is sent, late rather than lost.
    fn call(&self, use_case: UseCase, params: Value) -> Result<Value> {
        if !resent(use_case, &params) {
            return self.inner.call(use_case, params);
        }
        let params = with_record(params, self.generators.ids.uuid());
        if self.behind().is_some() {
            // What the sending met is in `behind`, for this record too.
            let _ = self.send_kept();
        }
        if let Some(behind) = self.behind()
            && self.hold(use_case, &params).is_some()
        {
            tracing::warn!(
                op = params["op"].as_str(),
                "the record is kept behind those kept before it, which the queue service has not taken"
            );
            return Err(kept_as(behind.error(), &params, true));
        }
        let unanswered = match self.inner.call(use_case, params.clone()) {
            Err(error) if not_taken(&error) => error,
            answered => return answered,
        };
        match self.mode {
            Resend::Hold => {
                tracing::warn!(
                    op = params["op"].as_str(),
                    "the record is kept and sent again once the queue service takes it: {unanswered:#}"
                );
                let kept = self.hold(use_case, &params).is_some();
                if kept {
                    self.set_behind(Some(Behind::of(&unanswered)));
                }
                Err(kept_as(unanswered, &params, kept))
            }
            Resend::Wait => self.wait_for(use_case, &params, unanswered, true),
        }
    }

    /// A record that is never kept: a caller that holds sends it once,
    /// behind the records it holds ([`Self::send_unheld`]); one that waits
    /// sends it again under one identity while it waits, the queue taking
    /// it once, and past the wait answers the error.
    fn call_now(&self, use_case: UseCase, params: Value) -> Result<Value> {
        if !resent(use_case, &params) {
            return self.inner.call(use_case, params);
        }
        match self.mode {
            Resend::Hold => self.send_unheld(use_case, params),
            Resend::Wait => {
                let params = with_record(params, self.generators.ids.uuid());
                match self.inner.call(use_case, params.clone()) {
                    Err(error) if not_taken(&error) => {
                        self.wait_for(use_case, &params, error, false)
                    }
                    answered => answered,
                }
            }
        }
    }

    /// A landing's integrator sends its records again on its own thread.
    fn with_token(&self, file: &std::path::Path) -> Result<Arc<dyn ServiceCaller>> {
        Ok(Arc::new(Self::with_waits(
            self.inner.with_token(file)?,
            Resend::Wait,
            self.generators.clone(),
            Waits::LANDING,
        )))
    }

    fn sign_in(&self, registration: &LeaseToken, pid: u32) -> Result<()> {
        self.inner.sign_in(registration, pid)
    }

    fn take_back(&self, registration: &LeaseToken, pid: u32) -> Result<()> {
        self.inner.take_back(registration, pid)
    }

    fn hand_over(&self) {
        self.inner.hand_over();
    }

    fn sign_out(&self) {
        self.inner.sign_out();
    }

    /// One the service refuses now is not held any longer: what it says is
    /// logged, as a record that failed is, and a kept one that is no
    /// result's record ([`kept_op`]) is let go unsent. A queue busy past
    /// its wait keeps it and the rest held for the next pass, as kept
    /// records that cannot be read do, which the later records do not wait
    /// behind.
    fn send_held(&self) -> Result<bool> {
        self.send_kept()
    }

    fn holds_for_busy_queue(&self) -> bool {
        self.behind() == Some(Behind::Busy)
    }

    /// A result's record kept unsent gets its identity here, as its first
    /// sending would have given it, so the queue takes it once.
    fn keep(&self, use_case: UseCase, params: Value) -> Result<()> {
        let params = if resent(use_case, &params) {
            with_record(params, self.generators.ids.uuid())
        } else {
            params
        };
        self.held.keep(use_case, &params).map(|_| ())
    }
}
