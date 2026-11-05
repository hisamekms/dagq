//! How many of the queue service's heavy reads run at once in its process
//! ([`HeavyReads`]). The service answers each request on a thread of its
//! own, and a read answers from all it read, so reads that overlap add up:
//! with no limit, their memory grew past the host's and each client ran out
//! its own wait. A request past the limit waits for a place, and one that
//! gets none in [`HEAVY_READ_WAIT`] is refused as
//! [`crate::domain::queue_service::ServiceErrorCode::Busy`] and reads
//! nothing.
//!
//! A read still takes no place in the write line's order and reads on a
//! read-only connection of its own (ADR-t3986-1 decision 1): the limit is
//! how many of them answer in parallel, and what waits for it holds up only
//! itself.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::Clock;
use crate::domain::queue_service::UseCase;

/// Whether `use_case` takes a place among the heavy reads:
///
/// - `kpi`, `stats`, `report`, `report_inputs`: they read the ledger's rows
///   or the events of a whole period, and the service's memory was seen to
///   grow by gigabytes with each.
/// - `forecast`, `timeline`, `events`: they read the queue's history into
///   one answer, whose size grows with the history asked for.
///
/// Left out: `stats_inputs`, which reads no ledger and returns the checkout
/// and the latest event's id. Left out too: the writes and the light reads
/// (`hello`, `service_status`, `status`...), which are short, and every
/// operation of a supervisor (`supervisor`, `control`), so that no pass of a
/// supervisor ever waits here and a refusal never stops one.
pub const fn is_heavy_read(use_case: UseCase) -> bool {
    matches!(
        use_case,
        UseCase::Kpi
            | UseCase::Stats
            | UseCase::Report
            | UseCase::ReportInputs
            | UseCase::Forecast
            | UseCase::Timeline
            | UseCase::Events
    )
}

/// How many heavy reads run at once. Two: with one, a narrow `events` would
/// wait behind a `kpi` of minutes and be refused, and each more adds a whole
/// read to the peak (two `kpi` together were seen near 32 GB on a host of
/// 16 GB). What one read holds is for that read to bring down; this bounds
/// the sum.
pub const MAX_HEAVY_READS: usize = 2;

/// How long a request waits for a place before it is refused. A minute: much
/// less than a client's own wait for the answer (300 seconds, which the
/// read has to fit in too, so that a client gets a refusal rather than a
/// timeout), and long enough to outlast a short read.
pub const HEAVY_READ_WAIT: Duration = Duration::from_secs(60);

/// How often a waiting request looks at its clock when no place is freed.
/// A freed place wakes it at once.
const LOOK_EVERY: Duration = Duration::from_millis(100);

/// What a request of a heavy read met at the limit: what a record of the
/// requests says of it, never what it is answered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeavyReadNote {
    /// How long it waited for a place, in milliseconds, on the clock of its
    /// [`HeavyReads`]: 0 when one was free.
    pub waited_ms: u64,
    /// It got none, and was refused.
    pub refused: bool,
}

/// A request of `use_case` that found no place in the time it waited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeavyReadsBusy {
    pub use_case: UseCase,
    /// The heavy reads running when it gave up.
    pub running: usize,
    pub waited: Duration,
}

impl std::fmt::Display for HeavyReadsBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the queue service is busy and read nothing for {}: heavy reads running: {}, the most \
             it runs at once, and no place came to this one in the {} seconds it waited; run the \
             same command again in a little while",
            self.use_case.as_str(),
            self.running,
            self.waited.as_secs(),
        )
    }
}

impl std::error::Error for HeavyReadsBusy {}

/// The places of a process's heavy reads: a counting semaphore whose wait
/// has a limit and goes by the clock it is given. It keeps no order: a place
/// given back goes to whichever request looks first.
pub struct HeavyReads(Arc<Places>);

struct Places {
    limit: usize,
    wait: Duration,
    clock: Arc<dyn Clock>,
    slots: Mutex<Slots>,
    changed: Condvar,
}

#[derive(Default)]
struct Slots {
    running: usize,
    waiting: usize,
}

/// A place among the heavy reads, given back when it drops: a read that
/// panics gives it back too. The request keeps it until its answer's line is
/// built ([`super::queue_service::RequestNotes::place`]), as the answer is
/// what the limit bounds as much as the read.
#[must_use = "the place is given back when this drops"]
pub struct HeavyReadPlace(Arc<Places>);

impl Drop for HeavyReadPlace {
    fn drop(&mut self) {
        self.0.lock().running -= 1;
        self.0.changed.notify_all();
    }
}

impl Places {
    fn lock(&self) -> MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl HeavyReads {
    /// The queue service's: [`MAX_HEAVY_READS`] and [`HEAVY_READ_WAIT`].
    pub fn of_service(clock: Arc<dyn Clock>) -> Self {
        Self::new(MAX_HEAVY_READS, HEAVY_READ_WAIT, clock)
    }

    pub fn new(limit: usize, wait: Duration, clock: Arc<dyn Clock>) -> Self {
        Self(Arc::new(Places {
            limit,
            wait,
            clock,
            slots: Mutex::default(),
            changed: Condvar::new(),
        }))
    }

    /// A place for a request of `use_case`, waiting for one: `None` at once
    /// for what is no heavy read ([`is_heavy_read`]), which takes none and
    /// leaves `note` as it is; the refusal when none is free in the time it
    /// may wait. `note` says what the heavy read met.
    pub fn admit(
        &self,
        use_case: UseCase,
        note: &mut Option<HeavyReadNote>,
    ) -> Result<Option<HeavyReadPlace>, HeavyReadsBusy> {
        if !is_heavy_read(use_case) {
            return Ok(None);
        }
        let places = &self.0;
        let began = places.clock.monotonic();
        let mut slots = places.lock();
        let mut counted = false;
        loop {
            if slots.running < places.limit {
                slots.running += 1;
                if counted {
                    slots.waiting -= 1;
                }
                drop(slots);
                *note = Some(HeavyReadNote {
                    waited_ms: millis(places.clock.since(began)),
                    refused: false,
                });
                return Ok(Some(HeavyReadPlace(places.clone())));
            }
            let waited = places.clock.since(began);
            if waited >= places.wait {
                if counted {
                    slots.waiting -= 1;
                }
                *note = Some(HeavyReadNote {
                    waited_ms: millis(waited),
                    refused: true,
                });
                return Err(HeavyReadsBusy {
                    use_case,
                    running: slots.running,
                    waited,
                });
            }
            if !counted {
                counted = true;
                slots.waiting += 1;
                places.changed.notify_all();
            }
            let look = LOOK_EVERY.min(places.wait.saturating_sub(waited));
            slots = places
                .changed
                .wait_timeout(slots, look)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Wait up to `limit` until `count` requests wait for a place: whether
    /// they do.
    #[cfg(test)]
    pub(crate) fn await_waiting(&self, count: usize, limit: Duration) -> bool {
        let slots = self
            .0
            .changed
            .wait_timeout_while(self.0.lock(), limit, |slots| slots.waiting < count)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
        slots.waiting >= count
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;
    use std::time::Instant;

    use super::*;

    /// How long a test waits for what another of its threads does.
    pub(crate) const STEP: Duration = Duration::from_secs(30);

    /// A clock that moves only as the test moves it, and by `step` at each
    /// look.
    pub(crate) struct TestClock {
        base: Instant,
        at: Mutex<Duration>,
        step: Duration,
    }

    impl TestClock {
        pub(crate) fn stepping(step: Duration) -> Arc<Self> {
            Arc::new(Self {
                base: Instant::now(),
                at: Mutex::default(),
                step,
            })
        }

        pub(crate) fn still() -> Arc<Self> {
            Self::stepping(Duration::ZERO)
        }

        pub(crate) fn advance(&self, by: Duration) {
            *self.at.lock().unwrap() += by;
        }
    }

    impl Clock for TestClock {
        fn system_time(&self) -> std::time::SystemTime {
            std::time::UNIX_EPOCH
        }
        fn monotonic(&self) -> Instant {
            let mut at = self.at.lock().unwrap();
            let now = self.base + *at;
            *at += self.step;
            now
        }
    }

    /// The set is the one its doc comment names.
    #[test]
    fn the_heavy_reads_are_the_ones_named_and_no_other() {
        for heavy in [
            UseCase::Kpi,
            UseCase::Stats,
            UseCase::Report,
            UseCase::ReportInputs,
            UseCase::Forecast,
            UseCase::Timeline,
            UseCase::Events,
        ] {
            assert!(is_heavy_read(heavy), "{heavy:?}");
        }
        for other in [
            UseCase::StatsInputs,
            UseCase::Hello,
            UseCase::ServiceStatus,
            UseCase::Status,
            UseCase::Show,
            UseCase::Supervisor,
            UseCase::Control,
            UseCase::Add,
            UseCase::Note,
        ] {
            assert!(!is_heavy_read(other), "{other:?}");
        }
    }

    /// The places go up to the limit, and one given back is free again; a
    /// heavy read that found one free notes no wait.
    #[test]
    fn places_go_up_to_the_limit_and_come_back() {
        let reads = HeavyReads::new(2, Duration::ZERO, TestClock::still());
        let mut note = None;
        let first = reads.admit(UseCase::Kpi, &mut note).unwrap();
        assert_eq!(
            note,
            Some(HeavyReadNote {
                waited_ms: 0,
                refused: false
            })
        );
        let second = reads.admit(UseCase::Events, &mut note).unwrap();
        assert!(first.is_some() && second.is_some());
        let refused = reads.admit(UseCase::Stats, &mut note).err().unwrap();
        assert_eq!(refused.running, 2);
        drop(first);
        assert!(reads.admit(UseCase::Stats, &mut note).unwrap().is_some());
    }

    /// Past the limit a request gives up when the time it may wait is over,
    /// by the clock it was given, and says which, how many run and how long
    /// it waited.
    #[test]
    fn a_request_past_the_limit_gives_up_at_the_end_of_its_wait() {
        let wait = Duration::from_secs(7);
        let reads = HeavyReads::new(1, wait, TestClock::stepping(wait));
        let mut note = None;
        let _running = reads.admit(UseCase::Kpi, &mut note).unwrap();
        let refused = reads.admit(UseCase::Forecast, &mut note).err().unwrap();
        assert_eq!(refused.use_case, UseCase::Forecast);
        assert_eq!(refused.running, 1);
        assert_eq!(refused.waited, wait);
        assert_eq!(
            note,
            Some(HeavyReadNote {
                waited_ms: 7_000,
                refused: true
            })
        );
        let said = refused.to_string();
        for part in [
            "forecast",
            "heavy reads running: 1",
            "7 seconds",
            "run the same",
        ] {
            assert!(said.contains(part), "{said}");
        }
    }

    /// What waits gets a place as soon as one is given back, and notes the
    /// time it waited.
    #[test]
    fn a_request_waits_for_a_place_and_takes_the_one_given_back() {
        let clock = TestClock::still();
        let reads = HeavyReads::new(1, Duration::from_secs(60), clock.clone());
        let mut note = None;
        let running = reads.admit(UseCase::Kpi, &mut note).unwrap();
        let noted = thread_scope_waiting(&reads, &clock, running);
        assert_eq!(
            noted,
            Some(HeavyReadNote {
                waited_ms: 5_000,
                refused: false
            })
        );
    }

    /// A request that waits on a thread while `running` holds the only place,
    /// let in 5 seconds later: its note. Whatever fails, the wait ends
    /// (the clock moves past it) before the scope joins the thread.
    fn thread_scope_waiting(
        reads: &HeavyReads,
        clock: &Arc<TestClock>,
        running: Option<HeavyReadPlace>,
    ) -> Option<HeavyReadNote> {
        struct PastTheWait<'a>(&'a TestClock);
        impl Drop for PastTheWait<'_> {
            fn drop(&mut self) {
                self.0.advance(Duration::from_secs(3_600));
            }
        }
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                let mut note = None;
                let place = reads.admit(UseCase::Events, &mut note);
                assert!(place.is_ok_and(|place| place.is_some()));
                note
            });
            let past = PastTheWait(clock);
            assert!(reads.await_waiting(1, STEP));
            clock.advance(Duration::from_secs(5));
            drop(running);
            let noted = waiter.join().unwrap();
            drop(past);
            noted
        })
    }

    /// What is no heavy read neither takes a place nor waits for one, nor
    /// leaves a note.
    #[test]
    fn what_is_no_heavy_read_takes_no_place() {
        let reads = HeavyReads::new(0, Duration::from_secs(60), TestClock::still());
        let mut note = None;
        for other in [UseCase::Hello, UseCase::Supervisor, UseCase::Add] {
            assert!(reads.admit(other, &mut note).unwrap().is_none());
        }
        assert_eq!(note, None);
    }
}
