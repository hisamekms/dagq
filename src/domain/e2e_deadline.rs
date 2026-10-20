//! The deadlines of the e2e test binary (`tests/e2e.rs`) and the overall
//! limit of its timeout cleanup, which `tests/common` keeps the clock, the
//! waits, the reports and the exit for (docs/development/testing.md, section
//! "待ちの上限").
//!
//! Every deadline is nested in what encloses it: the e2e process's (its
//! first fixture's start plus the run's limit), a test's inside it, and a
//! wait or a polling loop of the test inside the test's, each ending a
//! margin before the one around it. So whatever stops, a wait past its
//! deadline names it first, and nothing a test waits for outlives the
//! process's deadline, however late the test started.
//!
//! The cleanup past a timeout runs its hooks newest first, each within its
//! own limit cut to what is left of an overall limit when there is one; past
//! it the hooks not run yet are not started, and what ran, what was left
//! running and what was not run are named.
//!
//! Times are durations since one origin the caller keeps; nothing here
//! reads a clock.

use std::time::Duration;

/// How long before the deadline of what encloses it (its test, or the e2e
/// process) a wait the monitor times ends: far more than the monitor's poll,
/// so the inner wait is the one late first.
pub const WAIT_MARGIN: Duration = Duration::from_secs(30);

/// How long before its test's deadline a polling loop of the test ends:
/// more than [`WAIT_MARGIN`], so that what the loop does on its way to its
/// panic (a last `dagq show`, the join of a reader) still ends by its own
/// name before the test's deadline.
pub const LOOP_MARGIN: Duration = Duration::from_secs(60);

/// The deadline of a wait or a loop opened at `at` for `limit`, inside
/// what ends at `enclosing`: `margin` before that end when its own would
/// come later. One opened too late for that is past its deadline at once.
pub fn nested_deadline(
    at: Duration,
    limit: Duration,
    enclosing: Option<Duration>,
    margin: Duration,
) -> Duration {
    let own = at.saturating_add(limit);
    match enclosing {
        Some(end) => own.min(end.saturating_sub(margin)),
        None => own,
    }
}

/// Which of the open waits, by `(key, deadline)`, is past its deadline at
/// `now` (one is when `now` reaches it): the one whose deadline came first,
/// so a wait nested in another is named before it; the newest (the
/// greatest key) between equal deadlines.
pub fn first_late<K: Copy + Ord>(
    waits: impl IntoIterator<Item = (K, Duration)>,
    now: Duration,
) -> Option<K> {
    waits
        .into_iter()
        .filter(|(_, deadline)| *deadline <= now)
        .min_by_key(|(key, deadline)| (*deadline, std::cmp::Reverse(*key)))
        .map(|(key, _)| key)
}

/// How a hook of the cleanup ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    Done,
    Panicked,
    /// Its thread could not start, with why.
    CouldNotStart(String),
    /// Still running when its wait (the duration) ran out; its thread is
    /// left behind.
    DidNotFinish(Duration),
}

impl HookOutcome {
    fn text(&self) -> String {
        match self {
            Self::Done => "done".to_owned(),
            Self::Panicked => "panicked".to_owned(),
            Self::CouldNotStart(error) => format!("could not start: {error}"),
            Self::DidNotFinish(waited) => format!("did not finish within {waited:?}"),
        }
    }
}

/// The cleanup past a timeout as it goes: how long each hook is waited for
/// and the report of how it ended. With no overall limit (the `tests/it`
/// binary) every hook runs within its own limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleanup {
    total: Option<Duration>,
    ran: Vec<(String, HookOutcome)>,
    not_run: Vec<String>,
    cut_off: bool,
}

impl Cleanup {
    /// A cleanup within `total` overall, when there is one.
    pub fn new(total: Option<Duration>) -> Self {
        Self {
            total,
            ran: Vec::new(),
            not_run: Vec::new(),
            cut_off: false,
        }
    }

    /// How long to wait for the next hook, `what`, whose own limit is
    /// `limit`, `elapsed` into the cleanup: its limit, or what is left of
    /// the overall limit when that is less; `None`, recording it as not
    /// run, once the overall limit is reached.
    pub fn wait_for(&mut self, what: &str, limit: Duration, elapsed: Duration) -> Option<Duration> {
        let Some(total) = self.total else {
            return Some(limit);
        };
        if elapsed >= total {
            self.cut_off = true;
            self.not_run.push(what.to_owned());
            return None;
        }
        Some(limit.min(total - elapsed))
    }

    /// Record that the hook `what`, whose own limit is `limit`, ended with
    /// `outcome`; the line that reports it.
    pub fn ended(&mut self, what: &str, limit: Duration, outcome: HookOutcome) -> String {
        if matches!(outcome, HookOutcome::DidNotFinish(waited) if waited < limit) {
            self.cut_off = true;
        }
        let line = format!("  cleanup {what} {}\n", outcome.text());
        self.ran.push((what.to_owned(), outcome));
        line
    }

    /// The report that closes a cleanup cut off at its overall limit: the
    /// hooks that ended, those left running and those not run, by name;
    /// `None` when none was cut off.
    pub fn summary(&self) -> Option<String> {
        let total = self.total.filter(|_| self.cut_off)?;
        let names = |of: &mut dyn Iterator<Item = &String>| {
            let names: Vec<&str> = of.map(String::as_str).collect();
            if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(", ")
            }
        };
        let ended = names(
            &mut self
                .ran
                .iter()
                .filter(|(_, outcome)| !matches!(outcome, HookOutcome::DidNotFinish(_)))
                .map(|(what, _)| what),
        );
        let running = names(
            &mut self
                .ran
                .iter()
                .filter(|(_, outcome)| matches!(outcome, HookOutcome::DidNotFinish(_)))
                .map(|(what, _)| what),
        );
        let not_run = names(&mut self.not_run.iter());
        Some(format!(
            "cleanup cut off at its overall limit of {total:?}: ended: {ended}; \
             left running: {running}; not run: {not_run}\n"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_wait_keeps_its_own_limit_inside_nothing_or_far_from_its_enclosing_end() {
        assert_eq!(
            nested_deadline(secs(10), secs(300), None, WAIT_MARGIN),
            secs(310)
        );
        assert_eq!(
            nested_deadline(secs(10), secs(300), Some(secs(1200)), WAIT_MARGIN),
            secs(310)
        );
        // Ending exactly at the enclosing end less the margin keeps it.
        assert_eq!(
            nested_deadline(secs(870), secs(300), Some(secs(1200)), WAIT_MARGIN),
            secs(1170)
        );
    }

    #[test]
    fn a_wait_that_would_outlive_its_enclosing_end_ends_a_margin_before_it() {
        // A millisecond past the enclosing end less the margin is cut.
        assert_eq!(
            nested_deadline(secs(870) + MS, secs(300), Some(secs(1200)), WAIT_MARGIN),
            secs(1170)
        );
        assert_eq!(
            nested_deadline(secs(1000), secs(300), Some(secs(1200)), LOOP_MARGIN),
            secs(1140)
        );
        // Opened past that point it is late at once.
        assert_eq!(
            nested_deadline(secs(1190), secs(300), Some(secs(1200)), WAIT_MARGIN),
            secs(1170)
        );
    }

    #[test]
    fn a_test_started_late_ends_before_the_process_deadline() {
        let run_end = secs(1320);
        let test_limit = secs(1200);
        // The first test: its own limit.
        assert_eq!(
            nested_deadline(Duration::ZERO, test_limit, Some(run_end), WAIT_MARGIN),
            test_limit
        );
        // Any later one: never past the process's deadline.
        for start in [secs(90), secs(500), secs(1290), secs(1400)] {
            let end = nested_deadline(start, test_limit, Some(run_end), WAIT_MARGIN);
            assert!(end + WAIT_MARGIN <= run_end, "{start:?}: {end:?}");
            // A wait of that test ends before the test does.
            let wait = nested_deadline(start, secs(300), Some(end), WAIT_MARGIN);
            assert!(wait + WAIT_MARGIN <= end, "{start:?}: {wait:?}");
        }
    }

    #[test]
    fn a_wait_is_late_once_its_deadline_comes_and_the_innermost_is_named() {
        let test = (1_u64, secs(1170));
        let step = (2_u64, secs(1140));
        let run = (0_u64, secs(1320));
        assert_eq!(first_late([test, step, run], secs(1140) - MS), None);
        assert_eq!(first_late([test, step, run], secs(1140)), Some(2));
        // The monitor came late: every deadline passed, the first is named.
        assert_eq!(first_late([run, test, step], secs(1400)), Some(2));
        assert_eq!(first_late([run, test], secs(1200)), Some(1));
        // Equal deadlines: the newest.
        assert_eq!(first_late([(3, secs(5)), (4, secs(5))], secs(5)), Some(4));
        assert_eq!(first_late::<u64>([], secs(5)), None);
    }

    #[test]
    fn without_an_overall_limit_every_hook_runs_within_its_own() {
        let mut cleanup = Cleanup::new(None);
        for (what, elapsed) in [("a", secs(0)), ("b", secs(600)), ("c", secs(10_000))] {
            assert_eq!(cleanup.wait_for(what, secs(60), elapsed), Some(secs(60)));
            cleanup.ended(what, secs(60), HookOutcome::DidNotFinish(secs(60)));
        }
        assert_eq!(cleanup.summary(), None);
    }

    #[test]
    fn hooks_that_all_end_within_the_overall_limit_are_each_reported() {
        let mut cleanup = Cleanup::new(Some(secs(120)));
        assert_eq!(
            cleanup.wait_for("kill process 7", secs(60), Duration::ZERO),
            Some(secs(60))
        );
        assert_eq!(
            cleanup.ended("kill process 7", secs(60), HookOutcome::Done),
            "  cleanup kill process 7 done\n"
        );
        // One past its own limit, the overall one not reached, is not a cut.
        assert_eq!(cleanup.wait_for("b", secs(60), secs(1)), Some(secs(60)));
        assert_eq!(
            cleanup.ended("b", secs(60), HookOutcome::DidNotFinish(secs(60))),
            "  cleanup b did not finish within 60s\n"
        );
        assert_eq!(cleanup.wait_for("c", secs(300), secs(61)), Some(secs(59)));
        assert_eq!(
            cleanup.ended("c", secs(300), HookOutcome::Panicked),
            "  cleanup c panicked\n"
        );
        assert_eq!(cleanup.wait_for("d", secs(60), secs(62)), Some(secs(58)));
        assert_eq!(
            cleanup.ended(
                "d",
                secs(60),
                HookOutcome::CouldNotStart("no threads".into())
            ),
            "  cleanup d could not start: no threads\n"
        );
        assert_eq!(cleanup.summary(), None);
    }

    #[test]
    fn a_hook_waited_for_what_is_left_and_past_it_the_rest_are_not_run() {
        let mut cleanup = Cleanup::new(Some(secs(120)));
        assert_eq!(
            cleanup.wait_for("a", secs(60), Duration::ZERO),
            Some(secs(60))
        );
        cleanup.ended("a", secs(60), HookOutcome::Done);
        // A millisecond before the limit: waited for what is left.
        assert_eq!(cleanup.wait_for("b", secs(300), secs(120) - MS), Some(MS));
        cleanup.ended("b", secs(300), HookOutcome::DidNotFinish(MS));
        // At the limit: not run.
        assert_eq!(cleanup.wait_for("c", secs(60), secs(120)), None);
        assert_eq!(cleanup.wait_for("d", secs(60), secs(121)), None);
        assert_eq!(
            cleanup.summary().unwrap(),
            "cleanup cut off at its overall limit of 120s: ended: a; \
             left running: b; not run: c, d\n"
        );
    }

    #[test]
    fn a_hook_cut_short_by_the_limit_is_a_cut_off_with_none_left_to_run() {
        let mut cleanup = Cleanup::new(Some(secs(120)));
        assert_eq!(cleanup.wait_for("a", secs(300), secs(30)), Some(secs(90)));
        cleanup.ended("a", secs(300), HookOutcome::DidNotFinish(secs(90)));
        assert_eq!(
            cleanup.summary().unwrap(),
            "cleanup cut off at its overall limit of 120s: ended: none; \
             left running: a; not run: none\n"
        );
    }
}
