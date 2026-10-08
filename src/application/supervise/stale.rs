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
/// and a rewrite would name a valid one: it parses, is this run's and
/// `succeeded`, the worktree is clean, and HEAD is a new commit on top of the
/// run's base. Anything that cannot be read is `None`: the run goes on as
/// before.
pub(super) fn stale_receipt(sv: &Supervisor<'_>, run: &TaskRun) -> Option<StaleReceipt> {
    let receipt = sv
        .files
        .read_to_string(Path::new(run.receipt_path()?))
        .ok()
        .and_then(|text| Receipt::parse(&text).ok())?;
    if receipt.run_id() != run.id().as_str() || receipt.result() != ReceiptResult::Succeeded {
        return None;
    }
    let worktree = Path::new(run.worktree_path()?);
    let head = sv.repository.head(worktree).ok()?;
    let base = run.base_commit();
    let stale = !receipt.names_commit(head.as_str())
        && head != *base
        && sv
            .repository
            .status(worktree)
            .is_ok_and(|status| status.trim().is_empty())
        && sv
            .repository
            .is_ancestor(base.as_str(), head.as_str())
            .unwrap_or(false);
    stale.then_some(StaleReceipt {
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
    let events = queue.run_events(run.id())?;
    let of_phase = |e: &crate::domain::RunEvent| {
        e.payload["phase"] == phase && attempt.is_none_or(|a| e.payload["attempt"] == json!(a))
    };
    let Some(nudged) = events
        .iter()
        .rev()
        .find(|e| e.kind == event_kind::STALE_RECEIPT_NUDGED && of_phase(e))
    else {
        return Ok(None);
    };
    let at = super::file_time::recorded_at(nudged);
    Ok(Some(StaleNudge {
        at,
        typed_at: at,
        settled: events
            .iter()
            .any(|e| e.kind == event_kind::STALE_RECEIPT_RESOLVED && of_phase(e)),
    }))
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
    let mut payload = json!({
        "phase": phase,
        "receipt_commit": stale.receipt_commit,
        "head": stale.head,
        "workspace_id": workspace,
    });
    if let Some(attempt) = attempt {
        payload["attempt"] = json!(attempt);
    }
    sv.queue
        .record_runtime_event(run.id(), EventKind::StaleReceiptNudged, payload)?;
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
            let mut detail = json!({"phase": phase, "workspace_id": workspace});
            if let Some(attempt) = attempt {
                detail["attempt"] = json!(attempt);
            }
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
        files
            .now()
            .duration_since(self.at)
            .is_ok_and(|waited| waited >= sessions.resume_timeout())
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
        let mut payload = json!({"phase": phase, "outcome": outcome});
        if let Some(attempt) = attempt {
            payload["attempt"] = json!(attempt);
        }
        sv.queue
            .record_runtime_event(run.id(), EventKind::StaleReceiptResolved, payload)?;
        info!(run_id = %run.id(), "the request to rewrite the receipt of {} ended: {outcome}", run.id());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;

    use super::super::file_time::at_ns;

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
