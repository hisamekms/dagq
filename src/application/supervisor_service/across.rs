//! The steps of `install` and the update jobs across a replacement of the
//! queue service (docs/design/supervisor-lifecycle/install.md): once a
//! service answered one of the command's steps, a step no service took
//! ([`super::undone`], or a read whose answer was cut off) is sent again
//! to the service that accepts next, for a bounded while. The service is replaced between two builds by the
//! install, its drain, or the supervisor it handed over, whose new build
//! replaces the service at its start. A step a service may have done is not
//! sent again here; a record of one is [`super::resend::Resending`]'s.
//! Before any answer nothing is sent again, so a command that finds no
//! service ends at once (fail closed: the queue is never opened instead).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use super::{ServiceCaller, reads_only, undone, unreachable};
use crate::domain::LeaseToken;
use crate::domain::queue_service::UseCase;

/// Between two sendings of a step sent again.
pub const POLL: Duration = Duration::from_millis(200);

/// How many times a step is sent again: a minute of [`POLL`]s, past a
/// replacement's start and handover.
pub const TRIES: u32 = 300;

/// Whether a step that failed is sent again: only one no service took
/// (`undone`), once a service answered one of the command's steps
/// (`answered`), and `tries` times at most (`limit`).
pub fn sends_again(undone: bool, answered: bool, tries: u32, limit: u32) -> bool {
    undone && answered && tries < limit
}

/// Whether the step `params` that failed with `error` was taken by no
/// service: none was reached, or one was and the step only reads (the
/// command's first step included), which did nothing whether or not it ran.
fn took_nothing(error: &anyhow::Error, params: &Value) -> bool {
    let op = params["op"].as_str().unwrap_or_default();
    undone(error) || (unreachable(error) && (reads_only(op) || op == super::control::CONTROL_QUEUE))
}

/// The caller of a command's steps that sends again those no service took
/// ([`sends_again`]). `answered` is the command's, shared by every queue the
/// command opens, so a later opening's first step is sent again too.
pub struct AcrossReplacement {
    steps: Arc<dyn ServiceCaller>,
    answered: Arc<AtomicBool>,
    limit: u32,
    pause: Arc<dyn Fn(Duration) + Send + Sync>,
}

impl AcrossReplacement {
    pub fn new(steps: Arc<dyn ServiceCaller>, answered: Arc<AtomicBool>) -> Self {
        Self {
            steps,
            answered,
            limit: TRIES,
            pause: Arc::new(std::thread::sleep),
        }
    }

    /// This caller sending again at most `limit` times, waiting with
    /// `pause` (a test's) rather than a sleep.
    #[must_use]
    pub fn pausing_with(mut self, limit: u32, pause: Arc<dyn Fn(Duration) + Send + Sync>) -> Self {
        self.limit = limit;
        self.pause = pause;
        self
    }
}

impl ServiceCaller for AcrossReplacement {
    fn call(&self, use_case: UseCase, params: Value) -> Result<Value> {
        let mut tries = 0;
        loop {
            let answer = self.steps.call(use_case, params.clone());
            match answer {
                Err(error)
                    if sends_again(
                        took_nothing(&error, &params),
                        self.answered.load(Ordering::SeqCst),
                        tries,
                        self.limit,
                    ) =>
                {
                    tries += 1;
                    (self.pause)(POLL);
                }
                answer => {
                    if answer.is_ok() {
                        self.answered.store(true, Ordering::SeqCst);
                    }
                    return answer;
                }
            }
        }
    }

    fn with_token(&self, file: &std::path::Path) -> Result<Arc<dyn ServiceCaller>> {
        self.steps.with_token(file)
    }

    fn sign_in(&self, registration: &LeaseToken, pid: u32) -> Result<()> {
        self.steps.sign_in(registration, pid)
    }

    fn take_back(&self, registration: &LeaseToken, pid: u32) -> Result<()> {
        self.steps.take_back(registration, pid)
    }

    fn hand_over(&self) {
        self.steps.hand_over();
    }

    fn sign_out(&self) {
        self.steps.sign_out();
    }

    fn credential(&self) -> Option<std::path::PathBuf> {
        self.steps.credential()
    }
}

#[cfg(test)]
mod tests {
    use super::super::ServiceUnreachable;
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    #[test]
    fn only_a_step_no_service_took_after_an_answer_is_sent_again() {
        assert!(sends_again(true, true, 0, 3));
        assert!(sends_again(true, true, 2, 3));
        assert!(!sends_again(true, true, 3, 3));
        // Before any answer: the command ends at once.
        assert!(!sends_again(true, false, 0, 3));
        // A step a service may have done, or one it refused.
        assert!(!sends_again(false, true, 0, 3));
    }

    /// Answers its steps in turn from `answers`, then `ok`.
    struct Steps(Mutex<Vec<Result<Value>>>);

    impl ServiceCaller for Steps {
        fn call(&self, _: UseCase, _: Value) -> Result<Value> {
            let mut answers = self.0.lock().unwrap();
            if answers.is_empty() {
                Ok(json!("ok"))
            } else {
                answers.remove(0)
            }
        }
        fn with_token(&self, _: &std::path::Path) -> Result<Arc<dyn ServiceCaller>> {
            unimplemented!()
        }
        fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
            Ok(())
        }
        fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
            Ok(())
        }
        fn hand_over(&self) {}
        fn sign_out(&self) {}
        fn credential(&self) -> Option<std::path::PathBuf> {
            None
        }
    }

    fn not_done() -> Result<Value> {
        Err(anyhow::Error::new(ServiceUnreachable::not_done(
            "no socket",
        )))
    }

    /// The answered state is the command's: a second caller of the same
    /// command (a later opening) sends again its first step no service took.
    #[test]
    fn the_answer_of_one_opening_carries_to_the_next() {
        let answered = Arc::new(AtomicBool::new(false));
        let caller = |answers: Vec<Result<Value>>| {
            AcrossReplacement::new(Arc::new(Steps(Mutex::new(answers))), answered.clone())
                .pausing_with(3, Arc::new(|_| {}))
        };
        let step = |caller: &AcrossReplacement| caller.call(UseCase::Control, json!({}));
        assert!(step(&caller(vec![not_done()])).is_err());
        assert_eq!(step(&caller(vec![Ok(json!("first"))])).unwrap(), "first");
        let next = caller(vec![not_done(), not_done()]);
        assert_eq!(step(&next).unwrap(), "ok");
        // A read cut off with its answer is sent again; a write is not.
        let cut = || Err(anyhow::Error::new(ServiceUnreachable::maybe_done("EOF")));
        let read = caller(vec![cut()]);
        assert_eq!(
            read.call(UseCase::Supervisor, json!({"op": "supervisors"}))
                .unwrap(),
            "ok"
        );
        let write = caller(vec![cut()]);
        assert!(
            write
                .call(UseCase::Supervisor, json!({"op": "request_handoff"}))
                .is_err()
        );
    }
}
