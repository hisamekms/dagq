//! The recheck of the runs that wait to land after each landing
//! (ADR-0068): what it found for one run, how that is written into the
//! run's events, its asks and its `last_error`, and how the events are
//! read back, for a run found no longer landing and, since ADR-t1311-1,
//! for one found still landing. The supervisor runs the checks; this
//! module is the record.

use serde_json::{Value, json};

use super::{CommitSha, ReasonCode, RunEvent, RunId, TaskId, scope::glob_matches};

/// A run the recheck found no longer landing on main (ADR-0068 decision
/// 3). Its `action` says what followed: [`RESUMED`] (it was parked for a
/// resume in the same transaction) or [`HELD`] (a session or the landing
/// holds it; it is parked when it would land).
pub const LANDING_RECHECK_FAILED: &str = super::event_kind::LANDING_RECHECK_FAILED;

/// A run the recheck found still landing on main (ADR-t1311-1): the main
/// and head it checked, the command that passed on them (null when only
/// `git merge-tree` was looked at) and the landing that moved main.
pub const LANDING_RECHECK_CLEAN: &str =
    crate::domain::event_kind::EventKind::LandingRecheckClean.as_str();

/// One recheck ended, recorded on the run whose landing moved main (or,
/// when no dagq landing did, on the first run it checked, ADR-t1310-1):
/// the main it checked against and what it found.
pub const LANDING_RECHECK_FINISHED: &str =
    crate::domain::event_kind::EventKind::LandingRecheckFinished.as_str();

/// `action` of a failure that parked the run for a resume.
pub const RESUMED: &str = "resumed";

/// `action` of a failure recorded on a run this supervisor holds in a slot
/// (waiting for the landing slot, or for its session's `/exit`).
pub const HELD: &str = "held";

/// `command_skipped` of a clean finding whose run's diff touches none of
/// `[recheck] paths` (ADR-t2032-1): the recheck had a command and did not
/// run it. A clean finding without a command at all has no
/// `command_skipped`.
pub const SKIPPED_BY_PATHS: &str = "paths";

/// `[recheck]` of `dagq.toml` (ADR-0068 decision 2, ADR-t2032-1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecheckConfig {
    /// The command run on main's tree with a waiting run merged in; none
    /// checks the merge only.
    pub command: Option<String>,
    /// The globs a run's diff against main must touch for the command to
    /// run on it; empty (no `paths`) runs it on every run.
    pub paths: Vec<String>,
}

/// Whether the recheck runs its command on a run whose merged tree differs
/// from main in `changed` (ADR-t2032-1): always without `paths`, otherwise
/// only when a changed path matches one of them.
pub fn runs_command(paths: &[String], changed: &[String]) -> bool {
    paths.is_empty()
        || changed
            .iter()
            .any(|path| paths.iter().any(|glob| glob_matches(glob, path)))
}

/// The landing whose commit the recheck checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    pub run_id: RunId,
    pub task_id: TaskId,
}

/// Why a waiting run no longer lands on main.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecheckFailure {
    /// `git merge-tree` finds these paths conflicting.
    Conflict { paths: Vec<String> },
    /// The tree merged cleanly, but the recheck's command failed on it.
    CheckFailed {
        command: String,
        exit_code: i32,
        log_path: String,
        output_tail: String,
    },
}

impl RecheckFailure {
    /// A conflict is the landing's `rebase_conflict`, a failed command its
    /// `verification_failed` (ADR-0034's codes, ADR-0068 decision 3).
    pub const fn code(&self) -> ReasonCode {
        match self {
            Self::Conflict { .. } => ReasonCode::RebaseConflict,
            Self::CheckFailed { .. } => ReasonCode::VerificationFailed,
        }
    }

    /// The run's `last_error` once parked, and the reason of its resume.
    /// `landed` is `None` when main moved without a dagq landing.
    pub fn reason(&self, landed: Option<&Landed>, main: &CommitSha) -> String {
        let after = after(landed);
        match self {
            Self::Conflict { paths } => format!(
                "{after} that main {main} conflicts with the run in {}",
                paths.join(", ")
            ),
            Self::CheckFailed {
                command,
                exit_code,
                log_path,
                ..
            } => format!(
                "{after} that {command:?} exits with {exit_code} on main {main} with the run merged in (git merges it without a conflict); see {log_path}"
            ),
        }
    }

    /// The payload of [`LANDING_RECHECK_FAILED`], without its `action`.
    /// `landed_run_id` and `landed_task_id` are null when main moved
    /// without a dagq landing.
    pub fn payload(&self, landed: Option<&Landed>, main: &CommitSha, head: &CommitSha) -> Value {
        let mut payload = json!({
            "code": self.code(),
            "main": main,
            "head": head,
            "landed_run_id": landed.map(|l| &l.run_id),
            "landed_task_id": landed.map(|l| l.task_id),
        });
        match self {
            Self::Conflict { paths } => payload["conflicts"] = json!(paths),
            Self::CheckFailed {
                command,
                exit_code,
                log_path,
                output_tail,
            } => {
                payload["command"] = json!(command);
                payload["exit_code"] = json!(exit_code);
                payload["log_path"] = json!(log_path);
                payload["output_tail"] = json!(output_tail);
            }
        }
        payload
    }
}

/// What a recheck's finding starts with: the landing that moved main.
fn after(landed: Option<&Landed>) -> String {
    match landed {
        Some(landed) => format!(
            "after task {} (run {}) landed, the landing recheck found",
            landed.task_id, landed.run_id
        ),
        None => "after main moved without a dagq landing, the landing recheck found".to_owned(),
    }
}

/// The payload of [`LANDING_RECHECK_CLEAN`] (ADR-t1311-1). `command` is
/// null when only `git merge-tree` was looked at, and `landed_run_id` and
/// `landed_task_id` when main moved without a dagq landing. A run whose
/// diff touched none of `[recheck] paths` (`skipped_by_paths`) has a null
/// `command` and `command_skipped` [`SKIPPED_BY_PATHS`] (ADR-t2032-1), so
/// it is told apart from a recheck that had no command to run.
pub fn clean_payload(
    landed: Option<&Landed>,
    main: &CommitSha,
    head: &CommitSha,
    command: Option<&str>,
    skipped_by_paths: bool,
) -> Value {
    let mut payload = json!({
        "main": main,
        "head": head,
        "command": if skipped_by_paths { None } else { command },
        "landed_run_id": landed.map(|l| &l.run_id),
        "landed_task_id": landed.map(|l| l.task_id),
    });
    if skipped_by_paths {
        payload["command_skipped"] = json!(SKIPPED_BY_PATHS);
    }
    payload
}

/// How the recheck's paragraph in a run's open asks starts: the asks hold
/// one such paragraph, the latest finding's (ADR-t1311-1).
pub const ASK_NOTE_PREFIX: &str = "Landing recheck: ";

/// The paragraph a failure puts in the run's open asks (ADR-0068 decision
/// 4): what it found, and that the run is resumed without waiting for the
/// answer, which still applies once the run waits again.
pub fn ask_note(reason: &str, resumed: bool) -> String {
    let next = if resumed {
        "The supervisor resumes the run to bring it onto main without waiting for this answer; once it waits again, the answer applies to the rebased run."
    } else {
        "The supervisor parks the run for a resume instead of landing it once its session has exited."
    };
    format!("{ASK_NOTE_PREFIX}{reason}. {next}")
}

/// The paragraph a clean finding puts in the run's open asks
/// (ADR-t1311-1): the main's short commit, that the run still lands there
/// cleanly, and whether a command was run on it.
pub fn clean_ask_note(landed: Option<&Landed>, main: &CommitSha, command: Option<&str>) -> String {
    let short = &main.as_str()[..main.as_str().len().min(12)];
    let checked = match command {
        Some(command) => format!(
            "git merges it without a conflict and {command:?} passes on main with the run merged in"
        ),
        None => "git merges it without a conflict (no command was run)".to_owned(),
    };
    format!(
        "{ASK_NOTE_PREFIX}{} that the run still lands cleanly on main {short}: {checked}.",
        after(landed)
    )
}

/// `question` with `note` as its recheck paragraph: any paragraph that
/// starts with [`ASK_NOTE_PREFIX`] is dropped and `note` ends the question,
/// so the asks show only the latest finding (ADR-t1311-1).
pub fn noted_question(question: &str, note: &str) -> String {
    let kept: Vec<&str> = question
        .split("\n\n")
        .filter(|paragraph| !paragraph.trim_start().starts_with(ASK_NOTE_PREFIX))
        .collect();
    format!("{}\n\n{note}", kept.join("\n\n").trim_end())
}

/// Whether the event parked its run for a session.
pub fn parks(event: &RunEvent) -> bool {
    event.kind == LANDING_RECHECK_FAILED && event.payload["action"] == RESUMED
}

/// The failure a recheck recorded as [`HELD`] on the run against `main`
/// with `head`, when it is the run's latest recheck failure: the run would
/// land a head known not to land there.
pub fn held_against<'a>(events: &'a [RunEvent], main: &str, head: &str) -> Option<&'a RunEvent> {
    events
        .iter()
        .rev()
        .find(|e| e.kind == LANDING_RECHECK_FAILED)
        .filter(|e| {
            e.payload["action"] == HELD && e.payload["main"] == main && e.payload["head"] == head
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;

    fn landed() -> Landed {
        Landed {
            run_id: RunId::new("landed-run").unwrap(),
            task_id: TaskId::new(7),
        }
    }

    fn event(payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: LANDING_RECHECK_FAILED.to_owned(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    #[test]
    fn a_conflict_names_the_landing_and_the_paths() {
        let main = CommitSha::parse("a".repeat(40), "commit").unwrap();
        let head = CommitSha::parse("b".repeat(40), "commit").unwrap();
        let failure = RecheckFailure::Conflict {
            paths: vec!["x.rs".into(), "y.rs".into()],
        };
        assert_eq!(failure.code(), ReasonCode::RebaseConflict);
        let reason = failure.reason(Some(&landed()), &main);
        assert_eq!(
            reason,
            format!(
                "after task 7 (run landed-run) landed, the landing recheck found that main {main} conflicts with the run in x.rs, y.rs"
            )
        );
        assert_eq!(
            failure.payload(Some(&landed()), &main, &head),
            json!({
                "code": "rebase_conflict",
                "main": main,
                "head": head,
                "landed_run_id": "landed-run",
                "landed_task_id": 7,
                "conflicts": ["x.rs", "y.rs"],
            })
        );
        assert!(ask_note(&reason, true).contains("without waiting for this answer"));
        assert!(ask_note(&reason, false).contains("once its session has exited"));
    }

    #[test]
    fn a_main_moved_without_a_landing_names_no_run() {
        let main = CommitSha::parse("a".repeat(40), "commit").unwrap();
        let head = CommitSha::parse("b".repeat(40), "commit").unwrap();
        let failure = RecheckFailure::Conflict {
            paths: vec!["x.rs".into()],
        };
        assert_eq!(
            failure.reason(None, &main),
            format!(
                "after main moved without a dagq landing, the landing recheck found that main {main} conflicts with the run in x.rs"
            )
        );
        let payload = failure.payload(None, &main, &head);
        assert_eq!(payload["landed_run_id"], Value::Null);
        assert_eq!(payload["landed_task_id"], Value::Null);
    }

    #[test]
    fn a_failed_check_names_the_command_and_its_log() {
        let main = CommitSha::parse("a".repeat(40), "commit").unwrap();
        let head = CommitSha::parse("b".repeat(40), "commit").unwrap();
        let failure = RecheckFailure::CheckFailed {
            command: "cargo check".into(),
            exit_code: 101,
            log_path: "/r/recheck.log".into(),
            output_tail: "error[E0063]".into(),
        };
        assert_eq!(failure.code(), ReasonCode::VerificationFailed);
        let reason = failure.reason(Some(&landed()), &main);
        assert!(
            reason.contains("\"cargo check\" exits with 101"),
            "{reason}"
        );
        assert!(reason.ends_with("see /r/recheck.log"), "{reason}");
        let payload = failure.payload(Some(&landed()), &main, &head);
        assert_eq!(payload["code"], "verification_failed");
        assert_eq!(payload["exit_code"], 101);
        assert_eq!(payload["output_tail"], "error[E0063]");
    }

    #[test]
    fn a_clean_finding_names_the_main_and_whether_a_command_ran() {
        let main = CommitSha::parse("a".repeat(40), "commit").unwrap();
        let head = CommitSha::parse("b".repeat(40), "commit").unwrap();
        assert_eq!(
            clean_ask_note(Some(&landed()), &main, Some("cargo check")),
            "Landing recheck: after task 7 (run landed-run) landed, the landing recheck found that the run still lands cleanly on main aaaaaaaaaaaa: git merges it without a conflict and \"cargo check\" passes on main with the run merged in."
        );
        assert_eq!(
            clean_ask_note(None, &main, None),
            "Landing recheck: after main moved without a dagq landing, the landing recheck found that the run still lands cleanly on main aaaaaaaaaaaa: git merges it without a conflict (no command was run)."
        );
        assert_eq!(
            clean_payload(Some(&landed()), &main, &head, None, false),
            json!({
                "main": main,
                "head": head,
                "command": null,
                "landed_run_id": "landed-run",
                "landed_task_id": 7,
            })
        );
        assert_eq!(
            clean_payload(None, &main, &head, Some("cargo check"), false)["command"],
            "cargo check"
        );
        assert_eq!(
            clean_payload(None, &main, &head, Some("cargo check"), true),
            json!({
                "main": main,
                "head": head,
                "command": null,
                "command_skipped": "paths",
                "landed_run_id": null,
                "landed_task_id": null,
            })
        );
    }

    /// The command runs on every run without `[recheck] paths`, and with
    /// them only on a run whose diff touches one (ADR-t2032-1).
    #[test]
    fn the_command_runs_only_when_the_diff_touches_a_path() {
        let paths = ["**/*.rs".to_owned(), "Cargo.lock".to_owned()];
        let changed = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        assert!(runs_command(&[], &changed(&["docs/a.md"])));
        assert!(runs_command(&[], &[]));
        assert!(runs_command(&paths, &changed(&["docs/a.md", "src/lib.rs"])));
        assert!(runs_command(&paths, &changed(&["Cargo.lock"])));
        assert!(!runs_command(&paths, &changed(&["docs/a.md", "dagq.toml"])));
        assert!(!runs_command(&paths, &changed(&["sub/Cargo.lock"])));
        assert!(!runs_command(&paths, &[]));
    }

    #[test]
    fn the_latest_note_replaces_every_earlier_recheck_paragraph() {
        let question = "Land it?\n\nDetails.";
        let once = noted_question(question, "Landing recheck: first.");
        assert_eq!(once, "Land it?\n\nDetails.\n\nLanding recheck: first.");
        let twice = noted_question(&once, "Landing recheck: second.");
        assert_eq!(twice, "Land it?\n\nDetails.\n\nLanding recheck: second.");
        // Paragraphs an earlier binary stacked go too.
        let stacked = format!("{once}\n\nLanding recheck: older.");
        assert_eq!(noted_question(&stacked, "Landing recheck: second."), twice);
        assert_eq!(noted_question(&twice, "Landing recheck: second."), twice);
        // A question that ends in a newline does not stack them either.
        let trailing = noted_question("Land it?\n", "Landing recheck: first.");
        assert_eq!(trailing, "Land it?\n\nLanding recheck: first.");
        let stacked = "Land it?\n\n\nLanding recheck: older.";
        assert_eq!(noted_question(stacked, "Landing recheck: first."), trailing);
    }

    #[test]
    fn only_the_latest_held_failure_against_the_same_main_and_head_holds() {
        let held =
            |main: &str, head: &str| event(json!({"action": HELD, "main": main, "head": head}));
        let events = [held("m", "h")];
        assert!(held_against(&events, "m", "h").is_some());
        assert!(held_against(&events, "m2", "h").is_none());
        assert!(held_against(&events, "m", "h2").is_none());
        let resumed = event(json!({"action": RESUMED, "main": "m", "head": "h"}));
        assert!(parks(&resumed));
        assert!(!parks(&events[0]));
        assert!(held_against(&[held("m", "h"), resumed], "m", "h").is_none());
    }
}
