//! One line of work on one thread (ADR-t3986-1 decision 1), which the queue
//! service puts its writes on ([`super::ServiceWriteLine`]): work runs one at
//! a time in the order it came, and each caller waits for its own answer.
//!
//! A refused answer ends what waits behind it without running it. The same
//! answer is retained for later work until the service closes the line and
//! exits. Work no one waits for any more is logged as lost. Work that panics
//! answers nothing and its caller is told; the line goes on after a panic.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tracing::warn;

struct Queued<A> {
    work: Box<dyn FnOnce() -> A + Send>,
    answer: Sender<A>,
}

pub(super) struct WriteLine<A> {
    /// Taken by [`Self::finish`]: what comes after is refused.
    sender: Mutex<Option<Sender<Queued<A>>>>,
    thread: ThreadId,
    refused: Arc<AtomicBool>,
    /// Disconnected when the thread ends.
    ended: Mutex<Receiver<()>>,
}

impl<A: Clone + Send + 'static> WriteLine<A> {
    pub(super) fn start(refused: fn(&A) -> bool) -> Result<Self> {
        let (sender, queued) = channel();
        let (ending, ended) = channel::<()>();
        let refused_state = Arc::new(AtomicBool::new(false));
        let state = refused_state.clone();
        let thread = thread::Builder::new()
            .name("queue-service-writes".to_owned())
            .spawn(move || {
                let _ending = ending;
                work_through(&queued, refused, &state);
            })
            .context("start the queue service's write line")?
            .thread()
            .id();
        Ok(Self {
            sender: Mutex::new(Some(sender)),
            thread,
            refused: refused_state,
            ended: Mutex::new(ended),
        })
    }

    /// Whether this is the line's thread, where waiting for the line would
    /// never end.
    pub(super) fn here(&self) -> bool {
        thread::current().id() == self.thread
    }

    /// `work`'s answer, or the answer that refused what it waited behind,
    /// once its turn came; an error for a line that finished or work that
    /// panicked.
    pub(super) fn run(&self, work: impl FnOnce() -> A + Send + 'static) -> Result<A> {
        self.enqueue(work)?
            .recv()
            .map_err(|_| anyhow!("the queue service's write line dropped this request"))
    }

    /// Put `work` in the line: where its answer comes.
    pub(super) fn enqueue(&self, work: impl FnOnce() -> A + Send + 'static) -> Result<Receiver<A>> {
        let (answer, answered) = channel();
        let held = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sender = held
            .as_ref()
            .context("the queue service is stopping: its write line takes no more")?;
        sender
            .send(Queued {
                work: Box::new(work),
                answer,
            })
            .map_err(|_| anyhow!("the queue service's write line has ended"))?;
        Ok(answered)
    }

    /// Take no more, and wait up to `wait` for the work in the line to be
    /// answered. True means its worker has ended; false leaves the leader
    /// unreleased so a timed-out write cannot race a planned release.
    pub(super) fn finish(&self, wait: Duration) -> bool {
        drop(
            self.sender
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        self.ended
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv_timeout(wait)
            != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    }

    pub(super) fn refused(&self) -> bool {
        self.refused.load(Ordering::SeqCst)
    }
}

fn work_through<A: Clone>(
    queued: &Receiver<Queued<A>>,
    refused: fn(&A) -> bool,
    state: &AtomicBool,
) {
    let mut refusal: Option<A> = None;
    while let Ok(Queued { work, answer }) = queued.recv() {
        let answered = match &refusal {
            Some(answered) => Ok(answered.clone()),
            None => catch_unwind(AssertUnwindSafe(work)),
        };
        if let Ok(answered) = answered {
            if refused(&answered) {
                state.store(true, Ordering::SeqCst);
                refusal = Some(answered.clone());
            }
            if answer.send(answered).is_err() && refusal.is_some() {
                warn!("work no one waited for any more was refused with what was ahead of it");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const BOUND: Duration = Duration::from_secs(30);
    const REFUSED: i32 = -1;

    fn line() -> WriteLine<i32> {
        WriteLine::start(|answer| *answer == REFUSED).unwrap()
    }

    /// Work held on a channel, so what comes after it is in the line behind
    /// it when it is let go: told it began, it waits for `go`.
    fn held(line: &WriteLine<i32>, answer: i32) -> (Receiver<i32>, Sender<()>) {
        let (began, begun) = channel();
        let (go, gone) = channel::<()>();
        let answered = line
            .enqueue(move || {
                began.send(()).unwrap();
                gone.recv_timeout(BOUND).unwrap();
                answer
            })
            .unwrap();
        begun.recv_timeout(BOUND).unwrap();
        (answered, go)
    }

    #[test]
    fn work_runs_on_one_thread_in_the_order_it_came() {
        let line = line();
        let (first, go) = held(&line, 1);
        let (ran, runs) = channel();
        let behind: Vec<_> = (2..5)
            .map(|n| {
                let ran = ran.clone();
                line.enqueue(move || {
                    ran.send((n, thread::current().id())).unwrap();
                    n
                })
                .unwrap()
            })
            .collect();
        // Nothing behind the held work has run.
        assert!(runs.try_recv().is_err());
        go.send(()).unwrap();
        assert_eq!(first.recv_timeout(BOUND).unwrap(), 1);
        for (n, answered) in (2..5).zip(behind) {
            assert_eq!(answered.recv_timeout(BOUND).unwrap(), n);
            let (order, on) = runs.recv_timeout(BOUND).unwrap();
            assert_eq!(order, n);
            assert_eq!(on, line.thread);
            assert_ne!(on, thread::current().id());
        }
    }

    #[test]
    fn a_refusal_answers_what_waits_behind_it_and_all_later_work() {
        let line = line();
        let (first, go) = held(&line, REFUSED);
        let (ran, runs) = channel();
        let behind: Vec<_> = (0..2)
            .map(|n| {
                let ran = ran.clone();
                line.enqueue(move || {
                    ran.send(n).unwrap();
                    n
                })
                .unwrap()
            })
            .collect();
        // Work whose caller stopped waiting ends with the rest.
        drop(line.enqueue(|| 9).unwrap());
        go.send(()).unwrap();
        assert_eq!(first.recv_timeout(BOUND).unwrap(), REFUSED);
        for answered in behind {
            assert_eq!(answered.recv_timeout(BOUND).unwrap(), REFUSED);
        }
        assert!(runs.try_recv().is_err());
        // Later work is refused without running it, until the process exits.
        assert_eq!(
            line.enqueue(|| panic!("must not run"))
                .unwrap()
                .recv_timeout(BOUND)
                .unwrap(),
            REFUSED
        );
        assert!(line.refused());
    }

    /// [`WriteLine::run`], bounded: `line` waits for no one but its work.
    fn ran(line: &Arc<WriteLine<i32>>, work: impl FnOnce() -> i32 + Send + 'static) -> Result<i32> {
        let (answer, answered) = channel();
        let line = line.clone();
        thread::spawn(move || {
            let _ = answer.send(line.run(work));
        });
        answered.recv_timeout(BOUND).unwrap()
    }

    #[test]
    fn work_that_panics_tells_its_caller_and_the_line_goes_on() {
        let line = Arc::new(line());
        assert!(ran(&line, || panic!("a request's bug")).is_err());
        assert_eq!(ran(&line, || 7).unwrap(), 7);
    }

    #[test]
    fn it_finishes_what_it_took_and_takes_no_more() {
        let line = Arc::new(line());
        let (first, go) = held(&line, 1);
        let behind = line.enqueue(|| 2).unwrap();
        go.send(()).unwrap();
        line.finish(BOUND);
        assert_eq!(first.recv_timeout(BOUND).unwrap(), 1);
        assert_eq!(behind.recv_timeout(BOUND).unwrap(), 2);
        assert!(ran(&line, || 3).is_err());
    }

    #[test]
    fn only_its_thread_is_here() {
        let line = Arc::new(line());
        assert!(!line.here());
        let on = line.clone();
        assert_eq!(ran(&line, move || i32::from(on.here())).unwrap(), 1);
    }
}
