//! The review's judgment of an `already_satisfied` claim (ADR-t1818-1
//! decision 4): the headless review job asked whether main, at the
//! snapshot whose verification passed, meets the task's acceptance with
//! no change (execution and landing).

use super::*;
use crate::domain::satisfied::SnapshotVerification;

/// The bytes the whole prompt of a claim's judgment takes at most, the
/// language's instruction included (ADR-t1566-1 decision 4): the run
/// review's own room less its project's rules and known failures, which
/// the judgment does not carry. Its sections at their limits (25,000
/// bytes) and its instructions with the schema (about 3,500) fit in it
/// with the language's room.
pub const CLAIM_REVIEW_PROMPT_LIMIT: usize = 32_000;
/// The bytes of the task's title and of its acceptance, as the run
/// review's ([`RUN_REVIEW_TITLE_BYTES`], [`RUN_REVIEW_ACCEPTANCE_BYTES`]).
pub const CLAIM_REVIEW_TITLE_BYTES: usize = RUN_REVIEW_TITLE_BYTES;
pub const CLAIM_REVIEW_ACCEPTANCE_BYTES: usize = RUN_REVIEW_ACCEPTANCE_BYTES;
/// The bytes of the task's verification commands, one per line: they
/// passed on the snapshot, so they say what was checked, and a long list
/// is cut from its end (the receipt does not hold them; the job reads the
/// rest nowhere).
pub const CLAIM_REVIEW_VERIFY_BYTES: usize = 4_000;
/// The bytes of the receipt's summary and of its `base_checks`, the
/// worker's grounds for the claim: each cut from its end past this,
/// pointing at the receipt in the run directory.
pub const CLAIM_REVIEW_SUMMARY_BYTES: usize = 4_000;
pub const CLAIM_REVIEW_CHECKS_BYTES: usize = 6_000;
/// The bytes of the recovery job's diagnosis of a run it handed off
/// (ADR-t1818-2 decision 3), which stands for the grounds the worker did
/// not write: cut from its end past this (the job's output in the run
/// directory holds it whole).
pub const CLAIM_REVIEW_HAND_OFF_BYTES: usize = 2_000;

/// What the review is asked of a claim (ADR-t1818-1 decision 4): the
/// task's title, acceptance and verification commands, the worker's
/// grounds (the receipt's summary and `base_checks`, read from `receipt`,
/// whose file is `receipt_path`), and the snapshot `verification` that
/// passed on its head, which `tree`, a checkout of that head made for the
/// judgment, holds. `handed_off` is the recovery job's diagnosis when the
/// claim is the job's hand-off of a failed run rather than the worker's
/// (ADR-t1818-2 decision 3): the prompt says so and carries it. It is judged on that head alone, never the run's
/// worktree or main as it moved since. The verdict is the claim's own
/// (`ClaimJudgment`): `pass` only when every criterion holds there with no
/// change, its confidence whatever the verdict, and the `approve_close`
/// option it recommends. Each section is held to its limit and the whole
/// to [`CLAIM_REVIEW_PROMPT_LIMIT`]; what is cut says where the job reads
/// it whole, or that it reads it nowhere.
pub fn claim_review_prompt(
    task: &Task,
    run: &TaskRun,
    (receipt, receipt_path): (Option<&Receipt>, &str),
    verification: &SnapshotVerification,
    tree: &Path,
    handed_off: Option<&str>,
) -> FittedPrompt {
    let mut fit = Fit::new(CLAIM_REVIEW_PROMPT_LIMIT);
    let title = fit.text(
        "title",
        task.title(),
        CLAIM_REVIEW_TITLE_BYTES,
        Keep::Start,
        NOT_READABLE,
    );
    fit.section("title", &title);
    let acceptance = fit.required(
        "acceptance",
        or_none(task.acceptance()),
        CLAIM_REVIEW_ACCEPTANCE_BYTES,
        NOT_READABLE,
    );
    fit.section("acceptance", &acceptance);
    let commands = task.verification_commands().join("\n");
    let verify = fit.text(
        "verify",
        or_none(&commands),
        CLAIM_REVIEW_VERIFY_BYTES,
        Keep::Start,
        NOT_READABLE,
    );
    fit.section("verify", &verify);
    let at_receipt = format!("read it whole in the receipt at {receipt_path}");
    let summary = fit.text(
        "summary",
        or_none(receipt.map_or("", Receipt::summary)),
        CLAIM_REVIEW_SUMMARY_BYTES,
        Keep::Start,
        &at_receipt,
    );
    fit.section("summary", &summary);
    let checks = receipt.map_or_else(String::new, |receipt| {
        receipt
            .base_checks()
            .iter()
            .map(|check| format!("- {}: {}", check.command, check.result))
            .collect::<Vec<_>>()
            .join("\n")
    });
    let checks = fit.text(
        "base_checks",
        or_none(&checks),
        CLAIM_REVIEW_CHECKS_BYTES,
        Keep::Start,
        &format!("{at_receipt}, its base_checks"),
    );
    fit.section("base_checks", &checks);
    let claim = match handed_off {
        None => "Its worker reported already_satisfied: the base already met the task's acceptance, so it made no change and no commit.\n".to_owned(),
        Some(diagnosis) => {
            let diagnosis = fit.text(
                "hand_off",
                or_none(diagnosis),
                CLAIM_REVIEW_HAND_OFF_BYTES,
                Keep::Start,
                "read the recovery job's output (recovery-*.out) in the run directory",
            );
            fit.section("hand_off", &diagnosis);
            format!(
                "Its worker made no change and no commit but did not report already_satisfied, so the run failed; the recovery job then handed it over as a claim that the base already met the task's acceptance. Its worker's summary and checks below are the grounds it left, with the recovery job's diagnosis:\n{diagnosis}\n"
            )
        }
    };
    let text = format!(
        "You judge a claim of run {run_id} of dagq task {task_id} ({title}).\n\
         {claim}\
         The runtime then ran the task's {count} verification commands on main at {head} (the snapshot), and every one passed.\n\
         The directory {tree} is a checkout of main at {head} made for this judgment: judge the claim on that head and on nothing else (not the worker's worktree, not main as it may have moved since). Read it as you need; do not change any file.\n\n\
         Acceptance criteria of the task:\n{acceptance}\n\n\
         The task's verification commands, which passed on {head}:\n{verify}\n\n\
         The worker's summary:\n{summary}\n\n\
         The checks the worker ran on its base (base_checks):\n{checks}\n\n\
         {verdicts}\
         {answer}",
        run_id = run.id(),
        task_id = task.id(),
        count = verification.commands,
        head = verification.head,
        tree = tree.display(),
        verdicts = CLAIM_REVIEW_VERDICTS,
        answer = CLAIM_REVIEW_ANSWER,
    );
    fit.finish(text)
}

/// The claim's verdicts, its confidence and its recommendation.
const CLAIM_REVIEW_VERDICTS: &str = "Decide one verdict:\n\
- pass: main at that head meets every acceptance criterion as it is, with no change: the task is done.\n\
- revise: some criterion is not met at that head, and the work it needs is clear: the task should run again and make the change.\n\
- concern: whether the criteria are met calls for a person's judgment (a criterion is ambiguous, or only partly met, or the task may be obsolete).\n\
Say how sure you are whatever the verdict: confidence high when the criteria, the tree and the passed commands settle it and you would bet on a person judging the same; low when you hesitate, the material is not enough, or a person could reasonably judge otherwise; when in doubt, say low.\n\
Recommend what a person should answer: close (complete the task without a commit), retry (put the task back to run again) or cancel (cancel the task as obsolete).\n\
A person decides the claim with your judgment in front of them.\n\n";

/// The claim's answer: one JSON object of this schema.
const CLAIM_REVIEW_ANSWER: &str = "Answer with one JSON object and nothing else, matching this schema:\n\
{\"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [string], \"summary\": string, \"confidence\": \"high\" | \"low\", \"recommendation\": \"close\" | \"retry\" | \"cancel\"}\n\
reasons lists, for each criterion, whether and where the tree meets it (or why it does not); summary is one or two sentences.\n";
