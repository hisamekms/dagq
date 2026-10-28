//! The prompt of the read-only diagnosis job of a replanning operation
//! (ADR-t1521-1 decision 1, docs/plans/task-replanning.md, "入口と診断"):
//! the task's conditions, its turns by cause, the reviews of the window
//! that did not pass (the findings repeated), what the last turns did, the
//! causes outside the unit of work (conflicts, e2e and verification
//! failures, a provider's or the host's failures) and the task's earlier
//! diagnoses, then the verdict's schema.

use super::*;
use crate::domain::replan_detect::{TaskTurns, TurnCause, Window};
use crate::domain::task_replan::ReplanOperation;

/// The most the diagnosis prompt takes, in bytes.
pub const DIAGNOSIS_PROMPT_LIMIT: usize = 64_000;
const DIAGNOSIS_TEXT_BYTES: usize = 6_000;
const DIAGNOSIS_REVIEWS_BYTES: usize = 24_000;
const DIAGNOSIS_REVIEW_ITEM_BYTES: usize = 4_000;
const DIAGNOSIS_LIST_BYTES: usize = 6_000;
const DIAGNOSIS_LIST_ITEM_BYTES: usize = 1_000;

/// What the diagnosis of an operation reads ([`replan_diagnosis_prompt`]).
pub struct DiagnosisMaterial<'a> {
    pub operation: &'a ReplanOperation,
    pub task: &'a Task,
    pub run: &'a TaskRun,
    pub attempt: usize,
    pub turns: &'a TaskTurns,
    /// The window the detection reads now (one a person or a worker asked
    /// for has it too, whatever its count).
    pub window: &'a Window,
    /// The reviews that did not pass since the task's last pass, oldest
    /// first: each `review_finished`'s id, verdict, summary and reasons.
    pub reviews: &'a [Value],
    /// What happened beside the reviews, oldest first: the conflicts, the
    /// e2e and verification failures, the turns that failed.
    pub outside: &'a [Value],
    /// The task's earlier diagnoses, oldest first.
    pub earlier: &'a [Value],
    /// The run's latest receipt's summary and commit, if any.
    pub receipt: Option<&'a str>,
}

fn listed(
    fit: &mut Fit,
    name: &'static str,
    items: &[Value],
    (bytes, each): (usize, usize),
    read: &str,
) -> String {
    if items.is_empty() {
        return "none".to_owned();
    }
    // The newest first: those are what the next choice rests on.
    let (kept, left_out) = fit.lines(
        name,
        items,
        (0..items.len()).rev(),
        (usize::MAX, bytes, each),
        read,
    );
    let mut text = kept.join("\n");
    if !left_out.is_empty() {
        text.push('\n');
        text.push_str(&format!("({} older ones left out: {read})", left_out.len()));
    }
    fit.section(name, &text);
    text
}

/// What the diagnosis job of `material.operation` is asked: diagnose why
/// the task takes long and choose to go on, to replan or to ask a person,
/// as one JSON object.
pub fn replan_diagnosis_prompt(material: &DiagnosisMaterial<'_>) -> FittedPrompt {
    let task = material.task;
    let op = material.operation;
    let mut fit = Fit::new(DIAGNOSIS_PROMPT_LIMIT);
    let claimed = "the task as the run claimed it is in prompt.txt of the run directory";
    let description = fit.required(
        "task",
        or_none(task.description()),
        DIAGNOSIS_TEXT_BYTES,
        claimed,
    );
    let acceptance = fit.required(
        "task",
        or_none(task.acceptance()),
        DIAGNOSIS_TEXT_BYTES,
        claimed,
    );
    let verification = task.verification_commands().join("\n");
    fit.section("task", &description);
    fit.section("task", &acceptance);
    fit.section("task", &verification);
    let counts = &material.turns.total;
    let causes = TurnCause::ALL
        .iter()
        .map(|cause| format!("{} {}", cause.as_str(), counts.by_cause[cause.as_str()]))
        .collect::<Vec<_>>()
        .join(", ");
    let turns = format!(
        "{} turns sent and {} ended over {} run(s); by cause: {causes}. Reviews: {} ({} did not pass, {} passed).",
        counts.started,
        counts.finished,
        material.turns.runs.len(),
        counts.reviews,
        counts.non_pass,
        counts.passes,
    );
    fit.section("turns", &turns);
    let window = material.window;
    let window = format!(
        "{} non-pass reviews since {} (the threshold is {}){}",
        window.non_pass.len(),
        window
            .since
            .map_or_else(|| "the task's start".to_owned(), |id| format!("event {id}")),
        window.threshold,
        window
            .limit
            .map_or_else(String::new, |(trigger, id)| format!(
                "; the existing limit {} was reached at event {id}",
                trigger.as_str()
            )),
    );
    fit.section("window", &window);
    let read_events = format!("dagq events --task {} shows them in full", task.id());
    let reviews = listed(
        &mut fit,
        "reviews",
        material.reviews,
        (DIAGNOSIS_REVIEWS_BYTES, DIAGNOSIS_REVIEW_ITEM_BYTES),
        &read_events,
    );
    let outside = listed(
        &mut fit,
        "outside",
        material.outside,
        (DIAGNOSIS_LIST_BYTES, DIAGNOSIS_LIST_ITEM_BYTES),
        &read_events,
    );
    let earlier = listed(
        &mut fit,
        "earlier",
        material.earlier,
        (DIAGNOSIS_LIST_BYTES, DIAGNOSIS_LIST_ITEM_BYTES),
        &read_events,
    );
    let receipt = fit.text(
        "receipt",
        material.receipt.unwrap_or("(no receipt yet)"),
        DIAGNOSIS_TEXT_BYTES,
        Keep::Start,
        "the run directory has receipt.json",
    );
    fit.section("receipt", &receipt);
    let text = format!(
        "You are dagq's replanning diagnosis (attempt {attempt}) of task {task_id} ({title}), its run {run_id}, for replan operation {operation} (origin {origin}): {reason}\n\
         Decide why the task takes long, from the evidence below, and choose what the runtime does next: continue (the run goes on with the same conditions and its existing revise and resume limits), replan (the runtime parks the run at a safe boundary, saves its work, and a planner proposes a replacement that keeps every acceptance item), or ask (a person decides).\n\
         Read only: the material below, and the files it names if you need more (the worktree is {worktree}, the run directory {run_dir}). Do not change any file and do not run commands; the runtime checks your verdict again and applies it.\n\
         Tell the causes apart: local_defect (a local slip, missing test, evidence or docs, the findings shrink), acceptance_unclear (the conditions are ambiguous, contradict a decision or cannot be met), oversized_unit (the task mixes independent conditions whose work can be separated), conflict (rebase or precheck conflicts), verification (e2e or verification failures, the gates themselves), environment (the provider, authentication, resources or the runtime). Do not choose replan for the number of turns alone, nor for conflicts, verification failures or the environment only: a smaller unit would meet them as well, and the runtime goes on instead. Choose replan when the same findings come back on independent conditions that can be split, or the unit is too big to finish; continue when the findings are local and the remaining work goes down, naming that progress; ask when the intent or the acceptance would change, work would be thrown away, or you are not sure.\n\n\
         Task description:\n{description}\n\n\
         Acceptance criteria:\n{acceptance}\n\n\
         Verification commands:\n{verification}\n\n\
         Turns of the task (all of its runs):\n{turns}\n\n\
         The detection's window: {window}\n\n\
         Reviews that did not pass since the last pass (newest first):\n{reviews}\n\n\
         Beside the reviews (conflicts, e2e and verification failures, failed turns; newest first):\n{outside}\n\n\
         Earlier diagnoses of the task (newest first):\n{earlier}\n\n\
         The run's latest receipt:\n{receipt}\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"choice\": \"continue\" | \"replan\" | \"ask\", \"causes\": [\"local_defect\" | \"acceptance_unclear\" | \"oversized_unit\" | \"conflict\" | \"verification\" | \"environment\", ...], \"confidence\": \"high\" | \"low\", \"reason\": string, \"progress\": string, \"remaining\": number, \"next_check\": string, \"question\": string}}\n\
         causes lists the primary cause first. reason says what you compared and why this choice. progress says what the last turns fixed and what is left; continue needs it. remaining is how many acceptance items are not met yet. next_check says what to check at the next diagnosis. question is for a person, when you ask. Only a high confidence is applied without a person.\n",
        attempt = material.attempt,
        task_id = task.id(),
        title = task.title(),
        run_id = material.run.id(),
        operation = op.id,
        origin = op.origin.as_str(),
        reason = op.reason,
        worktree = material.run.worktree_path().unwrap_or("none"),
        run_dir = material.run.run_dir().unwrap_or("none"),
        verification = fenced("sh", or_none(&verification)),
    );
    fit.finish(text)
}
