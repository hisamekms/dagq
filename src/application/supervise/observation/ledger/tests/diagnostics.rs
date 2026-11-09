use super::*;

#[test]
fn first_pass_progress_obeys_the_injected_elapsed_clock() {
    let minute = Duration::from_secs(60);
    assert!(!progress_due(
        Duration::ZERO,
        minute - Duration::from_nanos(1)
    ));
    assert!(progress_due(Duration::ZERO, minute));
    assert!(!progress_due(
        minute,
        minute + minute - Duration::from_nanos(1)
    ));
    assert!(progress_due(minute, minute + minute));
    assert!(!progress_due(minute, Duration::ZERO));
}

#[test]
fn pass_reports_pages_and_acknowledged_marks_without_changing_rows() {
    let (log, _) = sessions_across();
    let memory = Memory::of(&log);
    let mut builder = start_builder(&memory, ms(1000)).unwrap();
    let built = build_pass_reading(
        &memory,
        &mut builder,
        (ms(1000), BUILT),
        Retain::default(),
        None,
        2,
    );
    assert!(built.failure.is_none(), "{built:?}");
    assert_eq!(built.work.events, log.events.len());
    assert_eq!(built.work.pages, log.events.len().div_ceil(2));
    assert_eq!(built.work.latest, log.last().as_i64());
    assert_eq!(built.work.marks_written, builder.observe_written_marks());
    assert_eq!(built.marks.rows.map(|r| r.through), builder.rows_written());
    let rows = memory.rows.borrow().clone();
    let again = build_pass(
        &memory,
        &mut builder,
        (ms(1000), BUILT),
        Retain::default(),
        None,
    );
    assert!(again.failure.is_none());
    assert_eq!(*memory.rows.borrow(), rows);
    assert!(again.work.marks_written.is_none());
    assert_eq!(again.work.no_marks, "marks_unchanged");
}

use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

#[derive(Clone, Default)]
struct LogLines(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl<S: Subscriber> Layer<S> for LogLines {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        struct Fields(String);
        impl Visit for Fields {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write;
                write!(&mut self.0, "{}={value:?} ", field.name()).unwrap();
            }
        }
        let mut fields = Fields(format!("{} ", event.metadata().level()));
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

#[test]
fn startup_end_and_progress_are_visible_at_info_with_bounded_details() {
    let output = LogLines::default();
    let subscriber = tracing_subscriber::registry().with(
        output
            .clone()
            .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
    );
    tracing::subscriber::with_default(subscriber, || {
        let memory = Memory::default();
        let mut builder = start_builder(&memory, 0).unwrap();
        let first = build_pass(&memory, &mut builder, (0, BUILT), Retain::default(), None);
        assert!(first.work.marks_written.is_some());
        let second = build_pass(&memory, &mut builder, (0, BUILT), Retain::default(), None);
        assert_eq!(second.work.no_marks, "marks_unchanged");
        let mut last = Duration::ZERO;
        log_progress(&mut last, Duration::from_secs(59), 2, 1);
        assert_eq!(last, Duration::ZERO);
        log_progress(&mut last, Duration::from_secs(60), 4, 2);
        log_progress(&mut last, Duration::from_secs(119), 6, 3);
        assert_eq!(last, Duration::from_secs(60));
    });
    let text = output.0.lock().unwrap().join("\n");
    assert!(text.contains("INFO"), "{text}");
    assert_eq!(text.matches("the ledger builder started").count(), 1);
    let lines = output.0.lock().unwrap();
    let started = lines
        .iter()
        .find(|line| line.contains("the ledger builder started"))
        .unwrap();
    assert!(started.contains("reason=\"no_marks\" "), "{started}");
    assert_eq!(text.matches("the ledger pass finished").count(), 2);
    assert!(text.contains("marks_unchanged"));
    assert_eq!(text.matches("the ledger first pass is reading").count(), 1);
    assert!(text.contains("events=4 pages=2"), "{text}");
    assert!(!text.contains("records:"));
}

/// Reads advance by a fixed ten seconds, independent of wall time. The
/// initial Instant is only an origin for these deterministic offsets.
struct Stepped(std::sync::Mutex<Instant>);

impl Stepped {
    fn new() -> Self {
        Self(std::sync::Mutex::new(Instant::now()))
    }
}

impl crate::application::Clock for Stepped {
    fn system_time(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH
    }

    fn monotonic(&self) -> Instant {
        let mut next = self.0.lock().unwrap();
        let at = *next;
        *next += Duration::from_secs(10);
        at
    }
}

/// Exercise the page loop itself with an injected clock: the first page
/// is before the interval, the second is due, and the third is not due
/// again. The next pass still takes longer than the interval but has
/// already swept, so none of its pages logs first-pass progress.
#[test]
fn timed_pass_logs_only_due_first_pass_pages_and_measures_stages() {
    let output = LogLines::default();
    let subscriber = tracing_subscriber::registry().with(
        output
            .clone()
            .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
    );
    tracing::subscriber::with_default(subscriber, || {
        let clock = Stepped::new();
        let mut log = Log::default();
        log.add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s1"}),
            0,
        );
        for task in 1..=201 {
            log.add(Some(task), None, "task_created", json!({}), task);
        }
        log.add(None, None, "claim_held", json!({"supervisor": "s1"}), 202);
        let memory = Memory::of(&log);
        let mut builder = Builder::default();
        let first = build_pass_timed(
            &memory,
            &mut builder,
            (ms(1000), BUILT),
            Retain::default(),
            None,
            (100, SESSION_BYTES_PER_WRITE),
            Timing(Some(&clock)),
        );
        assert!(first.failure.is_none(), "{first:?}");
        assert_eq!((first.work.events, first.work.pages), (203, 3));
        // Each read and sweep takes one clock tick. The page-loop clock
        // is 50/100/150 seconds after its start, so only page 2 is due.
        assert_eq!(first.work.event_read, Duration::from_secs(30));
        assert_eq!(first.work.page_sweep, Duration::from_secs(30));
        assert_eq!(first.work.final_sweep, Duration::from_secs(10));
        assert!(
            first.work.final_reads > 0,
            "the queue rows remain past the task write bound"
        );
        // One more tick: the write bound looks at the clock once, and the
        // pass, past [`PASS_TIME`], writes only the marks taken first.
        assert_eq!(first.work.elapsed, Duration::from_secs(200));
        let progress = output
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains("the ledger first pass is reading"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(progress.len(), 1, "{progress:?}");
        assert!(progress[0].contains("pages=2 "), "{progress:?}");
        assert!(
            progress[0].contains(&format!("events={} ", log.events[199].id.as_i64())),
            "{progress:?}"
        );
        assert!(!builder.sweeps_pages());

        log.add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s2"}),
            203,
        );
        for task in 202..=402 {
            log.add(Some(task), None, "task_created", json!({}), task + 2);
        }
        log.add(None, None, "claim_held", json!({"supervisor": "s2"}), 500);
        memory.events.replace(log.events.clone());
        output.0.lock().unwrap().clear();
        let second = build_pass_timed(
            &memory,
            &mut builder,
            (ms(1000), BUILT),
            Retain::default(),
            None,
            (100, SESSION_BYTES_PER_WRITE),
            Timing(Some(&clock)),
        );
        assert!(second.failure.is_none(), "{second:?}");
        assert_eq!((second.work.events, second.work.pages), (203, 3));
        assert_eq!(second.work.event_read, Duration::from_secs(30));
        assert_eq!(second.work.page_sweep, Duration::ZERO);
        assert_eq!(second.work.final_sweep, Duration::from_secs(10));
        // One more tick: the marking before the writes looks at the clock
        // once and, past `MARK_TIME`, marks the rest as if they read the
        // events read since, not built.
        assert_eq!(second.work.elapsed, Duration::from_secs(120));
        assert!(
            !output
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("the ledger first pass is reading"))
        );
    });
}
