use serde_json::json;

use super::*;
use crate::domain::ledger::testkit::omitted::{counted, covers};
use crate::domain::ledger::testkit::{Log, run_id};

const DENIED: &str = "authorization_denied";
const ISSUED: &str = "telemetry_token_issued";

fn kinds() -> Vec<String> {
    vec![DENIED.to_owned(), ISSUED.to_owned()]
}

/// The queue's refusals and tokens and those of a run and a task, other
/// kinds between, a clock turned back and events of no time.
fn mixed() -> Log {
    let mut log = Log::default();
    for n in 0..30_i64 {
        // The queue's, timed; every fifth turned back by 50 seconds.
        let secs = if n % 5 == 4 { 10 * n - 50 } else { 10 * n };
        log.add(None, None, DENIED, json!({}), secs);
        log.add(Some(n), None, DENIED, json!({}), 10 * n + 1);
        log.add(
            Some(n),
            Some(&run_id(1)),
            ISSUED,
            json!({"target": "run"}),
            10 * n + 2,
        );
        log.add(None, None, ISSUED, json!({"target": "job:1"}), 10 * n + 3);
        log.add(None, None, "mark_added", json!({"mark": "m"}), 10 * n + 4);
    }
    for event in log.events.iter_mut().skip(140) {
        if event.kind.as_str() == DENIED && event.task_id.is_none() {
            event.created_at = "not a time".to_owned();
        }
    }
    log
}

/// Read in pages of `page` from `from`, as a builder reads them.
fn read_in_pages(log: &Log, from: EventId, page: usize, every: u64) -> (Coverage, Vec<Point>) {
    let mut coverage = Coverage::start(&kinds(), from);
    let mut points = Vec::new();
    for events in log.after(from).chunks(page) {
        coverage.read(events, every, events.last().unwrap().id, &mut points);
    }
    points.sort_by(|a, b| (&a.kind, a.at).cmp(&(&b.kind, b.at)));
    (coverage, points)
}

/// Each kind's L, T, M and D are of the queue's events of it in (C, S]
/// alone, a run's and a task's left out; an event of no time moves L only;
/// the points fall on every `every`th event of the kind of any row.
#[test]
fn the_summaries_and_points_are_those_counted_of_c_to_s() {
    let log = mixed();
    for (from, page, every) in [(0, 7, 4), (23, 50, 3), (0, 1000, 1)] {
        let from = EventId::new(from);
        let (coverage, points) = read_in_pages(&log, from, page, every);
        assert_eq!(coverage.from, from);
        assert_eq!(coverage.through, log.last());
        let (summaries, expected) = counted(&log.events, &kinds(), (from, log.last()), every);
        assert_eq!(coverage.summaries, summaries);
        assert_eq!(points, expected);
    }
    let (coverage, _) = read_in_pages(&log, EventId::new(0), 9, 4);
    let denied = &coverage.summaries[DENIED];
    // The last refusals are of no time: L is past T.
    assert!(denied.last > denied.timed);
    assert!(denied.back_ms >= 40_000);
    let issued = &coverage.summaries[ISSUED];
    assert_eq!(issued.seen, 60);
    assert_eq!(issued.back_ms, 0);
}

/// Events read again up to S add nothing; S only moves on.
#[test]
fn events_up_to_s_read_again_add_nothing() {
    let log = mixed();
    let mut coverage = Coverage::start(&kinds(), EventId::new(0));
    let mut points = Vec::new();
    let half = &log.events[..60];
    coverage.read(half, 4, half.last().unwrap().id, &mut points);
    let once = (coverage.clone(), points.len());
    coverage.read(&log.events[10..60], 4, EventId::new(40), &mut points);
    assert_eq!((coverage.clone(), points.len()), once);
    coverage.read(&log.events, 4, log.last(), &mut points);
    assert!(covers(&coverage, &log.events, 4));
    assert_eq!(coverage.through, log.last());
}

/// A coverage replaces only one of its C and kinds whose S is lower.
#[test]
fn a_coverage_supersedes_only_a_lower_s_of_its_c_and_kinds() {
    let log = mixed();
    let (held, _) = read_in_pages(&log, EventId::new(0), 10, 4);
    assert!(held.supersedes(None));
    let mut lower = Coverage::start(&kinds(), EventId::new(0));
    lower.read(&log.events[..20], 4, EventId::new(20), &mut Vec::new());
    assert!(held.supersedes(Some(&lower)));
    assert!(!lower.supersedes(Some(&held)));
    assert!(!held.supersedes(Some(&held)));
    let other_c = Coverage {
        from: EventId::new(1),
        ..lower.clone()
    };
    assert!(!held.supersedes(Some(&other_c)));
    let other_kinds = Coverage {
        kinds: vec![DENIED.to_owned()],
        ..lower
    };
    assert!(!held.supersedes(Some(&other_kinds)));
}

/// A store stops a builder when its R is past its S or its kinds are not
/// the builder's; one of no coverage stops none.
#[test]
fn r_past_s_or_other_kinds_stop() {
    let mut stored = Coverage::start(&kinds(), EventId::new(0));
    stored.through = EventId::new(10);
    let kinds = kinds();
    assert!(!stops(None, Some(EventId::new(99)), &kinds));
    assert!(!stops(Some(&stored), None, &kinds));
    assert!(!stops(Some(&stored), Some(EventId::new(10)), &kinds));
    assert!(stops(Some(&stored), Some(EventId::new(11)), &kinds));
    assert!(stops(Some(&stored), None, &kinds[..1]));
    assert!(stops(Some(&stored), None, &[]));
}

/// A builder of no kind holds and writes none; one of some kind starts C
/// where it reads from once, keeps it, takes a store's further one, and
/// stops for good on a store that stops it or whose S is before where it
/// reads.
#[test]
fn a_covering_begins_once_and_a_stop_stays() {
    let log = mixed();
    let none = Omission::of([], 4);
    let mut off = Covering::default();
    off.begin(&none, (None, None), EventId::new(5));
    off.read(&log.events, 4, log.last());
    assert_eq!(off.to_write(), None);
    assert!(!off.stopped());

    let omission = Omission::of([ISSUED, DENIED, DENIED], 4);
    assert_eq!(omission.kinds, kinds());
    let mut covering = Covering::default();
    covering.begin(&omission, (None, None), EventId::new(5));
    covering.read(&log.events[5..50], 4, EventId::new(50));
    covering.begin(&omission, (None, None), EventId::new(50));
    let held = covering.to_write().unwrap();
    assert_eq!(held.coverage.from, EventId::new(5));
    assert_eq!(held.coverage.through, EventId::new(50));
    assert!(!held.points.is_empty());
    covering.wrote(EventId::new(50));
    assert!(covering.to_write().unwrap().points.is_empty());

    // A new builder takes the store's coverage, from no later than its S.
    let mut new = Covering::default();
    new.begin(&omission, (Some(&held.coverage), None), EventId::new(30));
    new.read(&log.events, 4, log.last());
    let taken = new.held().unwrap();
    assert_eq!(taken.from, EventId::new(5));
    assert!(covers(taken, &log.events, 4));

    // The store's S before where it reads: it takes the store's, behind
    // until it reads again the events up to where it read.
    let mut gap = Covering::default();
    gap.begin(&omission, (Some(&held.coverage), None), EventId::new(60));
    assert!(!gap.stopped());
    assert_eq!(gap.behind(EventId::new(60)), Some(EventId::new(50)));
    gap.read(&log.events[..60], 4, EventId::new(60));
    assert_eq!(gap.behind(EventId::new(60)), None);
    assert!(covers(gap.held().unwrap(), &log.events, 4));
    let mut whole = Covering::default();
    whole.begin(&omission, (None, None), EventId::new(5));
    whole.read(&log.events[5..60], 4, EventId::new(60));
    assert_eq!(gap.held(), whole.held());

    // Another builder's of another C written first: it takes the store's,
    // its own points not written dropped (they are of its own C).
    let mut other_c = Covering::default();
    other_c.begin(&omission, (None, None), EventId::new(20));
    other_c.read(&log.events[20..70], 4, EventId::new(70));
    assert!(!other_c.to_write().unwrap().points.is_empty());
    other_c.begin(&omission, (Some(&held.coverage), None), EventId::new(70));
    assert_eq!(other_c.held().unwrap().from, EventId::new(5));
    assert_eq!(other_c.behind(EventId::new(70)), Some(EventId::new(50)));
    assert!(other_c.to_write().unwrap().points.is_empty());
    // Of its own C and behind the store's: it keeps its points not written
    // past the store's S until the store holds them.
    let mut same_c = Covering::default();
    same_c.begin(&omission, (None, None), EventId::new(5));
    same_c.read(&log.events[5..90], 4, EventId::new(90));
    let points = same_c.to_write().unwrap().points;
    same_c.begin(&omission, (Some(&held.coverage), None), EventId::new(90));
    assert_eq!(
        same_c.to_write().unwrap().points,
        points
            .into_iter()
            .filter(|point| point.at > EventId::new(50))
            .collect::<Vec<_>>()
    );

    // R past S, then a store that would not stop it: it stays stopped.
    covering.begin(
        &omission,
        (Some(&held.coverage), Some(EventId::new(51))),
        EventId::new(50),
    );
    assert!(covering.stopped());
    covering.read(&log.events, 4, log.last());
    covering.begin(&omission, (Some(&held.coverage), None), EventId::new(50));
    assert!(covering.stopped());
    assert_eq!(covering.to_write(), None);

    let mut other = Covering::default();
    other.begin(
        &Omission::of([DENIED], 4),
        (Some(&held.coverage), None),
        EventId::new(0),
    );
    assert!(other.stopped());
}

/// A write carrying a coverage of another C or kinds, or of a lower S than
/// the store's, conflicts (the store refuses it whole); the same again or a
/// further one of the same C does not.
#[test]
fn a_coverage_conflicts_with_another_c_or_a_further_s() {
    let log = mixed();
    let (held, _) = read_in_pages(&log, EventId::new(0), 10, 4);
    let mut lower = Coverage::start(&kinds(), EventId::new(0));
    lower.read(&log.events[..20], 4, EventId::new(20), &mut Vec::new());
    assert!(!held.conflicts(None));
    assert!(!held.conflicts(Some(&held)));
    assert!(!held.conflicts(Some(&lower)));
    assert!(lower.conflicts(Some(&held)));
    let other_c = Coverage {
        from: EventId::new(1),
        ..held.clone()
    };
    assert!(other_c.conflicts(Some(&lower)));
    let other_kinds = Coverage {
        kinds: vec![DENIED.to_owned()],
        ..held.clone()
    };
    assert!(other_kinds.conflicts(Some(&lower)));
    assert!(held.conflicts(Some(&Coverage::unreadable())));
}
