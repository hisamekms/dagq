//! The record of main's first-parent history (docs/design/main-history.md):
//! a pass reads Git only when the files the landing branch is resolved from
//! changed (a commit to main changes them), when the reach is due again or
//! when a failing read is due a retry, and records what
//! [`crate::application::main_log::records`] gives. The runtime decides
//! nothing on the record.

use super::*;
use crate::application::LandingBranchStamp;
use crate::application::main_log::{MainReader, Recorded, records};
use crate::domain::stats::main_log::{
    MAIN_OBSERVED, MAIN_OBSERVED_INTERVAL_SECS, MAIN_READ_FAILED, MAIN_READ_RECOVERED, MainReading,
    Reach,
};

/// What this process last recorded or found recorded, so a pass on which
/// nothing moved starts no Git.
#[derive(Debug, Default)]
pub(in crate::application::supervise) struct MainLogWatch {
    /// The head of the reach and when it was recorded (unix seconds), and
    /// whether Git's reading was failing; `None` before the first look.
    last: Option<(Option<String>, i64, bool)>,
    /// The stamp of the landing branch's files taken before the last look
    /// that went through ([`RepositoryReads::landing_branch_stamp`](crate::application::RepositoryReads::landing_branch_stamp)).
    stamp: Option<LandingBranchStamp>,
    /// When this process last looked (unix seconds).
    looked: i64,
}

/// How long a failing read, or a repository that cannot stamp its landing
/// branch's files, waits before the next look, so a read that times out
/// does not hold every pass of the loop.
const MAIN_RETRY_SECS: i64 = 60;

/// What a look found: the head of the reach, when it was recorded and
/// whether Git's reading is failing.
type Looked = (Option<String>, i64, bool);

impl MainLogWatch {
    /// Whether a pass at `now`, with `stamp` taken of the landing branch's
    /// files, looks at the queue and Git: the first time, while failing once
    /// [`MAIN_RETRY_SECS`] passed, when the reach is due again, and when the
    /// stamp changed (without stamps, [`MAIN_RETRY_SECS`] after the last
    /// look). A Git that breaks while nothing changes is found at the next
    /// of these.
    fn due(&self, stamp: Option<&LandingBranchStamp>, now: i64) -> bool {
        let Some((_, at, failing)) = &self.last else {
            return true;
        };
        if *failing {
            return now - self.looked >= MAIN_RETRY_SECS;
        }
        if now - at >= MAIN_OBSERVED_INTERVAL_SECS {
            return true;
        }
        match (stamp, &self.stamp) {
            (Some(stamp), Some(last)) => stamp != last,
            _ => now - self.looked >= MAIN_RETRY_SECS,
        }
    }

    /// Run `look` (every read of the queue and of Git) when [`Self::due`];
    /// whether it ran. `stamp` is taken before, so a change during the look
    /// differs from it at the next pass; a look that fails keeps the last.
    fn look(
        &mut self,
        now: i64,
        stamp: Option<LandingBranchStamp>,
        look: impl FnOnce() -> Result<Looked>,
    ) -> Result<bool> {
        if !self.due(stamp.as_ref(), now) {
            return Ok(false);
        }
        self.looked = now;
        self.last = Some(look()?);
        self.stamp = stamp;
        Ok(true)
    }
}

impl ObservationState {
    /// Record what moved of main's history since the record's reach. A
    /// queue that cannot be read or written is warned of, and a later pass
    /// tries again: a commit recorded twice is folded once.
    pub(in crate::application::supervise) fn main_log_pass(
        &mut self,
        env: &mut ObservationEnv<'_>,
    ) {
        let now = env.generators.clock.now();
        // Read without starting Git.
        let stamp = env.repository.landing_branch_stamp();
        let result = self.main_log.look(now, stamp, || {
            look_at_main(&*env.queue, now, env.repository)
        });
        if let Err(error) = result {
            warn!(error = %format_args!("{error:#}"), "main's history could not be recorded: {error:#}");
        }
    }
}

/// Read the record's reach and Git's reading from the queue, record what
/// moved of main since ([`records`]) and give what this look found. The
/// queue's earliest event, for the first record only, is the time
/// [`EventStore::earliest_event_at`] answers, never every event read.
fn look_at_main<Q: EventStore + ?Sized, M: MainReader + ?Sized>(
    queue: &Q,
    now: i64,
    reader: &M,
) -> Result<Looked> {
    let recorded = Recorded {
        reach: queue
            .latest_queue_event(&[MAIN_OBSERVED])?
            .as_ref()
            .and_then(Reach::of),
        reading: MainReading::of(
            queue
                .latest_queue_event(&[MAIN_READ_FAILED, MAIN_READ_RECOVERED])?
                .as_ref(),
        ),
    };
    let earliest = || queue.earliest_event_at();
    let records = records(&recorded, now, &earliest, reader);
    let mut last = (
        recorded.reach.as_ref().map(|reach| reach.head.clone()),
        recorded.reach.as_ref().map_or(now, |reach| reach.at),
        matches!(recorded.reading, MainReading::Failing { .. }),
    );
    for (kind, payload) in records {
        match kind {
            EventKind::MainObserved => {
                last.0 = payload["head"].as_str().map(str::to_owned);
                last.1 = now;
            }
            EventKind::MainReadFailed => last.2 = true,
            EventKind::MainReadRecovered => last.2 = false,
            _ => {}
        }
        queue.record_queue_event(kind, payload)?;
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, path::PathBuf};

    fn stamp(len: u64) -> Option<LandingBranchStamp> {
        Some(LandingBranchStamp(vec![(
            PathBuf::from("refs/heads/main"),
            Some(crate::application::FileStamp {
                modified: None,
                len,
                inode: 1,
                changed: (0, 0),
            }),
        )]))
    }

    /// A pass reads Git (the look) the first time, when the landing
    /// branch's files changed, once the reach is due again, and while
    /// failing or without stamps only once the retry's time passed; a pass
    /// on which none of these holds starts no Git.
    #[test]
    fn a_pass_reads_git_only_when_main_may_have_moved_or_a_look_is_due() {
        let mut watch = MainLogWatch::default();
        let looks = Cell::new(0);
        let pass = |watch: &mut MainLogWatch, now, stamp, found: Looked| {
            watch
                .look(now, stamp, || {
                    looks.set(looks.get() + 1);
                    Ok(found)
                })
                .unwrap()
        };
        let fine = |at| (Some("a".to_owned()), at, false);
        assert!(pass(&mut watch, 100, stamp(1), fine(100)), "the first look");
        assert!(
            !pass(&mut watch, 101, stamp(1), fine(101)),
            "nothing changed"
        );
        let rest = 100 + MAIN_OBSERVED_INTERVAL_SECS;
        assert!(!pass(&mut watch, rest - 1, stamp(1), fine(rest)));
        assert!(pass(&mut watch, 102, stamp(2), fine(100)), "main moved");
        assert!(
            pass(&mut watch, rest, stamp(2), fine(rest)),
            "the reach is due"
        );
        // A failure found: retried only once the retry's time passed.
        assert!(pass(&mut watch, rest + 1, stamp(3), (None, rest, true)));
        assert!(!pass(
            &mut watch,
            rest + MAIN_RETRY_SECS,
            stamp(4),
            (None, rest, true)
        ));
        assert!(pass(
            &mut watch,
            rest + 1 + MAIN_RETRY_SECS,
            stamp(4),
            fine(rest + 61)
        ));
        // A look that fails keeps the last stamp, so the next pass looks
        // again.
        let at = rest + 61;
        let failed = watch.look(at + 1, stamp(5), || anyhow::bail!("queue"));
        assert!(failed.is_err());
        assert!(pass(&mut watch, at + 2, stamp(5), fine(at)));
        // Without stamps, a look once the retry's time passed.
        assert!(!pass(&mut watch, at + 3, None, fine(at)));
        assert!(pass(&mut watch, at + 2 + MAIN_RETRY_SECS, None, fine(at)));
        assert_eq!(looks.get(), 7);
    }

    /// The queue's events, newest last: the time of the earliest answered
    /// without [`EventStore::all_events`], which counts its reads.
    #[derive(Default)]
    struct Events {
        events: std::cell::RefCell<Vec<RunEvent>>,
        all_reads: Cell<usize>,
    }

    #[allow(unused_variables)]
    impl EventStore for Events {
        fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
            unreachable!("update_events is not what the test reaches")
        }
        fn all_events(&self) -> Result<Vec<RunEvent>> {
            self.all_reads.set(self.all_reads.get() + 1);
            Ok(self.events.borrow().clone())
        }
        fn earliest_event_at(&self) -> Result<Option<i64>> {
            Ok(self
                .events
                .borrow()
                .iter()
                .filter_map(|event| crate::domain::stats::timestamp_millis(&event.created_at))
                .min())
        }
        fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
            unreachable!("latest_task_events is not what the test reaches")
        }
        fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
            unreachable!("run_events is not what the test reaches")
        }
        fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
            unreachable!("has_run_event is not what the test reaches")
        }
        fn record_runtime_event(
            &self,
            id: &RunId,
            kind: EventKind,
            payload: serde_json::Value,
        ) -> Result<()> {
            unreachable!("record_runtime_event is not what the test reaches")
        }
        fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
            unreachable!("last_observe is not what the test reaches")
        }
        fn latest_event_id(&self) -> Result<EventId> {
            unreachable!("latest_event_id is not what the test reaches")
        }
        fn record_backend_failure(
            &self,
            run: Option<&RunId>,
            payload: serde_json::Value,
        ) -> Result<()> {
            unreachable!("record_backend_failure is not what the test reaches")
        }
        fn record_queue_event(
            &self,
            kind: EventKind,
            payload: serde_json::Value,
        ) -> Result<EventId> {
            let mut events = self.events.borrow_mut();
            let id = EventId::new(i64::try_from(events.len())? + 1);
            events.push(RunEvent {
                id,
                task_id: None,
                goal_id: None,
                run_id: None,
                kind: kind.as_str().into(),
                payload,
                created_at: String::new(),
                actor: None,
            });
            Ok(id)
        }
        fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
            unreachable!("latest_event_of is not what the test reaches")
        }
        fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
            unreachable!("latest_events_of is not what the test reaches")
        }
        fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
            Ok(self
                .events
                .borrow()
                .iter()
                .rev()
                .find(|event| kinds.contains(&event.kind.as_str()))
                .cloned())
        }
        fn latest_events_by_supervisor(
            &self,
            kinds: &[&str],
            supervisors: &[&str],
        ) -> Result<Vec<RunEvent>> {
            unreachable!("latest_events_by_supervisor is not what the test reaches")
        }
        fn events_of_between(
            &self,
            kinds: &[&str],
            after: EventId,
            upto: EventId,
            limit: usize,
        ) -> Result<Vec<RunEvent>> {
            unreachable!("events_of_between is not what the test reaches")
        }
        fn events_between(
            &self,
            after: EventId,
            upto: EventId,
            filter: &crate::domain::EventFilter,
            limit: usize,
        ) -> Result<Vec<RunEvent>> {
            unreachable!("events_between is not what the test reaches")
        }
    }

    /// Main at `h` with no commits and no paths, keeping the `since` its
    /// commits were read from.
    #[derive(Default)]
    struct Main {
        since: Cell<Option<i64>>,
    }

    impl MainReader for Main {
        fn head(&self) -> Result<String> {
            Ok("h".to_owned())
        }
        fn commits(
            &self,
            _head: &str,
            _after: Option<&str>,
            since: i64,
        ) -> Result<Vec<crate::domain::stats::conflicts::MainCommit>> {
            self.since.set(Some(since));
            Ok(Vec::new())
        }
        fn paths(&self, _commit: &str) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn reaches(&self, _commit: &str, _head: &str) -> Result<bool> {
            Ok(true)
        }
        fn merge_base(&self, _a: &str, _b: &str) -> Result<Option<String>> {
            Ok(None)
        }
    }

    /// The first record reads main's commits since a second before the
    /// queue's earliest event, the least time of every event's and not the
    /// first id's, as a read of every event finds it, without reading every
    /// event; a queue of few events or many reads none of them.
    #[test]
    fn the_first_record_reads_the_earliest_time_without_reading_every_event() {
        let event = |id: i64, created_at: &str| RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: "task_added".into(),
            payload: serde_json::json!({}),
            created_at: created_at.to_owned(),
            actor: None,
        };
        for count in [2, 500] {
            let queue = Events::default();
            let mut events: Vec<RunEvent> = (1..count)
                .map(|id| event(id, "2026-09-23T00:00:00.000Z"))
                .collect();
            events.push(event(count, "2026-09-22T00:00:01.500Z"));
            *queue.events.borrow_mut() = events;
            let every = queue
                .all_events()
                .unwrap()
                .iter()
                .filter_map(|event| crate::domain::stats::timestamp_millis(&event.created_at))
                .min()
                .unwrap();
            queue.all_reads.set(0);
            let main = Main::default();
            let now = every.div_euclid(1000) + 86_400;
            let (head, at, failing) = look_at_main(&queue, now, &main).unwrap();
            assert_eq!((head.as_deref(), at, failing), (Some("h"), now, false));
            assert_eq!(main.since.get(), Some(every.div_euclid(1000) - 1));
            assert_eq!(queue.all_reads.get(), 0, "{count} events");
        }
    }
}
