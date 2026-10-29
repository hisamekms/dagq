//! What the supervisor sends at the end of its loop bounded by one deadline
//! ([`EndDeadline`]): a supervisor whose heartbeat stopped has lost its
//! leases, so nothing it sends from then waits past that deadline for an
//! answer, neither a call of its queue service ([`EndBound`]), which on the
//! socket may take
//! [`crate::infrastructure::queue_service::SUPERVISOR_TIMEOUT`] for one call
//! while the queue stays locked, nor a report of its executors' steps
//! ([`EndBoundSinks`]), which may take
//! [`crate::infrastructure::queue_service::REPORT_TIMEOUT`] for each. The
//! deadline is set the moment the heartbeat stops, whatever the loop does
//! then, its end included, and read again at each send and at each look of
//! the wait for an answer ([`LOOK`]): a send that began before the
//! heartbeat stopped is waited for no longer than the deadline either. So
//! every send waits for its answer on a thread of its own.
//!
//! A call not answered by then is the service out of reach
//! ([`ServiceUnreachable`]): the records of results are held and kept
//! beside the queue for the next supervisor by the caller in front
//! ([`super::resend::Resending`]), as when the service does not answer, and
//! the call left waits on its own thread until the process ends. A report
//! not sent is not sent again: its target's token goes with the lease.
//! Nothing opens the DB instead (ADR-t3102-3 decision 5).

use super::{ServiceCaller, ServiceUnreachable};
use crate::application::{Clock, TelemetrySink, TelemetrySinks};
use crate::domain::LeaseToken;
use crate::domain::queue_service::UseCase;
use crate::domain::session_step::StepTarget;
use crate::domain::telemetry::{ReportRecorded, TelemetryReport};
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// The deadline of the end of the supervisor's loop, on its generators'
/// monotonic clock: none until its heartbeat stops, then the earliest set.
/// Shared by what bounds the supervisor's sends and by the waits of its
/// end, which read it again at each look.
#[derive(Debug, Default)]
pub struct EndDeadline(Mutex<Option<Instant>>);

impl EndDeadline {
    /// The deadline from now on: `at`, unless one earlier is set.
    pub fn set_by(&self, at: Instant) {
        let mut set = self.lock();
        *set = Some(set.map_or(at, |set| set.min(at)));
    }

    /// The deadline, once one is set.
    pub fn at(&self) -> Option<Instant> {
        *self.lock()
    }

    /// What is left of it at `now`, once one is set.
    pub fn left(&self, now: Instant) -> Option<Duration> {
        self.at().map(|at| at.saturating_duration_since(now))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// How a send is made given what is `left` of the end's deadline, if one
/// is set ([`end_call`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndCall {
    /// No deadline: the send waits for its answer as long as what sends it
    /// does.
    Unbounded,
    /// The answer is waited for this long at most.
    Within(Duration),
    /// The deadline is over: nothing is sent.
    Over,
}

/// [`EndCall`] of a send with `left` of the end's deadline (none before a
/// deadline is set).
pub const fn end_call(left: Option<Duration>) -> EndCall {
    match left {
        None => EndCall::Unbounded,
        Some(left) if left.is_zero() => EndCall::Over,
        Some(left) => EndCall::Within(left),
    }
}

/// How often a send's wait for its answer reads the end's deadline again.
pub const LOOK: Duration = Duration::from_millis(20);

/// Why a send of [`within`] has no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unsent {
    /// The deadline was over before it: nothing was sent.
    NotSent,
    /// It was sent, and its answer did not come by the deadline.
    NotAnswered,
}

/// How long a send's wait for its answer waits before it reads the end's
/// deadline again, given what is `left` of it now ([`end_call`]): `None`
/// once it is over.
pub const fn next_look(left: Option<Duration>) -> Option<Duration> {
    match end_call(left) {
        EndCall::Unbounded => Some(LOOK),
        EndCall::Within(left) => Some(if left.as_nanos() < LOOK.as_nanos() {
            left
        } else {
            LOOK
        }),
        EndCall::Over => None,
    }
}

/// `send`, unless `deadline` is over on `clock` already, on a thread of its
/// own, its answer waited for until it comes or `deadline`, read again at
/// each look ([`next_look`]), is over, whenever that deadline is set. A
/// panic of `send` is resumed here; a send left waits on its thread until
/// the process ends.
fn within<T: Send + 'static>(
    deadline: &EndDeadline,
    clock: &dyn Clock,
    send: impl FnOnce() -> T + Send + 'static,
) -> std::result::Result<T, Unsent> {
    if next_look(deadline.left(clock.monotonic())).is_none() {
        return Err(Unsent::NotSent);
    }
    let (answer, answered) = mpsc::channel();
    // Its records reach the subscriber of the thread that sends.
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let sending = std::thread::spawn(move || {
        tracing::dispatcher::with_default(&dispatch, || {
            let _ = answer.send(send());
        });
    });
    loop {
        let Some(look) = next_look(deadline.left(clock.monotonic())) else {
            return Err(Unsent::NotAnswered);
        };
        match answered.recv_timeout(look) {
            Ok(answer) => return Ok(answer),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Err(panic) = sending.join() {
                    std::panic::resume_unwind(panic);
                }
                return Err(Unsent::NotAnswered);
            }
        }
    }
}

/// The caller in front of `inner` whose calls the end's deadline bounds
/// once one is set, read on `clock`, a call already waiting for its answer
/// included.
pub struct EndBound {
    inner: Arc<dyn ServiceCaller>,
    clock: Arc<dyn Clock>,
    deadline: Arc<EndDeadline>,
}

impl EndBound {
    pub fn new(
        inner: Arc<dyn ServiceCaller>,
        clock: Arc<dyn Clock>,
        deadline: Arc<EndDeadline>,
    ) -> Self {
        Self {
            inner,
            clock,
            deadline,
        }
    }

    fn bounded(
        &self,
        op: &Value,
        send: impl FnOnce(Arc<dyn ServiceCaller>) -> Result<Value> + Send + 'static,
    ) -> Result<Value> {
        let inner = self.inner.clone();
        within(&self.deadline, &*self.clock, move || send(inner)).unwrap_or_else(|unsent| {
            Err(match unsent {
                Unsent::NotSent => ServiceUnreachable::not_done(format!(
                    "the supervisor's end is past its deadline: {op} is not sent"
                )),
                Unsent::NotAnswered => ServiceUnreachable::maybe_done(format!(
                    "the queue service did not answer {op} before the supervisor's end's deadline"
                )),
            }
            .into())
        })
    }
}

impl ServiceCaller for EndBound {
    fn call(&self, use_case: UseCase, params: Value) -> Result<Value> {
        let op = params["op"].clone();
        self.bounded(&op, move |inner| inner.call(use_case, params))
    }

    fn call_now(&self, use_case: UseCase, params: Value) -> Result<Value> {
        let op = params["op"].clone();
        self.bounded(&op, move |inner| inner.call_now(use_case, params))
    }

    fn with_token(&self, file: &Path) -> Result<Arc<dyn ServiceCaller>> {
        self.inner.with_token(file)
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

    fn credential(&self) -> Option<PathBuf> {
        self.inner.credential()
    }

    fn send_held(&self) -> Result<bool> {
        self.inner.send_held()
    }

    fn holds_for_busy_queue(&self) -> bool {
        self.inner.holds_for_busy_queue()
    }

    fn keep(&self, use_case: UseCase, params: Value) -> Result<()> {
        self.inner.keep(use_case, params)
    }
}

/// The sinks of the supervisor's executors in front of `inner`, whose
/// reports the end's deadline bounds once one is set, read on `clock`, a
/// report already waiting for its answer included: past it a target has no
/// sink, and its executor keeps its steps unsent.
pub struct EndBoundSinks {
    inner: Arc<dyn TelemetrySinks>,
    clock: Arc<dyn Clock>,
    deadline: Arc<EndDeadline>,
}

impl EndBoundSinks {
    pub fn new(
        inner: Arc<dyn TelemetrySinks>,
        clock: Arc<dyn Clock>,
        deadline: Arc<EndDeadline>,
    ) -> Self {
        Self {
            inner,
            clock,
            deadline,
        }
    }
}

impl TelemetrySinks for EndBoundSinks {
    fn sink(&self, db: &Path, target: &StepTarget) -> Option<Box<dyn TelemetrySink>> {
        next_look(self.deadline.left(self.clock.monotonic()))?;
        // Whether it has one is read from its token's file, not sent.
        self.inner.sink(db, target).map(|_| {
            Box::new(BoundSink {
                inner: self.inner.clone(),
                clock: self.clock.clone(),
                deadline: self.deadline.clone(),
                db: db.to_path_buf(),
                target: target.clone(),
            }) as Box<dyn TelemetrySink>
        })
    }

    fn digest_key(&self, db: &Path) -> Option<crate::domain::command_shape::DigestKey> {
        self.inner.digest_key(db)
    }

    fn steps_file(&self, db: &Path, target: &StepTarget) -> Option<PathBuf> {
        self.inner.steps_file(db, target)
    }
}

/// The sink of `target` of the queue at `db`: each report is sent by
/// `inner`'s sink on a thread of its own, and waited for no longer than the
/// end's deadline once one is set ([`within`]).
struct BoundSink {
    inner: Arc<dyn TelemetrySinks>,
    clock: Arc<dyn Clock>,
    deadline: Arc<EndDeadline>,
    db: PathBuf,
    target: StepTarget,
}

impl TelemetrySink for BoundSink {
    fn report(&self, report: &TelemetryReport) -> Result<ReportRecorded> {
        let (inner, db, target, report) = (
            self.inner.clone(),
            self.db.clone(),
            self.target.clone(),
            report.clone(),
        );
        within(&self.deadline, &*self.clock, move || {
            inner
                .sink(&db, &target)
                .map(|sink| sink.report(&report).map_err(|error| format!("{error:#}")))
        })
        .ok()
        .flatten()
        .unwrap_or_else(|| Err("no answer before the supervisor's end's deadline".to_owned()))
        .map_err(|error| anyhow::anyhow!("the report was not sent: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A clock whose monotonic time moves only when a test moves it.
    struct Stepped(Mutex<Instant>);

    impl Clock for Stepped {
        fn system_time(&self) -> std::time::SystemTime {
            std::time::SystemTime::UNIX_EPOCH
        }
        fn monotonic(&self) -> Instant {
            *self.0.lock().unwrap()
        }
    }

    /// Answers every call, counting them.
    #[derive(Default)]
    struct Counted(AtomicUsize);

    impl ServiceCaller for Counted {
        fn call(&self, _: UseCase, _: Value) -> Result<Value> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Value::Null)
        }
        fn with_token(&self, _: &Path) -> Result<Arc<dyn ServiceCaller>> {
            unreachable!()
        }
        fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
            Ok(())
        }
        fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
            Ok(())
        }
        fn hand_over(&self) {}
        fn sign_out(&self) {}
        fn credential(&self) -> Option<PathBuf> {
            None
        }
    }

    /// Has a sink for every target, counting them.
    #[derive(Default)]
    struct Sinks(AtomicUsize);

    impl TelemetrySinks for Sinks {
        fn sink(&self, _: &Path, _: &StepTarget) -> Option<Box<dyn TelemetrySink>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            None
        }
        fn digest_key(&self, _: &Path) -> Option<crate::domain::command_shape::DigestKey> {
            None
        }
        fn steps_file(&self, _: &Path, _: &StepTarget) -> Option<PathBuf> {
            None
        }
    }

    #[test]
    fn a_send_is_bounded_only_once_a_deadline_is_set() {
        let ms = Duration::from_millis(1);
        assert_eq!(end_call(None), EndCall::Unbounded);
        assert_eq!(end_call(Some(ms)), EndCall::Within(ms));
        assert_eq!(end_call(Some(Duration::ZERO)), EndCall::Over);
        // A wait looks at the deadline again every LOOK, sooner when less
        // is left, and stops once it is over: a deadline set while it
        // waits bounds it.
        assert_eq!(next_look(None), Some(LOOK));
        assert_eq!(next_look(Some(LOOK * 3)), Some(LOOK));
        assert_eq!(next_look(Some(ms)), Some(ms));
        assert_eq!(next_look(Some(Duration::ZERO)), None);
        // The earliest deadline set stays.
        let start = Instant::now();
        let deadline = EndDeadline::default();
        assert_eq!(deadline.left(start), None);
        deadline.set_by(start + Duration::from_secs(10));
        deadline.set_by(start + Duration::from_secs(20));
        assert_eq!(deadline.left(start), Some(Duration::from_secs(10)));
        assert_eq!(
            deadline.left(start + Duration::from_secs(11)),
            Some(Duration::ZERO)
        );
    }

    /// Before a deadline a call goes through and a target has its sink;
    /// past it no call is sent, and it fails as the service out of reach
    /// for a request not done, which the caller in front holds and keeps,
    /// and no target has a sink.
    #[test]
    fn past_the_ends_deadline_nothing_is_sent() {
        let start = Instant::now();
        let clock = Arc::new(Stepped(Mutex::new(start)));
        let deadline = Arc::new(EndDeadline::default());
        let inner = Arc::new(Counted::default());
        let bound = EndBound::new(inner.clone(), clock.clone(), deadline.clone());
        let sinks = Arc::new(Sinks::default());
        let bound_sinks = EndBoundSinks::new(sinks.clone(), clock.clone(), deadline.clone());
        let target = StepTarget::run(&crate::domain::RunId::new("r").unwrap());
        let params = serde_json::json!({"op": "record_queue_event"});
        bound.call(UseCase::Supervisor, params.clone()).unwrap();
        assert!(bound_sinks.sink(Path::new("q.db"), &target).is_none());
        assert_eq!(sinks.0.load(Ordering::SeqCst), 1);
        deadline.set_by(start + Duration::from_secs(10));
        *clock.0.lock().unwrap() = start + Duration::from_secs(10);
        let error = bound.call(UseCase::Supervisor, params).unwrap_err();
        assert!(super::super::undone(&error), "{error:#}");
        assert!(
            format!("{error:#}").contains("record_queue_event"),
            "{error:#}"
        );
        assert_eq!(inner.0.load(Ordering::SeqCst), 1);
        assert!(bound_sinks.sink(Path::new("q.db"), &target).is_none());
        assert_eq!(sinks.0.load(Ordering::SeqCst), 1);
    }
}
