//! A run's session steps folded as the inner layer of its intervals
//! (ADR-t1662-1 decision 7, ADR-t1662-2 decision 4, ADR-t1662-3 decision
//! 12, docs/design/measurement.md "畳む関数"), beside the fold of its
//! events ([`super::fold`]). A step lies on the supervisor's clock at its
//! session's start plus its offset. Per interval of the run (its parent),
//! [`StepFold::parents`] splits the time the supervisor sees only as the
//! agent's into model turns, tool calls, subagents and waits, with the
//! share any step covers (its coverage) and the rest no step accounts
//! for. The inner layer may overlap (a subagent and the calls inside it,
//! parallel calls): each kind counts the time any step of it covers once,
//! so the kinds' sums may exceed the covered time, which never exceeds
//! its parent. It never splits the run's intervals nor changes their
//! sums.
//!
//! A fold keeps each session's seqs it counted and each step's kind and
//! time (never what it ran), so a later fold goes on from it
//! ([`StepFold::fold_onto`]) once the retention removed the steps it
//! counted: a seq it counted is not counted again (a step sent again,
//! held or removed), and a seq it did not (one that fills a gap, seq 2
//! after seqs 1 and 3 were folded and removed) adds to the counts, the
//! gaps and the coverage. What lies outside the intervals is read against
//! the intervals given then, so a run's later intervals (a push that
//! closed it later) count its steps again. The same steps give the same
//! fold, in whatever order and batches they came. Pure: reads nothing but
//! its arguments.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::fold::Interval;
use crate::domain::session_step::{SeqRange, SessionStep, StepKind, gaps};

/// A time `[start, end)` on the supervisor's clock, unix milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Span {
    pub start: i64,
    pub end: i64,
}

impl Span {
    fn millis(self) -> i64 {
        self.end.saturating_sub(self.start)
    }

    /// Whether it lies in `interval`: shares some time with it, or for a
    /// step of no length starts in it.
    fn lies_in(self, interval: &Interval) -> bool {
        if self.end > self.start {
            self.start.max(interval.start) < self.end.min(interval.end)
        } else {
            interval.start <= self.start && self.start < interval.end
        }
    }
}

/// `spans` as the disjoint spans of their union, ascending; none of no
/// length.
fn union(mut spans: Vec<Span>) -> Vec<Span> {
    spans.retain(|span| span.end > span.start);
    spans.sort();
    let mut merged: Vec<Span> = Vec::new();
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// How long the disjoint `spans` cover of `[from, to)`.
fn within(spans: &[Span], from: i64, to: i64) -> i64 {
    spans.iter().fold(0, |sum: i64, span| {
        let part = Span {
            start: span.start.max(from),
            end: span.end.min(to),
        };
        sum.saturating_add(part.millis().max(0))
    })
}

/// `ranges` as the runs of consecutive seqs they hold together.
fn merge_ranges(mut ranges: Vec<SeqRange>) -> Vec<SeqRange> {
    ranges.sort_by_key(|range| range.from);
    let mut merged: Vec<SeqRange> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.from <= last.to.saturating_add(1) => {
                last.to = last.to.max(range.to);
            }
            _ => merged.push(range),
        }
    }
    merged
}

/// What one interval of the run (a parent) holds of the steps: each kind's
/// covered time (every kind named), the time any step covers, its share of
/// the interval and the rest no step accounts for (milliseconds).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParentCoverage {
    /// Its index in the run's intervals.
    pub interval: usize,
    pub phase: String,
    pub start: i64,
    pub end: i64,
    pub by_kind: BTreeMap<StepKind, i64>,
    pub covered_ms: i64,
    /// `covered_ms` over the interval's length, 0 for one of no length.
    pub coverage: f64,
    pub uncovered_ms: i64,
}

/// A step counted, placed on the supervisor's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PlacedStep {
    pub span: Span,
    pub kind: StepKind,
}

/// A step counted whose session's start was not known yet: placed by a
/// later fold that knows it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UnplacedStep {
    pub session_id: String,
    pub seq: i64,
    pub kind: StepKind,
    pub offset_ms: i64,
    pub duration_ms: i64,
}

impl UnplacedStep {
    fn place(&self, start: i64) -> PlacedStep {
        let at = start.saturating_add(self.offset_ms);
        PlacedStep {
            span: Span {
                start: at,
                end: at.saturating_add(self.duration_ms),
            },
            kind: self.kind,
        }
    }
}

/// The steps of a run folded, what a later fold goes on from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepFold {
    /// Each session's seqs counted, as runs of consecutive seqs: a step
    /// whose seq is in them is not counted again, whether the store still
    /// holds it or not.
    pub seqs: BTreeMap<String, Vec<SeqRange>>,
    /// How many steps of each kind were counted.
    pub counts: BTreeMap<StepKind, u64>,
    /// The steps counted and placed, ascending.
    pub placed: Vec<PlacedStep>,
    /// The steps counted of a session whose start was not known: in the
    /// counts and the seqs, covering no time until a fold places them.
    pub unplaced: Vec<UnplacedStep>,
    /// The start on the supervisor's clock of each session it placed steps
    /// of: a later fold places that session's late steps by it once the
    /// retention removed the session's owner with its steps.
    #[serde(default)]
    pub starts: BTreeMap<String, i64>,
}

/// The key of a run row's `detail` that holds its steps folded
/// ([`StepFold::detail`]); a run with no step counted has none.
pub const DETAIL_KEY: &str = "steps";

/// Fold a run's `steps` over nothing ([`StepFold::fold_onto`]). `starts`
/// are its sessions' starts on the supervisor's clock, by session id.
pub fn fold_steps(starts: &BTreeMap<String, i64>, steps: &[SessionStep]) -> StepFold {
    StepFold::default().fold_onto(starts, steps)
}

impl StepFold {
    /// This fold with the `steps` it did not count added: a step whose
    /// `(session_id, seq)` it counted, or one of the same `steps` before,
    /// is left out, so a step sent again changes nothing, and one that
    /// came in after the retention removed those around it fills their
    /// gap. Each added step counts by its kind and covers its time on the
    /// supervisor's clock, its session's start plus its offset: the start
    /// this fold kept of the session, else the one in `starts`. The steps
    /// it held unplaced whose start `starts` now knows are placed.
    pub fn fold_onto(&self, starts: &BTreeMap<String, i64>, steps: &[SessionStep]) -> StepFold {
        let mut fold = self.clone();
        let mut added: BTreeMap<&str, BTreeSet<i64>> = BTreeMap::new();
        let mut unplaced = std::mem::take(&mut fold.unplaced);
        for step in steps {
            let seq = step.step.seq;
            if self.holds(&step.session_id, seq)
                || !added.entry(&step.session_id).or_default().insert(seq)
            {
                continue;
            }
            *fold.counts.entry(step.step.kind).or_default() += 1;
            unplaced.push(UnplacedStep {
                session_id: step.session_id.clone(),
                seq,
                kind: step.step.kind,
                offset_ms: step.step.offset_ms,
                duration_ms: step.step.duration_ms,
            });
        }
        for step in unplaced {
            let start = self
                .starts
                .get(&step.session_id)
                .or_else(|| starts.get(&step.session_id));
            match start {
                Some(&start) => {
                    fold.starts.insert(step.session_id.clone(), start);
                    fold.placed.push(step.place(start));
                }
                None => fold.unplaced.push(step),
            }
        }
        fold.placed.sort();
        fold.unplaced.sort();
        for (session, seqs) in added {
            let ranges = fold.seqs.entry(session.to_owned()).or_default();
            ranges.extend(seqs.into_iter().map(|seq| SeqRange { from: seq, to: seq }));
            *ranges = merge_ranges(std::mem::take(ranges));
        }
        fold
    }

    /// What a run row's `detail` holds of it over the run's `intervals`
    /// under [`DETAIL_KEY`]: the fold itself (`fold`, what a later fold goes
    /// on from, [`Self::of_detail`]), each interval's kinds and coverage
    /// (`parents`), the steps and the time outside every interval
    /// (`outside`, `outside_ms`), and the gaps (`gaps`, `missing`).
    pub fn detail(&self, intervals: &[Interval]) -> Value {
        json!({
            "fold": self,
            "parents": self.parents(intervals),
            "outside": self.outside(intervals),
            "outside_ms": self.outside_ms(intervals),
            "gaps": self.gaps(),
            "missing": self.missing(),
        })
    }

    /// The fold a run row's `detail` holds ([`Self::detail`]); none when it
    /// holds none, or one this binary cannot read.
    pub fn of_detail(detail: &Value) -> Option<Self> {
        serde_json::from_value(detail.get(DETAIL_KEY)?.get("fold")?.clone()).ok()
    }

    /// Whether it counted the seq `seq` of `session_id`.
    pub fn holds(&self, session_id: &str, seq: i64) -> bool {
        self.seqs
            .get(session_id)
            .is_some_and(|ranges| ranges.iter().any(|range| range.contains(seq)))
    }

    /// How many steps it counted.
    pub fn count(&self) -> u64 {
        self.counts.values().sum()
    }

    /// Each session's gaps: the seqs below its largest one counted that it
    /// did not count ([`gaps`]).
    pub fn gaps(&self) -> BTreeMap<String, Vec<SeqRange>> {
        self.seqs
            .iter()
            .map(|(session, ranges)| (session.clone(), gaps(ranges.iter().copied())))
            .filter(|(_, gaps)| !gaps.is_empty())
            .collect()
    }

    /// How many seqs its gaps span.
    pub fn missing(&self) -> i64 {
        self.gaps()
            .values()
            .flatten()
            .fold(0, |sum: i64, gap| sum.saturating_add(gap.count()))
    }

    /// The disjoint spans the placed steps of `kind`, or of any kind, cover.
    fn covered(&self, kind: Option<StepKind>) -> Vec<Span> {
        union(
            self.placed
                .iter()
                .filter(|step| kind.is_none_or(|kind| step.kind == kind))
                .map(|step| step.span)
                .collect(),
        )
    }

    /// What each of `intervals` holds of the steps.
    pub fn parents(&self, intervals: &[Interval]) -> Vec<ParentCoverage> {
        let covered = self.covered(None);
        let kinds: Vec<(StepKind, Vec<Span>)> = StepKind::ALL
            .into_iter()
            .map(|kind| (kind, self.covered(Some(kind))))
            .collect();
        intervals
            .iter()
            .enumerate()
            .map(|(index, interval)| {
                let covered_ms = within(&covered, interval.start, interval.end);
                let length = interval.end.saturating_sub(interval.start).max(0);
                ParentCoverage {
                    interval: index,
                    phase: interval.phase.clone(),
                    start: interval.start,
                    end: interval.end,
                    by_kind: kinds
                        .iter()
                        .map(|(kind, spans)| (*kind, within(spans, interval.start, interval.end)))
                        .collect(),
                    covered_ms,
                    coverage: if length > 0 {
                        covered_ms as f64 / length as f64
                    } else {
                        0.0
                    },
                    uncovered_ms: length.saturating_sub(covered_ms).max(0),
                }
            })
            .collect()
    }

    /// How many placed steps lie in none of `intervals`.
    pub fn outside(&self, intervals: &[Interval]) -> usize {
        self.placed
            .iter()
            .filter(|step| !intervals.iter().any(|interval| step.span.lies_in(interval)))
            .count()
    }

    /// The time steps cover outside every one of `intervals` (which do not
    /// overlap).
    pub fn outside_ms(&self, intervals: &[Interval]) -> i64 {
        let covered = self.covered(None);
        let all = covered
            .iter()
            .fold(0, |sum: i64, span| sum.saturating_add(span.millis()));
        let inside = intervals.iter().fold(0, |sum: i64, interval| {
            sum.saturating_add(within(&covered, interval.start, interval.end))
        });
        all.saturating_sub(inside).max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RunId;
    use crate::domain::run_phase::{Blocker, Holds};
    use crate::domain::session_step::{Step, StepTarget};

    const SESSION: &str = "s-1";

    fn interval(phase: &str, start: i64, end: i64) -> Interval {
        Interval {
            phase: phase.to_owned(),
            blocker: Some(Blocker::Ai),
            holds: Some(Holds::WorkerSlot),
            attempt: None,
            start,
            end,
            cause: None,
            blocked_by: None,
            run_id: None,
        }
    }

    fn step(
        session: &str,
        seq: i64,
        kind: StepKind,
        offset_ms: i64,
        duration_ms: i64,
    ) -> SessionStep {
        SessionStep {
            id: seq,
            target: StepTarget::run(&RunId::new("r-1").unwrap()),
            session_id: session.to_owned(),
            received_at: 0,
            step: Step::new(seq, kind, offset_ms, duration_ms),
        }
    }

    /// One session that started at 1000 on the supervisor's clock.
    fn starts() -> BTreeMap<String, i64> {
        BTreeMap::from([(SESSION.to_owned(), 1_000)])
    }

    fn kinds(model: i64, tool: i64, subagent: i64, wait: i64) -> BTreeMap<StepKind, i64> {
        BTreeMap::from([
            (StepKind::ModelTurn, model),
            (StepKind::ToolCall, tool),
            (StepKind::Subagent, subagent),
            (StepKind::Wait, wait),
        ])
    }

    #[test]
    fn each_parent_splits_into_the_kinds_with_its_coverage_and_overlaps_count_once() {
        // work 1000..2000, validating 2000..3000.
        let intervals = [
            interval("work", 1_000, 2_000),
            interval("validating", 2_000, 3_000),
        ];
        let steps = [
            // A turn 1000..1200, then a subagent 1200..1600 with a tool's
            // call inside it 1300..1500 and another in parallel 1400..1700.
            step(SESSION, 1, StepKind::ModelTurn, 0, 200),
            step(SESSION, 2, StepKind::Subagent, 200, 400),
            step(SESSION, 3, StepKind::ToolCall, 300, 200),
            step(SESSION, 4, StepKind::ToolCall, 400, 300),
            // A wait across the boundary 1900..2100.
            step(SESSION, 5, StepKind::Wait, 900, 200),
        ];
        let fold = fold_steps(&starts(), &steps);
        assert_eq!(fold.count(), 5);
        let parents = fold.parents(&intervals);
        assert_eq!(parents.len(), 2);
        let work = &parents[0];
        assert_eq!((work.interval, work.phase.as_str()), (0, "work"));
        // Tool calls overlap each other: 1300..1700 is 400 once.
        assert_eq!(work.by_kind, kinds(200, 400, 400, 100));
        // Covered: 1000..1700 and 1900..2000.
        assert_eq!(work.covered_ms, 800);
        assert_eq!(work.uncovered_ms, 200);
        assert!((work.coverage - 0.8).abs() < 1e-9);
        // The kinds' sums exceed the covered time, never the parent.
        assert!(work.by_kind.values().sum::<i64>() > work.covered_ms);
        let validating = &parents[1];
        assert_eq!(validating.by_kind, kinds(0, 0, 0, 100));
        assert_eq!(validating.covered_ms, 100);
        assert_eq!(validating.uncovered_ms, 900);
        assert!((validating.coverage - 0.1).abs() < 1e-9);
        assert_eq!(fold.outside(&intervals), 0);
        assert_eq!(fold.outside_ms(&intervals), 0);
    }

    #[test]
    fn steps_outside_every_parent_and_the_gaps_are_counted() {
        let intervals = [interval("work", 1_000, 2_000)];
        let steps = [
            step(SESSION, 1, StepKind::ModelTurn, 0, 100),
            // 2500..2800: after the run's intervals.
            step(SESSION, 4, StepKind::ToolCall, 1_500, 300),
            // A step of no length at 2000, the end: outside.
            step(SESSION, 5, StepKind::Wait, 1_000, 0),
            step(SESSION, 8, StepKind::ModelTurn, 500, 100),
            // A session whose start is not known.
            step("s-2", 2, StepKind::ToolCall, 0, 100),
        ];
        let fold = fold_steps(&starts(), &steps);
        assert_eq!(fold.count(), 5);
        assert_eq!(fold.outside(&intervals), 2);
        assert_eq!(fold.outside_ms(&intervals), 300);
        assert_eq!(fold.unplaced.len(), 1);
        assert_eq!(
            fold.gaps(),
            BTreeMap::from([
                (
                    SESSION.to_owned(),
                    vec![SeqRange { from: 2, to: 3 }, SeqRange { from: 6, to: 7 }]
                ),
                ("s-2".to_owned(), vec![SeqRange { from: 1, to: 1 }]),
            ])
        );
        assert_eq!(fold.missing(), 5);
        // A parent of no length has no coverage.
        let empty = [interval("work", 1_000, 1_000)];
        let parents = fold.parents(&empty);
        assert_eq!((parents[0].covered_ms, parents[0].coverage), (0, 0.0));
        assert_eq!(parents[0].uncovered_ms, 0);
    }

    #[test]
    fn the_same_steps_twice_give_the_same_fold() {
        let steps = [
            step(SESSION, 1, StepKind::ModelTurn, 0, 200),
            step(SESSION, 3, StepKind::ToolCall, 300, 200),
        ];
        let once = fold_steps(&starts(), &steps);
        let twice: Vec<SessionStep> = steps.iter().chain(&steps).cloned().collect();
        assert_eq!(fold_steps(&starts(), &twice), once);
        assert_eq!(once.fold_onto(&starts(), &steps), once);
        // A step sent again with another body is still the one counted.
        let again = [step(SESSION, 1, StepKind::Wait, 900, 100)];
        assert_eq!(once.fold_onto(&starts(), &again), once);
        // The fold reads back as written.
        let text = serde_json::to_string(&once).unwrap();
        assert_eq!(serde_json::from_str::<StepFold>(&text).unwrap(), once);
    }

    #[test]
    fn a_fold_after_the_retention_counts_a_late_seq_and_not_a_removed_one_sent_again() {
        let intervals = [interval("work", 1_000, 2_000)];
        // Seqs 1 and 3 folded; the retention removes them from the store.
        let first = fold_steps(
            &starts(),
            &[
                step(SESSION, 1, StepKind::ModelTurn, 0, 200),
                step(SESSION, 3, StepKind::ToolCall, 400, 200),
            ],
        );
        assert_eq!(first.count(), 2);
        assert_eq!(first.missing(), 1);
        assert_eq!(first.parents(&intervals)[0].covered_ms, 400);
        // Then seq 2 comes in for the first time, and seq 3 is sent again.
        let later = first.fold_onto(
            &starts(),
            &[
                step(SESSION, 2, StepKind::Wait, 200, 200),
                step(SESSION, 3, StepKind::ToolCall, 400, 200),
            ],
        );
        assert_eq!(later.count(), 3);
        assert_eq!(later.counts[&StepKind::ToolCall], 1);
        assert_eq!(later.counts[&StepKind::Wait], 1);
        assert_eq!(later.missing(), 0);
        assert_eq!(later.gaps(), BTreeMap::new());
        assert_eq!(later.seqs[SESSION], vec![SeqRange { from: 1, to: 3 }]);
        let work = &later.parents(&intervals)[0];
        assert_eq!(work.by_kind, kinds(200, 200, 0, 200));
        assert_eq!(work.covered_ms, 600);
        assert!((work.coverage - 0.6).abs() < 1e-9);
        // Sent again once more, nothing changes.
        let resent = [step(SESSION, 2, StepKind::Wait, 200, 200)];
        assert_eq!(later.fold_onto(&starts(), &resent), later);

        // The retention removed the session's owner with its steps too: the
        // start the stored fold kept places seq 2 all the same.
        let stored = StepFold::of_detail(&json!({ DETAIL_KEY: first.detail(&intervals) }))
            .expect("the fold a row's detail holds");
        assert_eq!(stored.starts[SESSION], 1_000);
        let without_owner = stored.fold_onto(
            &BTreeMap::new(),
            &[step(SESSION, 2, StepKind::Wait, 200, 200)],
        );
        assert_eq!(without_owner, later);
        assert!(without_owner.unplaced.is_empty());
    }

    #[test]
    fn spans_and_ranges_merge_into_their_unions() {
        let span = |start, end| Span { start, end };
        assert_eq!(
            union(vec![
                span(5, 7),
                span(0, 2),
                span(2, 3),
                span(6, 9),
                span(4, 4)
            ]),
            vec![span(0, 3), span(5, 9)]
        );
        assert_eq!(within(&[span(0, 3), span(5, 9)], 2, 6), 2);
        assert_eq!(within(&[span(0, 3)], 4, 6), 0);
        let range = |from, to| SeqRange { from, to };
        assert_eq!(
            merge_ranges(vec![range(5, 5), range(1, 2), range(3, 3), range(7, 8)]),
            vec![range(1, 3), range(5, 5), range(7, 8)]
        );
        assert_eq!(
            merge_ranges(vec![range(i64::MAX, i64::MAX), range(1, i64::MAX)]),
            vec![range(1, i64::MAX)]
        );
    }

    #[test]
    fn what_lies_outside_follows_the_intervals_and_a_start_known_later_places_its_steps() {
        let pending = [interval("push_pending", 1_000, 2_000)];
        let steps = [
            step(SESSION, 1, StepKind::ModelTurn, 0, 100),
            // 2050..2150: after the run's intervals while its push pends.
            step(SESSION, 2, StepKind::ToolCall, 1_050, 100),
            step("s-2", 1, StepKind::Wait, 0, 300),
        ];
        let fold = fold_steps(&starts(), &steps);
        assert_eq!(fold.outside(&pending), 1);
        assert_eq!(fold.outside_ms(&pending), 100);
        assert_eq!(fold.unplaced.len(), 1);
        // The push closed it later: the same fold read over its intervals.
        let closed = [pending[0].clone(), interval("ended", 2_000, 3_000)];
        assert_eq!(fold.outside(&closed), 0);
        assert_eq!(fold.outside_ms(&closed), 0);
        // s-2's start is known now: the step held unplaced is placed, and
        // sending it again changes nothing.
        let mut known = starts();
        known.insert("s-2".to_owned(), 2_500);
        let placed = fold.fold_onto(&known, &steps[2..]);
        assert_eq!(placed.unplaced, vec![]);
        assert_eq!(placed.count(), 3);
        assert_eq!(placed.parents(&closed)[1].by_kind, kinds(0, 100, 0, 300));
        assert_eq!(fold.fold_onto(&known, &[]), placed);
        // The batches and their order do not matter.
        let reversed: Vec<SessionStep> = steps.iter().rev().cloned().collect();
        assert_eq!(fold_steps(&known, &reversed), placed);
    }

    #[test]
    fn a_step_across_an_interval_of_no_length_is_outside_and_sums_saturate() {
        let instant = [interval("ended", 1_100, 1_100)];
        let fold = fold_steps(&starts(), &[step(SESSION, 1, StepKind::ToolCall, 0, 200)]);
        assert_eq!(fold.outside(&instant), 1);
        assert_eq!(fold.outside_ms(&instant), 200);
        let far = fold_steps(
            &starts(),
            &[
                step(SESSION, 1, StepKind::Wait, i64::MAX, i64::MAX),
                step(SESSION, 2, StepKind::ModelTurn, 0, i64::MAX),
            ],
        );
        let whole = [interval("work", i64::MIN, i64::MAX)];
        let parent = &far.parents(&whole)[0];
        assert_eq!(parent.covered_ms, i64::MAX - 1_000);
        assert_eq!(far.outside(&whole), 1);
        assert_eq!(far.outside_ms(&whole), 0);
    }
}
