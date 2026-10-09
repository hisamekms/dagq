//! The record of main's first-parent history (docs/design/main-history.md):
//! a pass reads Git only when the files the landing branch is resolved from
//! changed (a commit to main changes them), when the reach is due again or
//! when a failing read is due a retry, and records what
//! [`crate::application::main_log::records`] gives. The runtime decides
//! nothing on the record.

use super::*;
use crate::application::LandingBranchStamp;
use crate::application::main_log::{Recorded, records};
use crate::domain::stats::main_log::{
    MAIN_OBSERVED, MAIN_OBSERVED_INTERVAL_SECS, MAIN_READ_FAILED, MAIN_READ_RECOVERED, MainReading,
    Reach,
};

/// What this process last recorded or found recorded, so a pass on which
/// nothing moved starts no Git.
#[derive(Debug, Default)]
pub(super) struct MainLogWatch {
    /// The head of the reach and when it was recorded (unix seconds), and
    /// whether Git's reading was failing; `None` before the first look.
    last: Option<(Option<String>, i64, bool)>,
    /// The stamp of the landing branch's files taken before the last look
    /// that went through ([`Repository::landing_branch_stamp`]).
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
    pub(super) fn main_log_pass(&mut self, env: &mut PassEnv<'_>) {
        let now = env.generators.clock.now();
        // Read without starting Git.
        let stamp = env.repository.landing_branch_stamp();
        let result = self.main_log.look(now, stamp, || {
            let recorded = Recorded {
                reach: env
                    .queue
                    .latest_queue_event(&[MAIN_OBSERVED])?
                    .as_ref()
                    .and_then(Reach::of),
                reading: MainReading::of(
                    env.queue
                        .latest_queue_event(&[MAIN_READ_FAILED, MAIN_READ_RECOVERED])?
                        .as_ref(),
                ),
            };
            let queue = &*env.queue;
            // The queue's earliest event, read for the first record only.
            let earliest = || -> Result<Option<i64>> {
                Ok(queue
                    .all_events()?
                    .iter()
                    .filter_map(|event| crate::domain::stats::timestamp_millis(&event.created_at))
                    .min())
            };
            let records = records(&recorded, now, &earliest, &**env.repository);
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
                env.queue.record_queue_event(kind, payload)?;
            }
            Ok(last)
        });
        if let Err(error) = result {
            warn!(error = %format_args!("{error:#}"), "main's history could not be recorded: {error:#}");
        }
    }
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
}
