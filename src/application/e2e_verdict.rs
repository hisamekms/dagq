//! Whether the e2e gate passes (ADR-t963-1 decision 1 as ADR-t1165-1
//! amends it), for the automatic update's job, `install` and the runtime's
//! e2e of a run after its review (ADR-t1233-2) alike: an e2e
//! that passed passes; one whose failed tests all passed their rerun by
//! name passes with them named `flaky`; one whose tests that failed the
//! rerun too all have a mark of `.config/e2e-quarantine.toml` that holds
//! passes with them named `quarantined`. Anything else fails, with why.
//!
//! Whether a marked test failed its rerun in the gates right before this
//! one is read from their `update_e2e_passed` and `update_failed` (`stage:
//! e2e`) events: the `failed` of their `rerun`.

use serde_json::{Value, json};

use super::install::{E2eOutcome, E2eSettings};
pub use crate::domain::e2e_quarantine::failures_in_a_row;
use crate::domain::{
    RunEvent,
    e2e_quarantine::{self, QuarantineFile},
    host_metrics,
};

/// How many update events a gate reads back for the failures in a row.
pub const HISTORY: usize = 200;

/// The gate's judgement of an [`E2eOutcome`].
#[derive(Debug, Clone, PartialEq)]
pub struct E2eVerdict {
    pub passed: bool,
    /// The tests that failed and passed their rerun.
    pub flaky: Vec<String>,
    /// The tests that failed their rerun too under a mark that holds.
    pub quarantined: Vec<String>,
    /// Why it did not pass, for an error and a question.
    pub failure: Option<String>,
    /// What the gate's events and `install`'s report add: `flaky`,
    /// `quarantined`, and `rerun` and `quarantine` when there are any.
    pub fields: Value,
}

/// Judge `outcome` of the gate run with `settings` at the unix second
/// `now`; `history` is the queue's update events, newest first (the
/// gates before this one).
pub fn judge(
    outcome: &E2eOutcome,
    settings: &E2eSettings,
    history: &[RunEvent],
    now: i64,
) -> E2eVerdict {
    let today = host_metrics::local_day(now, settings.utc_offset_secs);
    let still_failing = outcome
        .rerun
        .as_ref()
        .map(|rerun| rerun.failed.clone())
        .unwrap_or_default();
    let judged = e2e_quarantine::judge(&outcome.quarantine, &still_failing, today, &|test| {
        failures_in_a_row(history, test)
    });
    let flaky: Vec<String> = outcome
        .rerun
        .iter()
        .flat_map(|rerun| rerun.tests.iter())
        .filter(|test| !still_failing.contains(test))
        .cloned()
        .collect();
    let mut fields = json!({"flaky": flaky, "quarantined": judged.quarantined});
    if let Some(rerun) = &outcome.rerun {
        let mut value = json!({
            "tests": rerun.tests,
            "failed": rerun.failed,
            "timed_out": rerun.timed_out,
            "secs": rerun.secs,
            "log": settings.rerun_log(),
            "cleanup": rerun.cleanup,
        });
        if let Some(error) = &rerun.error {
            value["error"] = json!(error);
        }
        fields["rerun"] = value;
    }
    if outcome.quarantine != QuarantineFile::Absent {
        let marks: Vec<Value> = match &outcome.quarantine {
            QuarantineFile::Marks(marks) => marks.iter().map(|mark| mark.to_json()).collect(),
            _ => Vec::new(),
        };
        let ignored: Vec<Value> = judged
            .ignored
            .iter()
            .map(
                |(name, why)| json!({"name": name, "reason": why.code(), "detail": why.sentence()}),
            )
            .collect();
        fields["quarantine"] = json!({
            "file": e2e_quarantine::FILE,
            "marks": marks,
            "ignored": ignored,
            "error": outcome.quarantine.error(),
        });
    }
    let passed = outcome.passed
        || outcome.rerun.as_ref().is_some_and(|rerun| {
            !rerun.timed_out && rerun.error.is_none() && judged.unmarked.is_empty()
        });
    let failure = (!passed).then(|| match &outcome.rerun {
        None => outcome.failure(settings),
        Some(rerun) => {
            let mut parts = vec![format!("the e2e failed: {}", rerun.tests.join(", "))];
            if rerun.timed_out {
                parts.push(format!(
                    "their rerun by name did not finish within {}s and was stopped",
                    settings.timeout.as_secs()
                ));
            } else if let Some(error) = &rerun.error {
                parts.push(format!("their rerun by name could not start: {error}"));
            } else {
                parts.push(format!(
                    "the rerun by name failed too: {}",
                    rerun.failed.join(", ")
                ));
                if !flaky.is_empty() {
                    parts.push(format!("passed on the rerun (flaky): {}", flaky.join(", ")));
                }
                if !judged.quarantined.is_empty() {
                    parts.push(format!(
                        "failed the rerun under a mark (quarantined): {}",
                        judged.quarantined.join(", ")
                    ));
                }
                let mut unheld = vec![format!(
                    "no mark of {} holds for {}",
                    e2e_quarantine::FILE,
                    judged.unmarked.join(", ")
                )];
                if let Some(error) = outcome.quarantine.error() {
                    unheld.push(error);
                }
                for (name, why) in &judged.ignored {
                    if judged.unmarked.contains(name) && why.code() != "over_limit" {
                        unheld.push(format!("{name}: {}", why.sentence()));
                    }
                }
                parts.push(unheld.join("; "));
            }
            format!(
                "{}; see {} and {}",
                parts.join("; "),
                settings.log.display(),
                settings.rerun_log().display()
            )
        }
    });
    E2eVerdict {
        passed,
        flaky,
        quarantined: judged.quarantined,
        failure,
        fields,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::install::E2eRerun;
    use crate::domain::{EventId, UPDATE_E2E_PASSED, UPDATE_FAILED};
    use std::{path::Path, time::Duration};

    fn settings() -> E2eSettings {
        E2eSettings {
            command: None,
            timeout: Duration::from_secs(1800),
            cmux: None,
            run_env_root: None,
            queue_dir: None,
            scratch: "/q/e2e".into(),
            log: "/q/logs/update-1-abc.e2e.log".into(),
            podman: None,
            utc_offset_secs: 9 * 3600,
            lock: None,
        }
    }

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

    fn rerun(tests: &[&str], failed: &[&str]) -> E2eRerun {
        E2eRerun {
            tests: tests.iter().map(|t| (*t).to_owned()).collect(),
            failed: failed.iter().map(|t| (*t).to_owned()).collect(),
            secs: 20,
            cleanup: json!({"removed": true}),
            ..Default::default()
        }
    }

    fn marked(name: &str, until: &str) -> QuarantineFile {
        QuarantineFile::of(&format!(
            "[[test]]\nname = \"{name}\"\nreason = \"flaky\"\ntask = 1120\nuntil = {until}\n"
        ))
    }

    /// 2026-10-01T00:30+09:00, still 2026-09-30 in UTC.
    const NOW: i64 = 1_790_782_200;

    #[test]
    fn the_rerun_log_is_the_log_with_rerun() {
        assert_eq!(
            settings().rerun_log(),
            Path::new("/q/logs/update-1-abc.e2e.rerun.log")
        );
        let mut other = settings();
        other.log = "/q/e2e".into();
        assert_eq!(other.rerun_log(), Path::new("/q/e2e.rerun.log"));
    }

    #[test]
    fn failed_tests_that_pass_their_rerun_are_flaky_and_pass() {
        let outcome = E2eOutcome {
            failed_tests: vec!["a".into(), "b".into()],
            rerun: Some(rerun(&["a", "b"], &[])),
            ..Default::default()
        };
        let verdict = judge(&outcome, &settings(), &[], NOW);
        assert!(verdict.passed, "{verdict:?}");
        assert_eq!(verdict.flaky, ["a", "b"]);
        assert_eq!(verdict.fields["flaky"], json!(["a", "b"]));
        assert_eq!(verdict.fields["quarantined"], json!([]));
        assert_eq!(
            verdict.fields["rerun"]["log"],
            "/q/logs/update-1-abc.e2e.rerun.log"
        );
        assert!(verdict.fields.get("quarantine").is_none());

        // A plain pass has no rerun.
        let verdict = judge(
            &E2eOutcome {
                passed: true,
                ..Default::default()
            },
            &settings(),
            &[],
            NOW,
        );
        assert!(verdict.passed && verdict.failure.is_none());
        assert!(verdict.fields.get("rerun").is_none());
    }

    #[test]
    fn a_test_failing_its_rerun_passes_only_under_a_mark_that_holds() {
        // The mark holds through its day in the host's time (UTC is a day
        // behind at NOW, so this checks the offset is used).
        let outcome = E2eOutcome {
            failed_tests: vec!["a".into(), "b".into()],
            rerun: Some(rerun(&["a", "b"], &["b"])),
            quarantine: marked("b", "2026-10-01"),
            ..Default::default()
        };
        let verdict = judge(&outcome, &settings(), &[], NOW);
        assert!(verdict.passed, "{verdict:?}");
        assert_eq!(
            (verdict.flaky.clone(), verdict.quarantined.clone()),
            (vec!["a".to_owned()], vec!["b".to_owned()])
        );
        let quarantine = &verdict.fields["quarantine"];
        assert_eq!(quarantine["file"], ".config/e2e-quarantine.toml");
        assert_eq!(quarantine["marks"][0]["task"], 1120);
        assert_eq!(quarantine["ignored"], json!([]));
        assert_eq!(quarantine["error"], Value::Null);

        // Expired the day before.
        let outcome = E2eOutcome {
            quarantine: marked("b", "2026-09-30"),
            ..outcome
        };
        let verdict = judge(&outcome, &settings(), &[], NOW);
        assert!(!verdict.passed);
        let failure = verdict.failure.unwrap();
        assert!(
            failure.contains("the e2e failed: a, b")
                && failure.contains("the rerun by name failed too: b")
                && failure.contains("passed on the rerun (flaky): a")
                && failure.contains("no mark of .config/e2e-quarantine.toml holds for b")
                && failure.contains("b: its mark expired after 2026-09-30")
                && failure.ends_with(
                    "see /q/logs/update-1-abc.e2e.log and /q/logs/update-1-abc.e2e.rerun.log"
                ),
            "{failure}"
        );
        assert_eq!(
            verdict.fields["quarantine"]["ignored"][0]["reason"],
            "expired"
        );

        // A test without a mark next to one under a mark fails.
        let outcome = E2eOutcome {
            failed_tests: vec!["a".into(), "b".into()],
            rerun: Some(rerun(&["a", "b"], &["a", "b"])),
            quarantine: marked("b", "2026-12-31"),
            ..Default::default()
        };
        let verdict = judge(&outcome, &settings(), &[], NOW);
        assert!(!verdict.passed);
        let failure = verdict.failure.unwrap();
        assert!(
            failure.contains("failed the rerun under a mark (quarantined): b")
                && failure.contains("holds for a"),
            "{failure}"
        );
        assert_eq!(verdict.quarantined, ["b"]);

        // An unreadable file names why.
        let outcome = E2eOutcome {
            quarantine: QuarantineFile::of("[x]"),
            rerun: Some(rerun(&["b"], &["b"])),
            ..Default::default()
        };
        let verdict = judge(&outcome, &settings(), &[], NOW);
        assert!(!verdict.passed);
        assert!(
            verdict
                .failure
                .unwrap()
                .contains(".config/e2e-quarantine.toml could not be read"),
        );
        assert_eq!(verdict.fields["quarantine"]["marks"], json!([]));
    }

    #[test]
    fn a_rerun_past_its_timeout_or_that_could_not_start_fails() {
        let outcome = E2eOutcome {
            failed_tests: vec!["b".into()],
            rerun: Some(E2eRerun {
                timed_out: true,
                ..rerun(&["b"], &["b"])
            }),
            quarantine: marked("b", "2026-12-31"),
            ..Default::default()
        };
        let verdict = judge(&outcome, &settings(), &[], NOW);
        assert!(!verdict.passed);
        assert!(
            verdict
                .failure
                .unwrap()
                .contains("rerun by name did not finish within 1800s")
        );
        let outcome = E2eOutcome {
            rerun: Some(E2eRerun {
                error: Some("no cargo".into()),
                ..rerun(&["b"], &["b"])
            }),
            ..outcome
        };
        let verdict = judge(&outcome, &settings(), &[], NOW);
        assert!(!verdict.passed);
        assert!(
            verdict
                .failure
                .unwrap()
                .contains("could not start: no cargo")
        );
        assert_eq!(verdict.fields["rerun"]["error"], "no cargo");
    }

    #[test]
    fn the_failures_in_a_row_are_read_from_the_gates_before() {
        let failed_rerun = |tests: &[&str]| {
            event(
                UPDATE_E2E_PASSED,
                json!({"rerun": {"failed": tests}, "quarantined": tests}),
            )
        };
        let history = [
            event("update_started", json!({})),
            failed_rerun(&["b"]),
            // Tells nothing of its tests: passed over.
            event(UPDATE_FAILED, json!({"stage": "e2e", "timed_out": true})),
            event(UPDATE_FAILED, json!({"stage": "build"})),
            event(
                UPDATE_FAILED,
                json!({"stage": "e2e", "rerun": {"failed": ["c"], "timed_out": true}}),
            ),
            event(
                UPDATE_FAILED,
                json!({"stage": "e2e", "rerun": {"failed": ["c"], "error": "no cargo"}}),
            ),
            event(
                UPDATE_FAILED,
                json!({"stage": "e2e", "rerun": {"failed": ["b", "c"]}}),
            ),
            failed_rerun(&["c"]),
            failed_rerun(&["b"]),
        ];
        assert_eq!(failures_in_a_row(&history, "b"), 2);
        assert_eq!(failures_in_a_row(&history, "c"), 0);
        let passed = [
            failed_rerun(&["b"]),
            event(UPDATE_E2E_PASSED, json!({})),
            failed_rerun(&["b"]),
        ];
        assert_eq!(failures_in_a_row(&passed, "b"), 1);

        // The third failure in a row fails the gate under a mark.
        let outcome = E2eOutcome {
            failed_tests: vec!["b".into()],
            rerun: Some(rerun(&["b"], &["b"])),
            quarantine: marked("b", "2026-12-31"),
            ..Default::default()
        };
        let verdict = judge(&outcome, &settings(), &history, NOW);
        assert!(!verdict.passed);
        assert!(
            verdict
                .failure
                .unwrap()
                .contains("b: it failed the rerun of 3 gates in a row"),
        );
        assert_eq!(
            verdict.fields["quarantine"]["ignored"][0]["reason"],
            "failed_in_a_row"
        );
        assert!(judge(&outcome, &settings(), &passed, NOW).passed);
    }
}
