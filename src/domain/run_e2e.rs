//! The runtime's e2e of a run after its review (ADR-t1233-2): whether a
//! run needs it, whether it ran for the commit that lands, how the marks of
//! `.config/e2e-quarantine.toml` apply to the run, and when an e2e that
//! could not run is a person's to look at. The supervisor runs it
//! (`application::supervise::e2e`); this decides from the run's events.
//!
//! A run needs the e2e when its latest `validation_finished` says so
//! (`e2e_requirement.required`: the task's `--evidence e2e`, or a diff
//! touching `[e2e] paths`, ADR-t963-1 decisions 2 and 3). It is done for
//! a commit once a `run_e2e_finished` of that commit passed (or found no
//! e2e to run); a `run_e2e_failed` parks the run, and the commit its
//! resumed session makes needs its own.

use serde_json::{Value, json};

use super::{
    RunEvent,
    e2e_quarantine::{Mark, QuarantineFile},
    event_kind::{RUN_E2E_FAILED, RUN_E2E_FINISHED, RUN_E2E_STARTED, VALIDATION_FINISHED},
};

/// `run_e2e_finished`'s `outcome` of an e2e that passed (flaky tests and
/// tests under a mark included).
pub const PASSED: &str = "passed";
/// `run_e2e_finished`'s `outcome` of an e2e that could not run (`[run.env]`
/// could not be read, it could not start, past its timeout): not the
/// change's fault, so the run waits and it is tried again. A cmux that does
/// not answer is not one: the e2e that need it are left out (ADR-t2105-1).
pub const UNAVAILABLE: &str = "unavailable";
/// `run_e2e_finished`'s `outcome` when the repository has no e2e command
/// the runtime knows (not dagq's source, and no command given): the run
/// lands without it.
pub const NOT_CONFIGURED: &str = "not_configured";

/// How long a run waits before its e2e that could not run is tried again.
pub const RETRY_SECS: u64 = 300;

/// After this many e2e in a row that could not run, the last one's
/// `run_e2e_finished` carries `attention: true`: a person looks at the
/// host (`check the e2e host`). The runtime keeps trying.
pub const UNAVAILABLE_ATTENTION: usize = 3;

/// Whether the run's latest validation found that it needs the e2e.
pub fn required(events: &[RunEvent]) -> bool {
    events
        .iter()
        .rev()
        .find(|e| e.kind == VALIDATION_FINISHED)
        .is_some_and(|e| e.payload["e2e_requirement"]["required"] == true)
}

/// The e2e due for the run whose head is `commit`: needed, and not passed
/// (nor found unconfigured) for that commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Due {
    /// The attempt this e2e is: one more than the run's `run_e2e_started`.
    pub attempt: usize,
    /// The latest validation's `e2e_requirement`.
    pub requirement: Value,
}

/// The e2e due for the run whose head is `commit` (see [`Due`]); `None`
/// when it needs none or it ran for that commit.
pub fn due(events: &[RunEvent], commit: &str) -> Option<Due> {
    let validation = events
        .iter()
        .rev()
        .find(|e| e.kind == VALIDATION_FINISHED)?;
    if validation.payload["e2e_requirement"]["required"] != true {
        return None;
    }
    let done = events.iter().any(|e| {
        e.kind == RUN_E2E_FINISHED
            && e.payload["commit"] == commit
            && matches!(e.payload["outcome"].as_str(), Some(PASSED | NOT_CONFIGURED))
    });
    if done {
        return None;
    }
    Some(Due {
        attempt: events.iter().filter(|e| e.kind == RUN_E2E_STARTED).count() + 1,
        requirement: validation.payload["e2e_requirement"].clone(),
    })
}

/// How many of the run's latest e2e could not run, in a row.
pub fn unavailable_in_a_row(events: &[RunEvent]) -> usize {
    events
        .iter()
        .rev()
        .filter(|e| e.kind == RUN_E2E_FINISHED || e.kind == RUN_E2E_FAILED)
        .take_while(|e| e.kind == RUN_E2E_FINISHED && e.payload["outcome"] == UNAVAILABLE)
        .count()
}

/// The `run_e2e_finished` that stands as the run's attention: its latest
/// e2e could not run and carried `attention: true`.
pub fn standing_attention(events: &[RunEvent]) -> Option<&RunEvent> {
    events
        .iter()
        .rev()
        .find(|e| e.kind == RUN_E2E_FINISHED || e.kind == RUN_E2E_FAILED)
        .filter(|e| e.kind == RUN_E2E_FINISHED && e.payload["attention"] == true)
}

/// The file under `tests/` that holds the e2e test `name`: a test of a
/// module (`headless::…`) lives in `tests/e2e/<module>.rs`, any other in
/// `tests/e2e.rs`.
pub fn test_file(name: &str) -> String {
    match name.split_once("::") {
        Some((module, _)) => format!("tests/e2e/{module}.rs"),
        None => "tests/e2e.rs".to_owned(),
    }
}

/// The marks of `file` that may pass a test of the run of `task` whose
/// diff changed `changes` (ADR-t1233-2 decision 5, as ADR-t1165-1 decision
/// 6): not the mark of a test whose file the run changes (a coarser rule
/// than the test's function, which the runtime does not read), nor one
/// the run's task fixes. The marks left out come back with why, for the
/// event.
pub fn marks_for_run(
    file: QuarantineFile,
    task: i64,
    changes: &[String],
) -> (QuarantineFile, Vec<Value>) {
    let QuarantineFile::Marks(marks) = file else {
        return (file, Vec::new());
    };
    let mut left_out = Vec::new();
    let kept: Vec<Mark> = marks
        .into_iter()
        .filter(|mark| {
            let why = if mark.task == task {
                Some("fixed_by_this_task")
            } else if changes.contains(&test_file(&mark.name)) {
                Some("test_changed_by_the_run")
            } else {
                None
            };
            if let Some(why) = why {
                left_out.push(json!({"name": mark.name, "reason": why}));
            }
            why.is_none()
        })
        .collect();
    (QuarantineFile::Marks(kept), left_out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;

    fn event(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    fn validated(required: bool) -> RunEvent {
        event(
            VALIDATION_FINISHED,
            json!({"e2e_requirement": {"required": required, "source": "paths"}}),
        )
    }

    fn finished(commit: &str, outcome: &str) -> RunEvent {
        event(
            RUN_E2E_FINISHED,
            json!({"commit": commit, "outcome": outcome}),
        )
    }

    #[test]
    fn the_e2e_is_due_for_each_commit_until_it_passes() {
        assert_eq!(due(&[], "a"), None);
        assert_eq!(due(&[validated(false)], "a"), None);
        let mut events = vec![validated(true)];
        assert!(required(&events));
        assert_eq!(due(&events, "a").unwrap().attempt, 1);
        assert_eq!(
            due(&events, "a").unwrap().requirement,
            json!({"required": true, "source": "paths"})
        );
        events.push(event(RUN_E2E_STARTED, json!({"commit": "a"})));
        events.push(finished("a", UNAVAILABLE));
        assert_eq!(due(&events, "a").unwrap().attempt, 2);
        events.push(event(RUN_E2E_STARTED, json!({"commit": "a"})));
        events.push(finished("a", PASSED));
        assert_eq!(due(&events, "a"), None);
        // The commit a resume made needs its own.
        assert_eq!(due(&events, "b").unwrap().attempt, 3);
        events.push(finished("b", NOT_CONFIGURED));
        assert_eq!(due(&events, "b"), None);
        // A later validation that needs none needs none.
        events.push(validated(false));
        assert_eq!(due(&events, "c"), None);
        assert!(!required(&events));
    }

    #[test]
    fn an_e2e_that_could_not_run_in_a_row_stands_as_an_attention() {
        let attention = event(
            RUN_E2E_FINISHED,
            json!({"commit": "a", "outcome": UNAVAILABLE, "attention": true}),
        );
        let mut events = vec![
            finished("a", UNAVAILABLE),
            event(RUN_E2E_FAILED, json!({})),
            finished("a", UNAVAILABLE),
            finished("a", UNAVAILABLE),
        ];
        assert_eq!(unavailable_in_a_row(&events), 2);
        assert!(standing_attention(&events).is_none());
        events.push(attention);
        assert_eq!(unavailable_in_a_row(&events), 3);
        assert!(standing_attention(&events).is_some());
        // Another start does not end it; the next outcome does.
        events.push(event(RUN_E2E_STARTED, json!({})));
        assert!(standing_attention(&events).is_some());
        events.push(finished("a", PASSED));
        assert!(standing_attention(&events).is_none());
        assert_eq!(unavailable_in_a_row(&events), 0);
    }

    #[test]
    fn a_mark_does_not_pass_a_test_the_run_changes_or_fixes() {
        assert_eq!(test_file("headless::lands"), "tests/e2e/headless.rs");
        assert_eq!(test_file("up_lands"), "tests/e2e.rs");
        let file = QuarantineFile::of(
            "[[test]]\nname = \"headless::lands\"\nreason = \"r\"\ntask = 4\nuntil = 2999-01-01\n\n\
[[test]]\nname = \"up_lands\"\nreason = \"r\"\ntask = 5\nuntil = 2999-01-01\n\n\
[[test]]\nname = \"down_lands\"\nreason = \"r\"\ntask = 6\nuntil = 2999-01-01\n",
        );
        let (kept, left_out) = marks_for_run(file.clone(), 6, &["tests/e2e/headless.rs".into()]);
        let QuarantineFile::Marks(kept) = kept else {
            panic!("{kept:?}");
        };
        assert_eq!(
            kept.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["up_lands"]
        );
        assert_eq!(
            left_out,
            [
                json!({"name": "headless::lands", "reason": "test_changed_by_the_run"}),
                json!({"name": "down_lands", "reason": "fixed_by_this_task"}),
            ]
        );
        let (same, none) = marks_for_run(QuarantineFile::Absent, 6, &[]);
        assert_eq!((same, none), (QuarantineFile::Absent, Vec::new()));
        let (_, none) = marks_for_run(file, 9, &["src/lib.rs".into()]);
        assert!(none.is_empty());
    }
}
