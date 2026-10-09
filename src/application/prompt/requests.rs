//! The requests the supervisor types into a live worker session: a resume,
//! a revise, a receipt that does not match, a stall (execution and landing).

use super::*;

/// What the resolution request tells a resumed session.
pub(crate) struct ResumeRequest {
    /// The landing branch's head the session rebases onto.
    pub main: CommitSha,
    /// The landing branch's name (ADR-t615-1).
    pub branch: String,
    pub reason: String,
    pub kind: ResumeKind,
    /// The file in the run directory the caller writes the whole `reason`
    /// to when the request cuts it (its `omitted` names `reason`); `None`
    /// when there is none, and a cut reason is said to be in no file.
    pub reason_file: Option<String>,
}

/// The bytes a resolution request (`resume_request`) takes at most, the
/// language's instruction included (ADR-t2072-1): its fixed opening and
/// steps (under 4,000 bytes with a landing branch's name of 256 bytes),
/// the reason ([`RESUME_REASON_BYTES`]), the task's paths
/// ([`WORKER_PATHS_BYTES`]) and verification commands
/// ([`WORKER_VERIFY_BYTES`]) as in the worker's prompt, the section on
/// landed tasks, bounded by its own [`LANDED_SECTION_BYTES`] and never cut
/// here, and the notes of what was cut (the largest input of the unit
/// test takes 30,288 bytes), with the language's room. Of the 581 resolution and conflict requests kept on
/// the host on 2026-10-08, most of whose length was the landed tasks'
/// summaries the section no longer carries, the reason took 272 bytes at
/// the median, 1,070 at p90, 3,338 at p99 and 4,370 at most.
pub const RESUME_REQUEST_LIMIT: usize = 36_000;

/// The bytes of a resolution request's reason (a review's findings sent
/// back, a triage's instruction, why integrate or the landing recheck
/// failed): about twice the longest seen ([`RESUME_REQUEST_LIMIT`]). What
/// is cut is in the request's `reason_file`.
pub const RESUME_REASON_BYTES: usize = 8_000;

/// Why the run waits for a session, which decides the request's steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeKind {
    /// A landing was deferred (a conflict, failed verification): rebase.
    Landing,
    /// Validation's `evidence_missing`: add the evidence instead.
    EvidenceMissing,
    /// A person sent a review's concern back (`landing_decided`): fix
    /// the findings.
    SentBack,
    /// The diff changes paths outside the task's `paths` (validation's
    /// `scope_violation`, or a landing deferred for it): take them out.
    ScopeViolation,
    /// A passed run's live session, before its `/exit`: the precheck found
    /// that it conflicts with main (ADR-0027 decision 4). Rebase, like
    /// `Landing`.
    Precheck,
    /// The triage of a `failed` / `interrupted` run sent it back to its
    /// session (`triage_finished` with action `resume`, or a person's
    /// `resume` answer, `triage_decided`): do what the reason asks.
    Triage,
    /// A run that waited to land, parked by the landing recheck after
    /// another landing moved main (ADR-0068 decision 3): rebase, like
    /// `Landing`, and run the failed recheck command again.
    Recheck,
    /// A passed run whose `/exit` never reached its session, and whose
    /// workspace an adopter found gone while the run could not land as it
    /// stood (task 960): do what the reason says holds it, and rewrite the
    /// receipt, which validation and review check again.
    SessionGone,
    /// The e2e the runtime ran on the host after the review failed
    /// (ADR-t1233-2 decision 3): fix the failed tests the reason names,
    /// and the run is validated, reviewed and its e2e run again.
    E2e,
}

/// The section of a resolution request on the tasks landed on `branch`
/// between `base` and `main` (`landed` oldest first): the newest
/// [`LANDED_TASK_LINES`] by ID and title, and a line counting the rest.
pub(super) fn landed_lines(
    branch: &str,
    base: &str,
    main: &str,
    landed: &[LandedTask],
) -> Vec<String> {
    if landed.is_empty() {
        return vec![format!("Tasks landed on {branch} since your base: none.")];
    }
    let mut lines = vec![format!(
        "Tasks landed on {branch} since your base, newest first (git log {base}..{main} and git show <commit> tell what each changed):"
    )];
    let mut cut = false;
    for task in landed.iter().rev().take(LANDED_TASK_LINES) {
        let title = landed_title(&task.title);
        cut |= title.len() != task.title.len();
        lines.push(format!("- task {}: {title}", task.task_id));
    }
    if cut {
        lines.push(format!(
            "- titles ending {LANDED_TITLE_MARK}N bytes] are cut; each is whole as its commit's subject in git log {base}..{main}"
        ));
    }
    if let Some(more) = landed
        .len()
        .checked_sub(LANDED_TASK_LINES)
        .filter(|n| *n > 0)
    {
        lines.push(format!(
            "- … and {more} more; git log --oneline {base}..{main} lists them all"
        ));
    }
    lines
}

/// `title` within [`LANDED_TITLE_BYTES`]: whole when it fits, else its
/// start cut on a character boundary and the mark of the bytes left out.
pub(super) fn landed_title(title: &str) -> std::borrow::Cow<'_, str> {
    if title.len() <= LANDED_TITLE_BYTES {
        return title.into();
    }
    // The mark's length depends on the count it names: take the longest
    // it can be.
    let room = LANDED_TITLE_BYTES - format!("{LANDED_TITLE_MARK}{} bytes]", title.len()).len();
    let mut end = room;
    while !title.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}{LANDED_TITLE_MARK}{} bytes]",
        &title[..end],
        title.len() - end
    )
    .into()
}

/// The fixed resolution request the supervisor types into a resumed
/// session (ADR-0019 decision 1), or into a passed run's live session whose
/// head conflicts with main (ADR-0027 decision 4), one instruction per
/// line; the backend sends it as one line.
///
/// Held to [`RESUME_REQUEST_LIMIT`] (ADR-t2072-1): the reason is cut to
/// [`RESUME_REASON_BYTES`] and points at `reason_file`; the task's paths and
/// verification commands are the task's own and cut only past their
/// limits, said in `over_limit`; the section on landed tasks keeps its own
/// bounds; the steps are never cut.
pub(crate) fn resume_request(
    task: &Task,
    run: &TaskRun,
    request: &ResumeRequest,
    landed: &[LandedTask],
) -> Result<FittedPrompt> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let route = Route::of(run);
    let mut fit = Fit::new(RESUME_REQUEST_LIMIT);
    let reason_read = request.reason_file.as_ref().map_or_else(
        || NOT_READABLE.to_owned(),
        |file| format!("the whole reason is in {file}"),
    );
    let reason = fit.text(
        "reason",
        &request.reason,
        RESUME_REASON_BYTES,
        Keep::Start,
        &reason_read,
    );
    fit.section("reason", &reason);
    let paths = if request.kind == ResumeKind::ScopeViolation {
        let paths = fit.required(
            "paths",
            &task.paths().join(", "),
            WORKER_PATHS_BYTES,
            NOT_READABLE,
        );
        fit.section("paths", &paths);
        paths
    } else {
        String::new()
    };
    let verify = fit.required(
        "verify",
        &serde_json::to_string(task.verification_commands())?,
        WORKER_VERIFY_BYTES,
        NOT_READABLE,
    );
    fit.section("verify", &verify);
    let mut lines = route.opening(match request.kind {
        ResumeKind::EvidenceMissing => format!(
            "dagq: the supervisor's validation of run {} (task {}) found required evidence missing from the receipt, so the run is needs_session.",
            run.id(),
            task.id()
        ),
        ResumeKind::SentBack => format!(
            "dagq: the supervisor's review of run {} (task {}) raised findings a person sent back to you, so the run is needs_session.",
            run.id(),
            task.id()
        ),
        ResumeKind::ScopeViolation => format!(
            "dagq: run {} (task {}) changes paths outside the task's --paths ({paths}), so the run is needs_session.",
            run.id(),
            task.id(),
        ),
        ResumeKind::Landing => format!(
            "dagq: integrate could not land run {} (task {}) and returned needs_session.",
            run.id(),
            task.id()
        ),
        ResumeKind::Precheck => format!(
            "dagq: the supervisor's review of run {} (task {}) passed, but integrate would conflict with {}, so the run was not landed.",
            run.id(),
            task.id(),
            request.branch
        ),
        ResumeKind::Triage => format!(
            "dagq: run {} (task {}) failed or was interrupted, and the supervisor's triage sent it back to this session to finish, so the run is needs_session.",
            run.id(),
            task.id()
        ),
        ResumeKind::Recheck => format!(
            "dagq: run {} (task {}) was waiting to land, and after another landing moved {} the supervisor's landing recheck found that it no longer lands, so the run is needs_session before anyone answers for it.",
            run.id(),
            task.id(),
            request.branch
        ),
        ResumeKind::E2e => format!(
            "dagq: run {} (task {}) passed its review, but the e2e the runtime ran on the host before landing it failed, so the run is needs_session and is validated, reviewed and its e2e run again after this session.",
            run.id(),
            task.id()
        ),
        ResumeKind::SessionGone => format!(
            "dagq: run {} (task {}) passed its review, but its session's workspace was gone before the run could land, and the run cannot land as it stands, so the run is needs_session and is validated and reviewed again after this session.",
            run.id(),
            task.id()
        ),
    });
    lines.push(format!("Reason: {reason}"));
    let branch = &request.branch;
    lines.push(format!(
        "{branch} is now {} (your base commit was {}).",
        request.main,
        run.base_commit()
    ));
    let landed = landed_lines(
        branch,
        run.base_commit().as_str(),
        request.main.as_str(),
        landed,
    );
    fit.section("landed", &landed.join("\n"));
    lines.extend(landed);
    lines.push("Steps:".to_owned());
    let checks = local_checks(&verify);
    if request.kind == ResumeKind::EvidenceMissing {
        lines.push(
            "1. Run the checks the reason names as missing and write their evidence into the receipt."
                .to_owned(),
        );
        lines.push(format!("2. If that changes files, commit them. {checks}"));
    } else if request.kind == ResumeKind::ScopeViolation {
        lines.push(format!(
            "1. Take the changes to the paths the reason names out of the run branch: restore each to its state at git merge-base HEAD {} (delete the ones that did not exist there) and commit; if the task cannot be done without them, write the receipt with result failed and say which paths it needs.",
            request.main
        ));
        lines.push(format!("2. {checks}"));
    } else if request.kind == ResumeKind::SentBack {
        lines.push(format!(
            "1. Fix the findings in the reason and commit; if {branch} moved, git rebase {} first.",
            request.main
        ));
        lines.push(format!("2. {checks}"));
    } else if request.kind == ResumeKind::Triage {
        lines.push(format!(
            "1. Do what the reason asks in this worktree and commit; if {branch} moved, git rebase {} first.",
            request.main
        ));
        lines.push(format!("2. {checks}"));
    } else if request.kind == ResumeKind::E2e {
        lines.push(format!(
            "1. Read the logs the reason names and fix the e2e tests that failed (each failed once more on its rerun by name) and commit; if {branch} moved, git rebase {} first. You may run a failed test by name to reproduce it, the way the repository's instructions (AGENTS.md or CLAUDE.md) say, but not the whole e2e: the runtime runs it again after the review.",
            request.main
        ));
        lines.push(format!("2. {checks}"));
    } else if request.kind == ResumeKind::SessionGone {
        lines.push(format!(
            "1. Settle what the reason says holds the run in this worktree (finish or abort a rebase in progress, wait for the answer to an open worker_question, or commit the work the head holds) and commit; if {branch} moved, git rebase {} first.",
            request.main
        ));
        lines.push(format!("2. {checks}"));
    } else {
        lines.push(format!(
            "1. In this worktree run git rebase {} and resolve the conflicts.",
            request.main
        ));
        // Only integrate's deferral can name a failed verification command;
        // the precheck's reason is always a conflict.
        let reproduce = match request.kind {
            ResumeKind::Landing => {
                " If the reason is a verification command that failed after integrate's rebase, you may also run that command in the worktree to reproduce and fix the failure."
            }
            ResumeKind::Recheck => {
                " If the reason is a command that failed on the landing branch with the run merged in (git found no conflict), run that command in the worktree after the rebase to reproduce and fix the failure."
            }
            _ => "",
        };
        lines.push(format!("2. {checks}{reproduce} Commit the result."));
    }
    lines.push("3. Keep the worktree clean.".to_owned());
    lines.push(format!("4. {}", route.stop()));
    lines.push(format!(
        "5. Rewrite the receipt at {receipt} with the new head commit, writing a temporary file in the same directory and renaming it. {ACCEPTANCE_REMAP} {FOLLOW_UP_PROPOSAL_AGAIN}"
    ));
    lines.push(
        "6. If the change is no longer needed, write the receipt with result failed and the reason in summary."
            .to_owned(),
    );
    lines.push(format!("7. {}", route.done(run)));
    Ok(fit.finish(lines.join("\n")))
}

/// The fixed request the supervisor types into the live session when the
/// receipt it rewrote for a revise or a conflict request does not name its clean worktree HEAD.
/// Held to [`NEXT_TURN_LIMIT`]: `why` to [`NEXT_TURN_WHY_BYTES`].
pub(crate) fn revise_mismatch_request(
    run: &TaskRun,
    label: &str,
    why: &str,
) -> Result<FittedPrompt> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let route = Route::of(run);
    let mut fit = Fit::new(NEXT_TURN_LIMIT);
    let why = fit.text("why", why, NEXT_TURN_WHY_BYTES, Keep::Start, NOT_READABLE);
    fit.section("why", &why);
    let mut lines = route.opening(format!(
        "dagq: the receipt you rewrote for {label} of run {} cannot be accepted: {why}.",
        run.id()
    ));
    lines.extend([
        "Steps:".to_owned(),
        "1. Commit every change you meant to make, so the worktree is clean.".to_owned(),
        format!(
            "2. Rewrite the receipt at {receipt} with the current HEAD commit (git rev-parse HEAD), writing a temporary file in the same directory and renaming it."
        ),
        format!("3. {}", route.stop()),
        format!("4. {}", route.done(run)),
    ]);
    Ok(fit.finish(lines.join("\n")))
}

/// The one fixed request the supervisor sends, as the next turn, to a
/// session that ended its turn with a receipt naming `receipt_commit` while
/// its clean worktree HEAD is `head`, a new commit on top of its base (task
/// 357): rewrite the receipt for the head, or fix the worktree first.
/// Held to [`NEXT_TURN_LIMIT`]: the receipt's commit, which the worker
/// wrote, to [`NEXT_TURN_NAME_BYTES`]; the receipt has it whole.
pub(crate) fn stale_receipt_nudge(
    run: &TaskRun,
    receipt_commit: &str,
    head: &CommitSha,
) -> Result<FittedPrompt> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let route = Route::of(run);
    let mut fit = Fit::new(NEXT_TURN_LIMIT);
    let receipt_commit = fit.text(
        "receipt_commit",
        receipt_commit,
        NEXT_TURN_NAME_BYTES,
        Keep::Start,
        &format!("the receipt at {receipt} has it whole"),
    );
    fit.section("receipt_commit", &receipt_commit);
    let mut lines = route.opening(format!(
        "dagq: run {} ended its turn, but its receipt names commit {receipt_commit} while the clean worktree HEAD is {head} (for example after a rebase or a new commit). The supervisor cannot accept a receipt for another commit.",
        run.id()
    ));
    lines.extend([
        "Steps:".to_owned(),
        format!(
            "1. If HEAD is the work you mean to submit, rewrite the receipt at {receipt} with commit {head} (git rev-parse HEAD), writing a temporary file in the same directory and renaming it. Otherwise fix the worktree, commit, and rewrite the receipt with the new HEAD."
        ),
        format!("2. {}", route.stop()),
        format!("3. {}", route.done(run)),
        "If the receipt stays as it is, the run goes on as before and validation judges it."
            .to_owned(),
    ]);
    Ok(fit.finish(lines.join("\n")))
}

/// The one nudge the supervisor sends a worker's session whose turn ended
/// without a receipt or an open question (ADR-0043 decision 1, ADR-t813-1
/// decision 9): commit and write the receipt, ask with `dagq ask`, or run
/// again in the foreground what it ended the turn to wait for. Nothing of
/// the ended turn still runs, and the nudge is its next turn. It has no
/// text of variable length: its bytes are counted against
/// [`NEXT_TURN_LIMIT`] like the other next turns'.
pub(crate) fn stall_nudge(run: &TaskRun) -> Result<FittedPrompt> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let mut lines = Route::of(run).opening(format!(
        "dagq: the previous turn of run {} ended without a receipt or an open question.",
        run.id()
    ));
    lines.push("Do one of these in this turn:".to_owned());
    lines.push(format!(
        "1. If the work is done, commit it and write the receipt at {receipt} (a temporary file in the same directory, then rename). If it is not, go on with it now and end the turn with the receipt."
    ));
    lines.push(format!(
        "2. {ASK_RULES_FIRST} Otherwise, if you need a decision, run `dagq ask --run {} --kind worker_question --because scope --topic <code> --question '...'` (or `--because discard` for whether to throw work away) and end the turn.",
        run.id()
    ));
    lines.push(
        "3. If you ended the turn to wait for something, it was stopped with the turn: run it again in the foreground, wait for it to finish, and go on with the work."
            .to_owned(),
    );
    lines.push(
        "If the turns keep ending without a receipt or an ask, the supervisor hands the run to its recovery job."
            .to_owned(),
    );
    Ok(Fit::new(NEXT_TURN_LIMIT).finish(lines.join("\n")))
}

/// What the supervisor sends a worker's session, in place of the nudge,
/// once its `worker_question` `ask_id` was closed without its answer
/// reaching it (task 1372): who closed it and what was recorded with it
/// (`answer`; `ask close` takes no reason of its own), and that the worker
/// decides within the task or writes a failed receipt, without asking the
/// same question again. `closed_by` names the closer, `None` when the
/// close recorded none. Held to [`NEXT_TURN_LIMIT`]: the closer to
/// [`NEXT_TURN_NAME_BYTES`] and the answer to [`NEXT_TURN_TEXT_BYTES`].
pub(crate) fn closed_question_notice(
    run: &TaskRun,
    ask_id: i64,
    closed_by: Option<&str>,
    answer: Option<&str>,
) -> Result<FittedPrompt> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let route = Route::of(run);
    let mut fit = Fit::new(NEXT_TURN_LIMIT);
    let closer = closed_by.map_or_else(
        || "someone (not recorded)".to_owned(),
        |closer| {
            fit.text(
                "closer",
                closer,
                NEXT_TURN_NAME_BYTES,
                Keep::Start,
                NOT_READABLE,
            )
        },
    );
    fit.section("closer", &closer);
    let mut lines = route.opening(format!(
        "dagq: ask {ask_id} (your worker_question on run {}) was closed by {closer} without an answer delivered to you.",
        run.id()
    ));
    lines.push(match answer.map(str::trim).filter(|a| !a.is_empty()) {
        Some(answer) => {
            let answer = fit.text(
                "answer",
                answer,
                NEXT_TURN_TEXT_BYTES,
                Keep::Start,
                NOT_READABLE,
            );
            fit.section("answer", &answer);
            format!("What was recorded with it when it was closed: {answer}")
        }
        None => "No reason was recorded with the close.".to_owned(),
    });
    lines.push("Do not ask the same question again. Do one of these in this turn:".to_owned());
    lines.push(format!(
        "1. If the decision is within the task's scope, decide it yourself, go on with the work, commit it and write the receipt at {receipt} (a temporary file in the same directory, then rename), saying in its summary what you decided and why."
    ));
    lines.push(format!(
        "2. If it needs a change outside the task's scope, write a failed receipt at {receipt} whose summary says why and what is needed."
    ));
    lines.push(format!("3. {}", route.stop()));
    lines.push(
        "If the turns keep ending without a receipt, the supervisor hands the run to its recovery job."
            .to_owned(),
    );
    Ok(fit.finish(lines.join("\n")))
}

/// The fixed request the supervisor types into the live session for a
/// `revise` verdict (ADR-0027 decision 2), one instruction per line; the
/// backend sends it as one line.
///
/// Held to [`REVISE_REQUEST_LIMIT`] (ADR-t2072-1): the findings, as one
/// section, are cut to [`REVISE_FINDINGS_BYTES`] and point at
/// `findings_file`; the verification commands are the task's own and cut
/// only past [`WORKER_VERIFY_BYTES`], said in `over_limit`; the steps are
/// never cut.
pub(crate) fn revise_request(
    task: &Task,
    run: &TaskRun,
    round: usize,
    reasons: &[String],
    findings_file: Option<&str>,
) -> Result<FittedPrompt> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let mut fit = Fit::new(REVISE_REQUEST_LIMIT);
    let findings_read = findings_file.map_or_else(
        || NOT_READABLE.to_owned(),
        |file| format!("the whole findings are in {file}"),
    );
    let findings = fit.text(
        "findings",
        &revise_findings(reasons),
        REVISE_FINDINGS_BYTES,
        Keep::Start,
        &findings_read,
    );
    fit.section("findings", &findings);
    let verify = fit.required(
        "verify",
        &serde_json::to_string(task.verification_commands())?,
        WORKER_VERIFY_BYTES,
        NOT_READABLE,
    );
    fit.section("verify", &verify);
    let checks = local_checks(&verify);
    let route = Route::of(run);
    let mut lines = route.opening(format!(
        "dagq: the supervisor's review of run {} (task {}) asks for changes (revise {round} of {MAX_REVISE_ATTEMPTS}).",
        run.id(),
        task.id()
    ));
    lines.push("Findings:".to_owned());
    if !findings.is_empty() {
        lines.push(findings);
    }
    lines.push("Steps:".to_owned());
    lines.push("1. Fix the findings in this worktree and commit.".to_owned());
    lines.push(format!("2. {checks}"));
    lines.push("3. Keep the worktree clean.".to_owned());
    lines.push(format!("4. {}", route.stop()));
    lines.push(format!(
        "5. Rewrite the receipt at {receipt} with the new head commit, writing a temporary file in the same directory and renaming it. {ACCEPTANCE_REMAP} {FOLLOW_UP_PROPOSAL_AGAIN}"
    ));
    lines.push(format!("6. {}", route.done(run)));
    Ok(fit.finish(lines.join("\n")))
}

/// The findings of a revise request, one line each: what the request
/// carries, and what its `findings_file` holds whole when it was cut.
pub(crate) fn revise_findings(reasons: &[String]) -> String {
    reasons
        .iter()
        .map(|reason| format!("- {reason}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The bytes a revise request (`revise_request`) takes at most, the
/// language's instruction included (ADR-t2072-1): its fixed opening and
/// steps (under 3,000 bytes), the findings ([`REVISE_FINDINGS_BYTES`]),
/// the verification commands ([`WORKER_VERIFY_BYTES`]) as in the worker's
/// prompt and the notes of what was cut, with the language's room. The
/// revise requests kept on the host on 2026-10-08 took 4,227 bytes at the
/// median and 10,727 at most, nearly all of it the findings.
pub const REVISE_REQUEST_LIMIT: usize = 30_000;

/// The bytes of a revise request's findings, all of them as one section:
/// about twice the longest request seen ([`REVISE_REQUEST_LIMIT`]). What is
/// cut is in the request's findings file in the run directory.
pub const REVISE_FINDINGS_BYTES: usize = 20_000;
