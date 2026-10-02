//! The worker's prompt (`prompt.txt`): the task, its goal, the summaries
//! of its landed predecessors and of the goals it waited for, the tasks running alongside it and what the
//! receipt must hold. Built from what the queue returned at claim time.
//! Also the initial prompts of the inbox session `up` opens and of the
//! planner sessions a person or the runtime opens, and what the supervisor asks of an agent: the headless review and
//! triage, and the requests it types into a live session (a resume, a
//! revise, a receipt that does not match).

use crate::domain::event_kind;
use crate::domain::follow_up::FOLLOW_UP_ASK_DEPTH;
use crate::domain::headless_job::JobAccess;
use crate::domain::resume::ResumeConfig;
use crate::domain::review_reason;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::{
    RunFiles, TaskListItem, fenced,
    integrate::{integrate_logs, log_names},
    or_none, tail,
};
use crate::domain::worker::WorkerMode;
use crate::domain::{
    Ask, BundleKey, CommitSha, DraftOrigin, DraftTarget, EvidenceCheck, FindingView, Goal, GoalId,
    GoalPredecessor, GoalTask, LintViolation, MAX_DRAFT_PLANNERS, MAX_FINDING_PLANNERS,
    MAX_PLAN_REVISES, MAX_RESUME_ATTEMPTS, MAX_REVISE_ATTEMPTS, Predecessor, Proposal, ProposalId,
    Provider, Receipt, RunEvent, RunId, RunStatus, TRIAGE_RETRY_FAILURES, Task, TaskDetail, TaskId,
    TaskRun,
    recovery::{ProcessInfo, RecoveryAlert},
    related::RelatedTask,
    required_of, resume,
    search::SearchHit,
    stats::conflicts::ConflictHotspot,
};

/// What the prompt says about one direct predecessor: the task, the squash
/// commit `integrate` put on `main` for it, and the summary its agent wrote.
#[derive(Debug, Clone, Serialize)]
pub struct PredecessorSummary {
    pub task_id: TaskId,
    pub title: String,
    /// `result_commit` of the integrated run; `(not landed)` without one.
    pub result_commit: String,
    /// `summary` of the integrated run's receipt, whitespace collapsed;
    /// `(receipt unavailable)` when the receipt cannot be read or parsed.
    pub summary: String,
}

impl PredecessorSummary {
    /// The receipt is read where the run left it after landing (its planned
    /// `receipt_path`, else `<run_dir>/receipt.json`); a missing or
    /// unreadable one is described, never an error, so the successor still starts.
    pub fn from_predecessor(files: &dyn RunFiles, predecessor: &Predecessor) -> Self {
        let run = predecessor.integrated_run.as_ref();
        let summary = run
            .and_then(|run| {
                run.receipt_path()
                    .map(PathBuf::from)
                    .or_else(|| run.run_dir().map(|dir| Path::new(dir).join("receipt.json")))
            })
            .and_then(|path| files.read_to_string(&path).ok())
            .and_then(|text| Receipt::parse(&text).ok())
            .map(|receipt| {
                receipt
                    .summary()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .map(|summary| {
                if summary.is_empty() {
                    "(no summary)".to_owned()
                } else {
                    summary
                }
            })
            .unwrap_or_else(|| "(receipt unavailable)".to_owned());
        Self {
            task_id: predecessor.task.id(),
            title: predecessor.task.title().to_owned(),
            result_commit: run
                .and_then(|run| run.result_commit())
                .map_or_else(|| "(not landed)".to_owned(), CommitSha::to_string),
            summary,
        }
    }
}

/// Characters of a receipt summary the prompt keeps for each task of a goal
/// the task depended on: a goal may hold many tasks, so each is a hint of
/// what landed, not the whole account.
pub const GOAL_TASK_SUMMARY_CHARS: usize = 200;

/// What the prompt says about one goal the task depended on (ADR-0038):
/// its title and its completed tasks, each summarized like a predecessor
/// with the summary cut to [`GOAL_TASK_SUMMARY_CHARS`].
#[derive(Debug, Clone, Serialize)]
pub struct GoalPredecessorSummary {
    pub goal_id: GoalId,
    pub title: String,
    pub tasks: Vec<PredecessorSummary>,
}

impl GoalPredecessorSummary {
    pub fn from_goal_predecessor(files: &dyn RunFiles, predecessor: &GoalPredecessor) -> Self {
        Self {
            goal_id: predecessor.goal.id(),
            title: predecessor.goal.title().to_owned(),
            tasks: predecessor
                .tasks
                .iter()
                .map(|task| {
                    let mut summary = PredecessorSummary::from_predecessor(files, task);
                    if let Some(cut) =
                        super::health::truncate(&summary.summary, GOAL_TASK_SUMMARY_CHARS)
                    {
                        summary.summary = cut;
                    }
                    summary
                })
                .collect(),
        }
    }
}

/// The run a retry carries over (ADR-0047 decision 24): its resumes were
/// used up on conflicts with main after its review passed, so the next run
/// of its task starts from its commit instead of from scratch.
#[derive(Debug, Clone, Serialize)]
pub struct Inheritance {
    pub run_id: RunId,
    /// The commit the run's own commits start after: its base, until the
    /// caller narrows it to the merge base of the head and the current main
    /// (a resume that rebased part of the way put main's commits under it).
    pub base: CommitSha,
    /// The run's head, kept under `refs/dagq/runs/<run-id>`.
    pub head: String,
    pub branch: Option<String>,
    pub receipt_path: Option<String>,
    /// Its receipt's summary, whitespace collapsed; `(receipt unavailable)`
    /// when it cannot be read.
    pub summary: String,
}

impl Inheritance {
    /// What the next run of `previous`'s task inherits, when `previous` was
    /// ended by the retry that carries its branch over
    /// ([`crate::domain::resume::retried_with_inheritance`]): the head that
    /// retry recorded, and the summary of its receipt.
    pub fn of(files: &dyn RunFiles, previous: &TaskRun, events: &[RunEvent]) -> Option<Self> {
        if !resume::retried_with_inheritance(events) {
            return None;
        }
        let inherit = &events
            .iter()
            .rev()
            .find(|e| resume::is_inherit_retry(e))?
            .payload["inherit"];
        let head = inherit["head"].as_str()?.to_owned();
        let receipt_path = previous.receipt_path().map(str::to_owned);
        let summary = receipt_path
            .as_deref()
            .and_then(|path| files.read_to_string(Path::new(path)).ok())
            .and_then(|text| Receipt::parse(&text).ok())
            .map(|receipt| {
                receipt
                    .summary()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|summary| !summary.is_empty())
            .unwrap_or_else(|| "(receipt unavailable)".to_owned());
        Some(Self {
            run_id: previous.id().clone(),
            base: previous.base_commit().clone(),
            head,
            branch: inherit["branch"].as_str().map(str::to_owned),
            receipt_path,
            summary,
        })
    }

    /// The prompt's section on it: start from its commit, bring it onto the
    /// current main, resolve the conflicts, verify and write the receipt.
    fn section(&self) -> String {
        format!(
            "Carried over from run {run}: its review passed, but its landing kept conflicting with the landing branch until its resumes were used up, so this run starts from its work instead of from scratch. \
             Its work is commit {head} (kept as refs/dagq/runs/{run}{branch}); its own commits are {base}..{head}. \
             Bring them onto your base, the current landing branch (for example `git cherry-pick {base}..{head}` in your worktree), resolve the conflicts keeping what both sides meant, rerun your checks in the worktree as above, and write the receipt for your own head. \
             Its receipt ({receipt}) summary: {summary}\n",
            run = self.run_id,
            head = self.head,
            base = self.base,
            branch = self
                .branch
                .as_deref()
                .map(|branch| format!(", branch {branch}"))
                .unwrap_or_default(),
            receipt = self.receipt_path.as_deref().unwrap_or("no receipt path"),
            summary = self.summary,
        )
    }
}

/// The other tasks a worker is told are executing alongside it: of the
/// `in_progress` tasks (ID order), those sharing the task's goal, or all of
/// them when the task has no goal; the task itself is never listed.
pub fn siblings_in_progress(task: &Task, in_progress: Vec<Task>) -> Vec<Task> {
    in_progress
        .into_iter()
        .filter(|other| other.id() != task.id())
        .filter(|other| task.goal_id().is_none() || other.goal_id() == task.goal_id())
        .collect()
}

/// The line in the worker prompt and the resume request that asks the
/// session to stop its own background work before the receipt: a leftover
/// background shell makes Claude Code answer the supervisor's `/exit` with a
/// confirmation screen, and the exit request times out.
/// Its last clause keeps a session from signalling by name or pattern: every
/// run session's command line holds its prompt, so `pkill -f llvm-cov` from
/// one worker ended the others' sessions and `integrate`'s checks (task 359).
pub const STOP_BACKGROUND: &str = "Before writing the receipt, stop every background process you started (run_in_background shells, wait loops, watches); if any is left, /exit stops at a confirmation screen. Stop only what you started, by its pid or task; never signal by name or pattern (pkill, killall, kill $(pgrep ...)), which also hits other runs' sessions and checks on this host.";

/// What a headless worker is told of its session (ADR-t813-1): each turn is
/// one non-interactive call that ends when the agent stops, and whatever
/// runs in the background then is stopped with it. It names no `/exit` and
/// no terminal: the process ends with the turn, and nobody types into it.
pub const HEADLESS_WORKER: &str = "This session is headless: each of your turns is one non-interactive call, and the turn ends when you stop. Do the whole task in this turn and end it by writing the receipt, or by an ask when you need a decision. Nobody types into this session: the answer to your ask, a review's request to revise, or a request to go on arrives as the prompt of your next turn, in the same session. Do not rely on background work: what still runs when the turn ends is stopped, so run builds, tests and waits in the foreground and wait for them to finish before you go on.\n";

/// What a worker the supervisor gave the resource broker's tools is told
/// (`preferred`, ADR-t827-4 decision 1): prefer them, and fall back to the
/// built-in tools when the broker refuses or cannot be reached. It names
/// no token: the client reads its file.
pub const BROKER_TOOLS: &str = "The resource broker's tools are available as the MCP server `dagq-broker` (`mcp__dagq-broker__read_file`, `list_dir`, `write_file`, `edit_file`, `exec`, `git_status`, `git_diff`, `git_log`, `git_show`, `git_add`, `git_commit`, `git_restore`). Prefer them for reading, writing and editing files, for the commands the broker allows, and for Git on your run branch; paths are relative to the worktree. The broker refuses paths outside the worktree, `.git`, pushes and commands it does not allow; when it refuses or cannot be reached, use the built-in tools instead.\n";

/// [`STOP_BACKGROUND`] for a headless session: nothing waits for an `/exit`,
/// but a process the agent detached outlives its turn (the spike measured
/// Claude's `nohup ... &`), and the signalling rule is the same.
pub const HEADLESS_STOP: &str = "Before you end the turn, stop every process you started that still runs (a detached `nohup ... &` outlives the turn). Stop only what you started, by its pid; never signal by name or pattern (pkill, killall, kill $(pgrep ...)), which also hits other runs' sessions and checks on this host.";

/// What a worker checks before a worker_question (task 978): the runtime
/// holds none of the repository's rules, so it points at them; a rule
/// there may make the question the worker's own or a failed receipt.
macro_rules! ask_rules_first {
    () => {
        "Before you ask, check whether the repository's instructions (AGENTS.md or CLAUDE.md) say to decide that kind of question yourself or to write a failed receipt instead of asking; when they do, follow them and do not ask."
    };
}

/// [`ask_rules_first`] as a value, for the prompts built with `format!`.
pub const ASK_RULES_FIRST: &str = ask_rules_first!();

/// The last step of a request to a headless session: the turn is its reply.
const HEADLESS_DONE: &str = concat!(
    "Do not merge or push. Follow the repository's instructions for a worker (AGENTS.md or CLAUDE.md) as before. Do all of this in this turn. ",
    ask_rules_first!(),
    " If you need a decision, run `dagq ask --run <run> --kind worker_question --because <scope|discard> --topic <code> --question '...'` and end the turn: the answer comes as the prompt of your next turn. When done, report briefly and end the turn."
);

/// The last step of a request to an interactive session.
const INTERACTIVE_DONE: &str =
    "Do not merge or push. When done, report briefly and stop; do not run /exit.";

/// What a headless request opens with: the prompt of a resume reads as the
/// next turn of the same session.
const HEADLESS_NEXT_TURN: &str = "dagq: this is the next turn of your session; your previous turn has ended, and anything it left running in the background was stopped.";

/// How a run's worker takes what the runtime tells it (ADR-t813-1): an
/// interactive session typed into in its terminal, or a headless one whose
/// every request is the prompt of its next turn, on its provider. The
/// interactive texts are the ones the runtime has always sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    Interactive,
    Headless(Provider),
}

impl Route {
    /// The route of `run`'s worker: its mode, on the provider it runs on
    /// now (a fallback may have changed it).
    pub(crate) fn of(run: &TaskRun) -> Self {
        match run.worker_mode() {
            WorkerMode::Interactive => Self::Interactive,
            WorkerMode::Headless => Self::Headless(run.actual_provider()),
        }
    }

    fn headless(self) -> bool {
        matches!(self, Self::Headless(_))
    }

    /// The line that asks the session to stop its own processes.
    fn stop(self) -> &'static str {
        if self.headless() {
            HEADLESS_STOP
        } else {
            STOP_BACKGROUND
        }
    }

    /// The last step of a request to `run`'s session.
    fn done(self, run: &TaskRun) -> String {
        if self.headless() {
            HEADLESS_DONE.replace("<run>", run.id().as_str())
        } else {
            INTERACTIVE_DONE.to_owned()
        }
    }

    /// `first`, the opening line of a request, after the headless opening.
    fn opening(self, first: String) -> Vec<String> {
        if self.headless() {
            vec![HEADLESS_NEXT_TURN.to_owned(), first]
        } else {
            vec![first]
        }
    }
}

/// What a headless worker on `provider` is told of its provider (the
/// spike's measures): Claude Code stops a turn's background shells when
/// the turn ends, and Codex stops a command still running when the model
/// answers. A Codex worker is also told to keep its temporary files under
/// the `TMPDIR` the runtime gives its turns (task 1290): Claude Code's own
/// scratchpad is cleaned after the run (task 1100), what a Codex worker
/// made in `/tmp` was not.
fn headless_provider_line(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => {
            "Claude Code stops the background shells of a turn when the turn ends: do not start a build, a test or a wait with run_in_background and end the turn to wait for it.\n"
        }
        Provider::Codex => {
            "Wait for each command to finish before you answer: a command still running when you answer is stopped with the turn.\n\
             Put temporary files, throwaway repositories and any other CARGO_TARGET_DIR under $TMPDIR (a directory the runtime made for this run and removes after it), never directly in /tmp or /private/tmp; build in the worktree's own target/ (cargo's default).\n"
        }
    }
}

/// What the worker is told of the subagent review. A Codex worker has no
/// subagent (and a nested `codex exec review` could not write its session
/// under the sandbox), so it reviews its own diff and reports the check
/// `not_applicable`; its run does not need the evidence
/// ([`crate::domain::required_of`]), and the supervisor's review job reviews
/// the commit before it lands.
fn review_line(route: Route) -> &'static str {
    match route {
        Route::Headless(Provider::Codex) => {
            "Perform applicable unit tests. You have no subagent to review your change: before the receipt, read your own diff (git diff <base commit>..HEAD) against the acceptance criteria and fix what you find, then write subagent_review as not_applicable with the reason `codex worker: no subagent review; self-reviewed the diff, the supervisor's review job reviews the commit` and what the self-review found. Record evidence or an explicit reason when not applicable.\n"
        }
        _ => {
            "Perform applicable unit tests and subagent review. Record evidence or an explicit reason when not applicable.\n"
        }
    }
}

/// What follows a text a headless session reads as its next turn's prompt
/// (an answer, a recovery job's instruction): go on in this turn.
const HEADLESS_GO_ON: &str = "(dagq: this is the prompt of the next turn of your session; your previous turn has ended. Go on with the task from it in this turn, and end the turn with the receipt, or with an ask when you need a decision.)";

/// `text` for `run`'s session: as it is for an interactive one, followed
/// by [`HEADLESS_GO_ON`] for a headless one. The text stays first, so its
/// own first line still says what it is.
fn to_session(run: &TaskRun, text: String) -> String {
    if Route::of(run).headless() {
        format!("{text}\n\n{HEADLESS_GO_ON}")
    } else {
        text
    }
}

/// The text that tells a session held by a login or a usage limit to go
/// on, once a person answered the hold's ask `done`.
pub(crate) fn continue_text(run: &TaskRun) -> String {
    to_session(run, crate::domain::queue_hold::CONTINUE_TEXT.to_owned())
}

/// The text that carries a person's answer to the session that asked.
pub(crate) fn answer_text(run: &TaskRun, ask: impl std::fmt::Display, answer: &str) -> String {
    to_session(run, format!("answer to ask {ask}: {answer}"))
}

/// The text that carries a recovery job's `send_instruction` to the
/// session.
pub(crate) fn recovery_instruction(run: &TaskRun, alert: &str, instruction: &str) -> String {
    to_session(
        run,
        format!(
            "dagq: the supervisor's recovery job for run {} (alert {alert}) asks: {}",
            run.id(),
            instruction.trim()
        ),
    )
}

/// What a worker reads before it starts, and nothing more: everything else
/// about its run is in the prompt, and reading the queue or the whole docs
/// tree only delays the first commit (goal 11, decision 4).
pub const WORKER_READING: &str = "Read first, and only: the worker section of the repository instructions (AGENTS.md or CLAUDE.md), the task context below and the documents it names, the goal doc if there is one, and the predecessor summaries below. \
Do not run `dagq list` or `dagq show`, and skip the rest of the docs tree; open other files only when the task needs them.\n";

/// Which checks a session runs in its worktree before the receipt, given
/// how it names the task's verification commands (`above`, or the JSON
/// list in a one-line request). The verification of record for a commit is
/// integrate's single run of the verification commands after its rebase
/// (ADR-0049 decision 1), so the worker runs what the repository's own
/// instructions ask of it (they may leave a slow gate such as a coverage
/// run to integrate), and the verification commands only when the
/// repository says nothing. The runtime names no tool here: dagq runs in
/// any repository.
pub(crate) fn local_checks(verify: &str) -> String {
    format!(
        "Run in the worktree the checks the repository's instructions (AGENTS.md or CLAUDE.md) ask a worker to run, which may leave some of the verification commands to integrate; when the instructions name no such checks, run the verification commands {verify}."
    )
}

/// Text of `prompt.txt`. `goal` is the task's goal as it reads at claim
/// time, `predecessors` the task's direct dependencies, `goal_predecessors`
/// the goals it depends on (in the Predecessor section) and `siblings` the
/// other tasks executing at claim time (`siblings_in_progress`), and
/// `inherited` the run a retry carries over, if any. The Goal,
/// Context, Predecessor and Sibling sections are always present, `none`
/// when empty, so the prompt keeps one shape whether or not a task has a
/// goal, a context, dependencies or company.
#[allow(clippy::too_many_arguments)]
pub fn prompt(
    task: &Task,
    run: &TaskRun,
    goal: Option<&Goal>,
    predecessors: &[PredecessorSummary],
    goal_predecessors: &[GoalPredecessorSummary],
    siblings: &[Task],
    inherited: Option<&Inheritance>,
    e2e_paths: &[String],
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let inherited = inherited.map(Inheritance::section).unwrap_or_default();
    let goal = match goal {
        None => "Goal: none, this task stands alone\n".to_owned(),
        Some(goal) => format!(
            "Goal (the higher-level problem this task and its sibling tasks solve together):\n\
             Goal ID: {id}\nGoal title: {title}\nGoal description:\n{description}\n\
             Goal acceptance:\n{acceptance}\nGoal constraints:\n{constraints}\n\
             Goal doc: {doc}\n",
            id = goal.id(),
            title = goal.title(),
            description = goal.description(),
            acceptance = goal.acceptance(),
            constraints = goal.constraints(),
            doc = goal
                .doc()
                .map(|doc| format!(
                    "{doc} (a path in the repository; read it for the full picture)"
                ))
                .unwrap_or_else(|| "none".to_owned()),
        ),
    };
    let context = if task.context().trim().is_empty() {
        "Context: none\n".to_owned()
    } else {
        format!(
            "Context (why this task exists and what to read first):\n{}\n",
            task.context()
        )
    };
    let predecessors = if predecessors.is_empty() && goal_predecessors.is_empty() {
        "Predecessor tasks: none\n".to_owned()
    } else {
        let mut text =
            "Predecessor tasks (their changes are already in your base commit):\n".to_owned();
        for predecessor in predecessors {
            text.push_str(&format!(
                "- task {}: {}; result commit {}; summary: {}\n",
                predecessor.task_id,
                predecessor.title,
                predecessor.result_commit,
                predecessor.summary
            ));
        }
        for goal in goal_predecessors {
            text.push_str(&format!(
                "- goal {} (closed as achieved): {}; its completed tasks:\n",
                goal.goal_id, goal.title
            ));
            if goal.tasks.is_empty() {
                text.push_str("  - none\n");
            }
            for task in &goal.tasks {
                text.push_str(&format!(
                    "  - task {}: {}; result commit {}; summary: {}\n",
                    task.task_id, task.title, task.result_commit, task.summary
                ));
            }
        }
        text
    };
    let siblings = if siblings.is_empty() {
        "Sibling tasks in progress: none\n".to_owned()
    } else {
        let mut text =
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n"
                .to_owned();
        for other in siblings {
            text.push_str(&format!("- task {}: {}\n", other.id(), other.title()));
        }
        text
    };
    let route = Route::of(run);
    // Known up front, so the receipt carries it (ADR-0019 decision 5): what
    // this run's provider must back.
    let required = required_of(task.required_evidence(), run.actual_provider());
    let mut evidence = if required.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = required.iter().map(|c| c.as_str()).collect();
        format!(
            "Required evidence: {} (each must be passed with evidence in the receipt, or the run waits for a session to add it)\n",
            names.join(", ")
        )
    };
    evidence.push_str(&e2e_line(task, e2e_paths));
    // The declared scope (ADR-0029): changing anything else parks the run.
    let paths = if task.paths().is_empty() {
        String::new()
    } else {
        format!(
            "Paths you may change (globs from the repository root; `*` stays in one directory, `**` spans any depth): {}. A commit that changes any other path is not accepted: the run waits for a session to take it out. If the task needs another path, do not change it and do not run dagq ask: write the receipt with result failed and name in summary the paths it needs and what to change there (a running task's paths cannot change; the planner registers it again with wider paths).\n",
            task.paths().join(", ")
        )
    };
    // How the session ends a step and hears back: an interactive session
    // stops and is typed into; a headless one ends its turn and reads the
    // next turn's prompt.
    let (headless, answer_arrives, after_submitting) = match route {
        Route::Interactive => (
            String::new(),
            "The answer arrives in this terminal as `answer to ask <id>: ...`; continue from it.",
            "After submitting, report the outcome briefly and stop; do not run /exit yourself. Once you are idle the supervisor ends the session, and a person can still send /exit. A receipt does not itself end the session.",
        ),
        Route::Headless(provider) => (
            format!("{HEADLESS_WORKER}{}", headless_provider_line(provider)),
            "The answer arrives as the prompt of your next turn in this same session, as `answer to ask <id>: ...`; continue from it.",
            "After writing the receipt, report the outcome briefly and end the turn. A later turn comes only if a review, a landing or a person sends the run back.",
        ),
    };
    let (stop_word, dont_wait) = if route.headless() {
        (
            "end the turn",
            "end the turn with the question in your reply",
        )
    } else {
        ("stop", "write the question to the terminal and wait")
    };
    Ok(format!(
        "You are executing dagq task {task_id}, run {run_id}.\n\
         Work only in the assigned Git worktree.\n\
         {reading}\
         Implement the task, run the checks described below, and commit the result.\n\
         Do not merge, push, close the workspace, or modify the queue/runtime files.\n\
         {review}\
         Task title: {title}\nDescription:\n{description}\nAcceptance criteria:\n{acceptance}\n\
         Verification commands (integrate runs them once after rebasing onto main; that run is the verification of record for the commit):\n{verification}\n\
         {local_checks}\n\
         {evidence}{paths}{goal}{context}{predecessors}{siblings}{inherited}\
         Your assignment is this task only. Do not change what a sibling task owns; if you find work outside this task, record it in the receipt as follow_ups instead of doing it.\n\
         Write a completion receipt to {receipt} using a temporary file in the same directory and atomic rename.\n\
         Receipt JSON: {{\"run_id\":\"{run_id}\",\"result\":\"succeeded or failed\",\"commit\":\"full Git SHA of the branch head\",\"tests\":{{\"status\":\"passed, failed or not_applicable\",\"evidence_or_reason\":\"...\"}},\"e2e\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"subagent_review\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"summary\":\"...\",\"follow_ups\":[{{\"title\":\"...\",\"description\":\"...\",\"category\":\"...\"}}]}}\n\
         Each of tests, e2e and subagent_review needs evidence when passed and a reason when not_applicable.\n\
         follow_ups is optional: an array of work you found outside this task, each with a title, a description and a category, for the planner to decide on; omit it when there is none. {categories}\n\
         You may write this receipt outside the worktree. Keep the worktree clean after committing.\n\
         The supervisor rejects the run unless the commit is the clean head of your branch on top of the base commit, and integrate runs the verification commands itself after rebasing onto main.\n\
         {ask_rules_first} When you need a decision you cannot make from the task and the repository, do not {dont_wait}: run `dagq ask --run {run_id} --kind worker_question --because scope --topic <code> --question '...'` in the worktree (one ask at a time, with everything you need decided in its question), report briefly that you asked, and {stop_word}. `--because` says why a person is needed: `scope` (the acceptance or the scope changes) or `discard` (whether to throw work away); a question that fits neither is yours to decide and record in the receipt's summary, or, when it leads outside the task, a failed receipt saying why. {topics} {answer_arrives}\n\
         {stop_background}\n\
         {after_submitting}\n\
         {headless}",
        task_id = task.id(),
        run_id = run.id(),
        reading = WORKER_READING,
        stop_background = route.stop(),
        review = review_line(route),
        title = task.title(),
        description = task.description(),
        acceptance = task.acceptance(),
        verification = serde_json::to_string_pretty(&task.verification_commands())?,
        local_checks = local_checks("above"),
        categories = follow_up_categories_line(),
        topics = worker_question_topics_line(),
        ask_rules_first = ASK_RULES_FIRST,
    ))
}

/// What the worker's prompt says of the e2e (ADR-t1233-2 decision 1): the
/// runtime runs it on the host after the review passes, so the worker does
/// not. Only when the run may need it (the task asks for it, or the
/// repository names `[e2e] paths` its diff may touch); empty otherwise.
fn e2e_line(task: &Task, e2e_paths: &[String]) -> String {
    if e2e_paths.is_empty() && !task.required_evidence().contains(&EvidenceCheck::E2e) {
        return String::new();
    }
    "E2E: do not run the e2e (tests/e2e.rs) yourself. When the run needs it (the task asks for it, or the diff touches the repository's dagq.toml [e2e] paths), the runtime runs it on the host after the review passes, before the run lands, and sends the run back to a session if it fails. Report `e2e` in the receipt as not_applicable with that reason.\n".to_owned()
}

/// What the worker's prompt says of a worker_question's `--topic`
/// (ADR-t947-2): the codes, and how to choose the primary one.
pub fn worker_question_topics_line() -> String {
    let list: Vec<String> = crate::domain::WORKER_QUESTION_TOPICS
        .iter()
        .map(|(code, meaning)| format!("{code} ({meaning})"))
        .collect();
    format!(
        "`--topic` says what is left undecided: give the primary code first (what stopped you first; the earlier in this list when two came at once), then with more `--topic` any code that must be decided with it, from: {}.",
        list.join("; ")
    )
}

/// What the worker's prompt says of a follow_up's `category` (ADR-t947-3):
/// the list, and how to choose.
pub fn follow_up_categories_line() -> String {
    let list: Vec<String> = crate::domain::FOLLOW_UP_CATEGORIES
        .iter()
        .map(|(code, meaning)| format!("{code} ({meaning})"))
        .collect();
    format!(
        "Give each one category, one of: {}. When unsure, choose by what finishing it changes; whether it duplicates another task is not a category.",
        list.join("; ")
    )
}

/// The initial prompt of the inbox session that `up` opens in the
/// `[<repo>]inbox` workspace (ADR-0022): it relays each open ask to a person
/// and writes the person's answer back, deciding nothing itself. Every
/// other attention is the inbox's too (ADR-0024 decision 6): it reports it
/// and does only what the person says.
pub fn inbox_prompt(db: &Path) -> Result<String> {
    Ok(format!(
        "You are the inbox of the dagq queue at {db}: you relay its asks and attention to a person and never decide anything yourself.\n\
         Start with `dagq status --role inbox` and follow the dagq-inbox skill of the dagq plugin: run `dagq watch --role inbox --after <cursor>` in the background, wake when it returns and watch again from the cursor it returns.\n\
         On ask_opened, read the ask with `dagq asks --open --role inbox`, show the person its question and options (use AskUserQuestion when it is available), then write the person's answer with `dagq answer ID --text '<answer>'`. Report any other attention (an answered ask, a stopped supervisor, a failed review or triage) to the person and do only what they say, as the skill describes.\n\
         Never open the queue database directly; use the dagq CLI only.\n",
        db = super::path_text(db)?,
    ))
}

/// Where a planner takes a task's verification commands, declared paths
/// and required evidence from. The runtime has no such rules of its own:
/// they are the repository's, and a repository without an AGENTS.md still
/// shows them in its CLAUDE.md, README, CI and build configuration. `ask`
/// is the last step, when none of them settles it: the person in the
/// session, or a `planner_question` ask for a planner the runtime opened.
pub(crate) fn repository_rules(ask: &str) -> String {
    format!(
        "Take a task's verification commands (`--verify`), declared paths (`--paths`) and required evidence (`--evidence`) from the repository's instructions and the documents and rules they name, in this order: its AGENTS.md; without one, its CLAUDE.md; without either, what its README, CI configuration and build configuration show; when none of them settles it, {ask}."
    )
}

/// The last step of [`repository_rules`] for a planner the runtime opens:
/// it decides from the rest of its material as the Basic policy says
/// (ADR-t451-1 decision 5), and asks the inbox, since no person watches
/// it, only what that leaves to a person or to a low confidence.
const RUNTIME_PLANNER_ASK: &str = "decide them yourself from the source, the decisions the repository records and a person's precedents, and ask a person with a `planner_question` ask as below only when that material cannot settle them and the decision is a person's (`scope` or `discard`) or your confidence in it is low";

/// The initial prompt of a planner session a person opens with `dagq plan`
/// (ADR-0041 decisions 1, 6): it turns the person's problems into goals and
/// tasks, hands them over as the skill says (a proposal for plan review),
/// and closes a goal once its tasks meet the acceptance.
pub fn planner_prompt(db: &Path) -> Result<String> {
    Ok(format!(
        "You are a planner of the dagq queue at {db}: listen to the person's problems and turn them into goals and tasks.\n\
         Follow the dagq-planner skill of the dagq plugin: register them and submit them for plan review as its dagq skill describes. You do not land runs or answer asks.\n\
         {rules}\n\
         When every task of a goal is completed, check their receipts against the goal's acceptance and close the goal (`dagq goal close ID --verdict achieved`).\n\
         Never open the queue database directly; use the dagq CLI only.\n",
        db = super::path_text(db)?,
        rules = repository_rules("ask the person"),
    ))
}

/// The CLI that reads the queue's record (ADR-0044 decision 22), named by
/// the prompts of the planners the runtime opens and of the plan review so
/// they read evidence from the events rather than from prose.
pub const RECORD_READING: &str = "To see what happened, read the record rather than prose: \
`dagq events --full --task ID` gives a task's events with their run_id and whole payload, narrowed by `--run ID`, `--goal ID`, `--kind KIND` (repeatable), `--since TIME` and `--until TIME` (UTC, YYYY-MM-DD or YYYY-MM-DDTHH:MM:SSZ); \
without `--kind` it lists attention events only, so add `--all` for every kind; it gives the oldest 100 first, so page on with `--after <cursor>` or narrow with `--since`. \
`dagq timeline RUN` gives a run's events oldest first with each gap and its reason (idle, waiting_ask, background, after_receipt, ...).";

/// The initial prompt of a planner the runtime opens for a proposal plan
/// review sent back while its own planner was closed (ADR-0041 decision
/// 12): the proposal, its tasks, and the reasons to fix. No person watches
/// the session, so what needs one goes to the inbox as an ask (decision 13).
pub fn runtime_planner_prompt(
    db: &Path,
    proposal: ProposalId,
    tasks: &[Task],
    reasons: &[String],
) -> Result<String> {
    let tasks = if tasks.is_empty() {
        "(none)".to_owned()
    } else {
        tasks
            .iter()
            .map(|task| {
                format!(
                    "- task {} ({}): {}",
                    task.id(),
                    task.status().as_str(),
                    task.title()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let reasons = if reasons.is_empty() {
        "(none given)".to_owned()
    } else {
        reasons
            .iter()
            .map(|reason| format!("- {reason}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(format!(
        "You are a planner the dagq runtime opened for proposal {proposal} of the queue at {db}; no person watches this session.\n\
         Plan review sent the proposal back. Its reasons:\n{reasons}\n\
         Its tasks:\n{tasks}\n\
         Follow the dagq-planner skill of the dagq plugin: read the proposal with `dagq proposal show {proposal}` and each task with `dagq show ID`, fix what the reasons point at, and submit it again with `dagq submit --proposal {proposal}`.\n\
         {RECORD_READING}\n\
         {rules}\n\
         A fix that changes the plan's intent (acceptance, scope, the relation to the goal) needs a person: raise it to the inbox with `dagq ask --task ID --kind planner_question --because scope` as the skill describes, stop, and continue from the answer typed into this terminal.\n\
         Never open the queue database directly; use the dagq CLI only.\n",
        db = super::path_text(db)?,
        rules = repository_rules(RUNTIME_PLANNER_ASK),
    ))
}

/// What the initial prompt of a planner the runtime opens for a bundle of
/// drafts (ADR-0041 decision 16, bundled by ADR-t807-1) shows it: the
/// drafts and where they came from, the source task and its landed receipt
/// for follow_ups (once), the goals and their other tasks, and the answer
/// of a `planner_question` it carries when the planner that asked is gone.
pub struct DraftPlannerMaterial<'a> {
    pub db: &'a Path,
    /// What made the drafts one bundle.
    pub key: &'a BundleKey,
    /// The drafts, oldest first, each with which planner of the runtime's
    /// this is for it (1-based).
    pub members: &'a [(DraftTarget, usize)],
    /// The task whose run's receipt proposed the follow_ups.
    pub source: Option<&'a Task>,
    /// That run's landed receipt.
    pub receipt: Option<&'a Value>,
    /// The drafts' goals, each with whether it is closed and its tasks
    /// other than the bundle's.
    pub goals: &'a [(Goal, bool, Vec<GoalTask>)],
    pub answer: Option<&'a Ask>,
}

/// The line of a follow_up draft's section that shows the category its
/// worker gave it (ADR-t947-3), with what the category means when it is
/// one of the list; empty for a draft of another origin.
fn follow_up_category_line(target: &DraftTarget) -> String {
    if target.origin != DraftOrigin::FollowUp {
        return String::new();
    }
    let category = crate::domain::follow_up_category(&target.material);
    let meaning = crate::domain::FOLLOW_UP_CATEGORIES
        .iter()
        .find(|(code, _)| *code == category)
        .map_or_else(
            || {
                if category == crate::domain::UNLABELED_CATEGORY {
                    " (the worker gave none)".to_owned()
                } else {
                    " (not one of the runtime's categories)".to_owned()
                }
            },
            |(_, meaning)| format!(": {meaning}"),
        );
    format!(
        "\nCategory (the worker's; keep it as it is, and judge the draft on its merits): {category}{meaning}\n"
    )
}

/// The Basic policy of the dagq-planner skill (ADR-t451-1 decision 5) as
/// the planners the runtime opens for a draft or a finding read it: what
/// they can recommend they decide themselves and record why; only what
/// they cannot settle goes to a person.
const DECIDE_YOURSELF: &str = "decide what you can recommend yourself and go on, asking no one, and leave why in the record (a task's `--context`, a `note`, a `--reason`); raise to a person, with your recommendation and its confidence, only what step Ask below names.";

/// The initial prompt of a planner the runtime opens for a bundle of drafts
/// the runtime or a job registered (ADR-0041 decision 16, ADR-t807-1): the
/// material, and the three things it may do with each draft — submit it
/// completed (adopt), cancel it with a note (drop), or ask the inbox a
/// `planner_question` and apply the answer typed into its terminal — and,
/// for a bundle of more than one, what to weigh between its drafts.
pub fn draft_planner_prompt(material: &DraftPlannerMaterial<'_>) -> Result<String> {
    let (first, _) = material
        .members
        .first()
        .context("a bundle of drafts has at least one")?;
    let origin = first.origin;
    let single = material.members.len() == 1;
    let ids: Vec<String> = material
        .members
        .iter()
        .map(|(target, _)| target.task.id().to_string())
        .collect();
    // The ID the commands name: the draft's, or a placeholder for each.
    let t = if single {
        ids[0].clone()
    } else {
        "ID".to_owned()
    };
    let whence = match origin {
        DraftOrigin::Reopened => {
            "plan review took it back from ready and the proposal it was reopened into was withdrawn"
        }
        DraftOrigin::FollowUp | DraftOrigin::GoalGap => "the runtime or a job registered it",
    };
    let mut out = if single {
        format!(
            "You are a planner the dagq runtime opened for draft task {id} of the queue at {db}; no person watches this session. The {origin} draft is not ready: {whence}, and you decide what becomes of it (planner {attempt} of at most {max} the runtime opens for it).\n",
            id = ids[0],
            db = super::path_text(material.db)?,
            origin = origin.as_str(),
            attempt = material.members[0].1,
            max = MAX_DRAFT_PLANNERS,
        )
    } else {
        format!(
            "You are a planner the dagq runtime opened for draft tasks {list} of the queue at {db}; no person watches this session. The {n} {origin} drafts are one bundle: the same piece of work made them ({kind} {value}). None is ready: {whence}, and you decide what becomes of each (the runtime opens at most {max} planners for a draft).\n",
            list = ids.join(", "),
            n = ids.len(),
            db = super::path_text(material.db)?,
            origin = origin.as_str(),
            kind = material.key.kind.as_str(),
            value = material.key.value,
            max = MAX_DRAFT_PLANNERS,
        )
    };
    for (target, attempt) in material.members {
        let task = &target.task;
        let heading = if single {
            "## The draft".to_owned()
        } else {
            format!("## Draft {} (planner {attempt} for it)", task.id())
        };
        out.push_str(&format!(
            "\n{heading}\n\nTask {id}: {title}\n{category}\n### Description\n\n{description}\n\n### Context\n\n{context}\n",
            id = task.id(),
            title = task.title(),
            category = follow_up_category_line(target),
            description = or_none(task.description()),
            context = or_none(task.context()),
        ));
    }
    out.push_str(&format!("\n## Where it came from: {}\n\n", origin.as_str()));
    match origin {
        DraftOrigin::FollowUp => {
            out.push_str(&format!(
                "The receipt of run {run} of task {source} proposed {what} as a follow_up: work its worker found outside that task.\n",
                run = first.material["source_run_id"].as_str().unwrap_or("(unknown)"),
                source = first.material["source_task_id"],
                what = if single {
                    "it".to_owned()
                } else {
                    format!(
                        "them ({})",
                        material
                            .members
                            .iter()
                            .map(|(t, _)| format!("task {} is its follow_up {}", t.task.id(), t.material["index"]))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                },
            ));
            if let Some(source) = material.source {
                out.push_str(&format!(
                    "\n### Source task {sid}: {title} ({status})\n\n{description}\n\nAcceptance:\n{acceptance}\n\nVerification: {verify}\nPaths: {paths}\nEvidence: {evidence}\n",
                    sid = source.id(),
                    title = source.title(),
                    status = source.status().as_str(),
                    description = or_none(source.description()),
                    acceptance = or_none(source.acceptance()),
                    verify = list_or_none(source.verification_commands()),
                    paths = list_or_none(source.paths()),
                    evidence = list_or_none(
                        &source
                            .required_evidence()
                            .iter()
                            .map(|check| check.as_str().to_owned())
                            .collect::<Vec<_>>()
                    ),
                ));
            }
            if let Some(receipt) = material.receipt {
                out.push_str(&format!(
                    "\n### The landed receipt\n\nSummary:\n{summary}\n\nIts follow_ups:\n{follow_ups}",
                    summary = or_none(receipt["summary"].as_str().unwrap_or_default()),
                    follow_ups = fenced(
                        "json",
                        &serde_json::to_string_pretty(&receipt["follow_ups"])?
                    ),
                ));
            }
        }
        DraftOrigin::Reopened => {
            for (target, _) in material.members {
                out.push_str(&format!(
                    "{it} was a ready task. The plan review of proposal {reviewed} found that it has to change and reopened it into proposal {proposal}, which was then withdrawn, so it returned to draft. The reason plan review gave:\n\n{reason}\n",
                    it = if single {
                        "It".to_owned()
                    } else {
                        format!("Task {}", target.task.id())
                    },
                    reviewed = target.material["reviewed_proposal_id"],
                    proposal = target.material["proposal_id"],
                    reason = or_none(target.material["reason"].as_str().unwrap_or_default()),
                ));
            }
        }
        DraftOrigin::GoalGap => {
            out.push_str(if single {
                "A job that judged the goal below against its acceptance found this gap. Its findings:\n"
            } else {
                "A job that judged the goal below against its acceptance found these gaps. The findings of each draft:\n"
            });
            for (target, _) in material.members {
                if !single {
                    out.push_str(&format!("\nTask {}:\n", target.task.id()));
                }
                out.push_str(&fenced(
                    "json",
                    &serde_json::to_string_pretty(&target.material)?,
                ));
            }
        }
    }
    for (goal, closed, siblings) in material.goals {
        out.push_str(&format!(
            "\n## Goal {gid}: {title}{closed}\n\n{description}\n\nAcceptance:\n{acceptance}\n\nConstraints:\n{constraints}\n\nDoc: {doc}\n\nIts other tasks:\n{tasks}\n",
            gid = goal.id(),
            title = goal.title(),
            closed = if *closed { " (closed)" } else { "" },
            description = or_none(goal.description()),
            acceptance = or_none(goal.acceptance()),
            constraints = or_none(goal.constraints()),
            doc = goal.doc().unwrap_or("(none)"),
            tasks = if siblings.is_empty() {
                "(none)".to_owned()
            } else {
                siblings
                    .iter()
                    .map(|t| format!("- task {} ({}): {}", t.id, t.status.as_str(), t.title))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
        ));
    }
    let goalless: Vec<String> = material
        .members
        .iter()
        .filter(|(target, _)| target.task.goal_id().is_none())
        .map(|(target, _)| target.task.id().to_string())
        .collect();
    if !goalless.is_empty() {
        out.push_str(&if single {
            "\n## Goal\n\nThe draft belongs to no goal (the source's goal was closed, or it had none).\n".to_owned()
        } else {
            format!(
                "\n## Goal\n\nDraft {} belongs to no goal (the source's goal was closed, or it had none).\n",
                goalless.join(", ")
            )
        });
    }
    let (goal_of_first, run, source) = (
        first
            .task
            .goal_id()
            .map_or("?".to_owned(), |g| g.to_string()),
        first.material["source_run_id"].as_str().unwrap_or("?"),
        &first.material["source_task_id"],
    );
    let adopt = match origin {
        DraftOrigin::FollowUp => format!(
            "complete the draft with `dagq edit {t}` (acceptance, `--verify`, `--paths`, `--evidence`, and `--context` beginning with `follow-up draft (proposed by the receipt of run {run} of task {source})`),"
        ),
        DraftOrigin::GoalGap => format!(
            "complete the draft with `dagq edit {t}` (acceptance, `--verify`, `--paths`, `--evidence`, and `--context` beginning with `goal gap draft (proposed by the judgment of goal {goal_of_first})`),"
        ),
        DraftOrigin::Reopened => format!(
            "fix what plan review's reason points at with `dagq edit {t}` (its description, acceptance, `--verify`, `--paths`, `--evidence`, and a line in `--context` on the reopen of proposal {}), keeping the task's intent,",
            first.material["proposal_id"],
        ),
    };
    out.push_str(&format!(
        "\n## What to do\n\n\
         Follow the dagq-planner skill of the dagq plugin, its Basic policy above all: {DECIDE_YOURSELF} {rules} Look for tasks that already cover the {drafts} or code that already does {it} (`dagq search '<words>'`, `dagq show ID`, the source) before you decide. {RECORD_READING}\n\
         {each}Then do exactly one of these three{with_each}:\n\
         1. Adopt: {adopt} add its dependencies with `dagq dependency add`, check it with `dagq lint {t}` and submit it with `dagq submit {t}`. Say in its `--context` why you adopted it. Plan review checks it before it becomes ready.\n\
         2. Drop: when it is already done, duplicated or not worth doing, cancel it with `dagq cancel {t}` and record why with `dagq note --task {t} --text '<why>'`. When another task already covers it (a duplicate, or a completed task that already did it), cancel it with `dagq cancel {t} --duplicate-of <that task>` instead, so the queue records which task it duplicates, and still note why.\n\
         3. Ask: only for a draft you cannot decide yourself: (a) it needs a person's judgement, `scope` (the acceptance, the scope or a goal's decision would change with their intent) or `discard` (whether to throw work away), that the queue, the repository (its code and the decisions it records) and a person's precedents cannot settle; (b) your confidence in the decision is low; or (c) it is a follow_up draft past the runtime's follow_up limit, {FOLLOW_UP_ASK_DEPTH} or more follow-ups from a person's judgement or with no goal or a closed goal. Run `dagq ask --task {t} --kind planner_question --because scope --recommend <adopt|cancel|keep_draft> --confidence <high|low> --question '<everything the person needs, with your recommendation and why>' --option adopt --option cancel --option keep_draft` (`--because discard` when the question is whether to throw work away; for (c), recommend what you would do on your own), report briefly and stop. The answer arrives in this terminal as `answer to ask <id>: ...`: on adopt do 1, on cancel do 2 (the note names the ask), on keep_draft leave the draft as it is, record why with `dagq note --task {t} --text '<why>'` (naming the ask) and stop.\n\
         The runtime refuses your submit of a follow_up draft past that limit unless a person answered adopt: ask then, as (c) says.\n\
         When you are done, report the outcome in one or two sentences and stop; the runtime ends this session. Do not work on anything but {this}. Never open the queue database directly; use the dagq CLI only.\n",
        drafts = if single { "draft" } else { "drafts" },
        it = if single { "it" } else { "them" },
        each = if single {
            String::new()
        } else {
            format!(
                "The drafts came from one piece of work, so weigh them together first: when two of them are the same work, keep one and cancel the other with `dagq cancel ID --duplicate-of <the one you keep>`; when one needs another, add the dependency between them with `dagq dependency add`; submit those you adopt together in one proposal, `dagq submit ID ID ...` (with {}).\n",
                ids.join(", ")
            )
        },
        with_each = if single { "" } else { " for each draft, ID being its task ID" },
        this = if single { "this draft" } else { "these drafts" },
        rules = repository_rules(RUNTIME_PLANNER_ASK),
    ));
    if let Some(answer) = material.answer {
        out.push_str(&format!(
            "\nThe planner before you asked a person (ask {aid}) about draft {task} and is gone:\n{question}\n\nanswer to ask {aid}: {text}\n\nApply this answer as step 3 says.\n",
            aid = answer.id,
            task = answer.task_id.map_or("?".to_owned(), |t| t.to_string()),
            question = answer.question,
            text = answer.answer.as_deref().unwrap_or_default(),
        ));
    }
    Ok(out)
}

/// What the initial prompt of a planner opened for a finding shows
/// (ADR-0044 decision 19).
pub struct FindingPlannerMaterial<'a> {
    pub db: &'a Path,
    /// The finding with its evidence events.
    pub finding: &'a FindingView,
    /// Which planner of the runtime's this is since the finding was marked
    /// (1-based).
    pub attempt: usize,
    /// The asks about the finding: the observer's, a person's `propose`
    /// answer, earlier planners' questions.
    pub asks: &'a [Ask],
    /// The goal of the finding's target, if it has one.
    pub goal: Option<&'a Goal>,
    pub goal_closed: bool,
    /// That goal's tasks.
    pub siblings: &'a [GoalTask],
    /// The answered `planner_question` of a planner that is gone.
    pub answer: Option<&'a Ask>,
}

/// The initial prompt of a planner the runtime opens for a finding marked
/// for a proposal (ADR-0044 decisions 19, 20): the finding and its
/// evidence, the asks about it, the goal of its target, and what it may do:
/// a proposal of tasks for an open goal or of a new goal, a dismissal, or a
/// `planner_question` for a person.
pub fn finding_planner_prompt(material: &FindingPlannerMaterial<'_>) -> Result<String> {
    let view = material.finding;
    let finding = &view.finding;
    let id = finding.id;
    let mut out = format!(
        "You are a planner the dagq runtime opened for finding {id} of the queue at {db}; no person watches this session. The finding is marked for a proposal: make the plan that remedies it (planner {attempt} of at most {max} the runtime opens for it).\n",
        db = super::path_text(material.db)?,
        attempt = material.attempt,
        max = MAX_FINDING_PLANNERS,
    );
    let target = match (&finding.run_id, finding.task_id, finding.goal_id) {
        (Some(run), Some(task), _) => format!("run {run} (task {task})"),
        (None, Some(task), _) => format!("task {task}"),
        (_, _, Some(goal)) => format!("goal {goal}"),
        _ => "the queue".to_owned(),
    };
    out.push_str(&format!(
        "\n## Finding {id}: {summary}\n\n- kind: {kind}\n- on: {target}\n- subject: {subject}\n- impact: {impact}\n- occurrences: {occurrences}, first seen {first}, last seen {last} (Unix seconds)\n- recorded by: {by}\n- why a proposal: {why}\n\n### Detail\n\n{detail}\n",
        summary = finding.summary,
        kind = finding.kind,
        subject = or_none(&finding.subject),
        impact = finding.impact.as_str(),
        occurrences = finding.occurrences,
        first = finding.first_seen_at,
        last = finding.last_seen_at,
        by = finding.recorded_by,
        why = finding.propose_reason.as_deref().unwrap_or("(none)"),
        detail = or_none(&finding.detail),
    ));
    out.push_str("\n### Its evidence\n\n");
    match &view.evidence_events {
        Some(events) if !events.is_empty() => {
            out.push_str(&fenced("json", &serde_json::to_string_pretty(events)?));
        }
        _ => out.push_str("(no event)\n"),
    }
    out.push_str(&format!(
        "Read more with `dagq findings {id} --full`. {RECORD_READING}\n"
    ));
    if !material.asks.is_empty() {
        out.push_str("\n### Asks about it\n\n");
        for ask in material.asks {
            out.push_str(&format!(
                "- ask {aid} ({kind}): {question}\n  answer: {answer}\n",
                aid = ask.id,
                kind = ask.kind.as_str(),
                question = ask.question.replace('\n', "\n  "),
                answer = ask.answer.as_deref().unwrap_or("(none yet)"),
            ));
        }
    }
    match material.goal {
        Some(goal) => {
            out.push_str(&format!(
                "\n## Goal {gid} of its target: {title}{closed}\n\n{description}\n\nAcceptance:\n{acceptance}\n\nConstraints:\n{constraints}\n\nIts tasks:\n{tasks}\n",
                gid = goal.id(),
                title = goal.title(),
                closed = if material.goal_closed { " (closed)" } else { "" },
                description = or_none(goal.description()),
                acceptance = or_none(goal.acceptance()),
                constraints = or_none(goal.constraints()),
                tasks = if material.siblings.is_empty() {
                    "(none)".to_owned()
                } else {
                    material
                        .siblings
                        .iter()
                        .map(|t| format!("- task {} ({}): {}", t.id, t.status.as_str(), t.title))
                        .collect::<Vec<_>>()
                        .join("\n")
                },
            ));
        }
        None => out.push_str(
            "\n## Goal\n\nIts target belongs to no goal. `dagq goal list` shows the open goals.\n",
        ),
    }
    out.push_str(&format!(
        "\n## What to do\n\n\
         Follow the dagq-planner skill of the dagq plugin, its Basic policy above all: {DECIDE_YOURSELF} {rules} Before you plan, look for tasks that already remedy it or code that already does (`dagq search '<words>'`, `dagq related ID` for a task, `dagq show ID`). Then do exactly one of these:\n\
         1. Tasks for an open goal: when the remedy is within an open goal's scope (the one above, or another from `dagq goal list`), add its tasks to that goal as drafts (`dagq add --goal GOAL ...`, with `--context` beginning with `from finding {id} ({kind})` and saying why you chose this remedy), check them with `dagq lint`, and submit them with `dagq submit ID... --finding {id}`.\n\
         2. A new goal: when no open goal covers it, write a draft goal (`dagq goal add --draft ...`) and its draft tasks, lint them and submit with `dagq submit --goal GOAL --finding {id}`.\n\
         Either way the submission makes finding {id} proposed with the proposal, and plan review checks it before it becomes ready; you need no person's approval for it, even for a new goal. \
         An improvement's tasks are `--priority normal` (the default) or `low`, never higher: plan review lowers a higher one to normal.\n\
         3. Dismiss: when a task already remedies it (name the task), it no longer occurs, or it is not worth remedying, run `dagq finding dismiss {id} --reason '<why>'`, the reason saying why you decided so.\n\
         4. Ask: only when you cannot decide it yourself: (a) it needs a person's judgement, `scope` (the plan's intent, an acceptance, a contradiction with a goal's constraints or a decision the repository records, a precedent a person answered otherwise) or `discard` (whether to throw work away), that the queue, the repository (its code and the decisions it records) and a person's precedents cannot settle; or (b) your confidence in the decision is low. Run `dagq ask --finding {id} --kind planner_question --because scope --recommend <propose|dismiss> --confidence <high|low> --question '<everything the person needs, with your recommendation and why>' --option propose --option dismiss` (`--because discard` when the question is whether to throw work away), report briefly and stop. The answer arrives in this terminal as `answer to ask <id>: ...`: follow it (propose: do 1 or 2; dismiss: do 3).\n\
         When you are done, report the outcome in one or two sentences and stop; the runtime ends this session. Do not work on anything but this finding. Never open the queue database directly; use the dagq CLI only.\n",
        kind = finding.kind,
        rules = repository_rules(RUNTIME_PLANNER_ASK),
    ));
    if let Some(answer) = material.answer {
        out.push_str(&format!(
            "\nThe planner before you asked a person (ask {aid}) and is gone:\n{question}\n\nanswer to ask {aid}: {text}\n\nApply this answer as step 4 says.\n",
            aid = answer.id,
            question = answer.question,
            text = answer.answer.as_deref().unwrap_or_default(),
        ));
    }
    Ok(out)
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_owned()
    } else {
        items.join(", ")
    }
}

/// What the resolution request tells a resumed session.
pub(crate) struct ResumeRequest {
    /// The landing branch's head the session rebases onto.
    pub main: CommitSha,
    /// The landing branch's name (ADR-t615-1).
    pub branch: String,
    pub reason: String,
    pub kind: ResumeKind,
}

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

/// The fixed resolution request the supervisor types into a resumed
/// session (ADR-0019 decision 1), or into a passed run's live session whose
/// head conflicts with main (ADR-0027 decision 4), one instruction per
/// line; the backend sends it as one line.
pub(crate) fn resume_request(
    task: &Task,
    run: &TaskRun,
    request: &ResumeRequest,
    landed: &[PredecessorSummary],
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let route = Route::of(run);
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
            "dagq: run {} (task {}) changes paths outside the task's --paths ({}), so the run is needs_session.",
            run.id(),
            task.id(),
            task.paths().join(", ")
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
    lines.push(format!("Reason: {}", request.reason));
    let branch = &request.branch;
    lines.push(format!(
        "{branch} is now {} (your base commit was {}).",
        request.main,
        run.base_commit()
    ));
    if landed.is_empty() {
        lines.push(format!("Tasks landed on {branch} since your base: none."));
    } else {
        lines.push(format!("Tasks landed on {branch} since your base:"));
        for task in landed {
            lines.push(format!(
                "- task {}: {}; summary: {}",
                task.task_id, task.title, task.summary
            ));
        }
    }
    lines.push("Steps:".to_owned());
    let checks = local_checks(&serde_json::to_string(task.verification_commands())?);
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
            "1. Read the logs the reason names and fix the e2e tests that failed (each failed once more on its rerun by name) and commit; if {branch} moved, git rebase {} first. You may run a failed test by name to reproduce it (cargo test --locked --test e2e -- --ignored --exact <name>), but not the whole e2e: the runtime runs it again after the review.",
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
        "5. Rewrite the receipt at {receipt} with the new head commit, writing a temporary file in the same directory and renaming it."
    ));
    lines.push(
        "6. If the change is no longer needed, write the receipt with result failed and the reason in summary."
            .to_owned(),
    );
    lines.push(format!("7. {}", route.done(run)));
    Ok(lines.join("\n"))
}

/// The fixed request the supervisor types into the live session when the
/// receipt it rewrote for a revise or a conflict request does not name its clean worktree HEAD.
pub(crate) fn revise_mismatch_request(run: &TaskRun, label: &str, why: &str) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let route = Route::of(run);
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
    Ok(lines.join("\n"))
}

/// The one fixed request the supervisor types into a session that went idle
/// with a receipt naming `receipt_commit` while its clean worktree HEAD is
/// `head`, a new commit on top of its base (task 357): rewrite the receipt
/// for the head, or fix the worktree first.
pub(crate) fn stale_receipt_nudge(
    run: &TaskRun,
    receipt_commit: &str,
    head: &CommitSha,
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let route = Route::of(run);
    let stopped = if route.headless() {
        "ended its turn"
    } else {
        "went idle"
    };
    let mut lines = route.opening(format!(
        "dagq: run {} {stopped}, but its receipt names commit {receipt_commit} while the clean worktree HEAD is {head} (for example after a rebase or a new commit). The supervisor cannot accept a receipt for another commit.",
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
    Ok(lines.join("\n"))
}

/// The one nudge the supervisor types into a worker's session that stayed
/// idle without a receipt for `idle_secs` (ADR-0043 decision 1): commit and
/// write the receipt, ask with `dagq ask`, or say what background work it
/// waits for. `background` names the tasks its idle marker lists as running;
/// `running` says background work runs even when none is listed (an idle
/// read from a screen that shows it, task 823).
pub(crate) fn stall_nudge(
    run: &TaskRun,
    idle_secs: i64,
    background: &[crate::domain::stall::BackgroundTask],
    running: bool,
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let minutes = idle_secs / 60;
    let route = Route::of(run);
    if route.headless() {
        // A headless session's turn ended: nothing of it still runs, and
        // the nudge is its next turn (ADR-t813-1 decision 9).
        let mut lines = route.opening(format!(
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
        return Ok(lines.join("\n"));
    }
    let mut lines = vec![format!(
        "dagq: run {} has been idle for {minutes} minutes without a receipt.",
        run.id()
    )];
    if background.is_empty() && running {
        lines.push("Background work was still running when you stopped.".to_owned());
    } else if background.is_empty() {
        lines.push("No background task was running when you stopped.".to_owned());
    } else {
        lines.push("Background tasks still running when you stopped:".to_owned());
        for task in background {
            lines.push(format!("- {}: {}", task.description, task.command));
        }
    }
    lines.push("Do one of these now:".to_owned());
    lines.push(format!(
        "1. If the work is done, commit it and write the receipt at {receipt} (a temporary file in the same directory, then rename)."
    ));
    lines.push(format!(
        "2. {ASK_RULES_FIRST} Otherwise, if you need a decision, run `dagq ask --run {} --kind worker_question --because scope --topic <code> --question '...'` (or `--because discard` for whether to throw work away) and stop.",
        run.id()
    ));
    lines.push(
        "3. If you are waiting for background work, write here what you wait for, when it should end, and what you will do if it does not return; then go on with the work."
            .to_owned(),
    );
    lines.push(
        "If nothing changes, the supervisor asks a person to look at this session.".to_owned(),
    );
    Ok(lines.join("\n"))
}

/// What the supervisor sends a worker's session, in place of the nudge,
/// once its `worker_question` `ask_id` was closed without its answer
/// reaching it (task 1372): who closed it and what was recorded with it
/// (`answer`; `ask close` takes no reason of its own), and that the worker
/// decides within the task or writes a failed receipt, without asking the
/// same question again. `closed_by` names the closer, `None` when the
/// close recorded none.
pub(crate) fn closed_question_notice(
    run: &TaskRun,
    ask_id: i64,
    closed_by: Option<&str>,
    answer: Option<&str>,
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let route = Route::of(run);
    let closer = closed_by.map_or_else(|| "someone (not recorded)".to_owned(), str::to_owned);
    let mut lines = route.opening(format!(
        "dagq: ask {ask_id} (your worker_question on run {}) was closed by {closer} without an answer delivered to you.",
        run.id()
    ));
    lines.push(match answer.map(str::trim).filter(|a| !a.is_empty()) {
        Some(answer) => format!("What was recorded with it when it was closed: {answer}"),
        None => "No reason was recorded with the close.".to_owned(),
    });
    let when = if route.headless() {
        "in this turn"
    } else {
        "now"
    };
    lines.push(format!(
        "Do not ask the same question again. Do one of these {when}:"
    ));
    lines.push(format!(
        "1. If the decision is within the task's scope, decide it yourself, go on with the work, commit it and write the receipt at {receipt} (a temporary file in the same directory, then rename), saying in its summary what you decided and why."
    ));
    lines.push(format!(
        "2. If it needs a change outside the task's scope, write a failed receipt at {receipt} whose summary says why and what is needed."
    ));
    lines.push(format!("3. {}", route.stop()));
    lines.push(if route.headless() {
        "If the turns keep ending without a receipt, the supervisor hands the run to its recovery job.".to_owned()
    } else {
        "If nothing changes, the supervisor asks a person to look at this session.".to_owned()
    });
    Ok(lines.join("\n"))
}

/// What the headless reviewer is asked (ADR-0023 decision 2, ADR-0027
/// decision 2 as ADR-t451-1 decision 3 amends it): where the material is,
/// the task's acceptance, the verdict schema, where `revise` ends and
/// `concern` begins, and how a concern is judged and when it reaches a
/// person.
pub fn review_prompt(task: &Task, run: &TaskRun, review_path: &str) -> String {
    format!(
        "You review run {run_id} of dagq task {task_id} ({title}) before it lands.\n\
         Read the review material at {review_path}: the task, its goal, the receipt, the commits and the full diff. Read the worktree if you need more. Do not change any file.\n\n\
         Acceptance criteria of the task:\n{acceptance}\n\n\
         Decide one verdict:\n\
         - pass: the diff meets the acceptance criteria and the task's instructions and nothing needs fixing.\n\
         - revise: findings the worker can fix without a person's judgment: missing tests or evidence, findings of the repository's formatter, linter or other checks, a receipt that disagrees with the diff where fixing the diff settles it, or an obvious gap inside the instructed scope.\n\
         - concern: findings that call for a judgment rather than a mechanical fix: a mismatch with the acceptance criteria, changes the task did not ask for, or a finding that involves a judgment call. A concern does not by itself go to a person: you judge it below, recommending land or send_back with your confidence, and the runtime applies a sure judgment that needs no person (high, reason_category null) itself; only the rest (low, scope, discard) reaches a person.\n\n\
         {concern}\
         {codes}\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [{{\"text\": string, \"codes\": [string]}}], \"summary\": string, \"recommendation\": \"land\" | \"send_back\" | null, \"confidence\": \"high\" | \"low\" | null, \"reason_category\": \"scope\" | \"discard\" | null}}\n\
         reasons lists each finding (empty for pass); summary is one or two sentences; recommendation, confidence and reason_category are for a concern only (null for pass and revise).\n",
        run_id = run.id(),
        task_id = task.id(),
        title = task.title(),
        acceptance = or_none(task.acceptance()),
        concern = CONCERN_RECOMMENDATION,
        codes = reason_codes_section(review_reason::REVIEW_CODES),
    )
}

/// How a review recommends what to do with its `concern`, and which
/// judgements it leaves to a person (ADR-t451-1 decisions 1 and 3).
const CONCERN_RECOMMENDATION: &str = "For a concern, also recommend what to do, and how sure you are:\n\
- recommendation: land (the run may land as it is: the findings are minor or acceptable within the task and its acceptance) or send_back (the worker should fix the findings in the same run).\n\
- confidence: high when the task, its acceptance, the repository's decision records and rules settle it and you would bet on a person choosing the same; low when you hesitate, the material is not enough, or a person could reasonably choose otherwise.\n\
- reason_category: scope when landing would accept a departure from the acceptance criteria, a recorded decision of the repository or the goal's decisions (or meeting them would need a change of scope); discard when the judgement is whether to cancel the task or throw the work away; null otherwise.\n\
The runtime applies a high recommendation whose reason_category is null without asking: send_back goes to the worker's session like a revise, and land lands the run after its usual checks. Anything else (low, scope, discard) goes to a person with your recommendation. Leave scope and discard to the person rather than deciding them; when in doubt, say low.\n\n";

/// How a review job labels each finding (ADR-t947-1): the codes, their
/// definitions heaviest first, and how the primary one is chosen.
fn reason_codes_section(codes: &[(&str, &str)]) -> String {
    format!(
        "Give each finding one or more reason codes, the main one first: when a finding fits two, the one whose fix needs the heavier judgment (the list is heaviest first); other when none fits, explained in the text. \
         Put first the finding that decides the verdict; a note that would not stop it is never first. \
         The codes are recorded for statistics only and change nothing of how the verdict is applied. The codes:\n{}\n\n",
        review_reason::prompt_lines(codes)
    )
}

/// What the headless triage (the recovery job) may do beyond what needs no
/// permission: read files only (ADR-t1063-1 decision 2).
pub const TRIAGE_ACCESS: JobAccess = JobAccess::ReadFiles;

/// What the headless review of a run may do: read files only, since the
/// live worker session owns the worktree (ADR-0027, ADR-t1063-1 decision 2).
pub const REVIEW_ACCESS: JobAccess = JobAccess::ReadFiles;

/// Bytes of each log, receipt and screen the triage prompt carries (their
/// ends).
const TRIAGE_TAIL_BYTES: usize = 3000;

/// Logs of a run directory the triage reads: the latest integrate
/// attempt's `integrate-<attempt>-verify-N.log` (see [`integrate_logs`]) and
/// `verify-N.log`, at most this many.
const TRIAGE_LOGS: usize = 8;

/// What the recovery job of a run that ended `failed` or `interrupted`
/// reads beyond a live session's material (ADR-0047 decision 39, as the
/// triage read it before): the run's error, receipt, verification logs,
/// final screen and events, the task's earlier runs with their rounds, and
/// the rules the runtime holds `retry` and `resume` to. `dir` is where the
/// run's files are.
pub fn ended_run_material(
    files: &dyn RunFiles,
    detail: &TaskDetail,
    run: &TaskRun,
    resumes: crate::domain::resume::ResumeCount,
    config: ResumeConfig,
    dir: &Path,
) -> String {
    let failures = detail
        .runs
        .iter()
        .filter(|r| matches!(r.status(), RunStatus::Failed | RunStatus::Interrupted))
        .count();
    let read = |path: &Path| {
        files
            .read(path)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    let mut material = format!(
        "Last error of the run:\n{}\n\n",
        or_none(run.last_error().unwrap_or_default())
    );
    let receipt = run.receipt_path().map(Path::new).and_then(read);
    material.push_str(&format!(
        "Receipt ({}):\n{}\n",
        run.receipt_path().unwrap_or("none"),
        fenced(
            "json",
            or_none(tail(
                receipt.as_deref().unwrap_or_default().trim(),
                TRIAGE_TAIL_BYTES
            ))
        )
    ));
    let (latest, earlier) = integrate_logs(files, dir);
    let mut logs = latest;
    // `verify-N.log` is what validation wrote before ADR-0023.
    let mut validation: Vec<PathBuf> = log_names(files, dir)
        .into_iter()
        .filter(|(name, _)| name.starts_with("verify-") && name.ends_with(".log"))
        .map(|(_, path)| path)
        .collect();
    validation.sort();
    logs.extend(validation);
    logs.truncate(TRIAGE_LOGS);
    if !earlier.is_empty() {
        material.push_str(&format!(
            "Logs of earlier integrate attempts (not shown): {}\n",
            earlier
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if logs.is_empty() {
        material.push_str("Verification logs: none\n");
    }
    for log in &logs {
        let text = read(log).unwrap_or_default();
        material.push_str(&format!(
            "Verification log {} (end):\n{}\n",
            log.display(),
            fenced("text", or_none(tail(text.trim(), TRIAGE_TAIL_BYTES)))
        ));
    }
    let screen = read(&dir.join("terminal-final.txt"));
    material.push_str(&format!(
        "Final screen of the session (end of terminal-final.txt; a headless session has none, its turns follow):\n{}\n",
        fenced(
            "text",
            or_none(tail(
                screen.as_deref().unwrap_or_default().trim(),
                TRIAGE_TAIL_BYTES
            ))
        )
    ));
    // A headless session's turns (ADR-t813-1): how each ended, and why the
    // runtime stopped one.
    let turns: Vec<String> = detail
        .events
        .iter()
        .filter(|e| {
            e.run_id.as_ref() == Some(run.id())
                && e.kind == crate::domain::event_kind::TURN_FINISHED
        })
        .map(|e| e.payload.to_string())
        .collect();
    if !turns.is_empty() {
        let turns = &turns[turns.len().saturating_sub(5)..];
        material.push_str(&format!(
            "Turns of the headless session (the last {}; `stopped` says why the runtime stopped one):\n{}\n",
            turns.len(),
            fenced("json", &turns.join("\n"))
        ));
    }
    let events: Vec<Value> = detail
        .events
        .iter()
        .filter(|e| e.run_id.as_ref() == Some(run.id()))
        .map(super::health::compact_event)
        .collect();
    let events = &events[events.len().saturating_sub(40)..];
    material.push_str(&format!(
        "Events of the run (the last {}):\n{}\n",
        events.len(),
        fenced(
            "json",
            &events
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        )
    ));
    let earlier: Vec<String> = detail
        .runs
        .iter()
        .filter(|r| *r.id() != *run.id())
        .map(|r| {
            let actions: Vec<String> = detail
                .events
                .iter()
                .filter(|e| {
                    e.run_id.as_ref() == Some(r.id()) && e.kind == event_kind::TRIAGE_FINISHED
                })
                .map(|e| format!("{}", e.payload.get("action").unwrap_or(&Value::Null)))
                .collect();
            format!(
                "- run {} {}: {}{}",
                r.id(),
                r.status().as_str(),
                or_none(tail(r.last_error().unwrap_or_default(), 300)),
                if actions.is_empty() {
                    String::new()
                } else {
                    format!(" (recovered: {})", actions.join(", "))
                }
            )
        })
        .collect();
    material.push_str(&format!(
        "\nEarlier runs of the task:\n{}\n\n",
        if earlier.is_empty() {
            "none".to_owned()
        } else {
            earlier.join("\n")
        }
    ));
    let retry_rule = if failures >= TRIAGE_RETRY_FAILURES {
        format!(
            "This task has {failures} failed or interrupted runs, this one included: do not choose retry (the runtime escalates it)."
        )
    } else {
        format!(
            "This task has {failures} failed or interrupted run(s), this one included; from {TRIAGE_RETRY_FAILURES} on, the runtime does not apply retry."
        )
    };
    let resume_rule = if resumes.exhausted(config) {
        format!(
            "The run was resumed {} time(s) ({} of at most {MAX_RESUME_ATTEMPTS} counted, {} of at most {} conflict-only attempts with the conflict precheck's requests, {} of at most {} after a signal from outside killed its session) and its resumes are used up: do not choose resume.",
            resumes.total(),
            resumes.counted,
            resumes.conflict_attempts(),
            config.conflict_only_limit,
            resumes.kill_only,
            crate::domain::resume::KILL_ONLY_RESUME_LIMIT
        )
    } else {
        format!(
            "The run was resumed {} time(s) ({} of at most {MAX_RESUME_ATTEMPTS} counted, {} of at most {} after a signal from outside killed its session); resume needs the run's worktree.",
            resumes.total(),
            resumes.counted,
            resumes.kill_only,
            crate::domain::resume::KILL_ONLY_RESUME_LIMIT
        )
    };
    material.push_str(&format!("Rules: {retry_rule} {resume_rule}\n"));
    material
}

/// What the runtime read for a recovery job (ADR-0047 decision 39), at the
/// time of the alert.
pub struct RecoveryMaterial<'a> {
    pub alert: RecoveryAlert,
    /// For a run that ended: [`ended_run_material`]. `None` for a live
    /// session.
    pub ended: Option<String>,
    /// The alert's own facts (`recovery_requested`'s payload).
    pub facts: &'a Value,
    pub workspace: &'a str,
    /// The screen's excerpt, or why it could not be read.
    pub screen: &'a str,
    /// The processes that belong to the run (see
    /// [`crate::domain::recovery::run_processes`]), or why they could not
    /// be listed.
    pub processes: std::result::Result<Vec<ProcessInfo>, String>,
    pub git_status: &'a str,
    pub head: &'a str,
    /// The receipt's `commit`, when there is a receipt.
    pub receipt_commit: Option<&'a str>,
    /// The run's earlier recovery verdicts and automatic repairs.
    pub history: &'a [Value],
    /// The actions that apply to this alert.
    pub allowed: &'a [&'a str],
}

/// What each allowed action does, for the recovery prompt of a run on
/// `route`: an instruction to a headless session is its next turn's prompt.
fn recovery_action_help(route: Route, action: &str) -> &'static str {
    match action {
        "send_instruction" if route.headless() => {
            "{\"action\": \"send_instruction\", \"instruction\": string}: send this instruction once, as the prompt of the session's next turn (a resume of the same session; its previous turn has ended), for example to rerun the tests in the foreground, commit and write the receipt."
        }
        "stop_processes" => {
            "{\"action\": \"stop_processes\", \"pids\": [pid, ...]}: stop these processes (SIGTERM, then SIGKILL after a grace). Only processes listed below as the run's own are allowed; any other pid makes the whole verdict an escalation. Use it for a background process the session waits for that will not end by itself (an orphan holding a pipe, a hung test). Never the session's own wrapper or agent."
        }
        "send_instruction" => {
            "{\"action\": \"send_instruction\", \"instruction\": string}: type this instruction into the session once (it must be idle at its prompt), for example to stop a background command it waits for and rerun the tests."
        }
        "wait" => {
            "{\"action\": \"wait\", \"recheck_after_secs\": n}: do nothing now; if the alert still holds after n seconds (at most 3600), another recovery job runs. The work looks healthy and is only slow, or what holds it passes by itself."
        }
        "retry" => {
            "{\"action\": \"retry\"}: make the task ready again for a new run from the current main. Only for a run whose branch holds no commit of its own (nothing is thrown away); a run with commits needs retry_inherit or a person. Use it when the failure came from the environment (the machine slept, a process was killed, the session never started, an outage)."
        }
        "retry_inherit" => {
            "{\"action\": \"retry_inherit\"}: make the task ready again for a new run that carries this run's branch over onto the current main. Only for a run whose branch has commits, and once per task."
        }
        "resume" => {
            "{\"action\": \"resume\", \"instruction\": string}: send the run back to a session of its own in its worktree, with the instruction (what to do: fix the failing test, commit and rewrite the receipt, rebase) added to the resolution request. Only while its resumes are not used up."
        }
        "answer_known_dialog" => {
            "{\"action\": \"answer_known_dialog\", \"dialog\": \"background_work\" | \"settings_panel\"}: send the fixed keys to one of the known dialogs on the screen (Background work is running, only after the supervisor's /exit with a clean worktree and the receipt at HEAD; the Settings / Usage panel). Never another dialog."
        }
        "close_and_proceed" => {
            "{\"action\": \"close_and_proceed\"}: close the session's workspace and go on to land. Only when the review passed, the worktree is clean and the receipt names its HEAD, the reviewed commit."
        }
        _ => "",
    }
}

/// The recovery actions that answer an interactive session's screen, its
/// dialogs and its `/exit`: never offered for a headless run.
pub(crate) const HEADLESS_NEVER: [&str; 2] = ["answer_known_dialog", "close_and_proceed"];

/// What the recovery job of an alert is asked (ADR-0047 decisions 39 and
/// 40): the alert, the task, the screen, the run's processes, the
/// worktree's state and the run's earlier repairs, for a run that ended
/// also its error, receipt, logs and events, then the allowed actions and
/// the verdict schema.
pub fn recovery_prompt(
    task: &Task,
    run: &TaskRun,
    attempt: usize,
    material: &RecoveryMaterial<'_>,
) -> Result<String> {
    let processes = match &material.processes {
        Ok(processes) if processes.is_empty() => "none".to_owned(),
        Ok(processes) => processes
            .iter()
            .map(|p| {
                format!(
                    "- pid {} (parent {}, running {}s, cpu {}, cwd {}): {}",
                    p.pid,
                    p.ppid,
                    p.elapsed_secs,
                    p.cpu_ms.map_or_else(
                        || "unknown".to_owned(),
                        |ms| format!("{}.{:03}s", ms / 1000, ms % 1000)
                    ),
                    p.cwd.as_deref().unwrap_or("unknown"),
                    tail(&p.command, 300)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Err(error) => format!("(the processes could not be listed: {error})"),
    };
    let history = if material.history.is_empty() {
        "none".to_owned()
    } else {
        material
            .history
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let route = Route::of(run);
    // A headless session has no screen, dialog or `/exit`: the actions
    // that answer them are never offered for it (ADR-t813-1 decision 9).
    let actions = material
        .allowed
        .iter()
        .filter(|action| !route.headless() || !HEADLESS_NEVER.contains(action))
        .map(|action| format!("- {}", recovery_action_help(route, action)))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        "You are dagq's recovery job (attempt {attempt}) for run {run_id} of task {task_id} ({title}), {state}. The supervisor raised the alert {alert}: {meaning}\n\
         Decide whether the runtime can repair it with one of the allowed actions below, or whether a person has to look.\n\
         Read only: the material below, and the files it names if you need more (the worktree is {worktree}). Do not change any file and do not run commands; the runtime applies your verdict.\n\n\
         Task description:\n{description}\n\n\
         Acceptance criteria:\n{acceptance}\n\n\
         Current task verification commands (use these, including after a person's correction):\n{verification}\n\n\
         Alert facts:\n{facts}\n\n\
         {ended}\
         {screen_label}\n{screen}\n\n\
         Processes of the run (working directory in the worktree, or under the session's wrapper; the wrapper and the agent themselves are not listed):\n{processes}\n\n\
         Worktree: HEAD {head}, receipt commit {receipt}, git status:\n{status}\n\n\
         Earlier recovery verdicts, repairs, and task_edited events:\n{history}\n\n\
         Allowed actions:\n{actions}\n\
         Not allowed, ever: cancelling the task, retrying a run that has commits, editing the task, landing without review, writing to main, pushing, deleting branches or worktrees, touching anything outside this run's worktree and workspace, writing the queue database, {never_keys}. If the repair needs any of these, escalate.\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"repair\" | \"escalate\", \"confidence\": \"high\" | \"low\", \"diagnosis\": string, \"actions\": [action, ...], \"question\": string, \"options\": [string, ...], \"reason_category\": \"recovery_failed\" | \"discard\" | \"scope\"}}\n\
         diagnosis says what you found in one or two sentences. repair needs at least one action and is applied only with confidence high; with confidence low, or with escalate, a person is asked, with your actions as the recommendation, question as the question and options added to theirs. If a broken verification command caused an ended run to fail, offer `edit the task's --verify, then retry_inherit` to the person: user or inbox can edit only verification commands after the run ends; you cannot edit. Once the task_edited event and current commands show the correction, retry_inherit carries the committed work forward and integration uses the corrected commands. reason_category says why a person is needed: recovery_failed when you cannot repair it or are not sure, discard when the work would be thrown away, scope when it needs a permission you do not have.\n",
        run_id = run.id(),
        task_id = task.id(),
        title = task.title(),
        state = match &material.ended {
            Some(_) => format!("which ended {}; its session is gone", run.status().as_str()),
            None => format!(
                "whose session in workspace {} is still running",
                material.workspace
            ),
        },
        ended = material.ended.as_deref().unwrap_or_default(),
        alert = material.alert.as_str(),
        meaning = match material.alert {
            RecoveryAlert::LongBackground =>
                "background work the session started has run longer than the threshold, and the session waits for it. With phase after_receipt the session already wrote its receipt: the run goes on to its validation and landing only once the session goes idle with no background work running, so work left over from before the receipt (a wait loop, a watch) holds it.",
            RecoveryAlert::Failed =>
                "the run failed (its receipt said failed, its validation or landing failed, or its session exited without finishing).",
            RecoveryAlert::Interrupted =>
                "the run's session died and the supervisor recovered the run as interrupted.",
            RecoveryAlert::ResumeExhausted =>
                "the run still needed a session after its last resume, so the supervisor stopped resuming it.",
            RecoveryAlert::StuckExit =>
                "the session did not exit after the supervisor's /exit (or the /exit never reached it), and the runtime's own repairs did not apply.",
            RecoveryAlert::PromptWaiting =>
                "the session waits at a dialog the runtime does not answer by itself.",
            RecoveryAlert::Stalled if route.headless() =>
                "the headless session does not get on: its turns end with neither a receipt nor an open question after the supervisor's nudges, each sent as the next turn (reason turn_without_receipt), or a turn was refused permissions too often to get on (reason permission_denied). The alert facts say which. The session has no screen, input box or dialog: an instruction is the prompt of its next turn, and resume parks the run for a session of its own.",
            RecoveryAlert::Stalled =>
                "the session looks stuck: it stays idle without a receipt after the supervisor nudged it (reason idle_without_receipt), or it did not take a text the supervisor typed, which stays in its input box, showed no sign of work or brought up a dialog (reason send_unconfirmed). The alert facts say which. An instruction goes only into a session idle at its prompt, never over a text still in its input box. resume applies only to a run in its first session: the run is parked for a session of its own and this session is asked to /exit, which waits for background work it still runs, so stop what hangs with stop_processes in the same verdict.",
            RecoveryAlert::IdleProcess =>
                "processes of the run (listed in the alert facts with how long they have used almost no CPU time) are alive but have not made progress for longer than the threshold; the session may be waiting for them.",
        },
        worktree = run.worktree_path().unwrap_or("none"),
        screen_label = if route.headless() {
            "Last turns of the headless session (it has no screen):"
        } else {
            "Last lines of the session's screen:"
        },
        never_keys = if route.headless() {
            "typing into the session (a headless session takes no keys; an instruction goes as its next turn)"
        } else {
            "sending keys to a dialog that is not a known one"
        },
        description = or_none(task.description()),
        acceptance = or_none(task.acceptance()),
        verification = fenced("sh", &task.verification_commands().join("\n")),
        facts = fenced("json", &serde_json::to_string_pretty(material.facts)?),
        screen = fenced("text", or_none(material.screen.trim())),
        head = material.head,
        receipt = material.receipt_commit.unwrap_or("(no receipt)"),
        status = fenced("text", or_none(material.git_status.trim())),
    ))
}

/// The fixed request the supervisor types into the live session for a
/// `revise` verdict (ADR-0027 decision 2), one instruction per line; the
/// backend sends it as one line.
pub(crate) fn revise_request(
    task: &Task,
    run: &TaskRun,
    round: usize,
    reasons: &[String],
) -> Result<String> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let checks = local_checks(&serde_json::to_string(task.verification_commands())?);
    let route = Route::of(run);
    let mut lines = route.opening(format!(
        "dagq: the supervisor's review of run {} (task {}) asks for changes (revise {round} of {MAX_REVISE_ATTEMPTS}).",
        run.id(),
        task.id()
    ));
    lines.push("Findings:".to_owned());
    for reason in reasons {
        lines.push(format!("- {reason}"));
    }
    lines.push("Steps:".to_owned());
    lines.push("1. Fix the findings in this worktree and commit.".to_owned());
    lines.push(format!("2. {checks}"));
    lines.push("3. Keep the worktree clean.".to_owned());
    lines.push(format!("4. {}", route.stop()));
    lines.push(format!(
        "5. Rewrite the receipt at {receipt} with the new head commit, writing a temporary file in the same directory and renaming it."
    ));
    lines.push(format!("6. {}", route.done(run)));
    Ok(lines.join("\n"))
}

/// What the headless plan review may do beyond what needs no permission:
/// read files, and run the dagq CLI (ADR-0044 decision 22, ADR-t1063-1
/// decision 2). It runs with the reviewer's role in its environment, like
/// the review, so the CLI refuses every dagq command that writes. On Codex
/// it is `codex exec` in the read-only sandbox (ADR-t1063-1 decision 2):
/// its prompt names the repository's instructions and documents to read
/// and leans on no Claude plugin, skill or hook.
pub const PLAN_REVIEW_ACCESS: JobAccess = JobAccess::ReadFilesAndQueueCli;

/// What the headless goal review may do: the same as the plan review (read
/// files, and run the dagq CLI, whose policy lets it only read).
pub const GOAL_REVIEW_ACCESS: JobAccess = JobAccess::ReadFilesAndQueueCli;

/// Characters of a precedent's question and answer the plan review prompt
/// and the revise request quote.
const PRECEDENT_CHARS: usize = 400;

/// Candidates of each kind (related tasks, search hits) the plan review
/// prompt lists at most for one task of the proposal.
pub const DUPLICATE_CANDIDATES: usize = 5;

/// Where the plan review starts looking for duplicates and changes already
/// made for one task of the proposal (goal 29): the tasks `dagq related`
/// ranks highest with the clues that relate them, and the tasks and landed
/// commits `dagq search` finds for the words of its title, in any status,
/// none of them the proposal's own.
#[derive(Debug, Clone, Serialize)]
pub struct DuplicateCandidates {
    pub task_id: TaskId,
    pub related: Vec<RelatedTask>,
    pub search: Vec<SearchHit>,
}

/// Expected files the summary of a ready or in-progress task lists at
/// most; the hotspot table reads them all.
pub const SUMMARY_EXPECTED_FILES: usize = 10;

/// What the headless plan review reads (ADR-0041 decision 10): the
/// proposal and its tasks, the goals they belong to, what `dagq lint`
/// found, the other proposals not yet ready (oldest submission first), the
/// ready and in-progress tasks, the asks a person answered before, the
/// files that conflict often and each task's duplicate candidates.
pub struct PlanReviewMaterial<'a> {
    pub proposal: &'a Proposal,
    pub tasks: &'a [TaskDetail],
    pub goals: &'a [Goal],
    pub lint: &'a [LintViolation],
    /// Other submitted or revising proposals with their tasks.
    pub others: &'a [(Proposal, Vec<Task>)],
    /// Ready and in-progress tasks, with their long fields; the prompt
    /// lists each in summary and gives the long fields only of those it
    /// has a reason to (task 591).
    pub queued: &'a [TaskListItem],
    /// Ready and in-progress tasks past the limit of `queued`.
    pub queued_left_out: usize,
    /// The files each task of the proposal and of `queued` is expected to
    /// touch (ADR-0069 decisions 1, 2).
    pub expected: &'a BTreeMap<TaskId, Vec<String>>,
    /// Asks a person answered, newest first.
    pub precedents: &'a [Ask],
    /// The files the landings conflicted in most (`stats`
    /// `conflict_hotspots`), that main still has.
    pub hotspots: &'a [ConflictHotspot],
    /// One entry per task of the proposal, in the order of `tasks`.
    pub candidates: &'a [DuplicateCandidates],
    pub repo_root: &'a Path,
}

/// One line quoting an answered ask as a precedent.
pub fn precedent_line(ask: &Ask) -> String {
    let cut = |text: &str| {
        super::health::truncate(text, PRECEDENT_CHARS).unwrap_or_else(|| text.to_owned())
    };
    format!(
        "precedent: ask {id}{task} ({kind}) asked: {question} — a person answered: {answer}",
        id = ask.id,
        task = ask
            .task_id
            .map(|task| format!(" about task {task}"))
            .unwrap_or_default(),
        kind = ask.kind.as_str(),
        question = cut(&ask.question.replace('\n', " ")),
        answer = cut(ask.answer.as_deref().unwrap_or("(none)")),
    )
}

/// What the headless plan review is asked: the material, the checks, the
/// fixes it may make itself and the verdict schema (ADR-0041 decisions 10,
/// 11, 14, 15). The repository's own rules are not in the runtime: the job
/// reads them from the repository's documents.
pub fn plan_review_prompt(material: &PlanReviewMaterial<'_>) -> Result<String> {
    let proposal = material.proposal;
    let json_lines = |values: Vec<Value>| {
        if values.is_empty() {
            "(none)".to_owned()
        } else {
            fenced(
                "json",
                &values
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
    };
    let tasks = json_lines(
        material
            .tasks
            .iter()
            .map(|detail| {
                let mut task = serde_json::to_value(&detail.task)?;
                task["dependencies"] = serde_json::to_value(&detail.dependencies)?;
                task["goal_dependencies"] = serde_json::to_value(&detail.goal_dependencies)?;
                Ok(task)
            })
            .collect::<Result<_>>()?,
    );
    let goals = json_lines(
        material
            .goals
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let lint = json_lines(
        material
            .lint
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let others = if material.others.is_empty() {
        "(none)".to_owned()
    } else {
        material
            .others
            .iter()
            .map(|(other, tasks)| {
                let earlier =
                    (other.submitted_at(), other.id()) < (proposal.submitted_at(), proposal.id());
                let tasks = tasks
                    .iter()
                    .map(|task| {
                        serde_json::json!({
                            "id": task.id(), "status": task.status(), "title": task.title(),
                            "description": task.description(), "acceptance": task.acceptance(),
                            "paths": task.paths(),
                        })
                        .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "Proposal {} ({}, submitted {}, {} this one):\n{}",
                    other.id(),
                    other.status().as_str(),
                    other.submitted_at(),
                    if earlier { "before" } else { "after" },
                    fenced("json", &tasks)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let no_files = Vec::new();
    let expected = |id: TaskId| material.expected.get(&id).unwrap_or(&no_files);
    let touching = |path: &str, ids: &mut dyn Iterator<Item = TaskId>| {
        ids.filter(|&id| crate::domain::claim_defer::touches(expected(id), path))
            .collect::<Vec<_>>()
    };
    // Each hotspot with the tasks expected to touch it; a queued task on
    // the same hotspot as a task of the proposal is given in full.
    let mut full = BTreeSet::new();
    let mut hotspots = Vec::new();
    for file in material.hotspots {
        let path = file.renamed_to.as_deref().unwrap_or(&file.path);
        let own = touching(path, &mut material.tasks.iter().map(|d| d.task.id()));
        let queued = touching(path, &mut material.queued.iter().map(|item| item.id));
        if !own.is_empty() {
            full.extend(queued.iter().copied());
        }
        hotspots.push(serde_json::json!({
            "path": path,
            "conflicts": file.conflicts, "tasks": file.tasks,
            "landings": file.landings, "ratio": file.ratio,
            "last_conflict_at": file.last_conflict_at, "alert": file.alert,
            "proposal_tasks": own, "queued_tasks": queued,
        }));
    }
    let hotspots = json_lines(hotspots);
    for candidates in material.candidates {
        full.extend(candidates.related.iter().map(|task| TaskId::new(task.id)));
        full.extend(
            candidates
                .search
                .iter()
                .filter_map(|hit| match (hit.kind, &hit.id) {
                    (
                        crate::domain::search::SearchKind::Task,
                        crate::domain::search::SearchRef::Id(id),
                    ) => Some(TaskId::new(*id)),
                    _ => hit.task_id.map(TaskId::new),
                }),
        );
    }
    let summaries = material
        .queued
        .iter()
        .map(|item| {
            let files = expected(item.id);
            let mut summary = serde_json::json!({
                "id": item.id, "status": item.status, "priority": item.priority,
                "goal_id": item.goal_id, "title": item.title,
                "paths": item.details.as_ref().map(|details| &details.paths).unwrap_or(&no_files),
                "dependencies": item.dependencies, "goal_dependencies": item.goal_dependencies,
                "expected_files": files.iter().take(SUMMARY_EXPECTED_FILES).collect::<Vec<_>>(),
            });
            if files.len() > SUMMARY_EXPECTED_FILES {
                summary["more_expected_files"] = (files.len() - SUMMARY_EXPECTED_FILES).into();
            }
            if full.contains(&item.id) {
                summary["full_text_below"] = true.into();
            }
            summary
        })
        .collect();
    let mut queued = json_lines(summaries);
    if material.queued_left_out > 0 {
        queued.push_str(&format!(
            "\n({} more ready or in-progress tasks, those of the lowest IDs, are left out of this list)",
            material.queued_left_out
        ));
    }
    let queued_full = json_lines(
        material
            .queued
            .iter()
            .filter(|item| full.contains(&item.id))
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let own_expected = json_lines(
        material
            .tasks
            .iter()
            .map(|detail| {
                serde_json::json!({
                    "task_id": detail.task.id(),
                    "expected_files": expected(detail.task.id()),
                })
            })
            .collect(),
    );
    let precedents = if material.precedents.is_empty() {
        "(none)".to_owned()
    } else {
        material
            .precedents
            .iter()
            .map(|ask| format!("- {}", precedent_line(ask)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let candidates = json_lines(
        material
            .candidates
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let predicted = material
        .tasks
        .iter()
        .filter(|detail| detail.task.status() == crate::domain::TaskStatus::Submitted)
        .map(|detail| detail.task.id().to_string())
        .collect::<Vec<_>>();
    let predicted = if predicted.is_empty() {
        "(none)".to_owned()
    } else {
        predicted.join(", ")
    };
    Ok(format!(
        "You are the plan review of dagq proposal {id}: decide whether the queue may run its tasks as written, before they become ready.\n\
         Read only. Do not change any file and do not run dagq commands that write: the runtime lets you run the dagq commands that read (`dagq show ID`, `dagq proposal show ID`, `dagq search`, `dagq related`, `dagq findings`, `dagq stats`, ...) and refuses the rest.\n\
         {RECORD_READING}\n\n\
         First read the repository's own rules in {repo}: its instructions (AGENTS.md and CLAUDE.md), the documents and rules they name (the plan review's part of them above all), and the documents the tasks name. \
         Apply what they say (the verification each kind of change needs, the declared paths, the required evidence, the rules for the records they keep, ...); the runtime has no such rules of its own. \
         Where the repository has no AGENTS.md, judge a task's verification, paths and evidence in this order: CLAUDE.md; then what the README, the CI configuration and the build configuration show; when none of them settles it, it needs a person: a concern.\n\n\
         The proposal was submitted {submitted} and was sent back {revises} time(s) before (at most {max}; a revise past that goes to a person as a concern).\n\n\
         Tasks of the proposal:\n{tasks}\n\n\
         Files each task of the proposal is expected to touch (its declared paths; without them, the files the landings of its 3 most related completed tasks changed; a guess, so check it against the source):\n{own_expected}\n\n\
         Goals they belong to (description, acceptance, constraints; constraints win over a task's description):\n{goals}\n\n\
         The mechanical checks (`dagq lint`) found:\n{lint}\n\n\
         Other proposals not ready yet:\n{others}\n\n\
         Ready and in-progress tasks, in summary, newest first (expected_files as for the proposal's tasks, and for an in-progress task also what its run changed so far; full_text_below marks a task given in full below):\n{queued}\n\n\
         In full, the ready and in-progress tasks among the candidates below or expected to touch a hotspot a task of the proposal is expected to touch:\n{queued_full}\n\n\
         Asks a person answered before (newest first):\n{precedents}\n\n\
         Files the landings conflicted in most lately (`dagq stats` conflict_hotspots: conflicts, tasks, landings that changed the file, their ratio; alert when over the thresholds), each with the tasks of the proposal (proposal_tasks) and the ready and in-progress tasks (queued_tasks) expected to touch it:\n{hotspots}\n\n\
         Candidates of duplicates and of changes already made, one line per task of the proposal (related: the tasks `dagq related` ranks highest, in any status, with the clues that relate them; search: the tasks and landed commits `dagq search` finds for the words of the task's title, with their status; at most {most} of each, none of the proposal's own tasks; an empty list means none was found):\n{candidates}\n\n\
         Check the meaning of the plan:\n\
         - a task that repeats another task (ready, in progress, in another proposal, or already landed); start from its candidates above, a completed or canceled one included, and judge from their titles, clues and the source whether the task really repeats one;\n\
         - a task whose change has already landed (read the source; a completed candidate or a landed commit is where to look);\n\
         - a contradiction with a decision the repository records (in its instructions or the decision records they name) or with the goal's constraints;\n\
         - an acceptance criterion that contradicts the task's own description or a sibling task's acceptance (for example a change of a type whose acceptance says a test file that uses the type is not changed);\n\
         - tasks that change the same files without a dependency between them, above all a file listed as conflicting often: for each hotspot whose proposal_tasks and queued_tasks are both non-empty, add a dependency (add_dependency, the task of the proposal waiting for the queued one) or say in summary why none is needed; you may read the source to see which files a task of the proposal really touches;\n\
         - a task that partly repeats a ready or in-progress task (the overlap goes once the scope of one is cut): not pass but revise, saying in the reason which part to cut and which of the two keeps it;\n\
         - a contradiction with another proposal: with one submitted before this one, send this one back; with one submitted after, pass this one (the later one is checked against it);\n\
         - a ready task that has to change for this proposal to hold: name it in reopen, and the runtime takes it out of the claim for a planner to fix; an in-progress task is never changed: send this proposal back asking for a task that fixes it after it lands and depends on it;\n\
         - every finding of `dagq lint` is one to fix.\n\n\
         Decide one verdict:\n\
         - pass: the tasks may run as written, after the actions below.\n\
         - revise: findings the planner can fix without a person's judgment (wording, acceptance, verification, paths, a split, a scope that partly overlaps another task, a missing task or dependency). Each reason says what to change.\n\
         - concern: findings that need a judgment beyond a planner's fix: a doubtful duplicate, a change that looks already done, a contradiction with a decision the repository records or with the goal's constraints, a change of the plan's intent. A concern is not only for a person: you judge it too, with a recommendation and a confidence, and the runtime applies what you are sure of.\n\
         With a concern, give what you recommend and how sure you are; the runtime applies a sure recommendation itself and asks a person only what the record cannot settle. \
         recommendation is ready (the tasks may run as written, after the actions below), send_back (the planner fixes it, as a revise; it counts toward the revises above) or cancel. \
         confidence is high when the queue's records, the repository's documents and decisions and the answered asks settle it, low when they do not or you are unsure. \
         reason_category is scope when your recommendation would let a task through against a decision the repository records, the goal's constraints or a person's precedent; discard when you recommend cancel; null otherwise. \
         A high ready or send_back with reason_category null is applied without a person (a ready as a pass, its actions included); a low confidence, scope, discard, or a send_back past the revises above goes to a person with your recommendation.\n\
         When a finding is of the same kind as an answered ask above, put that ask's id in precedents and say in the reason how the person answered then.\n\n\
         actions are the only changes you make yourself, and only with pass (or a concern whose high ready is applied): add_dependency (a task of the proposal waits for another task), lower_priority (never raise one), cancel_duplicate (only an obvious duplicate; a doubtful one, or a change that looks already made, is a concern). Everything else is the planner's. A proposal that remedies a finding (an improvement) keeps its tasks at normal or low: lower a high or urgent one to normal with lower_priority and pass, never revise for it (a pass lowers any you miss).\n\n\
         Whatever the verdict, also estimate the weight of each submitted task of the proposal (tasks {predicted}), one entry per task in predictions, from what you read: \
         a worker (one Claude Opus session in its own Git worktree) implements the task, runs the checks the repository's instructions ask of a worker (formatting, lint, the tests of the change, ...), commits and writes a receipt; \
         then a headless review (pass / revise / concern) and `integrate`'s verification after the rebase onto main follow, and a failure, a conflict or missing evidence resumes the run. \
         size is S, M or L; nature is mechanical, implementation, design_judgment or investigation; uncertainty is 0 to 1 (1 the least certain); \
         expected_output_tokens is the output tokens (thinking included) of one worker run: a small one about 5000, a large one about 250000, the median about 35000; \
         rework_probability is 0 to 1, the chance the run is resumed or review answers revise or concern; reason is one sentence. The estimate is recorded only and changes nothing of the verdict.\n\n\
         {codes}\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [{{\"text\": string, \"codes\": [string]}}], \"summary\": string, \
         \"actions\": [{{\"action\": \"add_dependency\", \"task_id\": int, \"depends_on\": int}} | {{\"action\": \"lower_priority\", \"task_id\": int, \"priority\": \"low\" | \"normal\" | \"high\" | \"urgent\"}} | {{\"action\": \"cancel_duplicate\", \"task_id\": int, \"duplicate_of\": int}}], \
         \"reopen\": [{{\"task_id\": int, \"reason\": string}}], \"precedents\": [int], \
         \"recommendation\": \"ready\" | \"send_back\" | \"cancel\", \"confidence\": \"high\" | \"low\", \"reason_category\": \"scope\" | \"discard\" | null, \
         \"predictions\": [{{\"task_id\": int, \"size\": \"S\" | \"M\" | \"L\", \"nature\": \"mechanical\" | \"implementation\" | \"design_judgment\" | \"investigation\", \"uncertainty\": number, \"expected_output_tokens\": int, \"rework_probability\": number, \"reason\": string}}]}}\n\
         reasons lists each finding (empty for pass); summary is one or two sentences; actions, reopen and precedents may be empty; recommendation, confidence and reason_category go with a concern only; predictions has one entry for each submitted task and no other.\n",
        id = proposal.id(),
        repo = material.repo_root.display(),
        submitted = proposal.submitted_at(),
        revises = proposal.revise_count(),
        max = MAX_PLAN_REVISES,
        most = DUPLICATE_CANDIDATES,
        codes = reason_codes_section(review_reason::PLAN_REVIEW_CODES),
    ))
}

/// What the goal review job is shown about one goal (ADR-0047 decision
/// 43): the goal, each of its tasks with what landed for it, the goal's
/// notes and edits, and the goal's earlier reviews. Each value is one
/// JSON line of the prompt.
pub struct GoalReviewMaterial<'a> {
    pub goal: Value,
    pub tasks: Vec<Value>,
    pub events: Vec<Value>,
    pub previous: Vec<Value>,
    pub gaps_in_a_row: usize,
    pub repo_root: &'a Path,
}

/// The prompt of the headless goal review (ADR-0047 decision 43): whether
/// the goal whose tasks all ended met its acceptance.
pub fn goal_review_prompt(material: &GoalReviewMaterial<'_>) -> String {
    let lines = |values: &[Value]| {
        if values.is_empty() {
            "(none)".to_owned()
        } else {
            super::fenced(
                "json",
                &values
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
    };
    let goal_id = material.goal["id"].clone();
    format!(
        "You are the goal review of the dagq queue, a headless job. Every task of goal {goal_id} ended (completed or canceled): judge whether the goal met its acceptance. Change nothing: read the repository's documents and source in {repo} (the main checkout, where the tasks landed) and run read-only dagq commands (`dagq show ID`, `dagq goal show {goal_id} --full`, `dagq findings`, `dagq events --goal {goal_id} --full`, `dagq search ...`) as you need.\n\n\
         The goal:\n{goal}\n\n\
         Its tasks, each with the run that landed it (the receipt's summary, its evidence and its follow_ups) when it was completed by a run:\n{tasks}\n\n\
         The goal's notes, edits and earlier decisions:\n{events}\n\n\
         The goal's earlier reviews (gaps verdicts in a row before this one: {gaps}; after {max} in a row a gaps verdict is turned into a question to a person):\n{previous}\n\n\
         Split the acceptance into its items and check each against what landed, with evidence you saw (a commit, a file, a test, a receipt). Then answer one of:\n\
         - achieved: every item is met. The runtime closes the goal as achieved and records your criteria.\n\
         - gaps: some items are not met and the work to meet them is clear and within the goal. List each missing piece as a gap with a title and a description a planner can turn into a task; the runtime registers each as a draft of the goal and a planner of the runtime decides it. The goal stays open.\n\
         - ask: only when a person has to decide: the acceptance should change, the goal should be abandoned or split, or you cannot judge it. Write the question; the person answers achieved, abandoned, gaps (the gaps you listed, or `gaps: <what>`) or keep_open. reason_category is scope (the acceptance, the scope or a decision changes) or discard (work would be thrown away).\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"achieved\" | \"gaps\" | \"ask\", \"criteria\": [{{\"criterion\": string, \"met\": bool, \"evidence\": [string]}}], \"gaps\": [{{\"title\": string, \"description\": string, \"criterion\": string}}], \"summary\": string, \"question\": string, \"options\": [string], \"reason_category\": \"scope\" | \"discard\"}}\n\
         criteria has one entry for each item of the acceptance; gaps is empty unless the verdict is gaps (or ask, to offer them); summary is one or two sentences; question, options and reason_category are for ask only.\n",
        repo = material.repo_root.display(),
        goal = lines(std::slice::from_ref(&material.goal)),
        tasks = lines(&material.tasks),
        events = lines(&material.events),
        previous = lines(&material.previous),
        gaps = material.gaps_in_a_row,
        max = crate::domain::goal_review::MAX_GOAL_GAPS,
    )
}

/// What the supervisor types into the live planner a revise goes back to
/// (ADR-0041 decisions 12, 13).
pub fn plan_revise_request(proposal: ProposalId, reasons: &[String]) -> String {
    let reasons = if reasons.is_empty() {
        "- (none given)".to_owned()
    } else {
        reasons
            .iter()
            .map(|reason| format!("- {reason}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Plan review sent proposal {proposal} back. Fix what these reasons point at:\n{reasons}\n\
         Then submit it again with `dagq submit --proposal {proposal}`. A fix that changes the plan's intent (acceptance, scope, the relation to the goal) needs the person: ask them here first."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;
    use crate::domain::{
        EvidenceCheck, GoalRecord, GoalStatus, GoalVerdict, Provider, RunId, RunRecord, TaskRecord,
        TaskStatus,
    };
    use serde_json::json;
    use std::time::UNIX_EPOCH;

    const SHA: &str = "1111111111111111111111111111111111111111";
    const RUN: &str = "00000000-0000-4000-8000-000000000001";

    fn task(id: i64, title: &str, status: TaskStatus) -> Task {
        verified_task(id, title, status, Vec::new())
    }

    fn verified_task(
        id: i64,
        title: &str,
        status: TaskStatus,
        verification_commands: Vec<String>,
    ) -> Task {
        Task::restore(TaskRecord {
            id: TaskId::new(id),
            title: title.into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands,
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            status,
            goal_id: None,
            context: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            worker: crate::domain::worker::Worker::CLAUDE_INTERACTIVE,
            named_mode: None,
        })
        .unwrap()
    }

    fn task_with_evidence(id: i64, required_evidence: Vec<EvidenceCheck>) -> Task {
        Task::restore(TaskRecord {
            id: TaskId::new(id),
            title: "work".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence,
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            status: TaskStatus::InProgress,
            goal_id: None,
            context: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            worker: crate::domain::worker::Worker::CLAUDE_INTERACTIVE,
            named_mode: None,
        })
        .unwrap()
    }

    fn run(task_id: i64, status: RunStatus, result_commit: Option<&str>) -> TaskRun {
        TaskRun::restore(RunRecord {
            id: RunId::new(RUN).unwrap(),
            task_id: TaskId::new(task_id),
            status,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: crate::domain::worker::WorkerMode::Interactive,
            base_commit: CommitSha::try_from(SHA).unwrap(),
            branch: Some(format!("dagq/{RUN}")),
            worktree_path: Some("/runs/run/worktree".into()),
            workspace_id: None,
            receipt_path: Some("/runs/run/receipt.json".into()),
            log_path: None,
            result_commit: result_commit.map(|sha| CommitSha::try_from(sha).unwrap()),
            repo_path: None,
            run_dir: Some("/runs/run".into()),
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap()
    }

    #[test]
    fn goal_dependencies_share_the_predecessor_section_with_short_summaries() {
        let files = MemoryFiles::default();
        let receipt = json!({
            "run_id": RUN,
            "result": "succeeded",
            "commit": SHA,
            "tests": {"status": "passed", "evidence_or_reason": "cargo test"},
            "e2e": {"status": "not_applicable", "evidence_or_reason": "none"},
            "subagent_review": {"status": "not_applicable", "evidence_or_reason": "small"},
            "summary": "word ".repeat(100),
        });
        files.put(
            Path::new("/runs/run/receipt.json"),
            UNIX_EPOCH,
            &receipt.to_string(),
        );
        let goal = Goal::restore(GoalRecord {
            id: GoalId::new(4),
            title: "upstream goal".into(),
            description: String::new(),
            acceptance: String::new(),
            constraints: String::new(),
            doc: None,
            status: GoalStatus::Open,
            closed_at: Some("2026-09-25T00:00:00Z".into()),
            verdict: Some(GoalVerdict::Achieved),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        let landed = GoalPredecessorSummary::from_goal_predecessor(
            &files,
            &GoalPredecessor {
                goal: goal.clone(),
                tasks: vec![Predecessor {
                    task: task(2, "upstream work", TaskStatus::Completed),
                    integrated_run: Some(run(2, RunStatus::Integrated, Some(SHA))),
                }],
            },
        );
        let summary = landed.tasks[0].summary.clone();
        assert_eq!(summary.chars().count(), GOAL_TASK_SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
        let empty = GoalPredecessorSummary::from_goal_predecessor(
            &files,
            &GoalPredecessor {
                goal,
                tasks: Vec::new(),
            },
        );

        let waiting = task(9, "downstream", TaskStatus::InProgress);
        let own_run = run(9, RunStatus::Claimed, None);
        let text = prompt(
            &waiting,
            &own_run,
            None,
            &[],
            &[landed, empty],
            &[],
            None,
            &[],
        )
        .unwrap();
        assert!(
            text.contains(&format!(
                "Predecessor tasks (their changes are already in your base commit):\n\
                 - goal 4 (closed as achieved): upstream goal; its completed tasks:\n  \
                 - task 2: upstream work; result commit {SHA}; summary: {summary}\n\
                 - goal 4 (closed as achieved): upstream goal; its completed tasks:\n  - none\n"
            )),
            "{text}"
        );
        let alone = prompt(&waiting, &own_run, None, &[], &[], &[], None, &[]).unwrap();
        assert!(alone.contains("Predecessor tasks: none\n"));
        assert!(!alone.contains("Carried over from run"));
    }

    /// The worker and resume prompts tell the worker not to run the e2e,
    /// which the runtime runs after the review (ADR-t1233-2): no command,
    /// no marks and no exclusions for any provider. A run that cannot need
    /// it reads nothing of the e2e.
    #[test]
    fn the_worker_and_resume_prompts_leave_the_e2e_to_the_runtime() {
        let own_run = run(7, RunStatus::Claimed, None);
        let e2e = ["tests/e2e.rs".to_owned()];
        let open = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
        let line = "E2E: do not run the e2e (tests/e2e.rs) yourself.";
        let codex = run_on(Provider::Codex, WorkerMode::Headless);
        for worker in [&own_run, &codex] {
            let text = prompt(&open, worker, None, &[], &[], &[], None, &e2e).unwrap();
            assert!(text.contains(line), "{text}");
            for gone in [
                "E2E evidence is decided by your diff",
                "E2E marks",
                "Codex worker E2E",
                "--skip",
                "cargo test --locked --test e2e",
            ] {
                assert!(!text.contains(gone), "{gone}: {text}");
            }
        }
        let none = prompt(&open, &own_run, None, &[], &[], &[], None, &[]).unwrap();
        assert!(!none.contains("E2E"), "{none}");
        let required = task_with_evidence(8, vec![EvidenceCheck::E2e]);
        let text = prompt(&required, &own_run, None, &[], &[], &[], None, &[]).unwrap();
        assert!(text.contains(line), "{text}");
        assert!(!text.contains("Required evidence"), "{text}");

        let request = ResumeRequest {
            main: CommitSha::try_from(SHA).unwrap(),
            branch: "main".into(),
            reason: "the e2e failed: a; see /runs/r/e2e-1.log".into(),
            kind: ResumeKind::E2e,
        };
        for worker in [&own_run, &codex] {
            let resumed = resume_request(&open, worker, &request, &[]).unwrap();
            assert!(
                resumed.contains("the e2e the runtime ran on the host before landing it failed"),
                "{resumed}"
            );
            assert!(resumed.contains("Reason: the e2e failed: a; see /runs/r/e2e-1.log"));
            assert!(resumed.contains("--exact <name>"), "{resumed}");
            assert!(!resumed.contains("Codex worker E2E"), "{resumed}");
            assert!(!resumed.contains("E2E marks"), "{resumed}");
        }
    }

    /// The worker, resume and revise prompts show the verification commands
    /// as integrate's to run and send the session to the repository's own
    /// instructions for its checks, with the verification commands as the
    /// default (task 510).
    #[test]
    fn sessions_run_the_repository_checks_and_leave_the_verification_to_integrate() {
        let verified = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
        let own_run = run(7, RunStatus::Claimed, None);
        let checks = "the repository's instructions (AGENTS.md or CLAUDE.md) ask a worker to run";

        let worker = prompt(&verified, &own_run, None, &[], &[], &[], None, &[]).unwrap();
        assert!(worker.contains(
            "Verification commands (integrate runs them once after rebasing onto main; that run is the verification of record for the commit):\n[\n  \"make gate\"\n]\n"
        ));
        assert!(worker.contains(checks), "{worker}");
        assert!(worker.contains(
            "when the instructions name no such checks, run the verification commands above."
        ));
        assert!(!worker.contains("run in the worktree):"));
        assert!(worker.contains(
            "the worker section of the repository instructions (AGENTS.md or CLAUDE.md), the task context"
        ));
        let inheritance = Inheritance {
            run_id: RunId::new(RUN).unwrap(),
            base: CommitSha::try_from(SHA).unwrap(),
            head: SHA.into(),
            branch: None,
            receipt_path: None,
            summary: "earlier".into(),
        };
        let retried = prompt(
            &verified,
            &own_run,
            None,
            &[],
            &[],
            &[],
            Some(&inheritance),
            &[],
        )
        .unwrap();
        let (before, carried) = retried.split_once("Carried over from run").unwrap();
        assert!(before.contains(checks));
        assert!(carried.contains("rerun your checks in the worktree as above"));

        let default = r#"when the instructions name no such checks, run the verification commands ["make gate"]."#;
        let reproduce = "If the reason is a verification command that failed after integrate's rebase, you may also run that command in the worktree";
        for kind in [
            ResumeKind::Landing,
            ResumeKind::EvidenceMissing,
            ResumeKind::SentBack,
            ResumeKind::ScopeViolation,
            ResumeKind::Precheck,
            ResumeKind::Triage,
            ResumeKind::SessionGone,
        ] {
            let request = ResumeRequest {
                main: CommitSha::try_from(SHA).unwrap(),
                branch: "main".into(),
                reason: "why".into(),
                kind,
            };
            let text = resume_request(&verified, &own_run, &request, &[]).unwrap();
            assert!(text.contains(checks), "{kind:?}: {text}");
            assert!(text.contains(default), "{kind:?}: {text}");
            assert!(!text.contains("Rerun the verification commands"), "{text}");
            assert_eq!(
                text.contains(reproduce),
                kind == ResumeKind::Landing,
                "{kind:?}: {text}"
            );
        }

        let revise = revise_request(&verified, &own_run, 1, &["fix it".into()]).unwrap();
        assert!(revise.contains(&format!("2. {}", local_checks(r#"["make gate"]"#))));
        assert!(revise.contains(default), "{revise}");
    }

    /// A task as plan review reads it, with long fields of the size of this
    /// queue's (about 3 KB, task 591's measure of prompt 157).
    fn long_task(id: i64, status: TaskStatus, paths: &[&str]) -> Task {
        Task::restore(TaskRecord {
            id: TaskId::new(id),
            title: format!("task {id}"),
            description: format!("description of {id} ").repeat(100),
            acceptance: "acceptance ".repeat(60),
            verification_commands: vec!["cargo test".into()],
            required_evidence: Vec::new(),
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            priority: Default::default(),
            change: None,
            status,
            goal_id: None,
            context: "context ".repeat(80),
            created_at: String::new(),
            updated_at: String::new(),
            worker: crate::domain::worker::Worker::CLAUDE_INTERACTIVE,
            named_mode: None,
        })
        .unwrap()
    }

    /// The plan review prompt with `queued` ready tasks: the proposal's
    /// task 1000 touches `src/hot.rs`, and so does the ready task 5.
    fn plan_prompt(queued: i64, left_out: usize) -> (String, usize) {
        use crate::domain::{
            PlannerOrigin, PlannerOwner, ProposalRecord, ProposalStatus, related::RelatedTask,
        };
        let proposal = Proposal::restore(ProposalRecord {
            id: ProposalId::new(1),
            status: ProposalStatus::Submitted,
            owner: PlannerOwner {
                origin: PlannerOrigin::Person,
                workspace_id: None,
            },
            submitted_at: "now".into(),
            revise_count: 0,
            task_ids: vec![TaskId::new(1000)],
            goal_ids: Vec::new(),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        let tasks = [TaskDetail {
            task: long_task(1000, TaskStatus::Submitted, &["src/hot.rs"]),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            duplicate_of: None,
            duplicates: Vec::new(),
            runs: Vec::new(),
            events: Vec::new(),
            processes: Vec::new(),
            origin: None,
            follow_up_drafts: Vec::new(),
        }];
        let items: Vec<TaskListItem> = (1..=queued)
            .map(|id| {
                let paths: &[&str] = if id == 5 { &["src/*.rs"] } else { &[] };
                TaskListItem::new(
                    long_task(id, TaskStatus::Ready, paths),
                    vec![TaskId::new(1)],
                    Vec::new(),
                    None,
                    None,
                    true,
                )
            })
            .collect();
        let mut expected = BTreeMap::new();
        expected.insert(TaskId::new(1000), vec!["src/hot.rs".to_owned()]);
        for item in &items {
            let files = match item.id.as_i64() {
                5 => vec!["src/*.rs".to_owned()],
                6 => (0..30).map(|n| format!("src/other{n}.rs")).collect(),
                _ => vec!["src/cold.rs".to_owned()],
            };
            expected.insert(item.id, files);
        }
        let hotspot = |path: &str| ConflictHotspot {
            path: path.into(),
            conflicts: 3,
            tasks: 2,
            task_ids: Vec::new(),
            landings: Some(4),
            ratio: Some(0.75),
            last_conflict_at: "then".into(),
            state: "present",
            renamed_to: None,
            alert: true,
        };
        let candidates = [DuplicateCandidates {
            task_id: TaskId::new(1000),
            related: vec![RelatedTask {
                id: 7,
                status: "ready".into(),
                title: "task 7".into(),
                score: 1.0,
                clues: Vec::new(),
                duplicate_of: None,
            }],
            search: Vec::new(),
        }];
        let old_size = items
            .iter()
            .map(|item| serde_json::to_string(item).unwrap().len())
            .sum();
        let prompt = plan_review_prompt(&PlanReviewMaterial {
            proposal: &proposal,
            tasks: &tasks,
            goals: &[],
            lint: &[],
            others: &[],
            queued: &items,
            queued_left_out: left_out,
            expected: &expected,
            precedents: &[],
            hotspots: &[hotspot("src/hot.rs"), hotspot("src/cold.rs")],
            candidates: &candidates,
            repo_root: Path::new("/repo"),
        })
        .unwrap();
        (prompt, old_size)
    }

    #[test]
    fn plan_review_lists_the_queue_in_summary_and_in_full_only_what_it_meets() {
        let (prompt, _) = plan_prompt(10, 3);
        let lines: Vec<Value> = prompt
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let summary = |id: i64| {
            lines
                .iter()
                .find(|line| line["id"] == id && line.get("expected_files").is_some())
                .unwrap_or_else(|| panic!("no summary of {id} in {prompt}"))
        };
        let full: Vec<i64> = lines
            .iter()
            .filter(|line| line.get("description").is_some() && line["id"] != 1000)
            .map(|line| line["id"].as_i64().unwrap())
            .collect();
        // In full: the related candidate (7) and the task on the proposal's
        // hotspot (5); not the ones on the hotspot nobody of the proposal
        // touches.
        assert_eq!(full, [5, 7]);
        assert_eq!(summary(5)["full_text_below"], true);
        assert_eq!(summary(5)["paths"], json!(["src/*.rs"]));
        assert_eq!(summary(2).get("full_text_below"), None);
        assert_eq!(summary(2).get("description"), None);
        assert_eq!(summary(2)["expected_files"], json!(["src/cold.rs"]));
        assert_eq!(summary(2)["dependencies"], json!([1]));
        // A long list of expected files is cut, with the count left out.
        assert_eq!(
            summary(6)["expected_files"].as_array().unwrap().len(),
            SUMMARY_EXPECTED_FILES
        );
        assert_eq!(summary(6)["more_expected_files"], 20);
        // Each hotspot with the tasks expected to touch it.
        let hot = |path: &str| {
            lines
                .iter()
                .find(|line| line["path"] == path)
                .unwrap_or_else(|| panic!("no {path} in {prompt}"))
        };
        assert_eq!(hot("src/hot.rs")["proposal_tasks"], json!([1000]));
        assert_eq!(hot("src/hot.rs")["queued_tasks"], json!([5]));
        assert_eq!(hot("src/cold.rs")["proposal_tasks"], json!([]));
        assert_eq!(
            hot("src/cold.rs")["queued_tasks"].as_array().unwrap().len(),
            9
        );
        assert!(
            prompt.contains("(3 more ready or in-progress tasks, those of the lowest IDs, are left out of this list)"),
            "{prompt}"
        );
        assert!(prompt.contains("not pass but revise, saying in the reason which part to cut"));
        assert!(prompt.contains("for each hotspot whose proposal_tasks and queued_tasks are both non-empty, add a dependency"));
        let (prompt, _) = plan_prompt(4, 0);
        assert!(!prompt.contains("are left out of this list"), "{prompt}");
    }

    /// The prompts carry no rules of dagq's own repository (its ADRs, a
    /// Rust linter): they send each session to the repository's rules, and
    /// say where to find them when the repository has no AGENTS.md.
    #[test]
    fn prompts_take_the_rules_from_the_repository_in_order() {
        let db = Path::new("/q/queue.db");
        let (plan_review, _) = plan_prompt(4, 0);
        let planner = planner_prompt(db).unwrap();
        let revise = runtime_planner_prompt(db, ProposalId::new(3), &[], &["fix".into()]).unwrap();
        let review = review_prompt(
            &task(7, "work", TaskStatus::InProgress),
            &run(7, RunStatus::Succeeded, Some(SHA)),
            "/r/review.md",
        );
        let order = "in this order: its AGENTS.md; without one, its CLAUDE.md; without either, what its README, CI configuration and build configuration show; when none of them settles it, ";
        assert!(
            planner.contains(&format!("{order}ask the person.")),
            "{planner}"
        );
        assert!(
            revise.contains(&format!(
                "{order}decide them yourself from the source, the decisions the repository records and a person's precedents, and ask a person with a `planner_question` ask"
            )),
            "{revise}"
        );
        assert!(plan_review.contains("Where the repository has no AGENTS.md, judge a task's verification, paths and evidence in this order: CLAUDE.md; then what the README, the CI configuration and the build configuration show; when none of them settles it, it needs a person: a concern."));
        assert!(plan_review.contains(
            "the documents and rules they name (the plan review's part of them above all)"
        ));
        // A concern carries its recommendation and confidence, and what is
        // left to a person (ADR-t451-1 decisions 1 and 4).
        assert!(plan_review.contains("recommendation is ready"));
        assert!(
            plan_review.contains("- concern: findings that need a judgment beyond a planner's fix")
        );
        assert!(
            !plan_review.contains("- concern: findings that need a person's judgment: a doubtful")
        );
        assert!(plan_review.contains("confidence is high when"));
        assert!(plan_review.contains("reason_category is scope when"));
        assert!(plan_review.contains("\"reason_category\": \"scope\" | \"discard\" | null"));
        assert!(review.contains("findings of the repository's formatter, linter or other checks"));
        // A concern's recommendation, and what it leaves to a person
        // (ADR-t451-1 decisions 1 and 3).
        for part in [
            "\"recommendation\": \"land\" | \"send_back\" | null",
            "\"confidence\": \"high\" | \"low\" | null",
            "\"reason_category\": \"scope\" | \"discard\" | null",
            "Leave scope and discard to the person rather than deciding them; when in doubt, say low.",
            "- concern: findings that call for a judgment rather than a mechanical fix",
            "A concern does not by itself go to a person: you judge it below",
            "only the rest (low, scope, discard) reaches a person.",
        ] {
            assert!(review.contains(part), "{part} in {review}");
        }
        for text in [
            &plan_review,
            &planner,
            &revise,
            &review,
            &inbox_prompt(db).unwrap(),
        ] {
            for dagq_own in ["ADR", "docs/adr", "clippy"] {
                assert!(!text.contains(dagq_own), "{dagq_own} in {text}");
            }
        }
        assert!(!RECORD_READING.contains("ADR"));
    }

    #[test]
    fn plan_review_prompt_of_a_queue_of_170_ready_tasks_is_a_fraction_of_their_full_text() {
        let (prompt, full_text) = plan_prompt(170, 0);
        println!(
            "prompt {} bytes, full text of the queue {full_text} bytes",
            prompt.len()
        );
        // Before task 591 the list alone was the full text of every task.
        assert!(full_text > 500_000, "{full_text}");
        assert!(
            prompt.len() * 8 < full_text,
            "{} bytes against {full_text}",
            prompt.len()
        );
    }

    /// Task 823: an idle read from a screen that shows background work
    /// lists no task but does not tell the worker nothing was running.
    #[test]
    fn the_nudge_says_background_work_ran_even_when_none_is_listed() {
        let run = run_on(Provider::Claude, WorkerMode::Interactive);
        let none = stall_nudge(&run, 600, &[], false).unwrap();
        assert!(none.contains("No background task was running"), "{none}");
        let running = stall_nudge(&run, 600, &[], true).unwrap();
        assert!(
            running.contains("Background work was still running when you stopped."),
            "{running}"
        );
        assert!(!running.contains("No background task"), "{running}");
    }

    /// Task 1372: the notice of a question closed without its answer says
    /// who closed it and what was recorded with it (or that nothing was),
    /// and that the worker decides or writes a failed receipt instead of
    /// asking again; a headless session is told to do it in this turn.
    #[test]
    fn the_notice_of_a_closed_question_names_its_closer_and_what_to_do() {
        let run = run_on(Provider::Claude, WorkerMode::Interactive);
        let notice =
            closed_question_notice(&run, 5, Some("inbox"), Some("ask the planner")).unwrap();
        assert!(
            notice.starts_with(&format!(
                "dagq: ask 5 (your worker_question on run {RUN}) was closed by inbox without an answer delivered to you."
            )),
            "{notice}"
        );
        assert!(
            notice.contains("What was recorded with it when it was closed: ask the planner"),
            "{notice}"
        );
        assert!(notice.contains("Do not ask the same question again. Do one of these now:"));
        assert!(notice.contains("decide it yourself"), "{notice}");
        assert!(notice.contains("write a failed receipt at /runs/run/receipt.json"));
        assert!(!notice.contains("dagq ask"), "{notice}");
        let headless = run_on(Provider::Codex, WorkerMode::Headless);
        let notice = closed_question_notice(&headless, 5, None, Some("  ")).unwrap();
        assert!(
            notice.contains("closed by someone (not recorded)"),
            "{notice}"
        );
        assert!(notice.contains("No reason was recorded with the close."));
        assert!(notice.contains("Do one of these in this turn:"), "{notice}");
        assert!(
            notice.contains("hands the run to its recovery job"),
            "{notice}"
        );
        // Nothing a headless session is never told (task 817).
        assert!(notice.starts_with(HEADLESS_NEXT_TURN), "{notice}");
        assert!(notice.contains(HEADLESS_STOP), "{notice}");
        for never in ["/exit", "this terminal", STOP_BACKGROUND, "went idle"] {
            assert!(!notice.contains(never), "{never}: {notice}");
        }
    }

    /// Task 7's claimed run, whose worker is `provider` in `mode`.
    fn run_on(provider: Provider, mode: WorkerMode) -> TaskRun {
        TaskRun::restore(RunRecord {
            id: RunId::new(RUN).unwrap(),
            task_id: TaskId::new(7),
            status: RunStatus::Claimed,
            requested_provider: provider,
            actual_provider: provider,
            worker_mode: mode,
            base_commit: CommitSha::try_from(SHA).unwrap(),
            branch: Some(format!("dagq/{RUN}")),
            worktree_path: Some("/runs/run/worktree".into()),
            workspace_id: Some("ws".into()),
            receipt_path: Some("/runs/run/receipt.json".into()),
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: Some("/runs/run".into()),
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap()
    }

    /// Every text a worker's session is sent: its prompt, each resolution
    /// request, the revise, the receipt mismatch, the stale receipt, the
    /// nudge, an answer and a recovery job's instruction.
    fn session_texts(task: &Task, run: &TaskRun) -> Vec<String> {
        let mut texts = vec![prompt(task, run, None, &[], &[], &[], None, &[]).unwrap()];
        for kind in [
            ResumeKind::Landing,
            ResumeKind::EvidenceMissing,
            ResumeKind::SentBack,
            ResumeKind::ScopeViolation,
            ResumeKind::Precheck,
            ResumeKind::Triage,
            ResumeKind::Recheck,
            ResumeKind::SessionGone,
            ResumeKind::E2e,
        ] {
            let request = ResumeRequest {
                main: CommitSha::try_from(SHA).unwrap(),
                branch: "main".into(),
                reason: "why".into(),
                kind,
            };
            texts.push(resume_request(task, run, &request, &[]).unwrap());
        }
        texts.push(revise_request(task, run, 1, &["fix it".into()]).unwrap());
        texts.push(revise_mismatch_request(run, "the revise", "stale").unwrap());
        let head = CommitSha::try_from("2222222222222222222222222222222222222222").unwrap();
        texts.push(stale_receipt_nudge(run, SHA, &head).unwrap());
        texts.push(stall_nudge(run, 600, &[], false).unwrap());
        texts.push(answer_text(run, 3, "blue"));
        texts.push(recovery_instruction(run, "stalled", "write the receipt"));
        texts.push(continue_text(run));
        texts
    }

    /// Acceptance (1) of task 817: a headless worker, on Claude or Codex,
    /// is told to do its work in one turn, to end the turn with an ask when
    /// it needs a decision and to read the repository's AGENTS.md; nothing
    /// it is sent names /exit, a terminal typed into, or background work
    /// left running for later.
    #[test]
    fn headless_sessions_are_told_to_finish_in_a_turn_and_never_about_exit() {
        let task = Task::restore(TaskRecord {
            id: TaskId::new(7),
            title: "work".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec!["make gate".into()],
            required_evidence: vec![EvidenceCheck::E2e, EvidenceCheck::SubagentReview],
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            status: TaskStatus::InProgress,
            goal_id: None,
            context: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            worker: crate::domain::worker::Worker::CLAUDE_INTERACTIVE,
            named_mode: None,
        })
        .unwrap();
        for provider in [Provider::Claude, Provider::Codex] {
            let run = run_on(provider, WorkerMode::Headless);
            let texts = session_texts(&task, &run);
            let first = &texts[0];
            assert!(first.contains(HEADLESS_WORKER), "{first}");
            assert!(first.contains(headless_provider_line(provider)), "{first}");
            assert!(first.contains("Do the whole task in this turn"), "{first}");
            assert!(first.contains("report briefly that you asked, and end the turn"));
            assert!(first.contains("as the prompt of your next turn in this same session"));
            assert!(first.contains("AGENTS.md"), "{first}");
            assert!(first.contains(HEADLESS_STOP), "{first}");
            for text in &texts {
                for never in [
                    "/exit",
                    "this terminal",
                    "to the terminal",
                    STOP_BACKGROUND,
                    "write here what you wait for",
                    "went idle",
                ] {
                    assert!(!text.contains(never), "{provider:?} {never}: {text}");
                }
                assert!(
                    text.contains("in this turn") || text.contains("end the turn"),
                    "{provider:?}: {text}"
                );
            }
            for request in &texts[1..13] {
                assert!(request.starts_with(HEADLESS_NEXT_TURN), "{request}");
                assert!(request.contains(HEADLESS_STOP), "{request}");
                assert!(request.contains("pkill"), "{request}");
                assert!(request.contains("AGENTS.md"), "{request}");
                assert!(
                    request.contains(&format!("dagq ask --run {RUN} --kind worker_question")),
                    "{request}"
                );
                assert!(request.contains("and end the turn"), "{request}");
            }
            assert!(texts[16].ends_with(HEADLESS_GO_ON));
            assert!(texts[13].starts_with(HEADLESS_NEXT_TURN));
            assert!(texts[13].contains("dagq: the previous turn of run"));
            assert!(texts[14].starts_with("answer to ask 3: blue\n\n"));
            assert!(texts[14].ends_with(HEADLESS_GO_ON));
            assert!(texts[15].starts_with("dagq: the supervisor's recovery job for run"));
            assert!(texts[15].ends_with(HEADLESS_GO_ON));
        }
        // Codex reviews its own diff and does not owe subagent_review.
        let codex = session_texts(&task, &run_on(Provider::Codex, WorkerMode::Headless));
        assert!(codex[0].contains("You have no subagent to review your change"));
        // No worker backs e2e (ADR-t1233-2).
        assert!(!codex[0].contains("Required evidence"), "{}", codex[0]);
        let claude = session_texts(&task, &run_on(Provider::Claude, WorkerMode::Headless));
        assert!(claude[0].contains("and subagent review."));
        assert!(claude[0].contains("Required evidence: subagent_review (each"));
        // Only a Codex worker is told where its temporary files go (task
        // 1290): Claude Code's scratchpad is cleaned by task 1100's sweep.
        let tmp = "under $TMPDIR (a directory the runtime made for this run";
        assert!(codex[0].contains(tmp), "{}", codex[0]);
        assert!(codex[0].contains("never directly in /tmp or /private/tmp"));
        assert!(codex[0].contains("worktree's own target/"));
        assert!(!claude[0].contains("$TMPDIR"), "{}", claude[0]);
    }

    /// Task 978: a task that needs a path outside its declared paths ends
    /// in a failed receipt naming them, not in an ask (ADR-0029 decision
    /// 5); and before each worker_question the prompts, interactive and
    /// headless, first send the worker to the repository's rules on what
    /// is not asked.
    #[test]
    fn a_path_outside_the_scope_is_a_failed_receipt_and_asks_follow_the_repository_rules() {
        let scoped = Task::restore(TaskRecord {
            id: TaskId::new(7),
            title: "work".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec!["make gate".into()],
            required_evidence: Vec::new(),
            paths: vec!["docs/**".into()],
            priority: Default::default(),
            change: None,
            status: TaskStatus::InProgress,
            goal_id: None,
            context: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            worker: crate::domain::worker::Worker::CLAUDE_INTERACTIVE,
            named_mode: None,
        })
        .unwrap();
        for mode in [WorkerMode::Interactive, WorkerMode::Headless] {
            let texts = session_texts(&scoped, &run_on(Provider::Claude, mode));
            let first = &texts[0];
            assert!(first.contains("Paths you may change"), "{first}");
            assert!(!first.contains("ask instead of changing it"), "{first}");
            assert!(first.contains(
                "If the task needs another path, do not change it and do not run dagq ask: write the receipt with result failed and name in summary the paths it needs"
            ), "{first}");
            assert!(
                first.contains(&format!(
                    "{ASK_RULES_FIRST} When you need a decision you cannot make from the task and the repository, do not {}: run `dagq ask",
                    if mode == WorkerMode::Headless {
                        "end the turn with the question in your reply"
                    } else {
                        "write the question to the terminal and wait"
                    }
                )),
                "{first}"
            );
            // The nudge, fourth from the end of `session_texts` (before the
            // answer, the recovery instruction and the go-on), offers the ask
            // behind the same check.
            let nudge = texts.len() - 4;
            assert!(
                texts[nudge].contains(&format!(
                    "2. {ASK_RULES_FIRST} Otherwise, if you need a decision, run `dagq ask"
                )),
                "{}",
                texts[nudge]
            );
            if mode == WorkerMode::Headless {
                for request in &texts[1..nudge] {
                    assert!(
                        request.contains(&format!(
                            "{ASK_RULES_FIRST} If you need a decision, run `dagq ask"
                        )),
                        "{request}"
                    );
                }
            }
        }
        assert!(ASK_RULES_FIRST.contains("AGENTS.md or CLAUDE.md"));
        assert!(ASK_RULES_FIRST.contains("failed receipt"));
    }

    /// Acceptance (2): the interactive session's texts are the ones it has
    /// always been sent.
    #[test]
    fn interactive_sessions_keep_their_texts() {
        let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
        let texts = session_texts(&task, &run_on(Provider::Claude, WorkerMode::Interactive));
        let first = &texts[0];
        assert!(first.contains(STOP_BACKGROUND));
        assert!(first.contains(
            "The answer arrives in this terminal as `answer to ask <id>: ...`; continue from it."
        ));
        assert!(first.contains("do not write the question to the terminal and wait"));
        assert!(first.contains(
            "After submitting, report the outcome briefly and stop; do not run /exit yourself."
        ));
        assert!(first.contains("Perform applicable unit tests and subagent review."));
        assert!(!first.contains(HEADLESS_WORKER));
        for request in &texts[1..13] {
            assert!(request.starts_with("dagq: "), "{request}");
            assert!(request.contains(STOP_BACKGROUND), "{request}");
            assert!(
                request.ends_with(INTERACTIVE_DONE)
                    || request.contains(&format!(". {INTERACTIVE_DONE}")),
                "{request}"
            );
        }
        assert!(texts[13].starts_with(
            "dagq: run 00000000-0000-4000-8000-000000000001 has been idle for 10 minutes"
        ));
        assert_eq!(texts[14], "answer to ask 3: blue");
        assert_eq!(
            texts[15],
            format!(
                "dagq: the supervisor's recovery job for run {RUN} (alert stalled) asks: write the receipt"
            )
        );
    }

    /// A headless run's recovery job is never offered an action on a
    /// screen, a dialog or an /exit, and reads its turns instead.
    #[test]
    fn a_headless_recovery_job_is_never_offered_a_dialog() {
        let task = task(7, "work", TaskStatus::InProgress);
        let facts = json!({});
        let recovery = |run: &TaskRun, alert| {
            recovery_prompt(
                &task,
                run,
                1,
                &RecoveryMaterial {
                    alert,
                    ended: None,
                    facts: &facts,
                    workspace: "ws",
                    screen: "turns",
                    processes: Ok(Vec::new()),
                    git_status: "",
                    head: SHA,
                    receipt_commit: None,
                    history: &[],
                    allowed: &[
                        "send_instruction",
                        "answer_known_dialog",
                        "close_and_proceed",
                        "wait",
                    ],
                },
            )
            .unwrap()
        };
        let headless = recovery(
            &run_on(Provider::Codex, WorkerMode::Headless),
            RecoveryAlert::Stalled,
        );
        for never in [
            "answer_known_dialog",
            "close_and_proceed",
            "Last lines of the session's screen",
            "idle at its prompt",
            "known one",
        ] {
            assert!(!headless.contains(never), "{never}: {headless}");
        }
        assert!(headless.contains("Last turns of the headless session"));
        assert!(headless.contains("reason turn_without_receipt"));
        assert!(headless.contains("as the prompt of the session's next turn"));
        let interactive = recovery(
            &run_on(Provider::Claude, WorkerMode::Interactive),
            RecoveryAlert::Stalled,
        );
        for kept in [
            "answer_known_dialog",
            "close_and_proceed",
            "Last lines of the session's screen",
            "idle at its prompt",
            "a dialog that is not a known one",
        ] {
            assert!(interactive.contains(kept), "{kept}: {interactive}");
        }
    }

    /// The `dagq <command> ...` spans in backquotes of `text`, without
    /// `dagq`.
    fn dagq_commands(text: &str) -> Vec<&str> {
        text.split('`')
            .skip(1)
            .step_by(2)
            .filter_map(|span| span.strip_prefix("dagq "))
            .collect()
    }

    /// Every `dagq` command the prompts of the jobs that read the queue
    /// (the plan review, the goal review, the observer and the throughput
    /// review with the dagq skill's procedure it carries) and the
    /// worker's and planners' shared reading lines name goes to a use case
    /// of the queue service, so their client-mode `dagq` takes it
    /// (ADR-t1233-5 decision 1, docs/design/queue-service.md). The review
    /// and recovery jobs run no command (`JobAccess::ReadFiles`).
    #[test]
    fn every_dagq_command_a_job_s_prompt_names_is_a_use_case_of_the_queue_service() {
        use crate::domain::queue_service::UseCase;
        use crate::domain::throughput_review::{HOUR_MS, ReviewMode, window};
        let (plan, _) = plan_prompt(2, 0);
        let goal = goal_review_prompt(&GoalReviewMaterial {
            goal: json!({"id": 7}),
            tasks: Vec::new(),
            events: Vec::new(),
            previous: Vec::new(),
            gaps_in_a_row: 0,
            repo_root: Path::new("/repo"),
        });
        let observer = crate::observer::observer_prompt(
            crate::observer::ObserveMode::Hourly,
            "dagq",
            None,
            &json!({}),
        )
        .unwrap();
        let throughput = [ReviewMode::Hourly, ReviewMode::Daily, ReviewMode::Weekly]
            .map(|mode| {
                crate::throughput_review::review_prompt(
                    &window(mode, 1_790_655_900_000, 9 * HOUR_MS),
                    "dagq",
                    &json!({}),
                    Path::new("/q/input.json"),
                )
                .unwrap()
            })
            .join("\n");
        let mut named = std::collections::BTreeSet::new();
        for (who, text) in [
            ("plan review", plan.as_str()),
            ("goal review", &goal),
            ("observer", &observer),
            ("throughput review", &throughput),
            ("record reading", RECORD_READING),
            ("worker reading", WORKER_READING),
        ] {
            let commands = dagq_commands(text);
            assert!(!commands.is_empty(), "{who} names no dagq command");
            for command in commands {
                let words: Vec<&str> = command.split_whitespace().collect();
                let use_case = UseCase::of_command(&words);
                assert!(
                    use_case.is_some(),
                    "the {who} prompt names `dagq {command}`, which no use case of the queue service answers"
                );
                named.insert(use_case.unwrap().as_str());
            }
        }
        // The reads the prompts name, as docs/design/queue-service.md lists
        // them.
        for read in [
            "show",
            "proposal_show",
            "events",
            "timeline",
            "stats",
            "kpi",
            "marks",
            "search",
            "related",
            "findings",
            "goal_show",
            "forecast",
            "lint",
            "observe_history",
        ] {
            assert!(named.contains(read), "no prompt names {read}: {named:?}");
        }
    }

    /// The draft planner decides what it can recommend and records why;
    /// only what it cannot settle, a low confidence and a follow_up past
    /// ADR-t808-1's limit go to a person, with a recommendation and its
    /// confidence (ADR-t451-1 decision 5).
    #[test]
    fn the_draft_planner_decides_what_it_can_recommend() {
        let material = json!({"source_run_id": RUN, "source_task_id": 3, "index": 0});
        let draft = task(9, "follow", TaskStatus::Draft);
        let key = BundleKey::of(DraftOrigin::FollowUp, &material, draft.id());
        let members = [(
            DraftTarget {
                task: draft,
                origin: DraftOrigin::FollowUp,
                material,
                planners: 0,
            },
            1,
        )];
        let prompt = draft_planner_prompt(&DraftPlannerMaterial {
            db: Path::new("/q/queue.db"),
            key: &key,
            members: &members,
            source: None,
            receipt: None,
            goals: &[],
            answer: None,
        })
        .unwrap();
        for part in [
            "its Basic policy above all: decide what you can recommend yourself and go on, asking no one, and leave why in the record",
            "Say in its `--context` why you adopted it.",
            "record why with `dagq note --task 9 --text '<why>'`",
            "3. Ask: only for a draft you cannot decide yourself:",
            "that the queue, the repository (its code and the decisions it records) and a person's precedents cannot settle",
            "(b) your confidence in the decision is low; or (c)",
            "when none of them settles it, decide them yourself from the source, the decisions the repository records and a person's precedents, and ask a person with a `planner_question` ask as below only when that material cannot settle them and the decision is a person's (`scope` or `discard`) or your confidence in it is low.",
            "(c) it is a follow_up draft past the runtime's follow_up limit, 3 or more follow-ups from a person's judgement or with no goal or a closed goal",
            "dagq ask --task 9 --kind planner_question --because scope --recommend <adopt|cancel|keep_draft> --confidence <high|low>",
            "on keep_draft leave the draft as it is, record why with `dagq note --task 9",
            "The runtime refuses your submit of a follow_up draft past that limit unless a person answered adopt",
        ] {
            assert!(prompt.contains(part), "{part} in {prompt}");
        }
        assert!(
            !prompt.contains("when you cannot decide without a person"),
            "{prompt}"
        );
        // The runtime's prompts carry no rules of dagq's own repository.
        assert!(!prompt.contains("ADR"), "{prompt}");
    }

    /// The finding planner proposes or dismisses on its own and records
    /// why; it asks only what it cannot settle or holds with low
    /// confidence, with a recommendation (ADR-t451-1 decision 5).
    #[test]
    fn the_finding_planner_decides_what_it_can_recommend() {
        use crate::domain::{
            FindingId,
            finding::{Finding, FindingStatus, Impact},
        };
        let view = FindingView {
            finding: Finding {
                id: FindingId::new(4),
                kind: "conflict".into(),
                target: "queue".into(),
                task_id: None,
                run_id: None,
                goal_id: None,
                subject: String::new(),
                summary: "it conflicts".into(),
                detail: String::new(),
                impact: Impact::Normal,
                first_seen_at: 0,
                last_seen_at: 0,
                occurrences: 1,
                evidence: Vec::new(),
                status: FindingStatus::Open,
                status_reason: None,
                proposal_id: None,
                propose_reason: None,
                propose_requested_at: None,
                recorded_by: "observer".into(),
                updated_at: 0,
            },
            proposal_status: None,
            open_asks: Vec::new(),
            evidence_events: None,
        };
        let prompt = finding_planner_prompt(&FindingPlannerMaterial {
            db: Path::new("/q/queue.db"),
            finding: &view,
            attempt: 1,
            asks: &[],
            goal: None,
            goal_closed: false,
            siblings: &[],
            answer: None,
        })
        .unwrap();
        for part in [
            "its Basic policy above all: decide what you can recommend yourself and go on, asking no one",
            "`from finding 4 (conflict)` and saying why you chose this remedy",
            "`dagq finding dismiss 4 --reason '<why>'`, the reason saying why you decided so",
            "4. Ask: only when you cannot decide it yourself:",
            "(b) your confidence in the decision is low",
            "dagq ask --finding 4 --kind planner_question --because scope --recommend <propose|dismiss> --confidence <high|low>",
        ] {
            assert!(prompt.contains(part), "{part} in {prompt}");
        }
        assert!(
            !prompt.contains("a change that is large and hard to undo"),
            "{prompt}"
        );
        assert!(
            prompt.contains("only when that material cannot settle them and the decision is a person's (`scope` or `discard`) or your confidence in it is low."),
            "{prompt}"
        );
        assert!(!prompt.contains("rules above do not settle"), "{prompt}");
        assert!(!prompt.contains("ADR"), "{prompt}");
    }
}
