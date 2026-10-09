//! A session idle with a receipt for an older commit (task 357): the
//! session committed again (or rebased) after writing its receipt and went
//! idle without rewriting it. When the worktree HEAD is a clean new commit
//! on top of the run's base, the supervisor types one fixed request to
//! rewrite the receipt ([`stale_receipt_nudge`]) before it goes on; if the
//! session answers without rewriting it, the run goes on as before.
//!
//! Each phase (the first session, and each resume attempt) gets at most one
//! request, recorded as `stale_receipt_nudged` before it is typed, and how it
//! ended as `stale_receipt_resolved` (`rewritten`, `unchanged`,
//! `run_ended`, or `unsent` when it could not be typed).

use super::*;
use crate::domain::EventKind;

/// The phase of the worker's first session.
pub(super) const SESSION_PHASE: &str = "session";

/// The phase of a resumed session.
pub(super) const RESUME_PHASE: &str = "resume";

/// A receipt naming another commit than the clean worktree HEAD.
#[derive(Debug, Clone)]
pub(super) struct StaleReceipt {
    pub(super) receipt_commit: String,
    pub(super) head: CommitSha,
}

/// The one request of a phase, once typed.
#[derive(Debug, Clone, Copy)]
pub(super) struct StaleNudge {
    /// When it was typed, on the files' wall clock: only an idle marker
    /// newer than this answers it. A resume that comes back from a wait
    /// moves it to the return (ADR-0071 decision 15), so its timeout and
    /// the idle that answers it count from there.
    pub(super) at: SystemTime,
    /// When it was typed, never moved: the receipt counts as `rewritten`
    /// when it changed after this, even while the run waited.
    pub(super) typed_at: SystemTime,
    /// Its `stale_receipt_resolved` is recorded.
    pub(super) settled: bool,
}

/// The receipt of `run` when it names a commit other than the worktree HEAD
/// and a rewrite would name a valid one ([`stale_receipt_of`]). Anything
/// that cannot be read is `None`: the run goes on as before.
pub(super) fn stale_receipt(sv: &Supervisor<'_>, run: &TaskRun) -> Option<StaleReceipt> {
    let receipt = sv
        .files
        .read_to_string(Path::new(run.receipt_path()?))
        .ok()
        .and_then(|text| Receipt::parse(&text).ok())?;
    let worktree = Path::new(run.worktree_path()?);
    let base = run.base_commit();
    stale_receipt_of(
        &receipt,
        run,
        || sv.repository.head(worktree).ok(),
        || {
            sv.repository
                .status(worktree)
                .is_ok_and(|status| status.trim().is_empty())
        },
        |head| {
            sv.repository
                .is_ancestor(base.as_str(), head.as_str())
                .unwrap_or(false)
        },
    )
}

/// Whether `receipt` of `run` is stale: it is this run's and `succeeded`,
/// names another commit than the worktree HEAD (`head`, `None` when it
/// cannot be read), the worktree is `clean`, and HEAD is a new commit on
/// top of the run's base (`on_base`). A receipt that names HEAD is never
/// asked to be rewritten. Each observation is read only when the ones
/// before it leave the receipt stale.
pub(super) fn stale_receipt_of(
    receipt: &Receipt,
    run: &TaskRun,
    head: impl FnOnce() -> Option<CommitSha>,
    clean: impl FnOnce() -> bool,
    on_base: impl FnOnce(&CommitSha) -> bool,
) -> Option<StaleReceipt> {
    if receipt.run_id() != run.id().as_str() || receipt.result() != ReceiptResult::Succeeded {
        return None;
    }
    let head = head()?;
    let stale = !receipt.names_commit(head.as_str())
        && head != *run.base_commit()
        && clean()
        && on_base(&head);
    stale.then(|| StaleReceipt {
        receipt_commit: receipt.commit().to_owned(),
        head,
    })
}

/// The request of `phase` (`attempt` for a resume) an earlier supervisor
/// recorded, so an adopted session is not asked again.
pub(super) fn adopted_stale_nudge(
    queue: &dyn Queue,
    run: &TaskRun,
    phase: &str,
    attempt: Option<usize>,
) -> Result<Option<StaleNudge>> {
    Ok(recorded_stale_nudge(
        &queue.run_events(run.id())?,
        phase,
        attempt,
    ))
}

/// The last request of `phase` (`attempt` for a resume) in `events`, typed
/// when its `stale_receipt_nudged` was recorded and settled once a
/// `stale_receipt_resolved` of the same phase is there; `None` when the
/// phase was never asked, so it may be asked once.
pub(super) fn recorded_stale_nudge(
    events: &[crate::domain::RunEvent],
    phase: &str,
    attempt: Option<usize>,
) -> Option<StaleNudge> {
    let of_phase = |e: &crate::domain::RunEvent| {
        e.payload["phase"] == phase && attempt.is_none_or(|a| e.payload["attempt"] == json!(a))
    };
    let nudged = events
        .iter()
        .rev()
        .find(|e| e.kind == event_kind::STALE_RECEIPT_NUDGED && of_phase(e))?;
    let at = super::file_time::recorded_at(nudged);
    Some(StaleNudge {
        at,
        typed_at: at,
        settled: events
            .iter()
            .any(|e| e.kind == event_kind::STALE_RECEIPT_RESOLVED && of_phase(e)),
    })
}

/// `payload` with the resume's `attempt`, when it is one.
fn with_attempt(mut payload: Value, attempt: Option<usize>) -> Value {
    if let Some(attempt) = attempt {
        payload["attempt"] = json!(attempt);
    }
    payload
}

/// The payload of `stale_receipt_nudged`: the phase, the receipt's commit
/// and the HEAD it should name, and the workspace the request is typed
/// into.
pub(super) fn nudged_payload(
    phase: &str,
    attempt: Option<usize>,
    stale: &StaleReceipt,
    workspace: &str,
) -> Value {
    with_attempt(
        json!({
            "phase": phase,
            "receipt_commit": stale.receipt_commit,
            "head": stale.head,
            "workspace_id": workspace,
        }),
        attempt,
    )
}

/// The payload of `stale_receipt_resolved`: the phase and how the request
/// ended.
pub(super) fn resolved_payload(phase: &str, attempt: Option<usize>, outcome: &str) -> Value {
    with_attempt(json!({"phase": phase, "outcome": outcome}), attempt)
}

/// Record `stale_receipt_nudged` and type the request into `workspace`.
/// `None` when it could not be typed: the run then goes on as before.
pub(super) fn nudge_stale_receipt(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    phase: &str,
    attempt: Option<usize>,
    stale: &StaleReceipt,
) -> Result<Option<(StaleNudge, SystemTime)>> {
    sv.queue.record_runtime_event(
        run.id(),
        EventKind::StaleReceiptNudged,
        nudged_payload(phase, attempt, stale, workspace),
    )?;
    let text = stale_receipt_nudge(run, &stale.receipt_commit, &stale.head)?;
    let sent_at = sv.files.now();
    match submit(
        sv,
        run,
        workspace,
        Input::from(&text),
        "stale receipt nudge",
    ) {
        Ok(submission) => {
            let detail = with_attempt(json!({"phase": phase, "workspace_id": workspace}), attempt);
            // The request is written as the session's next turn; a record
            // that fails is only noted.
            if matches!(submission, Submission::Queued)
                && let Err(error) = sv.queue.record_runtime_event(
                    run.id(),
                    EventKind::AutoRepaired,
                    json!({
                        "layer": "runtime",
                        "repair": "receipt_rewrite_requested",
                        "conditions": {
                            "clean": true,
                            "on_base": true,
                            "receipt_commit": stale.receipt_commit,
                            "head": stale.head,
                        },
                        "detail": detail,
                    }),
                )
            {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "auto_repaired of {} could not be recorded: {error:#}", run.id());
            }
            info!(run_id = %run.id(), "run {} went idle with a receipt for {} while its clean HEAD is {}; asked it to rewrite the receipt in workspace {workspace}", run.id(), stale.receipt_commit, stale.head);
            Ok(Some((
                StaleNudge {
                    at: sent_at,
                    typed_at: sent_at,
                    settled: false,
                },
                sent_at,
            )))
        }
        Err(error) => {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "the request to rewrite the receipt of {} could not be typed into workspace {workspace}: {error:#}; going on as before", run.id());
            StaleNudge {
                at: sent_at,
                typed_at: sent_at,
                settled: false,
            }
            .settle(sv, run, phase, attempt, "unsent")?;
            Ok(None)
        }
    }
}

impl StaleNudge {
    /// The session did not answer the request within the resume timeout:
    /// the run goes on as before without waiting longer.
    pub(super) fn waited_out(&self, files: &dyn RunFiles, sessions: &dyn SessionWrappers) -> bool {
        self.waited_out_at(files.now(), sessions.resume_timeout())
    }

    /// Whether the request, typed (or its clock restarted by the return
    /// from a wait) at [`Self::at`], has waited its `timeout` at `now`.
    pub(super) fn waited_out_at(&self, now: SystemTime, timeout: Duration) -> bool {
        now.duration_since(self.at)
            .is_ok_and(|waited| waited >= timeout)
    }

    /// Whether the idle marker written at `idle` answers the request: written
    /// after it was typed (or after the return from a wait), in a later
    /// millisecond (task 1050).
    pub(super) fn answered_by(&self, idle: SystemTime) -> bool {
        super::file_time::written_after(idle, self.at)
    }

    /// The receipt at `path` changed after the request was typed, in a
    /// later millisecond (task 1050): one of the request's millisecond is
    /// the receipt from before it.
    pub(super) fn rewritten(&self, files: &dyn RunFiles, path: &Path) -> bool {
        files
            .modified(path)
            .is_ok_and(|modified| super::file_time::written_after(modified, self.typed_at))
    }

    /// Record how the request ended, once: `rewritten`, `unchanged` or
    /// `run_ended`.
    pub(super) fn settle(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        phase: &str,
        attempt: Option<usize>,
        outcome: &str,
    ) -> Result<()> {
        if self.settled {
            return Ok(());
        }
        self.settled = true;
        sv.queue.record_runtime_event(
            run.id(),
            EventKind::StaleReceiptResolved,
            resolved_payload(phase, attempt, outcome),
        )?;
        info!(run_id = %run.id(), "the request to rewrite the receipt of {} ended: {outcome}", run.id());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;

    use super::super::file_time::at_ns;
    use crate::domain::{EventId, RunEvent, RunId, RunRecord};

    const BASE: &str = "0000000000000000000000000000000000000001";
    const OLD: &str = "0000000000000000000000000000000000000002";
    const HEAD: &str = "0000000000000000000000000000000000000003";

    fn sha(text: &str) -> CommitSha {
        CommitSha::parse(text, "commit").unwrap()
    }

    fn run() -> TaskRun {
        TaskRun::restore(RunRecord {
            id: RunId::new("r1").unwrap(),
            task_id: TaskId::new(1),
            status: RunStatus::Running,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: crate::domain::worker::Worker::default_mode(Provider::Claude),
            base_commit: sha(BASE),
            branch: Some("dagq/r1".to_owned()),
            worktree_path: Some("/runs/r1/worktree".to_owned()),
            workspace_id: None,
            receipt_path: Some("/runs/r1/receipt.json".to_owned()),
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: Some("/runs/r1".to_owned()),
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap()
    }

    fn receipt(run_id: &str, result: &str, commit: &str) -> Receipt {
        Receipt::parse(
            &json!({
                "run_id": run_id,
                "result": result,
                "commit": commit,
                "tests": {"status": "passed", "evidence_or_reason": "cargo test"},
                "e2e": {"status": "not_applicable", "evidence_or_reason": "none"},
                "subagent_review": {"status": "not_applicable", "evidence_or_reason": "small"},
                "summary": "done",
            })
            .to_string(),
        )
        .unwrap()
    }

    /// Whether `receipt` is stale for a worktree at `head`, clean or not,
    /// whose HEAD is on the run's base or not.
    fn stale_at(receipt: &Receipt, head: &str, clean: bool, on_base: bool) -> Option<StaleReceipt> {
        stale_receipt_of(receipt, &run(), || Some(sha(head)), || clean, |_| on_base)
    }

    /// A receipt for an older commit than a clean new HEAD on the run's
    /// base is stale and names both commits; one for HEAD is not asked to
    /// be rewritten, nor one a rewrite would not make valid: another run's,
    /// a `failed` one, a dirty worktree, HEAD at the base or off it, an
    /// unreadable HEAD.
    #[test]
    fn only_a_receipt_for_an_older_commit_than_a_clean_new_head_is_stale() {
        let old = receipt("r1", "succeeded", OLD);
        let stale = stale_at(&old, HEAD, true, true).unwrap();
        assert_eq!(stale.receipt_commit, OLD);
        assert_eq!(stale.head, sha(HEAD));
        assert!(stale_at(&receipt("r1", "succeeded", HEAD), HEAD, true, true).is_none());
        assert!(stale_at(&receipt("r2", "succeeded", OLD), HEAD, true, true).is_none());
        assert!(stale_at(&receipt("r1", "failed", OLD), HEAD, true, true).is_none());
        assert!(stale_at(&old, HEAD, false, true).is_none());
        assert!(stale_at(&old, BASE, true, true).is_none());
        assert!(stale_at(&old, HEAD, true, false).is_none());
        assert!(stale_receipt_of(&old, &run(), || None, || true, |_| true).is_none());
    }

    /// A receipt that names HEAD is judged without reading the worktree's
    /// status or ancestry; one of another run without reading HEAD.
    #[test]
    fn a_receipt_for_the_head_reads_nothing_more() {
        let unread = |_: &CommitSha| -> bool { panic!("ancestry read") };
        assert!(
            stale_receipt_of(
                &receipt("r1", "succeeded", HEAD),
                &run(),
                || Some(sha(HEAD)),
                || panic!("status read"),
                unread,
            )
            .is_none()
        );
        assert!(
            stale_receipt_of(
                &receipt("r2", "succeeded", OLD),
                &run(),
                || panic!("head read"),
                || panic!("status read"),
                unread,
            )
            .is_none()
        );
    }

    fn event(id: i64, kind: &str, payload: Value, created_at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: created_at.to_owned(),
            actor: None,
        }
    }

    /// Each phase is asked once: a session phase or a resume attempt whose
    /// request is recorded is not asked again by an adopter, which reads
    /// when it was typed and whether it is settled; another attempt, or a
    /// phase never asked, may be asked.
    #[test]
    fn a_recorded_request_of_the_phase_is_not_asked_again() {
        let at = "2026-10-09T00:00:01.250Z";
        let events = [
            event(1, "stale_receipt_nudged", json!({"phase": "session"}), at),
            event(
                2,
                "stale_receipt_resolved",
                json!({"phase": "session", "outcome": "unchanged"}),
                at,
            ),
            event(
                3,
                "stale_receipt_nudged",
                json!({"phase": "resume", "attempt": 1}),
                at,
            ),
        ];
        let session = recorded_stale_nudge(&events, SESSION_PHASE, None).unwrap();
        assert!(session.settled);
        assert_eq!(session.at, session.typed_at);
        let ms = crate::domain::stats::timestamp_millis(at).unwrap();
        assert_eq!(session.at, UNIX_EPOCH + Duration::from_millis(ms as u64));
        assert_eq!(ms % 1000, 250);
        let first = recorded_stale_nudge(&events, RESUME_PHASE, Some(1)).unwrap();
        assert!(!first.settled);
        assert!(recorded_stale_nudge(&events, RESUME_PHASE, Some(2)).is_none());
        assert!(recorded_stale_nudge(&events[..2], RESUME_PHASE, Some(1)).is_none());
        assert!(recorded_stale_nudge(&[], SESSION_PHASE, None).is_none());
    }

    /// `stale_receipt_nudged` and `stale_receipt_resolved` name their phase,
    /// and a resume's its attempt too; `stale_receipt_resolved` says how
    /// the request ended.
    #[test]
    fn the_events_name_the_phase_and_a_resumes_attempt() {
        let stale = StaleReceipt {
            receipt_commit: OLD.to_owned(),
            head: sha(HEAD),
        };
        assert_eq!(
            nudged_payload(SESSION_PHASE, None, &stale, "w1"),
            json!({"phase": "session", "receipt_commit": OLD, "head": HEAD, "workspace_id": "w1"})
        );
        assert_eq!(
            nudged_payload(RESUME_PHASE, Some(1), &stale, "w2"),
            json!({"phase": "resume", "attempt": 1, "receipt_commit": OLD, "head": HEAD, "workspace_id": "w2"})
        );
        assert_eq!(
            resolved_payload(SESSION_PHASE, None, "rewritten"),
            json!({"phase": "session", "outcome": "rewritten"})
        );
        assert_eq!(
            resolved_payload(RESUME_PHASE, Some(1), "unchanged"),
            json!({"phase": "resume", "attempt": 1, "outcome": "unchanged"})
        );
    }

    /// A request whose run waited for an answer outside its slot has its
    /// clock restarted at the return (`at`, ADR-0071 decision 15): an idle
    /// marker from before the return does not answer it and its timeout
    /// counts from there, but a receipt rewritten during the wait counts as
    /// `rewritten`, judged from when it was typed; one left as it was does
    /// not.
    #[test]
    fn a_receipt_rewritten_during_a_wait_counts_from_when_the_request_was_typed() {
        let timeout = Duration::from_secs(120);
        let typed = at_ns(0, 0);
        let returned = typed + Duration::from_secs(300);
        let nudge = StaleNudge {
            at: returned,
            typed_at: typed,
            settled: false,
        };
        let during_wait = typed + Duration::from_secs(10);
        assert!(!nudge.answered_by(during_wait));
        assert!(nudge.answered_by(returned + Duration::from_millis(1)));
        assert!(!nudge.waited_out_at(typed + timeout, timeout));
        assert!(!nudge.waited_out_at(returned + timeout - Duration::from_millis(1), timeout));
        assert!(nudge.waited_out_at(returned + timeout, timeout));
        let files = MemoryFiles::default();
        let receipt = Path::new("/run/receipt.json");
        files.put(receipt, typed, "{}");
        assert!(!nudge.rewritten(&files, receipt));
        files.put(receipt, during_wait, "{}");
        assert!(nudge.rewritten(&files, receipt));
    }

    /// Task 1050: an adopter reads when the request was typed from its
    /// `stale_receipt_nudged` event, to the millisecond: a receipt of that
    /// millisecond is the one from before the request, not rewritten.
    #[test]
    fn a_receipt_of_the_requests_millisecond_is_not_rewritten() {
        let files = MemoryFiles::default();
        let receipt = Path::new("/run/receipt.json");
        let nudge = StaleNudge {
            at: at_ns(250, 0),
            typed_at: at_ns(250, 0),
            settled: false,
        };
        assert!(!nudge.rewritten(&files, receipt));
        files.put(receipt, at_ns(250, 700_000), "{}");
        assert!(!nudge.rewritten(&files, receipt));
        files.put(receipt, at_ns(251, 0), "{}");
        assert!(nudge.rewritten(&files, receipt));
    }

    /// Task 1050: an idle marker of the millisecond of the request (an
    /// adopter's, from `stale_receipt_nudged`) does not answer it.
    #[test]
    fn a_marker_of_the_requests_millisecond_does_not_answer_it() {
        let nudge = StaleNudge {
            at: at_ns(250, 0),
            typed_at: at_ns(250, 0),
            settled: false,
        };
        assert!(!nudge.answered_by(at_ns(250, 700_000)));
        assert!(!nudge.answered_by(at_ns(249, 0)));
        assert!(nudge.answered_by(at_ns(251, 0)));
    }
}
