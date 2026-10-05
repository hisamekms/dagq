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
    or_none,
    prompt_fit::{self, Fit, Keep, NOT_READABLE, left_out_note, shrink},
    tail,
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
/// (`preferred`, ADR-t827-4 decision 1, ADR-t840-1): prefer them, the
/// configured package commands included, and fall back to the built-in
/// tools when the broker refuses or cannot be reached. It names no token:
/// the client reads its file.
pub const BROKER_TOOLS: &str = "The resource broker's tools are available as the MCP server `dagq-broker` (`mcp__dagq-broker__read_file`, `list_dir`, `write_file`, `edit_file`, `exec`, `git_status`, `git_diff`, `git_log`, `git_show`, `git_add`, `git_commit`, `git_restore`, `package_install`). Prefer them for reading, writing and editing files, for the commands the broker allows, for the package commands the repository configured (`package_install` runs one by its name, such as fetching dependencies), and for Git on your run branch; paths are relative to the worktree. The broker refuses paths outside the worktree, `.git`, pushes and commands it does not allow; when it refuses or cannot be reached, use the built-in tools instead.\n";

/// What a worker of a `required` run is told (ADR-t838-1): the built-in
/// file and command tools are refused, the broker's tools are the way to
/// the worktree, `dagq` is the one command Bash runs, the receipt goes
/// through `write_receipt`, and a broker that stops answering ends the
/// run with a failed receipt or an ask, never a way around it. It names no
/// token: the client reads its file.
pub const BROKER_REQUIRED: &str = "This queue runs `[broker] mode = \"required\"`: the built-in Read, Edit, Write, MultiEdit, NotebookEdit, Glob, Grep and LS are refused, and Bash runs only `dagq` commands (one `dagq ...` per call, without pipes, redirections or other commands). Work through the resource broker's tools, the MCP server `dagq-broker`: `mcp__dagq-broker__read_file`, `list_dir`, `write_file`, `edit_file`, `exec` (only the programs the broker allows, without a shell), `git_status`, `git_diff`, `git_log`, `git_show`, `git_add`, `git_commit`, `git_restore` and `package_install` (only the package commands the repository configured, by their names); paths are relative to the worktree, and the broker refuses paths outside it, `.git`, pushes and programs it does not allow. Write the receipt with `mcp__dagq-broker__write_receipt` (its `receipt` argument is the receipt's JSON object): it writes the receipt file atomically, so do not write the file yourself. A check you cannot run through `exec` is reported in the receipt as not run, with that reason. When a broker tool fails with `unauthorized`, `transport`, `config` or `protocol` (the broker is not answering, or your token is gone), do not look for another way to the files: write a failed receipt with `write_receipt` that names the error, or ask with `dagq ask` when a person must decide.\n";

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

/// The worker's step before the receipt (ADR-t1420-1): each acceptance
/// criterion is mapped to what meets it, a criterion nothing meets is
/// fixed on the spot, and one the task cannot meet is an ask or a failed
/// receipt rather than a follow_up beside a succeeded receipt. It asks for
/// no new command: the mapping reads the diff and the receipt.
pub const ACCEPTANCE_MAP: &str = "Before the receipt, map each acceptance criterion to what meets it (a changed file, a test's name, the receipt's evidence, a document's section or a command that measures it) and fix on the spot any criterion nothing meets yet. Do not write succeeded with a criterion you cannot meet moved into follow_ups: when meeting it needs a person's decision (the acceptance or the scope changes), ask a worker_question with --because scope; when it needs work outside the task, write a failed receipt saying why. In summary, give each criterion's mapping in a short phrase.\n";

/// The worker's check of the documents (ADR-t1428-1), right after
/// [`ACCEPTANCE_MAP`] and part of the same step: the documents that
/// describe the changed behavior, named by the task or not, are read
/// against the diff, and summary says what was updated or why nothing was.
/// The candidates include what a search of the repository for each changed
/// name finds, and summary gives the names searched, so the review can
/// search again (ADR-t1688-1). It names no tool and no repository path,
/// asks for no command and no document change only to show the check.
pub const DOCS_CHECK: &str = "Then check against your diff the documents on what you changed: named by the task, found as you work, or found by searching the repository for each changed name (command, flag, setting, role, path). Fix stale ones in the task's paths, file the rest as docs_drift follow_ups with path and section (one the acceptance names is a criterion above); in summary give the names searched and each path and section updated or why none was. Touch no document only to show it.\n";

/// What a resumed or revised session adds before it rewrites the receipt
/// (ADR-t1420-1): the mapping of the criteria its fix touched, again, with
/// the documents checked against the diff (ADR-t1428-1).
pub const ACCEPTANCE_REMAP: &str = "Before you rewrite it, map each acceptance criterion your fix touched to what meets it again and rewrite its phrase in summary, with the documents you checked against the diff; a criterion the task cannot meet is a worker_question (--because scope) or a failed receipt, not a follow_up.";

/// How the worker writes each follow_up (ADR-t1504-1 decision 5,
/// ADR-t1504-2 decision 11): the problem and its evidence, and an optional
/// proposal of how it relates to the goal's acceptance, which the planner
/// decides on. The worker records no judgement.
pub const FOLLOW_UP_PROPOSAL: &str = "In each follow_up's description, write the problem and its evidence. You may add membership_proposal, your proposal of how it relates to the goal's acceptance: classification required when you think the goal's acceptance cannot be met without it, out_of_scope when it can, undecided when you cannot tell; acceptance_items, the goal's acceptance items it bears on; and reason, why. It is only a proposal: the planner decides where the follow_up belongs, so do not judge or move it yourself.\n";

/// What a resumed or revised session is told of a follow_up it adds or
/// rewrites: the same proposal as [`FOLLOW_UP_PROPOSAL`], in short.
pub const FOLLOW_UP_PROPOSAL_AGAIN: &str = "Write each follow_up as before: the problem and its evidence, and optionally a membership_proposal (the goal's acceptance items it bears on, and whether you think that acceptance can be met without it), a proposal the planner decides on, not a judgement.";

/// What the worker is told of the subagent review. A Codex worker has no
/// subagent (and a nested `codex exec review` could not write its session
/// under the sandbox), so it reviews its own diff and reports the check
/// `not_applicable`; its run does not need the evidence
/// ([`crate::domain::required_of`]), and the supervisor's review job reviews
/// the commit before it lands.
fn review_line(route: Route) -> &'static str {
    match route {
        Route::Headless(Provider::Codex) => {
            "Perform applicable unit tests. You have no subagent to review your change: before the receipt, read your own diff (git diff <base commit>..HEAD) as you map the acceptance criteria to it (the step below) and fix what you find, then write subagent_review as not_applicable with the reason `codex worker: no subagent review; self-reviewed the diff, the supervisor's review job reviews the commit` and what the self-review found. Record evidence or an explicit reason when not applicable.\n"
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
         {acceptance_map}{docs_check}\
         Write a completion receipt to {receipt} using a temporary file in the same directory and atomic rename.\n\
         Receipt JSON: {{\"run_id\":\"{run_id}\",\"result\":\"succeeded or failed\",\"commit\":\"full Git SHA of the branch head\",\"tests\":{{\"status\":\"passed, failed or not_applicable\",\"evidence_or_reason\":\"...\"}},\"e2e\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"subagent_review\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"summary\":\"...\",\"follow_ups\":[{{\"title\":\"...\",\"description\":\"...\",\"category\":\"...\",\"membership_proposal\":{{\"classification\":\"required, out_of_scope or undecided\",\"acceptance_items\":[\"...\"],\"reason\":\"...\"}}}}]}}\n\
         Each of tests, e2e and subagent_review needs evidence when passed and a reason when not_applicable.\n\
         follow_ups is optional: an array of work you found outside this task, each with a title, a description and a category, for the planner to decide on; omit it when there is none. {categories}\n\
         {follow_up_proposal}\
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
        acceptance_map = ACCEPTANCE_MAP,
        docs_check = DOCS_CHECK,
        title = task.title(),
        description = task.description(),
        acceptance = task.acceptance(),
        verification = serde_json::to_string_pretty(&task.verification_commands())?,
        local_checks = local_checks("above"),
        categories = follow_up_categories_line(),
        follow_up_proposal = FOLLOW_UP_PROPOSAL,
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
/// is the last step, when none of them settles it: for a planner the
/// runtime opened, a `planner_question` ask.
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

/// The CLI that reads the queue's record (ADR-0044 decision 22), named by
/// the prompts of the planners the runtime opens and of the plan review so
/// they read evidence from the events rather than from prose.
pub const RECORD_READING: &str = "To see what happened, read the record rather than prose: \
`dagq events --full --task ID` gives a task's events with their run_id and whole payload, narrowed by `--run ID`, `--goal ID`, `--kind KIND` (repeatable), `--since TIME` and `--until TIME` (UTC, YYYY-MM-DD or YYYY-MM-DDTHH:MM:SSZ); \
without `--kind` it lists attention events only, so add `--all` for every kind; it gives the oldest 100 first, so page on with `--after <cursor>` or narrow with `--since`. \
`dagq timeline RUN` gives a run's events oldest first with each gap and its reason (idle, waiting_ask, background, after_receipt, ...).";

/// The read-only dagq commands the prompts of the planners the runtime
/// opens name to read what their limits left out, as they write them
/// (`ID` for a number): each is one a planner's role may run (task 1571,
/// ADR-t1566-1 decision 3).
pub const PLANNER_READS: &[&str] = &[
    "dagq show ID --full",
    "dagq goal show ID --full",
    "dagq proposal show ID",
    "dagq findings ID --full",
    "dagq requests ID",
    "dagq asks --all",
    "dagq events --full --task ID --kind plan_review_finished",
    "dagq events --full --task ID --kind integration_receipt",
    "dagq events --full --run ID --kind integration_receipt",
    "dagq events --full --all --after ID --limit 1",
];

/// The bytes of one text field of a goal in a planner's prompt (its
/// description or acceptance; its constraints take half), of the lines of
/// its tasks, and of all the goals of one prompt.
pub const PLANNER_GOAL_TEXT_BYTES: usize = 4_000;
pub const PLANNER_GOAL_TASKS_BYTES: usize = 4_000;
pub const PLANNER_GOALS_BYTES: usize = 16_000;

/// The bytes of the asks a planner's prompt lists (the newest first) and of
/// one question or answer in them.
pub const PLANNER_ASKS_BYTES: usize = 8_000;
pub const PLANNER_ASK_TEXT_BYTES: usize = 1_000;

/// The bytes of the answered question a planner carries for the planner
/// before it, its question and its answer each.
pub const PLANNER_ANSWER_BYTES: usize = 3_000;

/// Characters of a task's title in a list of a planner's prompt.
const PLANNER_TITLE_CHARS: usize = 200;

/// `title` cut to [`PLANNER_TITLE_CHARS`] characters.
fn short_title(title: &str) -> String {
    super::health::truncate(title, PLANNER_TITLE_CHARS).unwrap_or_else(|| title.to_owned())
}

/// The lines of `tasks`, the newest first within `bytes`, kept in their
/// order, with a note of how many were left out and how to read them.
fn planner_task_lines(
    fit: &mut Fit,
    name: &'static str,
    tasks: &[GoalTask],
    bytes: usize,
    read: &str,
) -> String {
    if tasks.is_empty() {
        return "(none)".to_owned();
    }
    let lines: Vec<String> = tasks
        .iter()
        .map(|t| {
            format!(
                "- task {} ({}): {}",
                t.id,
                t.status.as_str(),
                short_title(&t.title)
            )
        })
        .collect();
    let sizes: Vec<usize> = lines.iter().map(String::len).collect();
    let kept = prompt_fit::pick(&sizes, (0..lines.len()).rev(), usize::MAX, bytes);
    let left_out: Vec<String> = tasks
        .iter()
        .zip(&kept)
        .filter(|(_, kept)| !**kept)
        .map(|(t, _)| t.id.to_string())
        .collect();
    fit.omit(name, left_out.len());
    let mut text = lines
        .into_iter()
        .zip(&kept)
        .filter(|(_, kept)| **kept)
        .map(|(line, _)| line)
        .collect::<Vec<_>>()
        .join("\n");
    if !left_out.is_empty() {
        text.push('\n');
        text.push_str(&left_out_note("tasks (the oldest)", &left_out, read));
    }
    text
}

/// The section of a goal in a planner's prompt: `heading`, its text
/// fields, its doc when `doc`, and the lines of `tasks` (headed
/// `tasks_label`), each held to its limit.
fn planner_goal(
    fit: &mut Fit,
    heading: &str,
    goal: &Goal,
    (tasks, tasks_label): (&[GoalTask], &str),
    doc: bool,
) -> String {
    let read = format!("read it whole with `dagq goal show {} --full`", goal.id());
    let description = fit.text(
        "goals",
        or_none(goal.description()),
        PLANNER_GOAL_TEXT_BYTES,
        Keep::Start,
        &read,
    );
    let acceptance = fit.text(
        "goals",
        or_none(goal.acceptance()),
        PLANNER_GOAL_TEXT_BYTES,
        Keep::Start,
        &read,
    );
    let constraints = fit.text(
        "goals",
        or_none(goal.constraints()),
        PLANNER_GOAL_TEXT_BYTES / 2,
        Keep::Start,
        &read,
    );
    let tasks = planner_task_lines(
        fit,
        "goals",
        tasks,
        PLANNER_GOAL_TASKS_BYTES,
        &format!("`dagq goal show {} --full`", goal.id()),
    );
    format!(
        "{heading}\n\n{description}\n\nAcceptance:\n{acceptance}\n\nConstraints:\n{constraints}\n\n{doc}{tasks_label}:\n{tasks}\n",
        doc = if doc {
            format!("Doc: {}\n\n", goal.doc().unwrap_or("(none)"))
        } else {
            String::new()
        },
    )
}

/// The goals' sections of a planner's prompt within
/// [`PLANNER_GOALS_BYTES`] in their order, with a note of the goals left
/// out.
fn planner_goals(fit: &mut Fit, sections: Vec<(GoalId, String)>) -> String {
    let sizes: Vec<usize> = sections.iter().map(|(_, text)| text.len()).collect();
    let kept = prompt_fit::pick(&sizes, 0..sections.len(), usize::MAX, PLANNER_GOALS_BYTES);
    let left_out: Vec<String> = sections
        .iter()
        .zip(&kept)
        .filter(|(_, kept)| !**kept)
        .map(|((id, _), _)| id.to_string())
        .collect();
    fit.omit("goals", left_out.len());
    let mut text: String = sections
        .into_iter()
        .zip(&kept)
        .filter(|(_, kept)| **kept)
        .map(|((_, text), _)| text)
        .collect();
    if !left_out.is_empty() {
        text.push_str(&format!(
            "\n## Goals left out\n\n{}",
            left_out_note("goals", &left_out, "`dagq goal show ID --full`")
        ));
    }
    fit.section("goals", &text);
    text
}

/// The lines of `asks`, the newest first within [`PLANNER_ASKS_BYTES`] and
/// kept in their order, each question and answer cut, with a note of the
/// asks left out. `kind` adds each ask's kind.
fn planner_asks(fit: &mut Fit, asks: &[Ask], kind: bool) -> String {
    let read = "read it whole with `dagq asks --all`";
    let lines: Vec<String> = asks
        .iter()
        .map(|ask| {
            let question = fit.text(
                "asks",
                &ask.question,
                PLANNER_ASK_TEXT_BYTES,
                Keep::Start,
                read,
            );
            let answer = fit.text(
                "asks",
                ask.answer.as_deref().unwrap_or("(none yet)"),
                PLANNER_ASK_TEXT_BYTES,
                Keep::Start,
                read,
            );
            format!(
                "- ask {aid}{kind}: {question}\n  answer: {answer}\n",
                aid = ask.id,
                kind = if kind {
                    format!(" ({})", ask.kind.as_str())
                } else {
                    String::new()
                },
                question = question.replace('\n', "\n  "),
            )
        })
        .collect();
    let sizes: Vec<usize> = lines.iter().map(String::len).collect();
    let kept = prompt_fit::pick(
        &sizes,
        (0..lines.len()).rev(),
        usize::MAX,
        PLANNER_ASKS_BYTES,
    );
    let left_out: Vec<String> = asks
        .iter()
        .zip(&kept)
        .filter(|(_, kept)| !**kept)
        .map(|(ask, _)| ask.id.to_string())
        .collect();
    fit.omit("asks", left_out.len());
    let mut text: String = lines
        .into_iter()
        .zip(&kept)
        .filter(|(_, kept)| **kept)
        .map(|(line, _)| line)
        .collect();
    if !left_out.is_empty() {
        text.push_str(&left_out_note(
            "asks (the oldest)",
            &left_out,
            "`dagq asks --all`",
        ));
    }
    fit.section("asks", &text);
    text
}

/// The question and the answer of the ask a planner carries for the
/// planner before it, each held to [`PLANNER_ANSWER_BYTES`].
fn planner_answer(fit: &mut Fit, answer: &Ask) -> (String, String) {
    let read = format!("read ask {} whole with `dagq asks --all`", answer.id);
    let question = fit.text(
        "answer",
        &answer.question,
        PLANNER_ANSWER_BYTES,
        Keep::Start,
        &read,
    );
    let text = fit.text(
        "answer",
        answer.answer.as_deref().unwrap_or_default(),
        PLANNER_ANSWER_BYTES,
        Keep::Start,
        &read,
    );
    (question, text)
}

/// The bytes the whole prompt of a planner the runtime opens for a revise
/// takes at most, the language's instruction included (task 1571): in
/// production it took 2,849 bytes at the median, 5,084 at p90 and 10,976
/// at most, mostly the reasons and the tasks' lines.
pub const RUNTIME_PLANNER_PROMPT_LIMIT: usize = 32_000;

/// The bytes of the reasons plan review gave and of one reason, and of
/// the lines of the proposal's tasks.
pub const RUNTIME_PLANNER_REASONS_BYTES: usize = 12_000;
pub const RUNTIME_PLANNER_REASON_BYTES: usize = 4_000;
pub const RUNTIME_PLANNER_TASKS_BYTES: usize = 8_000;

/// The initial prompt of a planner the runtime opens for a proposal plan
/// review sent back while its own planner was closed (ADR-0041 decision
/// 12): the proposal, its tasks, and the reasons to fix. No person watches
/// the session, so what needs one goes to the inbox as an ask (decision 13).
/// The reasons and the tasks are held to their limits and the whole to
/// [`RUNTIME_PLANNER_PROMPT_LIMIT`] (task 1571, ADR-t1566-1).
pub fn runtime_planner_prompt(
    db: &Path,
    proposal: ProposalId,
    tasks: &[Task],
    reasons: &[String],
) -> Result<FittedPrompt> {
    let mut fit = Fit::new(RUNTIME_PLANNER_PROMPT_LIMIT);
    let goal_tasks: Vec<GoalTask> = tasks
        .iter()
        .map(|task| GoalTask {
            id: task.id(),
            title: task.title().to_owned(),
            status: task.status(),
            priority: task.priority(),
            priority_source: task.priority_source(),
        })
        .collect();
    let tasks = planner_task_lines(
        &mut fit,
        "tasks",
        &goal_tasks,
        RUNTIME_PLANNER_TASKS_BYTES,
        &format!("`dagq proposal show {proposal}`"),
    );
    fit.section("tasks", &tasks);
    let reason_read = format!(
        "read it whole with `dagq events --full --task ID --kind plan_review_finished` for a task of proposal {proposal}"
    );
    let lines: Vec<String> = reasons
        .iter()
        .map(|reason| {
            format!(
                "- {}",
                fit.text(
                    "reasons",
                    reason,
                    RUNTIME_PLANNER_REASON_BYTES,
                    Keep::Start,
                    &reason_read
                )
            )
        })
        .collect();
    let sizes: Vec<usize> = lines.iter().map(String::len).collect();
    let kept = prompt_fit::pick(
        &sizes,
        0..lines.len(),
        usize::MAX,
        RUNTIME_PLANNER_REASONS_BYTES,
    );
    let left_out = kept.iter().filter(|kept| !**kept).count();
    fit.omit("reasons", left_out);
    let mut reasons = if lines.is_empty() {
        "(none given)".to_owned()
    } else {
        lines
            .into_iter()
            .zip(&kept)
            .filter(|(_, kept)| **kept)
            .map(|(line, _)| line)
            .collect::<Vec<_>>()
            .join("\n")
    };
    if left_out > 0 {
        reasons.push_str(&format!(
            "\n({left_out} more reasons left out by this section's limit. To read them: `dagq events --full --task ID --kind plan_review_finished` for a task of proposal {proposal}.)"
        ));
    }
    fit.section("reasons", &reasons);
    Ok(fit.finish(format!(
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
    )))
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

/// The line of a follow_up draft's section that shows the worker's
/// membership proposal (ADR-t1504-2 decision 11) as the material keeps
/// it; empty for a draft of another origin.
fn follow_up_proposal_line(target: &DraftTarget) -> Result<String> {
    if target.origin != DraftOrigin::FollowUp {
        return Ok(String::new());
    }
    let proposal = crate::domain::follow_up_membership_proposal(&target.material);
    let material = &target.material;
    let source_goal = match material["source_goal_id"].as_i64() {
        Some(goal) => format!(
            "goal {goal} ({}, {})",
            material["source_goal_state"].as_str().unwrap_or("unknown"),
            material["source_goal_provenance"]
                .as_str()
                .unwrap_or("unknown")
        ),
        None if material["source_goal_state"].as_str() == Some("none") => "none".to_owned(),
        None => "unknown".to_owned(),
    };
    Ok(format!(
        "Source goal (at registration; judge against its acceptance with `dagq goal show <id>`, not the current goal's): {source_goal}\n\
         Membership proposal (the worker's; where you start, not a judgement): {}\n",
        if proposal.is_null() {
            "(none)".to_owned()
        } else {
            serde_json::to_string(&proposal)?
        }
    ))
}

/// The step of a follow_up draft's planner before it adopts, drops or asks
/// (ADR-t1504-1 decisions 1 to 3 and 6, ADR-t1504-2 decisions 1 and 6):
/// judge from the worker's proposal whether the source goal's acceptance
/// can be met without the draft and record it, membership apart from
/// adoption and priority, another goal found before one is made, and no
/// acceptance weakened to leave a draft out.
fn follow_up_membership_step(t: &str) -> String {
    format!(
        "For a follow_up draft whose source goal is not none, first judge where it belongs, apart from whether it is worth doing and from its priority. Start from the worker's membership proposal and decide the meaning yourself: can the source goal's acceptance be met without this draft? When it cannot, it is required and belongs to the source goal; when it can, it is out_of_scope and belongs to another goal: look for a fitting existing goal with `dagq search` first, make one only when none fits, and never park it in an unrelated large goal. Record the judgement before you adopt, drop or ask: `dagq judge-follow-up {t} --classification <required|out_of_scope|undecided> --acceptance-item '<the acceptance item>' --reason '<why that acceptance can or cannot be met without it>' --evidence '<a receipt, commit, document section or task>'`, with `--destination-goal <goal>` for out_of_scope, and `--source-goal <goal>` when the source goal is unknown. Read its earlier judgements with `dagq show {t}` (membership_judgements): one that still holds needs no new row; to change a required or out_of_scope one, record the other with `--corrects <its id>`; it never goes back to undecided. A draft you drop (a duplicate, already done, not worth doing) may skip the record when it would need a new goal, as a canceled follow-up never holds a goal open. When the acceptance, the receipt, the source and the recorded decisions cannot settle it, record undecided with why, ask as step 3 says with the membership question in it, and on the answer record required or out_of_scope before you do what it says. Moving a draft to another goal neither adopts it nor raises its priority. Never weaken a goal's acceptance to leave a follow_up out: that changes the goal's intent, so ask a person (`--because scope`).\n"
    )
}

/// The Basic policy of the dagq-planner skill (ADR-t451-1 decision 5) as
/// the planners the runtime opens for a draft or a finding read it: what
/// they can recommend they decide themselves and record why; only what
/// they cannot settle goes to a person.
const DECIDE_YOURSELF: &str = "decide what you can recommend yourself and go on, asking no one, and leave why in the record (a task's `--context`, a `note`, a `--reason`); raise to a person, with your recommendation and its confidence, only what step Ask below names.";

/// The bytes the whole prompt of a planner the runtime opens for a bundle
/// of drafts takes at most, the language's instruction included (task
/// 1571): in production it took 18,021 bytes at the median, 26,352 at p90
/// and 38,241 at most (planner 828: the source task's receipt's summary
/// 8,423, the goal 7,398, the goal's other tasks 4,303).
pub const DRAFT_PLANNER_PROMPT_LIMIT: usize = 80_000;

/// The bytes of the drafts' sections, and of one draft's title,
/// description and context.
pub const DRAFT_MEMBERS_BYTES: usize = 20_000;
pub const DRAFT_TITLE_BYTES: usize = 1_000;
pub const DRAFT_DESCRIPTION_BYTES: usize = 6_000;
pub const DRAFT_CONTEXT_BYTES: usize = 4_000;

/// The bytes of where the drafts came from (the source task, its landed
/// receipt, plan review's reason, the goal review's findings), and in it
/// of the source task's description and acceptance each, of the receipt's
/// summary (8,423 at most in production) and of its follow_ups.
pub const DRAFT_ORIGIN_BYTES: usize = 20_000;
pub const DRAFT_SOURCE_TEXT_BYTES: usize = 3_000;
pub const DRAFT_RECEIPT_SUMMARY_BYTES: usize = 12_000;
pub const DRAFT_RECEIPT_FOLLOW_UPS_BYTES: usize = 6_000;

/// The initial prompt of a planner the runtime opens for a bundle of drafts
/// the runtime or a job registered (ADR-0041 decision 16, ADR-t807-1): the
/// material, and the three things it may do with each draft — submit it
/// completed (adopt), cancel it with a note (drop), or ask the inbox a
/// `planner_question` and apply the answer typed into its terminal — and,
/// for a bundle of more than one, what to weigh between its drafts. Each
/// section is held to its limit and the whole to
/// [`DRAFT_PLANNER_PROMPT_LIMIT`]; what is left out is counted and named
/// with the read-only dagq command that reads it (task 1571, ADR-t1566-1).
pub fn draft_planner_prompt(material: &DraftPlannerMaterial<'_>) -> Result<FittedPrompt> {
    let mut fit = Fit::new(DRAFT_PLANNER_PROMPT_LIMIT);
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
    let mut drafts = Vec::new();
    for (target, attempt) in material.members {
        let task = &target.task;
        let heading = if single {
            "## The draft".to_owned()
        } else {
            format!("## Draft {} (planner {attempt} for it)", task.id())
        };
        let read = format!("read it whole with `dagq show {} --full`", task.id());
        drafts.push(format!(
            "\n{heading}\n\nTask {id}: {title}\n{category}{proposal}\n### Description\n\n{description}\n\n### Context\n\n{context}\n",
            id = task.id(),
            title = fit.text("drafts", task.title(), DRAFT_TITLE_BYTES, Keep::Start, &read),
            category = follow_up_category_line(target),
            proposal = follow_up_proposal_line(target)?,
            description = fit.required("drafts", or_none(task.description()), DRAFT_DESCRIPTION_BYTES, &read),
            context = fit.text("drafts", or_none(task.context()), DRAFT_CONTEXT_BYTES, Keep::Start, &read),
        ));
    }
    // The drafts in their order (the oldest first) within their limit.
    let sizes: Vec<usize> = drafts.iter().map(String::len).collect();
    let kept = prompt_fit::pick(&sizes, 0..drafts.len(), usize::MAX, DRAFT_MEMBERS_BYTES);
    let left_out: Vec<String> = material
        .members
        .iter()
        .zip(&kept)
        .filter(|(_, kept)| !**kept)
        .map(|((target, _), _)| target.task.id().to_string())
        .collect();
    fit.omit("drafts", left_out.len());
    let mut members: String = drafts
        .into_iter()
        .zip(&kept)
        .filter(|(_, kept)| **kept)
        .map(|(text, _)| text)
        .collect();
    if !left_out.is_empty() {
        members.push_str(&format!(
            "\n## Drafts left out\n\n{}",
            left_out_note("drafts", &left_out, "`dagq show ID --full` for each")
        ));
    }
    fit.section("drafts", &members);
    out.push_str(&members);
    let mut whence_text = format!("\n## Where it came from: {}\n\n", origin.as_str());
    let out_before_origin = std::mem::take(&mut out);
    // Only a follow_up's drafts came from a run's receipt.
    let receipt_read = first.material["source_run_id"]
        .as_str()
        .map_or_else(String::new, |run| {
            format!(
                "read it whole with `dagq events --full --run {run} --kind integration_receipt`"
            )
        });
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
                    title = short_title(source.title()),
                    status = source.status().as_str(),
                    description = fit.text("origin", or_none(source.description()), DRAFT_SOURCE_TEXT_BYTES, Keep::Start, &format!("read it whole with `dagq show {} --full`", source.id())),
                    acceptance = fit.text("origin", or_none(source.acceptance()), DRAFT_SOURCE_TEXT_BYTES, Keep::Start, &format!("read it whole with `dagq show {} --full`", source.id())),
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
                    summary = fit.text(
                        "origin",
                        or_none(receipt["summary"].as_str().unwrap_or_default()),
                        DRAFT_RECEIPT_SUMMARY_BYTES,
                        Keep::Start,
                        &receipt_read,
                    ),
                    follow_ups = fenced(
                        "json",
                        &fit.text(
                            "origin",
                            &serde_json::to_string_pretty(&receipt["follow_ups"])?,
                            DRAFT_RECEIPT_FOLLOW_UPS_BYTES,
                            Keep::Start,
                            &receipt_read,
                        ),
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
    // Everything about where the drafts came from, within its limit.
    let origin_text = std::mem::replace(&mut out, out_before_origin);
    whence_text.push_str(&origin_text);
    let whence_text = fit.text(
        "origin",
        &whence_text,
        DRAFT_ORIGIN_BYTES,
        Keep::Start,
        &if receipt_read.is_empty() {
            "`dagq show ID --full` for the drafts".to_owned()
        } else {
            format!(
                "`dagq show ID --full` for the drafts and their source task, and {receipt_read}"
            )
        },
    );
    fit.section("origin", &whence_text);
    out.push_str(&whence_text);
    let goals = material
        .goals
        .iter()
        .map(|(goal, closed, siblings)| {
            let heading = format!(
                "\n## Goal {gid}: {title}{closed}",
                gid = goal.id(),
                title = short_title(goal.title()),
                closed = if *closed { " (closed)" } else { "" },
            );
            (
                goal.id(),
                planner_goal(
                    &mut fit,
                    &heading,
                    goal,
                    (siblings, "Its other tasks"),
                    true,
                ),
            )
        })
        .collect();
    out.push_str(&planner_goals(&mut fit, goals));
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
         {membership}{each}Then do exactly one of these three{with_each}:\n\
         1. Adopt: {adopt} add its dependencies with `dagq dependency add`, check it with `dagq lint {t}` and submit it with `dagq submit {t}`. Say in its `--context` why you adopted it. Plan review checks it before it becomes ready.\n\
         2. Drop: when it is already done, duplicated or not worth doing, cancel it with `dagq cancel {t}` and record why with `dagq note --task {t} --text '<why>'`. When another task already covers it (a duplicate, or a completed task that already did it), cancel it with `dagq cancel {t} --duplicate-of <that task>` instead, so the queue records which task it duplicates, and still note why.\n\
         3. Ask: only for a draft you cannot decide yourself: (a) it needs a person's judgement, `scope` (the acceptance, the scope or a goal's decision would change with their intent) or `discard` (whether to throw work away), that the queue, the repository (its code and the decisions it records) and a person's precedents cannot settle; (b) your confidence in the decision is low; or (c) it is a follow_up draft past the runtime's follow_up limit, {FOLLOW_UP_ASK_DEPTH} or more follow-ups from a person's judgement, a source goal that was missing, closed or unknown at registration (even if its current goal is open), or no current goal or a closed current goal. Run `dagq ask --task {t} --kind planner_question --because scope --recommend <adopt|cancel|keep_draft> --confidence <high|low> --question '<everything the person needs, with your recommendation and why>' --option adopt --option cancel --option keep_draft` (`--because discard` when the question is whether to throw work away; for (c), recommend what you would do on your own), report briefly and stop. The answer arrives in this terminal as `answer to ask <id>: ...`: on adopt do 1, on cancel do 2 (the note names the ask), on keep_draft leave the draft as it is, record why with `dagq note --task {t} --text '<why>'` (naming the ask) and stop. A draft kept so stays a draft until a person has the inbox record a planning request that names it; no planner of the runtime's is opened for it again.\n\
         The runtime refuses your submit of a follow_up draft past that limit unless a person answered adopt or already adopted it: ask then, as (c) says. Membership changes (`set-goal` or `judge-follow-up`) do not count as adoption or reset depth; an existing person's adopt remains valid.\n\
         When you are done, report the outcome in one or two sentences and stop; the runtime ends this session. Do not work on anything but {this}. Never open the queue database directly; use the dagq CLI only.\n",
        membership = if origin == DraftOrigin::FollowUp {
            follow_up_membership_step(&t)
        } else {
            String::new()
        },
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
        let (question, text) = planner_answer(&mut fit, answer);
        let carried = format!(
            "\nThe planner before you asked a person (ask {aid}) about draft {task} and is gone:\n{question}\n\nanswer to ask {aid}: {text}\n\nApply this answer as step 3 says.\n",
            aid = answer.id,
            task = answer.task_id.map_or("?".to_owned(), |t| t.to_string()),
        );
        fit.section("answer", &carried);
        out.push_str(&carried);
    }
    Ok(fit.finish(out))
}

/// The bytes the whole prompt of a planner the runtime opens for a finding
/// takes at most, the language's instruction included (task 1571): in
/// production it took 37,974 bytes at the median, 156,358 at p90 and
/// 276,417 at most (planner 768: finding 44's evidence took 270,669).
pub const FINDING_PLANNER_PROMPT_LIMIT: usize = 80_000;

/// The bytes of the finding's evidence events, the newest first, and of
/// one event.
pub const FINDING_EVIDENCE_BYTES: usize = 28_000;
pub const FINDING_EVENT_BYTES: usize = 8_000;

/// The bytes of the finding's detail, and of its summary, subject and why
/// it is proposed each.
pub const FINDING_DETAIL_BYTES: usize = 8_000;
pub const FINDING_SHORT_BYTES: usize = 2_000;

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
/// `planner_question` for a person. Each section is held to its limit and
/// the whole to [`FINDING_PLANNER_PROMPT_LIMIT`]: the evidence newest
/// first; what is left out is counted and named with the read-only dagq
/// command that reads it (task 1571, ADR-t1566-1).
pub fn finding_planner_prompt(material: &FindingPlannerMaterial<'_>) -> Result<FittedPrompt> {
    let mut fit = Fit::new(FINDING_PLANNER_PROMPT_LIMIT);
    let view = material.finding;
    let finding = &view.finding;
    let id = finding.id;
    let read = format!("read it whole with `dagq findings {id} --full`");
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
    let head = format!(
        "\n## Finding {id}: {summary}\n\n- kind: {kind}\n- on: {target}\n- subject: {subject}\n- impact: {impact}\n- occurrences: {occurrences}, first seen {first}, last seen {last} (Unix seconds)\n- recorded by: {by}\n- why a proposal: {why}\n\n### Detail\n\n{detail}\n",
        summary = fit.text(
            "finding",
            &finding.summary,
            FINDING_SHORT_BYTES,
            Keep::Start,
            &read
        ),
        kind = finding.kind,
        subject = fit.text(
            "finding",
            or_none(&finding.subject),
            FINDING_SHORT_BYTES,
            Keep::Start,
            &read
        ),
        impact = finding.impact.as_str(),
        occurrences = finding.occurrences,
        first = finding.first_seen_at,
        last = finding.last_seen_at,
        by = finding.recorded_by,
        why = fit.text(
            "finding",
            finding.propose_reason.as_deref().unwrap_or("(none)"),
            FINDING_SHORT_BYTES,
            Keep::Start,
            &read
        ),
        detail = fit.required(
            "finding",
            or_none(&finding.detail),
            FINDING_DETAIL_BYTES,
            &read
        ),
    );
    fit.section("finding", &head);
    out.push_str(&head);
    out.push_str("\n### Its evidence\n\n");
    match &view.evidence_events {
        Some(events) if !events.is_empty() => {
            // The newest evidence first within the section's limit.
            let values = events
                .iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let (kept, left_out) = fit.lines(
                "evidence",
                &values,
                (0..values.len()).rev(),
                (usize::MAX, FINDING_EVIDENCE_BYTES, FINDING_EVENT_BYTES),
                &read,
            );
            let mut evidence = fenced("json", &kept.join("\n"));
            if !left_out.is_empty() {
                let ids: Vec<String> = left_out
                    .iter()
                    .map(|&index| events[index].id.to_string())
                    .collect();
                evidence.push_str(&left_out_note(
                    "evidence events (the oldest)",
                    &ids,
                    &format!("`dagq findings {id} --full`, or one event with `dagq events --full --all --after <its ID - 1> --limit 1`"),
                ));
            }
            fit.section("evidence", &evidence);
            out.push_str(&evidence);
        }
        _ => out.push_str("(no event)\n"),
    }
    out.push_str(&format!(
        "Read more with `dagq findings {id} --full`. {RECORD_READING}\n"
    ));
    if !material.asks.is_empty() {
        out.push_str("\n### Asks about it\n\n");
        out.push_str(&planner_asks(&mut fit, material.asks, true));
    }
    match material.goal {
        Some(goal) => {
            let heading = format!(
                "\n## Goal {gid} of its target: {title}{closed}",
                gid = goal.id(),
                title = short_title(goal.title()),
                closed = if material.goal_closed {
                    " (closed)"
                } else {
                    ""
                },
            );
            let section = planner_goal(
                &mut fit,
                &heading,
                goal,
                (material.siblings, "Its tasks"),
                false,
            );
            out.push_str(&planner_goals(&mut fit, vec![(goal.id(), section)]));
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
         An improvement's tasks are `--priority normal` or `low`, never higher (without --priority a task inherits its goal's): plan review lowers a higher one to normal.\n\
         3. Dismiss: when a task already remedies it (name the task), it no longer occurs, or it is not worth remedying, run `dagq finding dismiss {id} --reason '<why>'`, the reason saying why you decided so.\n\
         4. Ask: only when you cannot decide it yourself: (a) it needs a person's judgement, `scope` (the plan's intent, an acceptance, a contradiction with a goal's constraints or a decision the repository records, a precedent a person answered otherwise) or `discard` (whether to throw work away), that the queue, the repository (its code and the decisions it records) and a person's precedents cannot settle; or (b) your confidence in the decision is low. Run `dagq ask --finding {id} --kind planner_question --because scope --recommend <propose|dismiss> --confidence <high|low> --question '<everything the person needs, with your recommendation and why>' --option propose --option dismiss` (`--because discard` when the question is whether to throw work away), report briefly and stop. The answer arrives in this terminal as `answer to ask <id>: ...`: follow it (propose: do 1 or 2; dismiss: do 3).\n\
         When you are done, report the outcome in one or two sentences and stop; the runtime ends this session. Do not work on anything but this finding. Never open the queue database directly; use the dagq CLI only.\n",
        kind = finding.kind,
        rules = repository_rules(RUNTIME_PLANNER_ASK),
    ));
    if let Some(answer) = material.answer {
        let (question, text) = planner_answer(&mut fit, answer);
        let carried = format!(
            "\nThe planner before you asked a person (ask {aid}) and is gone:\n{question}\n\nanswer to ask {aid}: {text}\n\nApply this answer as step 4 says.\n",
            aid = answer.id,
        );
        fit.section("answer", &carried);
        out.push_str(&carried);
    }
    Ok(fit.finish(out))
}

/// What a planning request refers to, as its planner's prompt shows it
/// (ADR-t1394-1 decision 5).
#[derive(Debug, Clone)]
pub enum RequestRefMaterial {
    /// An ask: its question and answer.
    Ask(Ask),
    /// A task, with the receipt of its last landed run.
    Task {
        task: Box<Task>,
        receipt: Option<Value>,
    },
    /// A run, with its task and its landed receipt.
    Run {
        run: RunId,
        task: Option<TaskId>,
        receipt: Option<Value>,
    },
    /// An event, payload and all.
    Event(RunEvent),
    /// A finding with its reading and evidence.
    Finding(Box<FindingView>),
    /// A goal: its section below shows it.
    Goal(GoalId),
    /// A reference that could not be read (gone, or not of this queue).
    Unreadable {
        reference: crate::domain::plan_request::RequestRef,
        error: String,
    },
}

/// The bytes the whole prompt of a planner the runtime opens for a
/// planning request takes at most, the language's instruction included
/// (task 1571). Production had no such prompt yet: the limit is the other
/// planners' (the draft planner's p90 26,352 and largest 38,241 fit in
/// it), and the unit test's largest input
/// (`a_request_planner_prompt_of_huge_references_stays_within_its_limits`)
/// measures what the sections' limits add up to.
pub const REQUEST_PLANNER_PROMPT_LIMIT: usize = 80_000;

/// The bytes of the inbox's note, of the references' sections (in the
/// order the inbox gave them) and of one reference.
pub const REQUEST_NOTE_BYTES: usize = 8_000;
pub const REQUEST_REFS_BYTES: usize = 32_000;
pub const REQUEST_REF_BYTES: usize = 8_000;

/// What the initial prompt of a planner the runtime opens for a planning
/// request is made of.
pub struct RequestPlannerMaterial<'a> {
    pub db: &'a Path,
    pub request: &'a crate::domain::plan_request::PlanRequest,
    /// The sentence that points at the file the request's words were
    /// handed over in ([`super::planner_handoff`]).
    pub handed: &'a str,
    /// Which planner of the runtime's this is for the request (1-based).
    pub attempt: usize,
    pub refs: &'a [RequestRefMaterial],
    /// The goals the references lead to: each with whether it is closed
    /// and its tasks.
    pub goals: &'a [(Goal, bool, Vec<GoalTask>)],
    /// The earlier planners' questions about the request.
    pub asks: &'a [Ask],
    /// The answered `planner_question` of a planner that is gone.
    pub answer: Option<&'a Ask>,
}

/// The initial prompt of a planner the runtime opens for a planning
/// request (ADR-t1394-1 decision 5): the file the person's words were
/// handed over in, the inbox's note apart from them, what the request
/// refers to, the goals it leads to, how to look for what already covers
/// it, the Basic policy of what to raise to a person, and what it may do:
/// submit a proposal, decline the request with a reason, or ask a
/// `planner_question` about it. Each section is held to its limit and the
/// whole to [`REQUEST_PLANNER_PROMPT_LIMIT`]; what is left out is counted
/// and named with the read-only dagq command that reads it (task 1571,
/// ADR-t1566-1).
pub fn request_planner_prompt(material: &RequestPlannerMaterial<'_>) -> Result<FittedPrompt> {
    let mut fit = Fit::new(REQUEST_PLANNER_PROMPT_LIMIT);
    let request = material.request;
    let id = request.id;
    let mut out = format!(
        "You are a planner the dagq runtime opened for planning request {id} of the queue at {db}; no person watches this session. A person asked the inbox for a plan, and the inbox recorded the request for you (planner {attempt} of at most {max} the runtime opens for it).\n",
        db = super::path_text(material.db)?,
        attempt = material.attempt,
        max = crate::domain::plan_request::MAX_REQUEST_PLANNERS,
    );
    out.push_str(&format!(
        "\n## Request {id}\n\n{handed} Those are the person's own words (recorded by the {by} at {at}, Unix seconds).\n",
        handed = material.handed,
        by = request.requested_by,
        at = request.created_at,
    ));
    if let Some(note) = &request.note {
        let note = format!(
            "\nThe inbox added this, apart from the person's words:\n\n{}\n",
            fit.text(
                "note",
                note,
                REQUEST_NOTE_BYTES,
                Keep::Start,
                &format!("read it whole with `dagq requests {id}`"),
            )
        );
        fit.section("note", &note);
        out.push_str(&note);
    }
    if !material.refs.is_empty() {
        out.push_str("\n## What it refers to\n");
    }
    // Each reference within its limit, in the order the inbox gave them.
    let mut refs = Vec::new();
    for reference in material.refs {
        let (label, read, text) = request_ref(reference)?;
        let text = fit.text(
            "refs",
            &text,
            REQUEST_REF_BYTES,
            Keep::Start,
            &format!("read it whole with {read}"),
        );
        refs.push((label, read, text));
    }
    let sizes: Vec<usize> = refs.iter().map(|(_, _, text)| text.len()).collect();
    let kept = prompt_fit::pick(&sizes, 0..refs.len(), usize::MAX, REQUEST_REFS_BYTES);
    let mut referred = String::new();
    let mut left_out = Vec::new();
    for ((label, read, text), kept) in refs.into_iter().zip(&kept) {
        if *kept {
            referred.push_str(&text);
        } else {
            left_out.push(format!("{label} ({read})"));
        }
    }
    fit.omit("refs", left_out.len());
    if !left_out.is_empty() {
        referred.push_str(&format!(
            "\n### Left out\n\n{}",
            left_out_note("references", &left_out, "the command named with each")
        ));
    }
    fit.section("refs", &referred);
    out.push_str(&referred);
    if material.goals.is_empty() {
        out.push_str("\n## Goals\n\nNothing it refers to belongs to a goal. `dagq goal list` shows the open goals.\n");
    }
    let goals = material
        .goals
        .iter()
        .map(|(goal, closed, tasks)| {
            let heading = format!(
                "\n## Goal {gid}: {title}{closed}",
                gid = goal.id(),
                title = short_title(goal.title()),
                closed = if *closed { " (closed)" } else { "" },
            );
            (
                goal.id(),
                planner_goal(&mut fit, &heading, goal, (tasks, "Its tasks"), false),
            )
        })
        .collect();
    out.push_str(&planner_goals(&mut fit, goals));
    if !material.asks.is_empty() {
        out.push_str("\n## Earlier questions about it\n\n");
        out.push_str(&planner_asks(&mut fit, material.asks, false));
    }
    out.push_str(&format!(
        "\n## What to do\n\n\
         Follow the dagq-planner skill of the dagq plugin, its Basic policy above all: {DECIDE_YOURSELF} Read the repository's AGENTS.md (or CLAUDE.md) first, and the documents it names for what you plan. {rules} Before you plan, look for tasks that already cover the request or code that already does it (`dagq search '<words>'`, `dagq related ID` for a task, `dagq show ID`, `dagq goal list`, the source). {RECORD_READING}\n\
         Then do exactly one of these:\n\
         1. Plan it: write the goal (`dagq goal add --draft ...`) or the tasks for an open goal (`dagq add --goal GOAL ...`) the request asks for, with `--context` beginning with `from request {id}` and saying why you planned it so, check them with `dagq lint`, and submit them with `dagq submit ...`. Your submission makes request {id} proposed with the proposal, and plan review checks it before it becomes ready; you need no person's approval for it, even for a new goal. You may submit more than one proposal for it.\n\
         2. Decline: when nothing should be planned of it (it is done already: name the task or the code; it duplicates a task in flight: name it; or it cannot be planned as asked: say why), run `dagq request decline {id} --reason '<why>'`. The inbox tells the person, who may ask again in other words.\n\
         3. Ask: only when you cannot decide it yourself: (a) it needs a person's judgement, `scope` (the plan's intent, an acceptance, a contradiction with a goal's constraints or a decision the repository records, a precedent a person answered otherwise) or `discard` (whether to throw work away), that the queue, the repository (its code and the decisions it records) and a person's precedents cannot settle; or (b) your confidence in the decision is low. Run `dagq ask --request {id} --kind planner_question --because scope --recommend <plan|decline> --confidence <high|low> --question '<everything the person needs, with your recommendation and why>' --option plan --option decline` (`--because discard` when the question is whether to throw work away), report briefly and stop. The answer arrives in this terminal as `answer to ask <id>: ...`: follow it (plan: do 1 as it says; decline: do 2).\n\
         When you are done, report the outcome in one or two sentences and stop; the runtime ends this session. Do not work on anything but this request. Never open the queue database directly; use the dagq CLI only.\n",
        rules = repository_rules(RUNTIME_PLANNER_ASK),
    ));
    if let Some(answer) = material.answer {
        let (question, text) = planner_answer(&mut fit, answer);
        let carried = format!(
            "\nThe planner before you asked a person (ask {aid}) and is gone:\n{question}\n\nanswer to ask {aid}: {text}\n\nApply this answer as step 3 says.\n",
            aid = answer.id,
        );
        fit.section("answer", &carried);
        out.push_str(&carried);
    }
    Ok(fit.finish(out))
}

/// One reference of a planning request as its planner's prompt shows it:
/// how the prompt names it, the read-only dagq command that reads it
/// whole, and its section.
fn request_ref(reference: &RequestRefMaterial) -> Result<(String, String, String)> {
    let mut out = String::new();
    let (label, read) = match reference {
        RequestRefMaterial::Ask(ask) => {
            out.push_str(&format!(
                "\n### Ask {aid} ({kind})\n\n{question}\n\nanswer: {answer}\n",
                aid = ask.id,
                kind = ask.kind.as_str(),
                question = ask.question,
                answer = ask.answer.as_deref().unwrap_or("(none yet)"),
            ));
            (format!("ask {}", ask.id), "`dagq asks --all`".to_owned())
        }
        RequestRefMaterial::Task { task, receipt } => {
            out.push_str(&format!(
                "\n### Task {tid} ({status}): {title}\n\n{description}\n\nAcceptance:\n{acceptance}\n",
                tid = task.id(),
                status = task.status().as_str(),
                title = short_title(task.title()),
                description = or_none(task.description()),
                acceptance = or_none(task.acceptance()),
            ));
            push_receipt(&mut out, receipt.as_ref())?;
            (
                format!("task {}", task.id()),
                format!(
                    "`dagq show {tid} --full` and `dagq events --full --task {tid} --kind integration_receipt`",
                    tid = task.id()
                ),
            )
        }
        RequestRefMaterial::Run { run, task, receipt } => {
            out.push_str(&format!(
                "\n### Run {run}{of}\n",
                of = task
                    .map(|task| format!(" (task {task})"))
                    .unwrap_or_default(),
            ));
            push_receipt(&mut out, receipt.as_ref())?;
            (
                format!("run {run}"),
                format!("`dagq events --full --run {run} --kind integration_receipt`"),
            )
        }
        RequestRefMaterial::Event(event) => {
            out.push_str(&format!(
                "\n### Event {eid} ({kind})\n\n",
                eid = event.id,
                kind = event.kind,
            ));
            out.push_str(&fenced(
                "json",
                &serde_json::to_string_pretty(&event.payload)?,
            ));
            (
                format!("event {}", event.id),
                format!(
                    "`dagq events --full --all --after {} --limit 1`",
                    event.id.as_i64() - 1
                ),
            )
        }
        RequestRefMaterial::Finding(view) => {
            let finding = &view.finding;
            out.push_str(&format!(
                "\n### Finding {fid} ({kind}, {status}): {summary}\n\n{detail}\n\nRead its evidence with `dagq findings {fid} --full`.\n",
                fid = finding.id,
                kind = finding.kind,
                status = finding.status.as_str(),
                summary = finding.summary,
                detail = or_none(&finding.detail),
            ));
            (
                format!("finding {}", finding.id),
                format!("`dagq findings {} --full`", finding.id),
            )
        }
        RequestRefMaterial::Goal(goal) => {
            out.push_str(&format!("\n### Goal {goal}\n\nSee the goals below.\n"));
            (
                format!("goal {goal}"),
                format!("`dagq goal show {goal} --full`"),
            )
        }
        RequestRefMaterial::Unreadable { reference, error } => {
            out.push_str(&format!(
                "\n### {reference}\n\nIt could not be read: {error}\n"
            ));
            (
                reference.to_string(),
                "nothing: it could not be read".to_owned(),
            )
        }
    };
    Ok((label, read, out))
}

/// The `summary` and `follow_ups` of a landed receipt, or that there is
/// none.
fn push_receipt(out: &mut String, receipt: Option<&Value>) -> Result<()> {
    match receipt {
        Some(receipt) => {
            out.push_str(&format!(
                "\nIts landed receipt's summary:\n\n{summary}\n\nIts follow_ups:\n\n",
                summary = or_none(receipt["summary"].as_str().unwrap_or_default()),
            ));
            out.push_str(&fenced(
                "json",
                &serde_json::to_string_pretty(&receipt["follow_ups"])?,
            ));
        }
        None => out.push_str("\nNo landed receipt.\n"),
    }
    Ok(())
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
        "5. Rewrite the receipt at {receipt} with the new head commit, writing a temporary file in the same directory and renaming it. {ACCEPTANCE_REMAP} {FOLLOW_UP_PROPOSAL_AGAIN}"
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

/// The bytes the whole run review prompt takes at most, the language's
/// instruction included (task 1571, ADR-t1566-1 decision 4): in production
/// it took 6,385 bytes at the median, 10,218 at p90 and 18,535 at most.
pub const RUN_REVIEW_PROMPT_LIMIT: usize = 32_000;

/// The bytes of the task's title, of its acceptance (1,544 at most in
/// production) and of the required subagents' list in the review prompt.
pub const RUN_REVIEW_TITLE_BYTES: usize = 1_000;
pub const RUN_REVIEW_ACCEPTANCE_BYTES: usize = 8_000;
pub const RUN_REVIEW_SUBAGENTS_BYTES: usize = 8_000;

/// What the headless reviewer is asked (ADR-0023 decision 2, ADR-0027
/// decision 2 as ADR-t451-1 decision 3 amends it): where the material is,
/// the task's acceptance, the verdict schema, where `revise` ends and
/// `concern` begins, and how a concern is judged and when it reaches a
/// person; then the required subagents (`subagents`, from
/// [`super::review::review_subagents_prompt`]), when there are any. The
/// title, the acceptance and the subagents are held to their limits and
/// the whole to [`RUN_REVIEW_PROMPT_LIMIT`]; what is cut says where in
/// the worktree or the run directory the job reads it whole (task 1571,
/// ADR-t1566-1 decision 3).
pub fn review_prompt(
    task: &Task,
    run: &TaskRun,
    review_path: &str,
    subagents: Option<&str>,
) -> FittedPrompt {
    let mut fit = Fit::new(RUN_REVIEW_PROMPT_LIMIT);
    let material = format!("read it whole in the review material at {review_path}");
    let title = fit.text(
        "title",
        task.title(),
        RUN_REVIEW_TITLE_BYTES,
        Keep::Start,
        &material,
    );
    fit.section("title", &title);
    let acceptance = fit.required(
        "acceptance",
        or_none(task.acceptance()),
        RUN_REVIEW_ACCEPTANCE_BYTES,
        &format!("{material}, its section Acceptance"),
    );
    fit.section("acceptance", &acceptance);
    // The list of agents is held to its limit; how to run them and report
    // their results is never cut.
    let subagents = subagents.map_or_else(String::new, |text| {
        let instruction = super::review::SUBAGENTS_INSTRUCTION;
        let list = text.strip_suffix(instruction).unwrap_or(text);
        let mut kept = fit.text(
            "subagents",
            list,
            RUN_REVIEW_SUBAGENTS_BYTES - instruction.len(),
            Keep::Start,
            "every agent and the paths that selected it are in the file the list names",
        );
        if kept.len() != list.len() {
            kept.push('\n');
        }
        if list.len() != text.len() {
            kept.push_str(instruction);
        }
        fit.section("subagents", &kept);
        kept
    });
    let mut text = format!(
        "You review run {run_id} of dagq task {task_id} ({title}) before it lands.\n\
         Read the review material at {review_path}: the task, its goal, the receipt, the commits and the full diff. Read the worktree if you need more. Do not change any file.\n\
         {rules}\n\
         {docs}\n\
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
        docs = REVIEW_DOCS_CHECK,
        concern = CONCERN_RECOMMENDATION,
        codes = reason_codes_section(review_reason::REVIEW_CODES),
        rules = REVIEW_RULES,
    );
    text.push_str(&subagents);
    fit.finish(text)
}

/// How a review checks the documents on the changed behavior
/// (ADR-t1428-1 decision 5): against the diff and the summary, never taking
/// a document's diff alone as proof, and naming stale ones as before.
pub const REVIEW_DOCS_CHECK: &str = "Read the documents on the behavior the diff changes (named by the task's description or context or by the summary, or found as you read) against the diff and the summary. A document's diff alone does not show the change is right; check the summary's reason for leaving a document as it is like any other claim. Report a stale document as docs_drift whether or not the task named it.\n";

/// Where the review finds the repository's rules: its instructions in the
/// worktree, which a Claude review that loads no setting sources no longer
/// gets as its memory (ADR-t1470-1 decision 2).
const REVIEW_RULES: &str = "The repository's rules are in its instructions at the root of the worktree (AGENTS.md and CLAUDE.md, whichever it has) and the documents they name: read the instructions, and of what they name the rules that bear on this change, and judge the diff by them.";

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

/// What each allowed action does, for the recovery prompt of a worker
/// run: an instruction is the prompt of the session's next turn (every
/// worker run is headless since task 1437).
fn recovery_action_help(action: &str) -> &'static str {
    match action {
        "send_instruction" => {
            "{\"action\": \"send_instruction\", \"instruction\": string}: send this instruction once, as the prompt of the session's next turn (a resume of the same session; its previous turn has ended), for example to rerun the tests in the foreground, commit and write the receipt."
        }
        "stop_processes" => {
            "{\"action\": \"stop_processes\", \"pids\": [pid, ...]}: stop these processes (SIGTERM, then SIGKILL after a grace). Only processes listed below as the run's own are allowed; any other pid makes the whole verdict an escalation. Use it for a background process the session waits for that will not end by itself (an orphan holding a pipe, a hung test). Never the session's own wrapper or agent."
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
        _ => "",
    }
}

/// The recovery actions that answered the retired interactive session's
/// screen, its dialogs and its `/exit`: never offered, and refused in a
/// verdict (task 1437).
pub(crate) const HEADLESS_NEVER: [&str; 2] = ["answer_known_dialog", "close_and_proceed"];

/// The bytes the whole recovery job prompt takes at most, the language's
/// instruction included (task 1571, ADR-t1566-1 decision 4): in production
/// it took 19,773 bytes at the median, 27,553 at p90 and 66,814 at most,
/// of which a headless session's last turns took 46,559. The limit is the
/// sum of the sections' limits below (81,000 bytes) and the instructions
/// (about 6,000) with room for the language's instruction.
pub const RECOVERY_PROMPT_LIMIT: usize = 96_000;

/// The bytes of the task's title, description (2,166 at most in
/// production), acceptance and verification commands.
pub const RECOVERY_TITLE_BYTES: usize = 1_000;
pub const RECOVERY_DESCRIPTION_BYTES: usize = 6_000;
pub const RECOVERY_ACCEPTANCE_BYTES: usize = 4_000;
pub const RECOVERY_VERIFY_BYTES: usize = 2_000;

/// The bytes of the alert's facts (6,806 at most in production).
pub const RECOVERY_FACTS_BYTES: usize = 12_000;

/// The bytes of the screen's end, or of a headless session's last turns
/// (46,559 at most in production), newest first.
pub const RECOVERY_SCREEN_BYTES: usize = 16_000;

/// The bytes of the run's processes and of its worktree's `git status`.
pub const RECOVERY_PROCESSES_BYTES: usize = 4_000;
pub const RECOVERY_STATUS_BYTES: usize = 4_000;

/// The bytes of the earlier verdicts, repairs and `task_edited` (6,748 at
/// most in production), newest first, and of one of them.
pub const RECOVERY_HISTORY_BYTES: usize = 8_000;
pub const RECOVERY_HISTORY_ITEM_BYTES: usize = 2_000;

/// The bytes of what a run that ended adds ([`ended_run_material`]: its
/// error, receipt, logs, final screen, turns and events).
pub const RECOVERY_ENDED_BYTES: usize = 24_000;

/// What the recovery job of an alert is asked (ADR-0047 decisions 39 and
/// 40): the alert, the task, the screen, the run's processes, the
/// worktree's state and the run's earlier repairs, for a run that ended
/// also its error, receipt, logs and events, then the allowed actions and
/// the verdict schema. Each section is held to its limit and the whole to
/// [`RECOVERY_PROMPT_LIMIT`]; what is cut names the file in the run
/// directory or the worktree it is in, or says the job cannot read it
/// (the job reads files only, ADR-t1566-1 decision 3; task 1571).
pub fn recovery_prompt(
    task: &Task,
    run: &TaskRun,
    attempt: usize,
    material: &RecoveryMaterial<'_>,
) -> Result<FittedPrompt> {
    let mut fit = Fit::new(RECOVERY_PROMPT_LIMIT);
    let claimed = "the task as the run claimed it is in prompt.txt of the run directory, and a later edit is a task_edited below";
    let title = fit.text(
        "task",
        task.title(),
        RECOVERY_TITLE_BYTES,
        Keep::Start,
        claimed,
    );
    let description = fit.required(
        "task",
        or_none(task.description()),
        RECOVERY_DESCRIPTION_BYTES,
        claimed,
    );
    let acceptance = fit.required(
        "task",
        or_none(task.acceptance()),
        RECOVERY_ACCEPTANCE_BYTES,
        claimed,
    );
    let verification = fit.required(
        "task",
        &task.verification_commands().join("\n"),
        RECOVERY_VERIFY_BYTES,
        claimed,
    );
    let pretty = serde_json::to_string_pretty(material.facts)?;
    // Cut as a value, so that what is kept stays JSON.
    let facts = if pretty.len() > RECOVERY_FACTS_BYTES - 16 {
        fenced(
            "json",
            &fit.json(
                "facts",
                material.facts,
                RECOVERY_FACTS_BYTES - 16,
                NOT_READABLE,
            ),
        )
    } else {
        fenced("json", &pretty)
    };
    fit.section("facts", &facts);
    // Every worker run is headless since task 1437, a historical
    // interactive one too: its material is its last turns, newest first.
    let screen = fenced(
        "text",
        &fit.text(
            "screen",
            or_none(material.screen.trim()),
            RECOVERY_SCREEN_BYTES,
            Keep::Start,
            "each turn's whole output is in turns/turn-NNNNNN.jsonl of the run directory",
        ),
    );
    fit.section("screen", &screen);
    let ended = material.ended.as_deref().map_or_else(String::new, |ended| {
        let ended = fit.text(
            "ended",
            ended,
            RECOVERY_ENDED_BYTES,
            Keep::Start,
            "the run directory has the receipt, the verification logs (integrate-*-verify-*.log, verify-*.log), terminal-final.txt and turns/",
        );
        fit.section("ended", &ended);
        ended
    });
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
    let processes = fit.text(
        "processes",
        &processes,
        RECOVERY_PROCESSES_BYTES,
        Keep::Start,
        NOT_READABLE,
    );
    fit.section("processes", &processes);
    let status = fenced(
        "text",
        &fit.text(
            "git_status",
            or_none(material.git_status.trim()),
            RECOVERY_STATUS_BYTES,
            Keep::Start,
            &format!(
                "read the files of the worktree at {}",
                run.worktree_path().unwrap_or("none")
            ),
        ),
    );
    fit.section("git_status", &status);
    let (kept, left_out) = fit.lines(
        "history",
        material.history,
        (0..material.history.len()).rev(),
        (
            usize::MAX,
            RECOVERY_HISTORY_BYTES,
            RECOVERY_HISTORY_ITEM_BYTES,
        ),
        NOT_READABLE,
    );
    let mut history = if kept.is_empty() {
        "none".to_owned()
    } else {
        kept.join("\n")
    };
    if !left_out.is_empty() {
        let ids: Vec<String> = left_out
            .iter()
            .map(|&index| {
                material.history[index]
                    .get("id")
                    .map_or_else(|| format!("#{}", index + 1), Value::to_string)
            })
            .collect();
        history.push('\n');
        history.push_str(&left_out_note("of them (the oldest)", &ids, NOT_READABLE));
    }
    fit.section("history", &history);
    // A headless session has no screen, dialog or `/exit`: the actions
    // that answer them are never offered (ADR-t813-1 decision 9).
    let actions = material
        .allowed
        .iter()
        .filter(|action| !HEADLESS_NEVER.contains(action))
        .map(|action| format!("- {}", recovery_action_help(action)))
        .collect::<Vec<_>>()
        .join("\n");
    let text = format!(
        "You are dagq's recovery job (attempt {attempt}) for run {run_id} of task {task_id} ({title}), {state}. The supervisor raised the alert {alert}: {meaning}\n\
         Decide whether the runtime can repair it with one of the allowed actions below, or whether a person has to look.\n\
         Read only: the material below, and the files it names if you need more (the worktree is {worktree}, the run directory {run_dir}). Do not change any file and do not run commands; the runtime applies your verdict. The material is held to limits: what was cut says how much and in which file it is, or that you cannot read it.\n\n\
         Task description:\n{description}\n\n\
         Acceptance criteria:\n{acceptance}\n\n\
         Current task verification commands (use these, including after a person's correction):\n{verification}\n\n\
         Alert facts:\n{facts}\n\n\
         {ended}\
         Last turns of the headless session (it has no screen):\n{screen}\n\n\
         Processes of the run (working directory in the worktree, or under the session's wrapper; the wrapper and the agent themselves are not listed):\n{processes}\n\n\
         Worktree: HEAD {head}, receipt commit {receipt}, git status:\n{status}\n\n\
         Earlier recovery verdicts, repairs, and task_edited events:\n{history}\n\n\
         Allowed actions:\n{actions}\n\
         Not allowed, ever: cancelling the task, retrying a run that has commits, editing the task, landing without review, writing to main, pushing, deleting branches or worktrees, touching anything outside this run's worktree and workspace, writing the queue database, typing into the session (a headless session takes no keys; an instruction goes as its next turn). If the repair needs any of these, escalate.\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"repair\" | \"escalate\", \"confidence\": \"high\" | \"low\", \"diagnosis\": string, \"actions\": [action, ...], \"question\": string, \"options\": [string, ...], \"reason_category\": \"recovery_failed\" | \"discard\" | \"scope\"}}\n\
         diagnosis says what you found in one or two sentences. repair needs at least one action and is applied only with confidence high; with confidence low, or with escalate, a person is asked, with your actions as the recommendation, question as the question and options added to theirs. If a broken verification command caused an ended run to fail, offer `edit the task's --verify, then retry_inherit` to the person: user or inbox can edit only verification commands after the run ends; you cannot edit. Once the task_edited event and current commands show the correction, retry_inherit carries the committed work forward and integration uses the corrected commands. reason_category says why a person is needed: recovery_failed when you cannot repair it or are not sure, discard when the work would be thrown away, scope when it needs a permission you do not have.\n",
        run_id = run.id(),
        task_id = task.id(),
        state = match &material.ended {
            Some(_) => format!("which ended {}; its session is gone", run.status().as_str()),
            None => format!(
                "whose session in workspace {} is still running",
                material.workspace
            ),
        },
        alert = material.alert.as_str(),
        meaning = match material.alert {
            RecoveryAlert::Failed =>
                "the run failed (its receipt said failed, its validation or landing failed, or its session exited without finishing).",
            RecoveryAlert::Interrupted =>
                "the run's session died and the supervisor recovered the run as interrupted.",
            RecoveryAlert::ResumeExhausted =>
                "the run still needed a session after its last resume, so the supervisor stopped resuming it.",
            RecoveryAlert::Stalled =>
                "the headless session does not get on: its turns end with neither a receipt nor an open question after the supervisor's nudges, each sent as the next turn (reason turn_without_receipt), or a turn was refused permissions too often to get on (reason permission_denied). The alert facts say which. The session has no screen, input box or dialog: an instruction is the prompt of its next turn, and resume parks the run for a session of its own.",
            RecoveryAlert::IdleProcess =>
                "processes of the run (listed in the alert facts with how long they have used almost no CPU time) are alive but have not made progress for longer than the threshold; the session may be waiting for them.",
            RecoveryAlert::StuckExit
            | RecoveryAlert::PromptWaiting
            | RecoveryAlert::LongBackground =>
                "an alert of the retired interactive worker run (task 1437), which the supervisor no longer raises.",
        },
        worktree = run.worktree_path().unwrap_or("none"),
        run_dir = run.run_dir().unwrap_or("none"),
        verification = fenced("sh", &verification),
        head = material.head,
        receipt = material.receipt_commit.unwrap_or("(no receipt)"),
    );
    for counted in [&title, &description, &acceptance, &verification] {
        fit.section("task", counted);
    }
    Ok(fit.finish(text))
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
        "5. Rewrite the receipt at {receipt} with the new head commit, writing a temporary file in the same directory and renaming it. {ACCEPTANCE_REMAP} {FOLLOW_UP_PROPOSAL_AGAIN}"
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
    /// Asks a person answered, newest first ([`PRECEDENT_ASKS`] of them
    /// are fetched).
    pub precedents: &'a [Ask],
    /// The files the landings conflicted in most (`stats`
    /// `conflict_hotspots`), that main still has.
    pub hotspots: &'a [ConflictHotspot],
    /// One entry per task of the proposal, in the order of `tasks`.
    pub candidates: &'a [DuplicateCandidates],
    pub repo_root: &'a Path,
    /// The language whose instruction ends the prompt (ADR-t616-2
    /// decision 3); counted within its limit.
    pub language: Option<&'a crate::domain::language::Language>,
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

/// The bytes the whole plan review prompt takes at most, the language's
/// instruction included (task 1561, ADR-t1566-1 decision 4): about two
/// fifths of macOS's `ARG_MAX`, and about a third of the prompt of plan
/// review 724 that could not start.
pub const PLAN_REVIEW_PROMPT_LIMIT: usize = 400_000;

/// The bytes the sections the plan review cannot do without (the
/// instructions and the verdict's schema, the proposal's tasks and their
/// expected files, the goals, `lint`, the language's instruction) take at
/// most; past it their largest material is replaced by how to read it with
/// the read-only dagq commands (ADR-t1566-1 decisions 2, 3).
pub const PLAN_REVIEW_REQUIRED_LIMIT: usize = 200_000;

/// Ready and in-progress tasks the plan review prompt gives in full at
/// most, and the bytes of their full text.
pub const QUEUED_FULL_TASKS: usize = 20;
pub const QUEUED_FULL_BYTES: usize = 100_000;

/// The bytes of the summaries of the ready and in-progress tasks.
pub const QUEUED_SUMMARY_BYTES: usize = 64_000;

/// Asks a person answered the plan review prompt quotes at most, newest
/// first, and the bytes they take.
pub const PRECEDENT_ASKS: usize = 20;
pub const PRECEDENT_BYTES: usize = 16_000;

/// The bytes of the conflict hotspots, of the duplicate candidates and of
/// the other proposals.
pub const HOTSPOT_BYTES: usize = 16_000;
pub const CANDIDATE_BYTES: usize = 48_000;
pub const OTHER_PROPOSAL_BYTES: usize = 48_000;

/// The read-only dagq commands the plan review prompt names to read what
/// its limits left out, as it writes them (`ID` for a number): each is one
/// the plan review job's role may run (task 1561, ADR-t1566-1 decision 3).
pub const PLAN_REVIEW_READS: &[&str] = &[
    "dagq show ID --full",
    "dagq goal show ID --full",
    "dagq proposal show ID",
    "dagq lint --proposal ID",
    "dagq related ID",
    "dagq search '<words of its title>'",
    "dagq asks --all",
    "dagq stats",
    "dagq list --status ready,in_progress --limit ID",
];

/// What the note of each section that left something out may take at most;
/// the overall limit keeps it free.
const OMISSION_NOTE_BYTES: usize = 2_000;

/// IDs a note of what was left out names at most.
const NOTE_IDS: usize = 40;

/// The note heading the proposal's tasks when the required sections were
/// over [`PLAN_REVIEW_REQUIRED_LIMIT`] may take at most.
const OVER_LIMIT_NOTE_BYTES: usize = 1_000;

/// Characters of a title a stub of left-out material keeps.
const STUB_TITLE_CHARS: usize = 200;

/// The bytes a stub takes at most (its title of [`STUB_TITLE_CHARS`] in
/// up to 4 bytes each, and its fields): a smaller piece is not replaced,
/// as its stub would not be smaller.
const STUB_BYTES: usize = 1_000;

/// The optional sections of the plan review prompt, each of which keeps
/// [`OMISSION_NOTE_BYTES`] free for its note.
const OPTIONAL_SECTIONS: usize = 6;

/// What the plan review prompt takes, in bytes, section by section, and
/// what its limits left out (task 1561, ADR-t1566-1 decision 6): recorded
/// as `prompt_bytes` on `plan_review_finished` and `plan_review_failed`.
/// The sections add up to `total`; `omitted` counts, by section, the items
/// left out or replaced by how to read them; `over_limit` says why the
/// required sections were cut, when they were.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PromptBytes {
    pub total: usize,
    pub limit: usize,
    pub sections: BTreeMap<&'static str, usize>,
    pub omitted: BTreeMap<&'static str, usize>,
    pub over_limit: Option<String>,
}

/// The plan review prompt and what it takes.
#[derive(Debug, Clone)]
pub struct PlanReviewPrompt {
    pub text: String,
    pub bytes: PromptBytes,
}

/// The prompt of a headless job or of a planner of the runtime's held to
/// its limits, and what it takes (task 1571, ADR-t1566-1 decisions 4 to
/// 6): goal review, run review, recovery job and the four planners'.
#[derive(Debug, Clone)]
pub struct FittedPrompt {
    pub text: String,
    pub bytes: PromptBytes,
}

/// The variable sections of the plan review prompt, each as it is
/// written into it.
#[derive(Default)]
struct PlanSections {
    tasks: String,
    own_expected: String,
    goals: String,
    lint: String,
    others: String,
    queued: String,
    queued_full: String,
    precedents: String,
    hotspots: String,
    candidates: String,
    predicted: String,
}

/// The lines of a section to keep, in the order given: within `count`
/// lines and `bytes` (each line with its newline). A line that does not
/// fit is skipped and the next one tried, so one huge line does not hide
/// the rest.
fn fit_lines(lines: &[String], count: usize, bytes: usize) -> Vec<bool> {
    let mut kept = vec![false; lines.len()];
    let (mut taken, mut used) = (0, 0);
    for (index, line) in lines.iter().enumerate() {
        if taken == count {
            break;
        }
        let size = line.len() + 1;
        if used + size <= bytes {
            kept[index] = true;
            taken += 1;
            used += size;
        }
    }
    kept
}

/// What a fenced block adds to its lines at most.
fn fence_overhead(lines: &[String]) -> usize {
    let longest = lines
        .iter()
        .flat_map(|line| line.split(|c| c != '`').map(str::len))
        .max()
        .unwrap_or(0);
    2 * (longest.max(2) + 1) + 16
}

/// `lines` as one fenced JSON block, or `(none)`.
fn json_block(lines: &[String]) -> String {
    if lines.is_empty() {
        "(none)".to_owned()
    } else {
        fenced("json", &lines.join("\n"))
    }
}

/// At most [`NOTE_IDS`] of `ids`, with how many more there are.
fn id_list(ids: &[i64]) -> String {
    let mut text = ids
        .iter()
        .take(NOTE_IDS)
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if ids.len() > NOTE_IDS {
        text.push_str(&format!(" and {} more", ids.len() - NOTE_IDS));
    }
    text
}

/// The lines of an optional section kept within `limit` and what is left
/// of the prompt's room, with the indices left out.
struct Fitted {
    kept: Vec<bool>,
}

impl Fitted {
    fn new(lines: &[String], count: usize, limit: usize, room: usize) -> Self {
        let budget = limit
            .min(room.saturating_sub(OMISSION_NOTE_BYTES))
            .saturating_sub(fence_overhead(lines));
        Self {
            kept: fit_lines(lines, count, budget),
        }
    }

    fn left_out(&self) -> impl Iterator<Item = usize> + '_ {
        self.kept
            .iter()
            .enumerate()
            .filter(|(_, kept)| !**kept)
            .map(|(index, _)| index)
    }

    fn keep<T: Clone>(&self, items: &[T]) -> Vec<T> {
        items
            .iter()
            .zip(&self.kept)
            .filter(|(_, kept)| **kept)
            .map(|(item, _)| item.clone())
            .collect()
    }
}

/// The JSON line of a piece of required material left out of the prompt:
/// what it is and how to read it.
fn stub(id: i64, title: Option<&str>, bytes: usize, read: String) -> String {
    let mut stub = serde_json::json!({"id": id, "left_out_bytes": bytes, "read_with": read});
    if let Some(title) = title {
        stub["title"] = super::health::truncate(title, STUB_TITLE_CHARS)
            .unwrap_or_else(|| title.to_owned())
            .into();
    }
    stub.to_string()
}

/// The membership material of a follow_up task for plan review
/// (ADR-t1504-2 decision 7), or `None` for any other task: where it came
/// from (the source goal and its state at registration, the worker's
/// category and membership_proposal, as written or null), every membership judgement (classification, acceptance
/// items, reason, evidence, destination goal, the acceptance version it
/// was judged at and the source goal's current one) and whether the latest
/// one was judged at the current version (`null` without a judgement).
fn follow_up_membership(detail: &TaskDetail) -> Option<Value> {
    let origin = detail
        .origin
        .as_ref()
        .filter(|origin| origin.origin == crate::domain::DraftOrigin::FollowUp)?;
    let latest = detail.membership_judgements.last();
    Some(serde_json::json!({
        "origin": origin.material,
        "judgements": detail.membership_judgements,
        "latest_classification": latest.map(|row| row["classification"].clone()),
        "latest_version_matches": latest.map(|row| row["needs_recheck"] == Value::Bool(false)),
    }))
}

/// What the headless plan review is asked: the material, the checks, the
/// fixes it may make itself and the verdict schema (ADR-0041 decisions 10,
/// 11, 14, 15). The repository's own rules are not in the runtime: the job
/// reads them from the repository's documents. Each section is held to its
/// limit and the whole to [`PLAN_REVIEW_PROMPT_LIMIT`]; what is left out is
/// counted and named with the read-only dagq command that reads it (task
/// 1561, ADR-t1566-1).
pub fn plan_review_prompt(material: &PlanReviewMaterial<'_>) -> Result<PlanReviewPrompt> {
    let proposal = material.proposal;
    let to_lines = |values: Vec<Value>| values.iter().map(Value::to_string).collect::<Vec<_>>();
    let mut task_lines = to_lines(
        material
            .tasks
            .iter()
            .map(|detail| {
                let mut task = serde_json::to_value(&detail.task)?;
                task["dependencies"] = serde_json::to_value(&detail.dependencies)?;
                task["goal_dependencies"] = serde_json::to_value(&detail.goal_dependencies)?;
                if let Some(membership) = follow_up_membership(detail) {
                    task["follow_up_membership"] = membership;
                }
                Ok(task)
            })
            .collect::<Result<_>>()?,
    );
    let mut goal_lines = to_lines(
        material
            .goals
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let lint_lines = to_lines(
        material
            .lint
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let no_files = Vec::new();
    let expected = |id: TaskId| material.expected.get(&id).unwrap_or(&no_files);
    let mut expected_lines = to_lines(
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
    let predicted = material
        .tasks
        .iter()
        .filter(|detail| detail.task.status() == crate::domain::TaskStatus::Submitted)
        .map(|detail| detail.task.id().to_string())
        .collect::<Vec<_>>();
    let mut sections = PlanSections {
        predicted: if predicted.is_empty() {
            "(none)".to_owned()
        } else {
            predicted.join(", ")
        },
        ..PlanSections::default()
    };
    let language = material
        .language
        .map_or(0, |language| language.instruction().len() + 2);
    let fixed = plan_review_text(material, &PlanSections::default()).len() + language;
    let mut omitted: BTreeMap<&'static str, usize> = BTreeMap::new();

    // The required sections, cut to their limit by replacing their largest
    // pieces with how to read them.
    let mut lint = json_block(&lint_lines);
    let before = fixed
        + json_block(&task_lines).len()
        + json_block(&expected_lines).len()
        + json_block(&goal_lines).len()
        + lint.len()
        + sections.predicted.len();
    let mut over_limit = None;
    let mut over_note = String::new();
    if before > PLAN_REVIEW_REQUIRED_LIMIT {
        let room = PLAN_REVIEW_REQUIRED_LIMIT - OVER_LIMIT_NOTE_BYTES;
        // What the required sections take at most, kept as pieces are
        // replaced: each line with its newline and each block's fences.
        let lines_bytes = |lines: &[String]| {
            lines.iter().map(|line| line.len() + 1).sum::<usize>() + fence_overhead(lines)
        };
        let mut estimate = fixed
            + lines_bytes(&task_lines)
            + lines_bytes(&expected_lines)
            + lines_bytes(&goal_lines)
            + lint.len()
            + sections.predicted.len();
        // (bytes, section, index): the largest first, then the section's
        // order, then the piece's.
        let mut pieces: Vec<(usize, usize, usize)> = Vec::new();
        pieces.extend(task_lines.iter().enumerate().map(|(i, l)| (l.len(), 0, i)));
        pieces.extend(
            expected_lines
                .iter()
                .enumerate()
                .map(|(i, l)| (l.len(), 1, i)),
        );
        pieces.extend(goal_lines.iter().enumerate().map(|(i, l)| (l.len(), 2, i)));
        if !lint_lines.is_empty() {
            pieces.push((lint.len(), 3, 0));
        }
        pieces.retain(|&(bytes, _, _)| bytes > STUB_BYTES);
        pieces.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        for (bytes, section, index) in pieces {
            if estimate <= room {
                break;
            }
            let replaced = match section {
                0 => {
                    let task = &material.tasks[index].task;
                    let id = task.id().as_i64();
                    *omitted.entry("tasks").or_default() += 1;
                    let line = &mut task_lines[index];
                    *line = stub(
                        id,
                        Some(task.title()),
                        bytes,
                        format!("dagq show {id} --full"),
                    );
                    line.len()
                }
                1 => {
                    let id = material.tasks[index].task.id().as_i64();
                    *omitted.entry("expected_files").or_default() += 1;
                    let line = &mut expected_lines[index];
                    *line = stub(
                        id,
                        None,
                        bytes,
                        format!(
                            "dagq show {id} --full (its declared paths; without them, dagq related {id})"
                        ),
                    );
                    line.len()
                }
                2 => {
                    let goal = &material.goals[index];
                    let id = goal.id().as_i64();
                    *omitted.entry("goals").or_default() += 1;
                    let line = &mut goal_lines[index];
                    *line = stub(
                        id,
                        Some(goal.title()),
                        bytes,
                        format!("dagq goal show {id} --full"),
                    );
                    line.len()
                }
                _ => {
                    *omitted.entry("lint").or_default() += lint_lines.len();
                    lint = format!(
                        "({} findings, {bytes} bytes, left out: read them with `dagq lint --proposal {}`)",
                        lint_lines.len(),
                        proposal.id()
                    );
                    lint.len()
                }
            };
            estimate = (estimate + replaced).saturating_sub(bytes);
        }
        // Too many pieces for even their stubs: each list becomes one note.
        let proposal_id = proposal.id();
        if estimate > room {
            for (section, count) in [
                ("tasks", task_lines.len()),
                ("expected_files", expected_lines.len()),
                ("goals", goal_lines.len()),
            ] {
                if count > 0 {
                    omitted.insert(section, count);
                }
            }
            task_lines.clear();
            expected_lines.clear();
            goal_lines.clear();
            sections.predicted = format!(
                "every submitted task of the proposal ({} of them; `dagq proposal show {proposal_id}` lists them)",
                predicted.len()
            );
        }
        let reason = format!(
            "the required sections took {before} bytes, over their limit of {PLAN_REVIEW_REQUIRED_LIMIT}: their largest pieces are replaced by how to read them"
        );
        over_note = format!(
            "({reason}, with the read-only dagq commands each names (`dagq proposal show {proposal_id}` lists the proposal's tasks and goals); read them before you decide.)\n"
        );
        over_limit = Some(reason);
    }
    sections.tasks = if task_lines.is_empty() && omitted.contains_key("tasks") {
        format!(
            "{over_note}({} tasks, left out: list them with `dagq proposal show {}` and read each with `dagq show ID --full`)",
            material.tasks.len(),
            proposal.id()
        )
    } else {
        format!("{over_note}{}", json_block(&task_lines))
    };
    sections.own_expected = if expected_lines.is_empty() && omitted.contains_key("expected_files") {
        "(left out: read each task's declared paths with `dagq show ID --full`; without them, `dagq related ID`)".to_owned()
    } else {
        json_block(&expected_lines)
    };
    sections.goals = if goal_lines.is_empty() && omitted.contains_key("goals") {
        format!(
            "({} goals, left out: read each with `dagq goal show ID --full`)",
            material.goals.len()
        )
    } else {
        json_block(&goal_lines)
    };
    sections.lint = lint;
    // A note's bytes are kept free for each optional section, which still
    // writes `(none)` or its note past the room's end.
    let mut room = PLAN_REVIEW_PROMPT_LIMIT.saturating_sub(
        OPTIONAL_SECTIONS * OMISSION_NOTE_BYTES
            + fixed
            + sections.tasks.len()
            + sections.own_expected.len()
            + sections.goals.len()
            + sections.lint.len()
            + sections.predicted.len(),
    );

    // The optional sections, in the order they keep their room: the
    // hotspots, the duplicate candidates, the other proposals, the asks a
    // person answered, the full text, the summaries.
    let touching = |path: &str, ids: &mut dyn Iterator<Item = TaskId>| {
        ids.filter(|&id| crate::domain::claim_defer::touches(expected(id), path))
            .collect::<Vec<_>>()
    };
    // Each hotspot with the tasks expected to touch it; a queued task on
    // the same hotspot as a task of the proposal may be given in full.
    let mut eligible = BTreeSet::new();
    let mut shared_hotspots: BTreeMap<TaskId, usize> = BTreeMap::new();
    let mut hotspot_lines = Vec::new();
    for file in material.hotspots {
        let path = file.renamed_to.as_deref().unwrap_or(&file.path);
        let own = touching(path, &mut material.tasks.iter().map(|d| d.task.id()));
        let queued = touching(path, &mut material.queued.iter().map(|item| item.id));
        if !own.is_empty() {
            eligible.extend(queued.iter().copied());
            for &id in &queued {
                *shared_hotspots.entry(id).or_default() += 1;
            }
        }
        hotspot_lines.push(
            serde_json::json!({
                "path": path,
                "conflicts": file.conflicts, "tasks": file.tasks,
                "landings": file.landings, "ratio": file.ratio,
                "last_conflict_at": file.last_conflict_at, "alert": file.alert,
                "proposal_tasks": own, "queued_tasks": queued,
            })
            .to_string(),
        );
    }
    let fitted = Fitted::new(&hotspot_lines, usize::MAX, HOTSPOT_BYTES, room);
    let left_out = fitted.left_out().count();
    sections.hotspots = json_block(&fitted.keep(&hotspot_lines));
    if left_out > 0 {
        omitted.insert("hotspots", left_out);
        sections.hotspots.push_str(&format!(
            "\n({left_out} more files are left out by the limit of {HOTSPOT_BYTES} bytes; read them with `dagq stats` (conflict_hotspots))"
        ));
    }
    room = room.saturating_sub(sections.hotspots.len());

    // The best place of each queued task among the candidates.
    let mut candidate_rank: BTreeMap<TaskId, usize> = BTreeMap::new();
    for candidates in material.candidates {
        let related = candidates
            .related
            .iter()
            .map(|task| TaskId::new(task.id))
            .enumerate();
        let search = candidates
            .search
            .iter()
            .filter_map(|hit| match (hit.kind, &hit.id) {
                (
                    crate::domain::search::SearchKind::Task,
                    crate::domain::search::SearchRef::Id(id),
                ) => Some(TaskId::new(*id)),
                _ => hit.task_id.map(TaskId::new),
            })
            .enumerate();
        for (place, id) in related.chain(search) {
            eligible.insert(id);
            let best = candidate_rank.entry(id).or_insert(place);
            *best = (*best).min(place);
        }
    }
    let candidate_lines = to_lines(
        material
            .candidates
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let fitted = Fitted::new(&candidate_lines, usize::MAX, CANDIDATE_BYTES, room);
    let left_out: Vec<i64> = fitted
        .left_out()
        .map(|index| material.candidates[index].task_id.as_i64())
        .collect();
    sections.candidates = json_block(&fitted.keep(&candidate_lines));
    if !left_out.is_empty() {
        omitted.insert("candidates", left_out.len());
        sections.candidates.push_str(&format!(
            "\n(the candidates of {} tasks of the proposal are left out by the limit of {CANDIDATE_BYTES} bytes: {}; look for them with `dagq related ID` and `dagq search '<words of its title>'`)",
            left_out.len(),
            id_list(&left_out)
        ));
    }
    room = room.saturating_sub(sections.candidates.len());

    let other_blocks: Vec<String> = material
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
        .collect();
    let fitted = Fitted::new(&other_blocks, usize::MAX, OTHER_PROPOSAL_BYTES, room);
    let left_out: Vec<i64> = fitted
        .left_out()
        .map(|index| material.others[index].0.id().as_i64())
        .collect();
    let kept = fitted.keep(&other_blocks);
    sections.others = if kept.is_empty() && left_out.is_empty() {
        "(none)".to_owned()
    } else {
        kept.join("\n")
    };
    if !left_out.is_empty() {
        omitted.insert("other_proposals", left_out.len());
        sections.others.push_str(&format!(
            "\n({} more proposals are left out by the limit of {OTHER_PROPOSAL_BYTES} bytes: {}; read one with `dagq proposal show ID` and its tasks with `dagq show ID --full`)",
            left_out.len(),
            id_list(&left_out)
        ));
    }
    room = room.saturating_sub(sections.others.len());

    let precedent_lines: Vec<String> = material
        .precedents
        .iter()
        .map(|ask| format!("- {}", precedent_line(ask)))
        .collect();
    let fitted = Fitted::new(&precedent_lines, PRECEDENT_ASKS, PRECEDENT_BYTES, room);
    let left_out = fitted.left_out().count();
    let kept = fitted.keep(&precedent_lines);
    sections.precedents = if kept.is_empty() && left_out == 0 {
        "(none)".to_owned()
    } else {
        kept.join("\n")
    };
    if left_out > 0 {
        omitted.insert("precedents", left_out);
        sections.precedents.push_str(&format!(
            "\n({left_out} more of the newest asks a person answered are left out by the limit of {PRECEDENT_ASKS} asks and {PRECEDENT_BYTES} bytes; read them with `dagq asks --all`)"
        ));
    }
    if material.precedents.len() >= PRECEDENT_ASKS {
        sections.precedents.push_str(
            "\n(older asks a person answered are not listed: read them with `dagq asks --all`)",
        );
    }
    room = room.saturating_sub(sections.precedents.len());

    // The full text: the eligible tasks, the most related to the proposal
    // first: one a task of the proposal depends on, then the most hotspots
    // shared with the proposal, the most of its expected files shared, the
    // best place among the candidates, the newest.
    let own_files: Vec<&String> = material
        .tasks
        .iter()
        .flat_map(|detail| expected(detail.task.id()))
        .collect();
    let depended: BTreeSet<TaskId> = material
        .tasks
        .iter()
        .flat_map(|detail| detail.dependencies.iter().copied())
        .collect();
    let shared_files = |id: TaskId| {
        let files = expected(id);
        own_files
            .iter()
            .filter(|own| {
                crate::domain::claim_defer::touches(files, own)
                    || files.iter().any(|file| {
                        crate::domain::claim_defer::touches(std::slice::from_ref(own), file)
                    })
            })
            .count()
    };
    // (a task of the proposal depends on it, hotspots shared, expected
    // files shared, best place among the candidates)
    type Relation = (bool, usize, usize, usize);
    let mut ranked: Vec<(&TaskListItem, Relation)> = material
        .queued
        .iter()
        .filter(|item| eligible.contains(&item.id))
        .map(|item| {
            (
                item,
                (
                    depended.contains(&item.id),
                    shared_hotspots.get(&item.id).copied().unwrap_or(0),
                    shared_files(item.id),
                    candidate_rank.get(&item.id).copied().unwrap_or(usize::MAX),
                ),
            )
        })
        .collect();
    ranked.sort_by(|(a, ka), (b, kb)| {
        kb.0.cmp(&ka.0)
            .then(kb.1.cmp(&ka.1))
            .then(kb.2.cmp(&ka.2))
            .then(ka.3.cmp(&kb.3))
            .then(b.id.cmp(&a.id))
    });
    let ranked: Vec<&TaskListItem> = ranked.into_iter().map(|(item, _)| item).collect();
    let full_lines = ranked
        .iter()
        .map(serde_json::to_string)
        .collect::<serde_json::Result<Vec<_>>>()?;
    let fitted = Fitted::new(&full_lines, QUEUED_FULL_TASKS, QUEUED_FULL_BYTES, room);
    let full: BTreeSet<TaskId> = fitted.keep(&ranked).iter().map(|item| item.id).collect();
    let left_out: Vec<i64> = fitted
        .left_out()
        .map(|index| ranked[index].id.as_i64())
        .collect();
    sections.queued_full = json_block(&fitted.keep(&full_lines));
    if !left_out.is_empty() {
        omitted.insert("full_text", left_out.len());
        sections.queued_full.push_str(&format!(
            "\n({} more ready or in-progress tasks that meet this rule are left out by its limit of {QUEUED_FULL_TASKS} tasks and {QUEUED_FULL_BYTES} bytes, the most related to the proposal first: {}; read one in full with `dagq show ID --full`)",
            left_out.len(),
            id_list(&left_out)
        ));
    }
    room = room.saturating_sub(sections.queued_full.len());

    // The summaries: the eligible tasks in the order above, then the rest,
    // newest first; written newest first.
    let summary_lines: Vec<String> = material
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
            summary.to_string()
        })
        .collect();
    let place: BTreeMap<TaskId, usize> = material
        .queued
        .iter()
        .enumerate()
        .map(|(index, item)| (item.id, index))
        .collect();
    let order: Vec<usize> = ranked
        .iter()
        .map(|item| place[&item.id])
        .chain(
            material
                .queued
                .iter()
                .enumerate()
                .filter(|(_, item)| !eligible.contains(&item.id))
                .map(|(index, _)| index),
        )
        .collect();
    let ordered: Vec<String> = order.iter().map(|&i| summary_lines[i].clone()).collect();
    let fitted = Fitted::new(&ordered, usize::MAX, QUEUED_SUMMARY_BYTES, room);
    let mut kept = vec![false; summary_lines.len()];
    for (&index, &keep) in order.iter().zip(&fitted.kept) {
        kept[index] = keep;
    }
    let cut = fitted.left_out().count();
    let shown: Vec<String> = summary_lines
        .iter()
        .zip(&kept)
        .filter(|(_, keep)| **keep)
        .map(|(line, _)| line.clone())
        .collect();
    sections.queued = json_block(&shown);
    if material.queued_left_out > 0 {
        sections.queued.push_str(&format!(
            "\n({} more ready or in-progress tasks, those of the lowest IDs, are left out of this list)",
            material.queued_left_out
        ));
    }
    if cut > 0 {
        sections.queued.push_str(&format!(
            "\n({cut} more ready or in-progress tasks, the least related to the proposal, are left out of this list by its limit of {QUEUED_SUMMARY_BYTES} bytes; list them with `dagq list --status ready,in_progress --limit 200` and read one with `dagq show ID --full`)"
        ));
    }
    if cut + material.queued_left_out > 0 {
        omitted.insert("summaries", cut + material.queued_left_out);
    }

    let body = plan_review_text(material, &sections);
    let body_len = body.trim_end().len();
    let text = crate::domain::language::with_instruction(body, material.language);
    let mut bytes = BTreeMap::from([
        ("tasks", sections.tasks.len()),
        ("expected_files", sections.own_expected.len()),
        ("goals", sections.goals.len()),
        ("lint", sections.lint.len()),
        ("other_proposals", sections.others.len()),
        ("summaries", sections.queued.len()),
        ("full_text", sections.queued_full.len()),
        ("precedents", sections.precedents.len()),
        ("hotspots", sections.hotspots.len()),
        ("candidates", sections.candidates.len()),
        (
            "language",
            material.language.map_or(0, |_| text.len() - body_len),
        ),
    ]);
    let counted: usize = bytes.values().sum();
    bytes.insert("instructions", text.len() - counted);
    debug_assert!(text.len() <= PLAN_REVIEW_PROMPT_LIMIT, "{}", text.len());
    Ok(PlanReviewPrompt {
        bytes: PromptBytes {
            total: text.len(),
            limit: PLAN_REVIEW_PROMPT_LIMIT,
            sections: bytes,
            omitted,
            over_limit,
        },
        text,
    })
}

/// The plan review prompt with its variable sections written in.
fn plan_review_text(material: &PlanReviewMaterial<'_>, sections: &PlanSections) -> String {
    let proposal = material.proposal;
    let PlanSections {
        tasks,
        own_expected,
        goals,
        lint,
        others,
        queued,
        queued_full,
        precedents,
        hotspots,
        candidates,
        predicted,
    } = sections;
    format!(
        "You are the plan review of dagq proposal {id}: decide whether the queue may run its tasks as written, before they become ready.\n\
         Read only. Do not change any file and do not run dagq commands that write: the runtime lets you run the dagq commands that read (`dagq show ID`, `dagq proposal show ID`, `dagq search`, `dagq related`, `dagq findings`, `dagq stats`, ...) and refuses the rest.\n\
         {RECORD_READING}\n\
         The material below is held to limits: a list cut to fit says how many it left out and which read-only dagq command reads them; read what you need with it.\n\n\
         First read the repository's own rules in {repo}: its instructions (AGENTS.md and CLAUDE.md), the documents and rules they name (the plan review's part of them above all), and the documents the tasks name. \
         Apply what they say (the verification each kind of change needs, the declared paths, the required evidence, the rules for the records they keep, ...); the runtime has no such rules of its own. \
         Where the repository has no AGENTS.md, judge a task's verification, paths and evidence in this order: CLAUDE.md; then what the README, the CI configuration and the build configuration show; when none of them settles it, it needs a person: a concern.\n\n\
         The proposal was submitted {submitted} and was sent back {revises} time(s) before (at most {max}; a revise past that goes to a person as a concern).\n\n\
         Tasks of the proposal:\n{tasks}\n\n\
         Files each task of the proposal is expected to touch (its declared paths; without them, the files the landings of its 3 most related completed tasks changed; a guess, so check it against the source):\n{own_expected}\n\n\
         Goals they belong to (description, acceptance, constraints; constraints win over a task's description):\n{goals}\n\n\
         The mechanical checks (`dagq lint`) found:\n{lint}\n\n\
         Other proposals not ready yet:\n{others}\n\n\
         Ready and in-progress tasks, in summary, newest first (expected_files as for the proposal's tasks, and for an in-progress task also what its run changed so far; full_text_below marks a task given in full below; `dagq show ID --full` reads any of them in full):\n{queued}\n\n\
         In full, the ready and in-progress tasks among the candidates below or expected to touch a hotspot a task of the proposal is expected to touch, the most related to the proposal first:\n{queued_full}\n\n\
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
         - a follow_up's membership (its follow_up_membership: the planner's judgements of whether its source goal's acceptance needs it, each with the acceptance items, reason, evidence, destination goal and the acceptance version it was judged at against the source goal's current one): start from the planner's mapping and check that it holds against the source goal's acceptance; you need not repeat the whole investigation, but never pass a judgement on its form alone. When it looks doubtful (the reason names no item of the acceptance, the evidence disagrees with the receipt or the diff it cites, the destination is an unrelated catch-all goal, the acceptance was weakened so that the follow-up falls out of scope, the version does not match), read the evidence around it (the source run's receipt and commits, `dagq goal show ID --full` for the goal's acceptance and its history) before you decide. A wrong mapping the planner can fix is a revise; an acceptance weakened to drop a follow-up needs a person's intent, a concern;\n\
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
    )
}

/// What the goal review job is shown about one goal (ADR-0047 decision
/// 43): the goal, each of its tasks with what landed for it, the
/// follow-ups found from it with their membership judgements (ADR-t1504-2),
/// the goal's notes and edits, and the goal's earlier reviews. Each value
/// is one JSON line of the prompt.
pub struct GoalReviewMaterial<'a> {
    pub goal: Value,
    pub tasks: Vec<Value>,
    pub follow_ups: Vec<Value>,
    pub events: Vec<Value>,
    pub previous: Vec<Value>,
    pub gaps_in_a_row: usize,
    pub repo_root: &'a Path,
}

/// The bytes the whole goal review prompt takes at most, the language's
/// instruction included (task 1571, ADR-t1566-1 decision 4): the largest
/// in production, goal review job 24 of 2026-10-02, took 218,427 bytes, of
/// which its 50 tasks took 200,200.
pub const GOAL_REVIEW_PROMPT_LIMIT: usize = 200_000;

/// The bytes of the goal itself (5,241 at most in production): it is
/// required, and past this its longest fields are cut.
pub const GOAL_REVIEW_GOAL_BYTES: usize = 16_000;

/// The bytes of the goal's tasks, each with what landed for it, and of
/// one task: in production a task took 4,000 bytes on average and 12,126
/// at most, most of it its landed receipt.
pub const GOAL_REVIEW_TASKS_BYTES: usize = 110_000;
pub const GOAL_REVIEW_TASK_BYTES: usize = 8_000;

/// The bytes of the stubs (ID, title, status) of the tasks left out.
pub const GOAL_REVIEW_STUB_BYTES: usize = 8_000;

/// The bytes of the follow-ups, of the notes and edits (10,877 at most in
/// production) and of the earlier reviews, and of one item of them.
pub const GOAL_REVIEW_FOLLOW_UPS_BYTES: usize = 16_000;
pub const GOAL_REVIEW_EVENTS_BYTES: usize = 16_000;
pub const GOAL_REVIEW_PREVIOUS_BYTES: usize = 12_000;
pub const GOAL_REVIEW_ITEM_BYTES: usize = 4_000;

/// The read-only dagq commands the goal review prompt names to read what
/// its limits left out, as it writes them (`ID` for a number): each is one
/// the goal review job's role may run (task 1571, ADR-t1566-1 decision 3).
pub const GOAL_REVIEW_READS: &[&str] = &[
    "dagq show ID --full",
    "dagq goal show ID --full",
    "dagq events --full --all --goal ID",
    "dagq events --full --goal ID --kind goal_review_finished",
    "dagq events --full --task ID --kind integration_receipt",
];

/// The prompt of the headless goal review (ADR-0047 decision 43): whether
/// the goal whose tasks all ended met its acceptance. Each section is held
/// to its limit and the whole to [`GOAL_REVIEW_PROMPT_LIMIT`]: the tasks
/// that landed come first and the newest first, the others after them;
/// the notes and the earlier reviews newest first. What is left out is
/// counted and named with the read-only dagq command that reads it (task
/// 1571, ADR-t1566-1).
pub fn goal_review_prompt(material: &GoalReviewMaterial<'_>) -> FittedPrompt {
    let goal_id = material.goal["id"].clone();
    let goal_read = format!("read it whole with `dagq goal show {goal_id} --full`");
    let mut fit = Fit::new(GOAL_REVIEW_PROMPT_LIMIT);
    let block = |lines: &[String]| {
        if lines.is_empty() {
            "(none)".to_owned()
        } else {
            fenced("json", &lines.join("\n"))
        }
    };
    let goal = match shrink(&material.goal, GOAL_REVIEW_GOAL_BYTES, &goal_read) {
        Some((shrunk, left_out)) => {
            fit.omit("goal", 1);
            fit.over(format!(
                "goal: {left_out} bytes left out by its limit of {GOAL_REVIEW_GOAL_BYTES}"
            ));
            shrunk.to_string()
        }
        None => material.goal.to_string(),
    };
    let goal = block(&[goal]);
    fit.section("goal", &goal);
    // The tasks that landed, newest first, then the others, newest first.
    let tasks = &material.tasks;
    let landed = |task: &Value| task.get("landed").is_some();
    let order: Vec<usize> = (0..tasks.len())
        .rev()
        .filter(|&index| landed(&tasks[index]))
        .chain(
            (0..tasks.len())
                .rev()
                .filter(|&index| !landed(&tasks[index])),
        )
        .collect();
    let task_read = "read it whole with `dagq show ID --full` and what landed for it with `dagq events --full --task ID --kind integration_receipt`";
    let (kept, left_out) = fit.lines(
        "tasks",
        tasks,
        order,
        (usize::MAX, GOAL_REVIEW_TASKS_BYTES, GOAL_REVIEW_TASK_BYTES),
        task_read,
    );
    let mut task_text = block(&kept);
    if !left_out.is_empty() {
        let stubs: Vec<String> = left_out
            .iter()
            .map(|&index| {
                let task = &tasks[index];
                serde_json::json!({"id": task["id"], "title": task["title"], "status": task["status"], "landed": landed(task)}).to_string()
            })
            .collect();
        let sizes: Vec<usize> = stubs.iter().map(String::len).collect();
        let shown = prompt_fit::pick(&sizes, 0..stubs.len(), usize::MAX, GOAL_REVIEW_STUB_BYTES);
        let shown: Vec<String> = stubs
            .into_iter()
            .zip(&shown)
            .filter(|(_, shown)| **shown)
            .map(|(stub, _)| stub)
            .collect();
        let ids: Vec<String> = left_out
            .iter()
            .map(|&index| tasks[index]["id"].to_string())
            .collect();
        task_text.push_str(&format!(
            "The tasks left out, in summary:\n{}{}",
            block(&shown),
            left_out_note(
                "tasks",
                &ids,
                "`dagq show ID --full` for each, and `dagq events --full --task ID --kind integration_receipt` for what landed for it"
            )
        ));
    }
    fit.section("tasks", &task_text);
    let newest_first = |items: &[Value]| (0..items.len()).rev().collect::<Vec<_>>();
    let listed = |fit: &mut Fit,
                  name: &'static str,
                  items: &[Value],
                  bytes: usize,
                  what: &str,
                  read: &str| {
        let (kept, left_out) = fit.lines(
            name,
            items,
            newest_first(items),
            (usize::MAX, bytes, GOAL_REVIEW_ITEM_BYTES),
            read,
        );
        let mut text = block(&kept);
        if !left_out.is_empty() {
            let ids: Vec<String> = left_out
                .iter()
                .map(|&index| {
                    let item = &items[index];
                    item.get("task_id")
                        .or_else(|| item.get("id"))
                        .or_else(|| item.get("at"))
                        .map_or_else(|| format!("#{}", index + 1), Value::to_string)
                })
                .collect();
            text.push_str(&left_out_note(what, &ids, read));
        }
        fit.section(name, &text);
        text
    };
    let follow_ups = listed(
        &mut fit,
        "follow_ups",
        &material.follow_ups,
        GOAL_REVIEW_FOLLOW_UPS_BYTES,
        "follow-ups",
        &format!("`dagq goal show {goal_id} --full`, and `dagq show ID --full` for each"),
    );
    let events = listed(
        &mut fit,
        "events",
        &material.events,
        GOAL_REVIEW_EVENTS_BYTES,
        "notes and edits (the oldest)",
        &format!("`dagq events --full --all --goal {goal_id}`"),
    );
    let previous = listed(
        &mut fit,
        "previous",
        &material.previous,
        GOAL_REVIEW_PREVIOUS_BYTES,
        "earlier reviews (the oldest)",
        &format!("`dagq events --full --goal {goal_id} --kind goal_review_finished`"),
    );
    fit.finish(format!(
        "You are the goal review of the dagq queue, a headless job. Every task of goal {goal_id} ended (completed or canceled): judge whether the goal met its acceptance. Change nothing: read the repository's documents and source in {repo} (the main checkout, where the tasks landed) and run read-only dagq commands (`dagq show ID`, `dagq goal show {goal_id} --full`, `dagq findings`, `dagq events --goal {goal_id} --full`, `dagq search ...`) as you need. \
         The material below is held to limits: a section that left something out says how many and how to read them, and an item cut short says so in its `cut`.\n\n\
         The goal:\n{goal}\n\n\
         Its tasks, each with the run that landed it (the receipt's summary, its evidence and its follow_ups) when it was completed by a run:\n{task_text}\n\n\
         The follow-ups registered from its tasks' receipts, or that belong to it now, wherever they belong (goal_id), with their membership judgements (classification, the acceptance items, reason, evidence, acceptance_version against current_acceptance_version, needs_recheck):\n{follow_ups}\n\n\
         The goal's notes, edits and earlier decisions:\n{events}\n\n\
         The goal's earlier reviews (gaps verdicts in a row before this one: {gaps}; after {max} in a row a gaps verdict is turned into a question to a person):\n{previous}\n\n\
         Split the acceptance into its items and check each against what landed, with evidence you saw (a commit, a file, a test, a receipt). \
         A follow-up judged out_of_scope is not part of the acceptance: leave its work out of your judgement and do not wait for it or list it as a gap. A follow-up judged required is part of it: judge the goal with its work, as with the goal's own tasks. \
         The runtime starts this review only when each follow-up of the goal that is not completed or canceled is judged against the current acceptance. Then answer one of:\n\
         - achieved: every item is met. The runtime closes the goal as achieved and records your criteria.\n\
         - gaps: some items are not met and the work to meet them is clear and within the goal. List each missing piece as a gap with a title and a description a planner can turn into a task; the runtime registers each as a draft of the goal and a planner of the runtime decides it. The goal stays open.\n\
         - ask: only when a person has to decide: the acceptance should change, the goal should be abandoned or split, or you cannot judge it. Write the question; the person answers achieved, abandoned, gaps (the gaps you listed, or `gaps: <what>`) or keep_open. reason_category is scope (the acceptance, the scope or a decision changes) or discard (work would be thrown away).\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"achieved\" | \"gaps\" | \"ask\", \"criteria\": [{{\"criterion\": string, \"met\": bool, \"evidence\": [string]}}], \"gaps\": [{{\"title\": string, \"description\": string, \"criterion\": string}}], \"summary\": string, \"question\": string, \"options\": [string], \"reason_category\": \"scope\" | \"discard\"}}\n\
         criteria has one entry for each item of the acceptance; gaps is empty unless the verdict is gaps (or ask, to offer them); summary is one or two sentences; question, options and reason_category are for ask only.\n",
        repo = material.repo_root.display(),
        gaps = material.gaps_in_a_row,
        max = crate::domain::goal_review::MAX_GOAL_GAPS,
    ))
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
            goal_priority: None,
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
            wait_for_build: false,
        })
        .unwrap()
    }

    fn task_with_evidence(id: i64, required_evidence: Vec<EvidenceCheck>) -> Task {
        Task::restore(TaskRecord {
            goal_priority: None,
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
            wait_for_build: false,
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
            priority: Default::default(),
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
            goal_priority: None,
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
            wait_for_build: false,
        })
        .unwrap()
    }

    /// The material of a plan review prompt, as the tests vary it.
    struct PlanCase {
        tasks: Vec<TaskDetail>,
        goals: Vec<Goal>,
        queued: Vec<TaskListItem>,
        left_out: usize,
        expected: BTreeMap<TaskId, Vec<String>>,
        precedents: Vec<Ask>,
        hotspots: Vec<ConflictHotspot>,
        candidates: Vec<DuplicateCandidates>,
        language: Option<crate::domain::language::Language>,
    }

    impl PlanCase {
        fn prompt(&self) -> PlanReviewPrompt {
            use crate::domain::{PlannerOrigin, PlannerOwner, ProposalRecord, ProposalStatus};
            let proposal = Proposal::restore(ProposalRecord {
                id: ProposalId::new(1),
                status: ProposalStatus::Submitted,
                owner: PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: None,
                },
                submitted_at: "now".into(),
                revise_count: 0,
                task_ids: self.tasks.iter().map(|detail| detail.task.id()).collect(),
                goal_ids: Vec::new(),
                created_at: String::new(),
                updated_at: String::new(),
            })
            .unwrap();
            plan_review_prompt(&PlanReviewMaterial {
                proposal: &proposal,
                tasks: &self.tasks,
                goals: &self.goals,
                lint: &[],
                others: &[],
                queued: &self.queued,
                queued_left_out: self.left_out,
                expected: &self.expected,
                precedents: &self.precedents,
                hotspots: &self.hotspots,
                candidates: &self.candidates,
                repo_root: Path::new("/repo"),
                language: self.language.as_ref(),
            })
            .unwrap()
        }
    }

    /// A task of the proposal as plan review reads it.
    fn proposal_task(task: Task, dependencies: Vec<TaskId>) -> TaskDetail {
        TaskDetail {
            membership_judgements: Vec::new(),
            task,
            dependencies,
            goal_dependencies: Vec::new(),
            duplicate_of: None,
            duplicates: Vec::new(),
            runs: Vec::new(),
            events: Vec::new(),
            processes: Vec::new(),
            origin: None,
            follow_up_drafts: Vec::new(),
            asks: Vec::new(),
        }
    }

    /// A ready task as the queue lists it.
    fn queued_task(task: Task) -> TaskListItem {
        TaskListItem::new(task, vec![TaskId::new(1)], Vec::new(), None, None, true)
    }

    /// A file the landings conflicted in.
    fn hotspot(path: &str) -> ConflictHotspot {
        ConflictHotspot {
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
        }
    }

    /// The plan review material with `queued` ready tasks: the proposal's
    /// task 1000 touches `src/hot.rs`, and so does the ready task 5.
    fn plan_case(queued: i64, left_out: usize) -> PlanCase {
        use crate::domain::related::RelatedTask;
        let tasks = vec![proposal_task(
            long_task(1000, TaskStatus::Submitted, &["src/hot.rs"]),
            Vec::new(),
        )];
        let items: Vec<TaskListItem> = (1..=queued)
            .map(|id| {
                let paths: &[&str] = if id == 5 { &["src/*.rs"] } else { &[] };
                queued_task(long_task(id, TaskStatus::Ready, paths))
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
        let candidates = vec![DuplicateCandidates {
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
        PlanCase {
            tasks,
            goals: Vec::new(),
            queued: items,
            left_out,
            expected,
            precedents: Vec::new(),
            hotspots: vec![hotspot("src/hot.rs"), hotspot("src/cold.rs")],
            candidates,
            language: None,
        }
    }

    /// The plan review prompt of [`plan_case`], and the full text of its
    /// queue.
    fn plan_prompt(queued: i64, left_out: usize) -> (String, usize) {
        let case = plan_case(queued, left_out);
        let old_size = case
            .queued
            .iter()
            .map(|item| serde_json::to_string(item).unwrap().len())
            .sum();
        (case.prompt().text, old_size)
    }

    /// A follow_up of the proposal carries its membership judgements with
    /// the version check, another task none, and the prompt asks plan
    /// review to start from the planner's mapping and read the evidence
    /// around a doubtful one (ADR-t1504-2 decision 7).
    #[test]
    fn plan_review_checks_follow_up_membership_from_the_planner_s_mapping() {
        use crate::domain::{BundleKey, DraftOrigin, TaskOrigin};
        let mut case = plan_case(1, 0);
        let material = serde_json::json!({"source_task_id": 3, "source_run_id": "r",
            "source_goal_id": 9, "source_goal_state": "open", "source_goal_provenance": "recorded",
            "membership_proposal": {"classification": "required", "acceptance_items": ["(1)"], "reason": "w"}});
        case.tasks[0].origin = Some(TaskOrigin {
            origin: DraftOrigin::FollowUp,
            material: material.clone(),
            source_task_id: Some(TaskId::new(3)),
            source_run_id: Some("r".into()),
            index: Some(0),
            bundle_key: BundleKey::of(DraftOrigin::FollowUp, &material, TaskId::new(1000)),
            bundles: Vec::new(),
        });
        case.tasks[0].membership_judgements = vec![serde_json::json!({
            "id": 4, "classification": "out_of_scope", "acceptance_items": ["(2)"],
            "reason": "(2) holds without it", "evidence": ["receipt:r"],
            "destination_goal_id": 10, "acceptance_version": 1,
            "current_acceptance_version": 2, "needs_recheck": true})];
        case.tasks.push(proposal_task(
            long_task(1001, TaskStatus::Submitted, &[]),
            Vec::new(),
        ));
        let prompt = case.prompt().text;
        let task = |id: i64| -> Value {
            prompt
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find(|line| line["id"] == id && line.get("acceptance").is_some())
                .unwrap_or_else(|| panic!("no task {id} in {prompt}"))
        };
        let membership = &task(1000)["follow_up_membership"];
        assert_eq!(membership["origin"]["source_goal_id"], 9);
        // The worker's proposal reaches plan review as written (task 1508).
        assert_eq!(
            membership["origin"]["membership_proposal"],
            serde_json::json!({"classification": "required", "acceptance_items": ["(1)"], "reason": "w"})
        );
        assert_eq!(membership["latest_classification"], "out_of_scope");
        assert_eq!(membership["latest_version_matches"], false);
        let row = &membership["judgements"][0];
        for field in [
            "acceptance_items",
            "reason",
            "evidence",
            "destination_goal_id",
        ] {
            assert!(!row[field].is_null(), "{field}");
        }
        assert_eq!(
            (
                row["acceptance_version"].clone(),
                row["current_acceptance_version"].clone()
            ),
            (serde_json::json!(1), serde_json::json!(2))
        );
        assert!(task(1001).get("follow_up_membership").is_none());
        for instruction in [
            "start from the planner's mapping",
            "need not repeat the whole investigation",
            "never pass a judgement on its form alone",
            "the reason names no item of the acceptance",
            "the evidence disagrees with the receipt or the diff",
            "an unrelated catch-all goal",
            "the acceptance was weakened",
            "read the evidence around it",
        ] {
            assert!(prompt.contains(instruction), "{instruction}");
        }
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
        let revise = runtime_planner_prompt(db, ProposalId::new(3), &[], &["fix".into()])
            .unwrap()
            .text;
        let review = review_prompt(
            &task(7, "work", TaskStatus::InProgress),
            &run(7, RunStatus::Succeeded, Some(SHA)),
            "/r/review.md",
            None,
        )
        .text;
        let order = "in this order: its AGENTS.md; without one, its CLAUDE.md; without either, what its README, CI configuration and build configuration show; when none of them settles it, ";
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
        for text in [&plan_review, &revise, &review, &inbox_prompt(db).unwrap()] {
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

    /// A task whose description takes about `bytes`.
    fn sized_task(id: i64, status: TaskStatus, bytes: usize) -> Task {
        Task::restore(TaskRecord {
            goal_priority: None,
            id: TaskId::new(id),
            title: format!("task {id}"),
            description: "d".repeat(bytes),
            acceptance: "acceptance".into(),
            verification_commands: vec!["cargo test".into()],
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
            wait_for_build: false,
        })
        .unwrap()
    }

    /// The `dagq ...` commands a prompt names in backticks, each number
    /// written `ID`.
    fn named_commands(text: &str) -> BTreeSet<String> {
        let mut commands = BTreeSet::new();
        for part in text.split('`').skip(1).step_by(2) {
            if let Some(command) = part.strip_prefix("dagq ") {
                let mut normal = String::new();
                let mut in_number = false;
                for c in format!("dagq {command}").chars() {
                    if c.is_ascii_digit() {
                        if !in_number {
                            normal.push_str("ID");
                        }
                        in_number = true;
                    } else {
                        in_number = false;
                        normal.push(c);
                    }
                }
                commands.insert(normal);
            }
        }
        commands
    }

    /// What a prompt's notes name to read beyond the commands any plan
    /// review prompt names is one of [`PLAN_REVIEW_READS`].
    fn assert_reads_are_the_jobs(prompt: &str) {
        let base = named_commands(&plan_prompt(4, 0).0);
        for command in named_commands(prompt) {
            assert!(
                base.contains(&command) || PLAN_REVIEW_READS.contains(&command.as_str()),
                "{command} is not a read of the plan review job"
            );
        }
    }

    /// The prompt's sections add up to its bytes, which keep to the limit.
    fn assert_within_limit(prompt: &PlanReviewPrompt) {
        assert_eq!(prompt.bytes.total, prompt.text.len());
        assert_eq!(
            prompt.bytes.sections.values().sum::<usize>(),
            prompt.bytes.total
        );
        assert_eq!(prompt.bytes.limit, PLAN_REVIEW_PROMPT_LIMIT);
        assert!(
            prompt.bytes.total <= PLAN_REVIEW_PROMPT_LIMIT,
            "{:?}",
            prompt.bytes
        );
    }

    /// The JSON lines of `prompt` carrying `field`, by id.
    fn lines_with(prompt: &str, field: &str) -> BTreeMap<i64, Value> {
        prompt
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line.get(field).is_some())
            .filter_map(|line| Some((line["id"].as_i64()?, line)))
            .collect()
    }

    /// Task 1561: the full text keeps to its limit of tasks, and takes the
    /// most related first: one a task of the proposal depends on, then the
    /// most hotspots shared, the best place among the candidates, the
    /// newest; the rest are named with how to read them in full.
    #[test]
    fn plan_review_gives_in_full_the_most_related_tasks_first_within_its_limit() {
        use crate::domain::related::RelatedTask;
        let mut case = plan_case(0, 0);
        case.tasks = vec![proposal_task(
            long_task(1000, TaskStatus::Submitted, &[]),
            vec![TaskId::new(30)],
        )];
        case.expected.insert(
            TaskId::new(1000),
            vec!["src/hot.rs".to_owned(), "src/hot2.rs".to_owned()],
        );
        case.hotspots = vec![hotspot("src/hot.rs"), hotspot("src/hot2.rs")];
        case.candidates[0].related = vec![RelatedTask {
            id: 3,
            status: "ready".into(),
            title: "task 3".into(),
            score: 1.0,
            clues: Vec::new(),
            duplicate_of: None,
        }];
        case.queued = (1..=60)
            .rev()
            .map(|id| queued_task(long_task(id, TaskStatus::Ready, &[])))
            .collect();
        for id in 1..=60 {
            let files = if id == 10 || id == 11 {
                vec!["src/hot.rs".to_owned(), "src/hot2.rs".to_owned()]
            } else {
                vec!["src/hot.rs".to_owned()]
            };
            case.expected.insert(TaskId::new(id), files);
        }
        let prompt = case.prompt();
        assert_within_limit(&prompt);
        let full: Vec<i64> = prompt
            .text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line.get("description").is_some() && line["id"] != 1000)
            .map(|line| line["id"].as_i64().unwrap())
            .collect();
        let mut expected = vec![30, 11, 10, 3];
        expected.extend((45..=60).rev());
        assert_eq!(full, expected);
        assert_eq!(full.len(), QUEUED_FULL_TASKS);
        let summaries = lines_with(&prompt.text, "expected_files");
        assert_eq!(summaries[&45]["full_text_below"], true);
        assert_eq!(summaries[&44].get("full_text_below"), None);
        assert_eq!(prompt.bytes.omitted["full_text"], 40);
        assert!(
            prompt.text.contains(&format!(
                "(40 more ready or in-progress tasks that meet this rule are left out by its limit of {QUEUED_FULL_TASKS} tasks and {QUEUED_FULL_BYTES} bytes, the most related to the proposal first: 44, 43, 42,"
            )),
            "{}",
            prompt.text
        );
        assert!(
            prompt
                .text
                .contains("read one in full with `dagq show ID --full`)")
        );
        assert!(prompt.text.contains("The material below is held to limits"));
        assert_reads_are_the_jobs(&prompt.text);
    }

    /// Task 1561: one huge task does not take the full text's room: it is
    /// skipped and named, and the next ones fill the bytes left.
    #[test]
    fn a_huge_task_is_left_out_of_the_full_text_and_the_next_ones_fill_its_bytes() {
        let mut case = plan_case(0, 0);
        case.candidates.clear();
        case.queued = (1..=40)
            .rev()
            .map(|id| {
                let bytes = if id == 40 { 2_000_000 } else { 9_000 };
                queued_task(sized_task(id, TaskStatus::Ready, bytes))
            })
            .collect();
        for id in 1..=40 {
            case.expected
                .insert(TaskId::new(id), vec!["src/hot.rs".to_owned()]);
        }
        let prompt = case.prompt();
        assert_within_limit(&prompt);
        let full = lines_with(&prompt.text, "description");
        assert!(!full.contains_key(&40), "the huge task is not in full");
        let queued_full: usize = full
            .iter()
            .filter(|(id, _)| **id != 1000)
            .map(|(_, line)| line.to_string().len() + 1)
            .sum();
        assert!(queued_full <= QUEUED_FULL_BYTES, "{queued_full}");
        assert!(
            full.contains_key(&39) && full.len() > 5,
            "{:?}",
            full.keys()
        );
        assert!(prompt.bytes.sections["full_text"] <= QUEUED_FULL_BYTES + OMISSION_NOTE_BYTES);
        let summaries = lines_with(&prompt.text, "expected_files");
        assert_eq!(summaries[&40].get("full_text_below"), None);
        assert!(
            prompt
                .text
                .contains("the most related to the proposal first: 40, "),
            "{}",
            prompt.text
        );
    }

    /// Task 1561: a queue of hundreds of ready tasks that all touch the
    /// proposal's hotspot, with long precedents, many hotspots and the
    /// language's instruction, keeps to the overall limit: the summaries,
    /// the precedents and the full text are cut, each saying how many and
    /// how to read them.
    #[test]
    fn plan_review_prompt_of_hundreds_of_ready_tasks_on_a_shared_hotspot_keeps_to_its_limit() {
        let mut case = plan_case(600, 0);
        for id in 1..=600 {
            case.expected
                .insert(TaskId::new(id), vec!["src/hot.rs".to_owned()]);
        }
        case.hotspots = (0..15)
            .map(|n| hotspot(&format!("src/hot{n}.rs")))
            .collect();
        case.hotspots[0] = hotspot("src/hot.rs");
        case.precedents = (1..=30)
            .map(|id| {
                serde_json::from_value(json!({
                    "id": id, "kind": "decide", "task_id": 5, "run_id": null,
                    "question": "長い質問".repeat(200), "options": [],
                    "answer": "長い答え".repeat(200), "asked_by": "worker",
                    "reason_category": "scope", "created_at": 0,
                    "answered_at": 1, "closed_at": null,
                }))
                .unwrap()
            })
            .collect();
        case.language = Some(crate::domain::language::Language {
            tag: "ja".into(),
            source: crate::domain::language::LanguageSource::Repository,
        });
        let prompt = case.prompt();
        assert_within_limit(&prompt);
        let instruction = crate::domain::language::instruction("ja");
        assert!(prompt.text.ends_with(&instruction));
        assert_eq!(prompt.bytes.sections["language"], instruction.len() + 2);
        assert_eq!(prompt.bytes.omitted["full_text"], 600 - QUEUED_FULL_TASKS);
        let shown = lines_with(&prompt.text, "expected_files").len();
        assert!(shown < 600);
        assert_eq!(prompt.bytes.omitted["summaries"], 600 - shown);
        assert!(prompt.bytes.sections["summaries"] <= QUEUED_SUMMARY_BYTES + OMISSION_NOTE_BYTES);
        assert!(prompt.text.contains(&format!(
            "({} more ready or in-progress tasks, the least related to the proposal, are left out of this list by its limit of {QUEUED_SUMMARY_BYTES} bytes; list them with `dagq list --status ready,in_progress --limit 200` and read one with `dagq show ID --full`)",
            600 - shown
        )));
        let quoted = prompt.text.matches("\n- precedent: ask ").count();
        assert!(quoted <= PRECEDENT_ASKS, "{quoted}");
        assert_eq!(prompt.bytes.omitted["precedents"], 30 - quoted);
        assert!(prompt.bytes.sections["precedents"] <= PRECEDENT_BYTES + OMISSION_NOTE_BYTES);
        assert!(prompt.text.contains("read them with `dagq asks --all`)"));
        assert!(prompt.text.contains("\"path\":\"src/hot14.rs\""));
        assert_reads_are_the_jobs(&prompt.text);
    }

    /// Task 1561: required sections over their limit (a huge task of the
    /// proposal, a huge goal) are not cut silently: their largest pieces
    /// are replaced by the command that reads them, which the job's role
    /// may run, the prompt and `over_limit` say why, and the whole keeps to
    /// its limit; a proposal of thousands of tasks becomes one note.
    #[test]
    fn required_sections_over_their_limit_are_read_with_the_commands_the_job_may_run() {
        let mut case = plan_case(10, 0);
        case.tasks = vec![
            proposal_task(
                sized_task(1000, TaskStatus::Submitted, 1_000_000),
                Vec::new(),
            ),
            proposal_task(long_task(1001, TaskStatus::Submitted, &[]), Vec::new()),
        ];
        case.goals = vec![
            Goal::restore(GoalRecord {
                priority: Default::default(),
                id: GoalId::new(4),
                title: "a huge goal".into(),
                description: "g".repeat(300_000),
                acceptance: String::new(),
                constraints: String::new(),
                doc: None,
                status: GoalStatus::Open,
                closed_at: None,
                verdict: None,
                created_at: String::new(),
                updated_at: String::new(),
            })
            .unwrap(),
        ];
        let prompt = case.prompt();
        assert_within_limit(&prompt);
        let reason = prompt.bytes.over_limit.as_deref().unwrap();
        assert!(
            reason.contains(&format!("over their limit of {PLAN_REVIEW_REQUIRED_LIMIT}")),
            "{reason}"
        );
        assert!(prompt.text.contains(reason));
        assert_eq!(prompt.bytes.omitted["tasks"], 1);
        assert_eq!(prompt.bytes.omitted["goals"], 1);
        let stubs = lines_with(&prompt.text, "read_with");
        assert_eq!(stubs[&1000]["read_with"], "dagq show 1000 --full");
        assert_eq!(stubs[&1000]["title"], "task 1000");
        assert_eq!(stubs[&4]["read_with"], "dagq goal show 4 --full");
        for stub in stubs.values() {
            let read = named_commands(&format!("`{}`", stub["read_with"].as_str().unwrap()));
            assert!(PLAN_REVIEW_READS.contains(&read.first().unwrap().as_str()));
        }
        // The smaller task of the proposal stays whole.
        assert!(prompt.text.contains("description of 1001"));
        assert!(
            prompt.bytes.sections["tasks"] + prompt.bytes.sections["goals"]
                < PLAN_REVIEW_REQUIRED_LIMIT
        );
        assert_reads_are_the_jobs(&prompt.text);

        case.tasks = (1000..4000)
            .map(|id| proposal_task(long_task(id, TaskStatus::Submitted, &[]), Vec::new()))
            .collect();
        let prompt = case.prompt();
        assert_within_limit(&prompt);
        assert_eq!(prompt.bytes.omitted["tasks"], 3000);
        assert!(prompt.text.contains(
            "(3000 tasks, left out: list them with `dagq proposal show 1` and read each with `dagq show ID --full`)"
        ));
        assert!(prompt.text.contains(
            "every submitted task of the proposal (3000 of them; `dagq proposal show 1` lists them)"
        ));
        assert_reads_are_the_jobs(&prompt.text);
    }

    /// Task 1561: with the required sections near their limit, the optional
    /// ones share what is left of the overall limit in their order: the
    /// summaries, last, get what the others left, and the whole keeps to
    /// the limit.
    #[test]
    fn the_optional_sections_share_the_room_the_required_ones_leave() {
        use crate::domain::related::RelatedTask;
        let mut case = plan_case(0, 0);
        case.tasks = vec![proposal_task(
            sized_task(1000, TaskStatus::Submitted, 185_000),
            Vec::new(),
        )];
        case.queued = (1..=600)
            .rev()
            .map(|id| queued_task(sized_task(id, TaskStatus::Ready, 9_000)))
            .collect();
        for id in 1..=600 {
            case.expected
                .insert(TaskId::new(id), vec!["src/hot.rs".to_owned()]);
        }
        case.candidates[0].related = (1..=5)
            .map(|id| RelatedTask {
                id,
                status: "ready".into(),
                title: "t".repeat(9_000),
                score: 1.0,
                clues: Vec::new(),
                duplicate_of: None,
            })
            .collect();
        case.precedents = (1..=20)
            .map(|id| {
                serde_json::from_value(json!({
                    "id": id, "kind": "decide", "task_id": 5, "run_id": null,
                    "question": "q".repeat(400), "options": [],
                    "answer": "a".repeat(400), "asked_by": "worker",
                    "reason_category": "scope", "created_at": 0,
                    "answered_at": 1, "closed_at": null,
                }))
                .unwrap()
            })
            .collect();
        let prompt = case.prompt();
        assert_within_limit(&prompt);
        assert_eq!(prompt.bytes.over_limit, None);
        assert!(prompt.bytes.sections["tasks"] > 185_000);
        assert!(
            prompt.bytes.sections["full_text"] > 90_000,
            "{:?}",
            prompt.bytes
        );
        assert!(
            prompt.bytes.sections["summaries"] < QUEUED_SUMMARY_BYTES,
            "{:?}",
            prompt.bytes
        );
        assert!(
            prompt.bytes.total > PLAN_REVIEW_PROMPT_LIMIT - 20_000,
            "{:?}",
            prompt.bytes
        );
        assert!(
            prompt
                .text
                .contains("are left out of this list by its limit of")
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
            goal_priority: None,
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
            wait_for_build: false,
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

    /// Task 1420 (ADR-t1420-1): every worker's prompt, interactive or
    /// headless, on Claude or Codex, maps the acceptance criteria before
    /// the receipt once, right before the receipt's instructions; every
    /// resume and revise request maps them again before it rewrites the
    /// receipt; the Codex review line leaves the comparison with the
    /// criteria to that step, and the run's review prompt is untouched.
    #[test]
    fn every_worker_text_that_writes_a_receipt_maps_the_acceptance_once() {
        let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
        for (provider, mode) in [
            (Provider::Claude, WorkerMode::Interactive),
            (Provider::Claude, WorkerMode::Headless),
            (Provider::Codex, WorkerMode::Headless),
        ] {
            let run = run_on(provider, mode);
            let texts = session_texts(&task, &run);
            let first = &texts[0];
            assert_eq!(first.matches(ACCEPTANCE_MAP).count(), 1, "{first}");
            assert!(
                first.contains(&format!(
                    "{ACCEPTANCE_MAP}{DOCS_CHECK}Write a completion receipt to"
                )),
                "{first}"
            );
            assert_eq!(
                first.matches("map each acceptance criterion").count(),
                1,
                "{first}"
            );
            assert!(!first.contains(ACCEPTANCE_REMAP), "{first}");
            // The nine resume requests and the revise request.
            for request in &texts[1..11] {
                assert!(
                    request.contains(&format!("renaming it. {ACCEPTANCE_REMAP}")),
                    "{provider:?} {mode:?}: {request}"
                );
                assert!(!request.contains(ACCEPTANCE_MAP), "{request}");
            }
        }
        let codex = review_line(Route::Headless(Provider::Codex));
        assert!(
            !codex.contains("against the acceptance criteria"),
            "{codex}"
        );
        assert!(!codex.contains("map each acceptance criterion"), "{codex}");
        assert!(codex.contains("as you map the acceptance criteria to it"));
        // The steps ask for no new command, and stay short.
        for text in [ACCEPTANCE_MAP, ACCEPTANCE_REMAP] {
            assert!(text.len() <= 600, "{}", text.len());
            assert!(!text.contains("cargo") && !text.contains('`'), "{text}");
        }
        let review = review_prompt(
            &task,
            &run_on(Provider::Claude, WorkerMode::Headless),
            "r",
            None,
        )
        .text;
        assert!(!review.contains(ACCEPTANCE_MAP) && !review.contains(ACCEPTANCE_REMAP));
    }

    /// Task 1508 (ADR-t1504-2 decision 11): every worker's prompt shows a
    /// follow_up's membership proposal in the receipt's example and says
    /// once how to write it, as a proposal and not a judgement; every
    /// resume and revise request says it again in short.
    #[test]
    fn every_worker_text_that_writes_a_receipt_proposes_follow_up_membership() {
        let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
        for (provider, mode) in [
            (Provider::Claude, WorkerMode::Interactive),
            (Provider::Claude, WorkerMode::Headless),
            (Provider::Codex, WorkerMode::Headless),
        ] {
            let run = run_on(provider, mode);
            let texts = session_texts(&task, &run);
            let first = &texts[0];
            assert_eq!(first.matches(FOLLOW_UP_PROPOSAL).count(), 1, "{first}");
            assert!(
                first.contains(r#""membership_proposal":{"classification":"required, out_of_scope or undecided","acceptance_items":["..."],"reason":"..."}"#),
                "{first}"
            );
            assert!(!first.contains(FOLLOW_UP_PROPOSAL_AGAIN), "{first}");
            for request in &texts[1..11] {
                assert!(
                    request.contains(&format!("{ACCEPTANCE_REMAP} {FOLLOW_UP_PROPOSAL_AGAIN}")),
                    "{provider:?} {mode:?}: {request}"
                );
            }
        }
        for part in [
            "write the problem and its evidence",
            "required when you think the goal's acceptance cannot be met without it, out_of_scope when it can, undecided when you cannot tell",
            "acceptance_items, the goal's acceptance items it bears on",
            "It is only a proposal: the planner decides where the follow_up belongs, so do not judge or move it yourself.",
        ] {
            assert!(FOLLOW_UP_PROPOSAL.contains(part), "{part}");
        }
        assert!(FOLLOW_UP_PROPOSAL_AGAIN.contains("the problem and its evidence"));
        assert!(FOLLOW_UP_PROPOSAL_AGAIN.contains("not a judgement"));
        for text in [FOLLOW_UP_PROPOSAL, FOLLOW_UP_PROPOSAL_AGAIN] {
            assert!(!text.contains("cargo") && !text.contains('`'), "{text}");
        }
    }

    /// Task 1428 (ADR-t1428-1): every worker's prompt, interactive or
    /// headless, on Claude or Codex, checks the documents against the diff
    /// once, right after the acceptance map and as part of it, not as a
    /// second map; the Codex review line does not say it again; every
    /// resume and revise request keeps the record of the check inside the
    /// remap's phrase, not as another sentence; the run's review prompt is
    /// untouched. Task 1688 (ADR-t1688-1): the check finds candidates by a
    /// search for each changed name and gives the names searched in
    /// summary, still naming no command, tool or repository path.
    #[test]
    fn every_worker_text_that_writes_a_receipt_checks_the_documents_once() {
        let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
        for (provider, mode) in [
            (Provider::Claude, WorkerMode::Interactive),
            (Provider::Claude, WorkerMode::Headless),
            (Provider::Codex, WorkerMode::Headless),
        ] {
            let run = run_on(provider, mode);
            let texts = session_texts(&task, &run);
            let first = &texts[0];
            assert_eq!(first.matches(DOCS_CHECK).count(), 1, "{first}");
            assert!(first.contains(&format!("{ACCEPTANCE_MAP}{DOCS_CHECK}")));
            for phrase in ["the documents on what you changed", "the names searched"] {
                assert_eq!(
                    first.matches(phrase).count(),
                    1,
                    "{provider:?} {mode:?}: {first}"
                );
            }
            for request in &texts[1..11] {
                assert!(!request.contains(DOCS_CHECK), "{request}");
                assert_eq!(
                    request
                        .matches("the documents you checked against the diff")
                        .count(),
                    1,
                    "{provider:?} {mode:?}: {request}"
                );
            }
        }
        // The check names neither a criterion map nor the own-diff reading.
        assert!(!DOCS_CHECK.contains("map each acceptance criterion"));
        assert!(ACCEPTANCE_REMAP.contains("rewrite its phrase in summary, with the documents"));
        // The Codex review line reads the own diff; the check of the
        // documents is said once, in DOCS_CHECK, not again there.
        let codex = review_line(Route::Headless(Provider::Codex));
        assert!(codex.contains("read your own diff"), "{codex}");
        assert!(!codex.contains("documents") && !codex.contains("docs_drift"));
        assert!(!DOCS_CHECK.contains("own diff"));
        // Candidates come from a search for each changed name, and summary
        // gives the names searched (ADR-t1688-1).
        assert!(DOCS_CHECK.contains("searching the repository for each changed name"));
        // No new command, no tool, no repository path, and short
        // (ADR-t1428-1, ADR-t1688-1; goals 91 and 112's constraints).
        assert!(DOCS_CHECK.len() <= 470, "{}", DOCS_CHECK.len());
        for word in ["cargo", "`", "docs/", "grep"] {
            assert!(!DOCS_CHECK.contains(word), "{word}");
        }
        let review = review_prompt(
            &task,
            &run_on(Provider::Claude, WorkerMode::Headless),
            "r",
            None,
        )
        .text;
        assert!(!review.contains(DOCS_CHECK));
        assert!(!review.contains("the documents you checked against the diff"));
    }

    /// Task 1429 (ADR-t1428-1 decision 5): the run's review reads the
    /// documents on the changed behavior against the diff and the summary,
    /// once, before the acceptance; the verdict's JSON, the concern's
    /// recommendation and the reason codes stay as they were.
    #[test]
    fn the_review_prompt_checks_the_documents_and_keeps_its_verdict() {
        let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
        let review = review_prompt(
            &task,
            &run_on(Provider::Codex, WorkerMode::Headless),
            "r",
            None,
        )
        .text;
        assert_eq!(review.matches(REVIEW_DOCS_CHECK).count(), 1, "{review}");
        assert!(review.contains(&format!(
            "Do not change any file.\n{REVIEW_RULES}\n{REVIEW_DOCS_CHECK}\nAcceptance criteria of the task:"
        )));
        for part in [
            "named by the task's description or context or by the summary",
            "A document's diff alone does not show the change is right",
            "check the summary's reason for leaving a document as it is",
            "as docs_drift whether or not the task named it",
        ] {
            assert!(REVIEW_DOCS_CHECK.contains(part), "{part}");
        }
        assert!(
            REVIEW_DOCS_CHECK.len() <= 400,
            "{}",
            REVIEW_DOCS_CHECK.len()
        );
        assert!(!REVIEW_DOCS_CHECK.contains("cargo") && !REVIEW_DOCS_CHECK.contains('`'));
        // Unchanged: the verdict's shape, the concern's recommendation and
        // the reason codes with their definitions.
        assert!(review.contains(
            "Answer with one JSON object and nothing else, matching this schema:\n\
             {\"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [{\"text\": string, \"codes\": [string]}], \"summary\": string, \"recommendation\": \"land\" | \"send_back\" | null, \"confidence\": \"high\" | \"low\" | null, \"reason_category\": \"scope\" | \"discard\" | null}\n\
             reasons lists each finding (empty for pass); summary is one or two sentences; recommendation, confidence and reason_category are for a concern only (null for pass and revise).\n"
        ));
        assert!(review.contains(&format!(
            "{CONCERN_RECOMMENDATION}{}Answer with one JSON object",
            reason_codes_section(review_reason::REVIEW_CODES)
        )));
        assert!(
            CONCERN_RECOMMENDATION
                .starts_with("For a concern, also recommend what to do, and how sure you are:\n")
        );
        assert!(CONCERN_RECOMMENDATION.ends_with(
            "Leave scope and discard to the person rather than deciding them; when in doubt, say low.\n\n"
        ));
        assert!(!CONCERN_RECOMMENDATION.contains("document"));
        assert!(!reason_codes_section(review_reason::REVIEW_CODES).contains(REVIEW_DOCS_CHECK));
    }

    /// ADR-t1470-1 decision 2: a Claude review loads no setting sources,
    /// so the worktree's CLAUDE.md is not its memory; the review prompt
    /// names the repository's instructions to read instead, right after
    /// the material and before the check of the documents (task 1429),
    /// and says nothing else of a provider's settings.
    #[test]
    fn the_review_prompt_names_the_repositorys_instructions() {
        let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
        let review = review_prompt(
            &task,
            &run_on(Provider::Claude, WorkerMode::Headless),
            "r",
            None,
        )
        .text;
        let material = "Read the worktree if you need more. Do not change any file.\n";
        assert!(
            review.contains(&format!("{material}{REVIEW_RULES}\n{REVIEW_DOCS_CHECK}")),
            "{review}"
        );
        assert!(REVIEW_RULES.contains("AGENTS.md and CLAUDE.md"));
        assert!(!REVIEW_RULES.contains(".claude") && !REVIEW_RULES.contains('`'));
    }

    /// Task 978: a task that needs a path outside its declared paths ends
    /// in a failed receipt naming them, not in an ask (ADR-0029 decision
    /// 5); and before each worker_question the prompts, interactive and
    /// headless, first send the worker to the repository's rules on what
    /// is not asked.
    #[test]
    fn a_path_outside_the_scope_is_a_failed_receipt_and_asks_follow_the_repository_rules() {
        let scoped = Task::restore(TaskRecord {
            goal_priority: None,
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
            wait_for_build: false,
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
            .text
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
        for retired in [
            "answer_known_dialog",
            "close_and_proceed",
            "Last lines of the session's screen",
            "idle at its prompt",
        ] {
            assert!(!interactive.contains(retired), "{retired}: {interactive}");
        }
        assert!(interactive.contains("Last turns of the headless session"));
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

    /// The goal review is shown the goal's follow-ups and told that an
    /// out-of-scope one is left out of the acceptance and a required one is
    /// judged with it (ADR-t1504-2 decision 8).
    #[test]
    fn the_goal_review_judges_required_follow_ups_and_leaves_out_of_scope_ones_out() {
        let prompt = goal_review_prompt(&GoalReviewMaterial {
            goal: json!({"id": 7}),
            tasks: Vec::new(),
            follow_ups: vec![
                json!({"task_id": 9, "judgements": [{"classification": "out_of_scope"}]}),
            ],
            events: Vec::new(),
            previous: Vec::new(),
            gaps_in_a_row: 0,
            repo_root: Path::new("/repo"),
        })
        .text;
        assert!(prompt.contains(r#""task_id":9"#), "{prompt}");
        assert!(prompt.contains(
            "A follow-up judged out_of_scope is not part of the acceptance: leave its work out of your judgement and do not wait for it or list it as a gap."
        ));
        assert!(
            prompt.contains(
                "A follow-up judged required is part of it: judge the goal with its work"
            )
        );
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
            follow_ups: Vec::new(),
            events: Vec::new(),
            previous: Vec::new(),
            gaps_in_a_row: 0,
            repo_root: Path::new("/repo"),
        })
        .text;
        // With every section cut, so the prompt names each read of what it
        // left out.
        let observer = crate::application::observer::observer_prompt(
            crate::application::observer::ObserveMode::Hourly,
            "dagq",
            Some(crate::domain::EventId::new(12)),
            "1791005872",
            &json!({"kpi": {"breaches": [{}], "config": {}}, "open_asks": [{}], "findings": [{}], "notes": [{}],
                    "stats": {"alerts": [{}], "running_alerts": [{}], "overall": {}},
                    "improvements": {"running": 0}, "graph": {"candidates": [1], "critical": [1]}}),
            1,
        )
        .unwrap()
        .text;
        let throughput = [ReviewMode::Hourly, ReviewMode::Daily, ReviewMode::Weekly]
            .map(|mode| {
                crate::application::throughput_review::review_prompt(
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
            "observe_input",
        ] {
            assert!(named.contains(read), "no prompt names {read}: {named:?}");
        }
    }

    /// The draft planner decides what it can recommend and records why;
    /// only what it cannot settle, low confidence or a follow_up requiring
    /// person adoption for registration-time facts or depth goes to a person,
    /// with a recommendation and confidence (ADR-t451-1 decision 5).
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
        .unwrap()
        .text;
        for part in [
            "its Basic policy above all: decide what you can recommend yourself and go on, asking no one, and leave why in the record",
            "Say in its `--context` why you adopted it.",
            "record why with `dagq note --task 9 --text '<why>'`",
            "3. Ask: only for a draft you cannot decide yourself:",
            "that the queue, the repository (its code and the decisions it records) and a person's precedents cannot settle",
            "(b) your confidence in the decision is low; or (c)",
            "when none of them settles it, decide them yourself from the source, the decisions the repository records and a person's precedents, and ask a person with a `planner_question` ask as below only when that material cannot settle them and the decision is a person's (`scope` or `discard`) or your confidence in it is low.",
            "(c) it is a follow_up draft past the runtime's follow_up limit, 3 or more follow-ups from a person's judgement, a source goal that was missing, closed or unknown at registration (even if its current goal is open), or no current goal or a closed current goal",
            "dagq ask --task 9 --kind planner_question --because scope --recommend <adopt|cancel|keep_draft> --confidence <high|low>",
            "on keep_draft leave the draft as it is, record why with `dagq note --task 9",
            "A draft kept so stays a draft until a person has the inbox record a planning request that names it; no planner of the runtime's is opened for it again.",
            "The runtime refuses your submit of a follow_up draft past that limit unless a person answered adopt or already adopted it",
            "Membership changes (`set-goal` or `judge-follow-up`) do not count as adoption or reset depth; an existing person's adopt remains valid",
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

    /// Task 1508 (ADR-t1504-1, ADR-t1504-2): a follow_up draft's planner
    /// sees the worker's membership proposal, and judges and records where
    /// the draft belongs before it adopts, drops or asks: membership apart
    /// from adoption and priority, an existing goal found first, no
    /// unrelated large goal, no acceptance weakened. A goal_gap draft gets
    /// neither.
    #[test]
    fn the_follow_up_draft_planner_records_membership_before_it_decides() {
        let planner = |origin, material: Value| {
            let draft = task(9, "follow", TaskStatus::Draft);
            let key = BundleKey::of(origin, &material, draft.id());
            let members = [(
                DraftTarget {
                    task: draft,
                    origin,
                    material,
                    planners: 0,
                },
                1,
            )];
            draft_planner_prompt(&DraftPlannerMaterial {
                db: Path::new("/q/queue.db"),
                key: &key,
                members: &members,
                source: None,
                receipt: None,
                goals: &[],
                answer: None,
            })
            .unwrap()
            .text
        };
        let proposal =
            json!({"classification": "out_of_scope", "acceptance_items": ["(1)"], "reason": "r"});
        let prompt = planner(
            DraftOrigin::FollowUp,
            json!({"source_run_id": RUN, "source_task_id": 3, "index": 0, "membership_proposal": proposal,
                "source_goal_id": 12, "source_goal_state": "open", "source_goal_provenance": "recorded"}),
        );
        assert!(prompt.contains("Source goal (at registration; judge against its acceptance with `dagq goal show <id>`, not the current goal's): goal 12 (open, recorded)\n"), "{prompt}");
        for part in [
            "Read its earlier judgements with `dagq show 9` (membership_judgements): one that still holds needs no new row; to change a required or out_of_scope one, record the other with `--corrects <its id>`; it never goes back to undecided.",
            "A draft you drop (a duplicate, already done, not worth doing) may skip the record when it would need a new goal",
            "record undecided with why, ask as step 3 says with the membership question in it, and on the answer record required or out_of_scope before you do what it says",
        ] {
            assert!(prompt.contains(part), "{part} in {prompt}");
        }
        let line = format!(
            "Membership proposal (the worker's; where you start, not a judgement): {proposal}\n"
        );
        assert!(prompt.contains(&line), "{prompt}");
        for part in [
            "first judge where it belongs, apart from whether it is worth doing and from its priority",
            "Start from the worker's membership proposal and decide the meaning yourself: can the source goal's acceptance be met without this draft?",
            "When it cannot, it is required and belongs to the source goal; when it can, it is out_of_scope and belongs to another goal",
            "look for a fitting existing goal with `dagq search` first, make one only when none fits, and never park it in an unrelated large goal",
            "Record the judgement before you adopt, drop or ask: `dagq judge-follow-up 9 --classification <required|out_of_scope|undecided> --acceptance-item",
            "`--destination-goal <goal>` for out_of_scope",
            "Moving a draft to another goal neither adopts it nor raises its priority.",
            "Never weaken a goal's acceptance to leave a follow_up out",
        ] {
            assert!(prompt.contains(part), "{part} in {prompt}");
        }
        let step = prompt.find("first judge where it belongs").unwrap();
        assert!(step < prompt.find("Then do exactly one of these three").unwrap());
        let none = planner(
            DraftOrigin::FollowUp,
            json!({"source_run_id": RUN, "source_task_id": 3, "index": 0}),
        );
        assert!(none.contains(
            "Membership proposal (the worker's; where you start, not a judgement): (none)\n"
        ));
        assert!(
            none.contains("not the current goal's): unknown\n"),
            "{none}"
        );
        let text = planner(
            DraftOrigin::FollowUp,
            json!({"source_run_id": RUN, "source_task_id": 3, "index": 0, "membership_proposal": "unsure", "source_goal_id": null, "source_goal_state": "none"}),
        );
        assert!(text.contains("not the current goal's): none\n"), "{text}");
        assert!(text.contains("not a judgement): \"unsure\"\n"), "{text}");
        let gap = planner(
            DraftOrigin::GoalGap,
            json!({"goal_id": 1, "goal_review_id": 2, "criterion": "c", "summary": "s"}),
        );
        assert!(!gap.contains("Membership proposal"), "{gap}");
        let reopened = planner(
            DraftOrigin::Reopened,
            json!({"reason": "r", "proposal_id": 4, "reviewed_proposal_id": 5}),
        );
        assert!(!reopened.contains("Membership proposal"), "{reopened}");
        assert!(
            !reopened.contains("first judge where it belongs"),
            "{reopened}"
        );
        assert!(!gap.contains("first judge where it belongs"), "{gap}");
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
        .unwrap()
        .text;
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

    /// A task in progress with `goal` and `context`.
    fn grouped_task(id: i64, title: &str, goal: Option<i64>, context: &str) -> Task {
        Task::restore(TaskRecord {
            goal_priority: None,
            id: TaskId::new(id),
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            status: TaskStatus::InProgress,
            goal_id: goal.map(GoalId::new),
            context: context.into(),
            created_at: String::new(),
            updated_at: String::new(),
            worker: crate::domain::worker::Worker::CLAUDE_INTERACTIVE,
            named_mode: None,
            wait_for_build: false,
        })
        .unwrap()
    }

    /// The Sibling section of a task with a goal lists only the in-progress
    /// tasks of that goal; a task without a goal sees every task in
    /// progress; neither lists itself.
    #[test]
    fn siblings_are_the_tasks_in_progress_of_the_same_goal() {
        let in_progress = || {
            vec![
                grouped_task(1, "test task", None, ""),
                grouped_task(2, "a first", Some(1), ""),
                grouped_task(3, "b only", Some(2), ""),
                grouped_task(4, "a second", Some(1), ""),
                grouped_task(5, "alone", None, ""),
            ]
        };
        let ids = |task: &Task| -> Vec<i64> {
            siblings_in_progress(task, in_progress())
                .iter()
                .map(|task| task.id().as_i64())
                .collect()
        };
        // Goal a: its other task only, not task 1 without a goal.
        assert_eq!(ids(&grouped_task(4, "a second", Some(1), "")), [2]);
        assert_eq!(ids(&grouped_task(2, "a first", Some(1), "")), [4]);
        // Goal b: nothing else of it is in progress.
        assert!(ids(&grouped_task(3, "b only", Some(2), "")).is_empty());
        // No goal: every task in progress but itself, in the given order.
        assert_eq!(ids(&grouped_task(5, "alone", None, "")), [1, 2, 3, 4]);
        assert!(siblings_in_progress(&grouped_task(9, "x", None, ""), Vec::new()).is_empty());
    }

    /// A task's prompt carries its goal as a Goal section (ID, title,
    /// description, acceptance, constraints and the doc path, unread), its
    /// context as a Context section, its predecessors and its siblings; a
    /// task without them says so in the same place, so both prompts have
    /// the same sequence of sections.
    #[test]
    fn prompt_describes_the_goal_the_context_and_its_company_and_keeps_one_shape_without_them() {
        let goal = Goal::restore(GoalRecord {
            priority: Default::default(),
            id: GoalId::new(3),
            title: "goal title".into(),
            description: "goal description\nsecond line".into(),
            acceptance: "goal acceptance".into(),
            constraints: "goal constraints".into(),
            doc: Some("docs/plans/goal.md".into()),
            status: GoalStatus::Open,
            closed_at: None,
            verdict: None,
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        let own_run = run(2, RunStatus::Claimed, None);
        let grouped_task = grouped_task(
            2,
            "grouped",
            Some(3),
            "why this task exists\nread docs/design/x.md first",
        );
        let landed = PredecessorSummary {
            task_id: TaskId::new(1),
            title: "test task".into(),
            result_commit: SHA.into(),
            summary: "done".into(),
        };
        let sibling = task(4, "a second", TaskStatus::InProgress);
        let grouped = prompt(
            &grouped_task,
            &own_run,
            Some(&goal),
            &[landed],
            &[],
            &[sibling],
            None,
            &[],
        )
        .unwrap();
        let alone_task = task(2, "alone", TaskStatus::InProgress);
        let alone = prompt(&alone_task, &own_run, None, &[], &[], &[], None, &[]).unwrap();

        assert!(
            grouped.contains(
                "Goal (the higher-level problem this task and its sibling tasks solve together):\n\
                 Goal ID: 3\nGoal title: goal title\nGoal description:\ngoal description\nsecond line\n\
                 Goal acceptance:\ngoal acceptance\nGoal constraints:\ngoal constraints\n\
                 Goal doc: docs/plans/goal.md (a path in the repository; read it for the full picture)\n"
            ),
            "{grouped}"
        );
        assert!(
            grouped.contains(
                "Context (why this task exists and what to read first):\n\
                 why this task exists\nread docs/design/x.md first\n"
            ),
            "{grouped}"
        );
        assert!(
            grouped.contains(&format!(
                "Predecessor tasks (their changes are already in your base commit):\n\
                 - task 1: test task; result commit {SHA}; summary: done\n"
            )),
            "{grouped}"
        );
        assert!(
            grouped.contains(
                "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n\
                 - task 4: a second\n"
            ),
            "{grouped}"
        );
        for absent in [
            "Goal: none",
            "Context: none",
            "Predecessor tasks: none",
            "Sibling tasks in progress: none",
        ] {
            assert!(!grouped.contains(absent), "{absent}: {grouped}");
        }
        for none in [
            "Goal: none, this task stands alone\n",
            "Context: none\n",
            "Predecessor tasks: none\n",
            "Sibling tasks in progress: none\n",
        ] {
            assert!(alone.contains(none), "{none}: {alone}");
        }
        assert!(!alone.contains("goal title"), "{alone}");
        // Both prompts have the same sections in the same order, between the
        // verification commands and the receipt contract.
        let order = |text: &str| -> Vec<usize> {
            [
                "Task title:",
                "Verification commands",
                "Goal",
                "Context",
                "Predecessor tasks",
                "Sibling tasks in progress",
                "Your assignment is this task only.",
                "Write a completion receipt",
            ]
            .iter()
            .map(|heading| {
                text.find(heading)
                    .unwrap_or_else(|| panic!("{heading}: {text}"))
            })
            .collect()
        };
        for text in [&grouped, &alone] {
            assert!(
                order(text).windows(2).all(|pair| pair[0] < pair[1]),
                "{text}"
            );
            // The receipt example shows the optional follow_ups, and the scope rule names it.
            assert!(
                text.contains(
                    "\"summary\":\"...\",\"follow_ups\":[{\"title\":\"...\",\"description\":\"...\",\"category\":\"...\",\"membership_proposal\":{\"classification\":\"required, out_of_scope or undecided\",\"acceptance_items\":[\"...\"],\"reason\":\"...\"}}]}\n"
                ),
                "{text}"
            );
            assert!(
                text.contains(
                    "Your assignment is this task only. Do not change what a sibling task owns; \
                     if you find work outside this task, record it in the receipt as follow_ups instead of doing it.\n"
                ),
                "{text}"
            );
            assert!(text.contains("follow_ups is optional"), "{text}");
            // Each follow_up carries a category (ADR-t947-3), a worker_question a topic (ADR-t947-2).
            assert!(text.contains(&follow_up_categories_line()), "{text}");
            assert!(text.contains("flaky_test ("), "{text}");
            assert!(text.contains(&worker_question_topics_line()), "{text}");
            assert!(text.contains("--topic <code>"), "{text}");
            assert!(text.contains("task_overlap ("), "{text}");
            // The worker reads only what its run needs, never the queue.
            assert!(text.contains(WORKER_READING), "{text}");
            assert!(
                text.contains("the worker section of the repository instructions"),
                "{text}"
            );
            assert!(text.contains("Do not run `dagq list`"), "{text}");
            assert!(!text.contains("Read its repository instructions"), "{text}");
        }
    }

    /// Task 1571 (ADR-t1566-1 decisions 4 to 6): how much a prompt held to
    /// its limits may take before the language's instruction is added.
    fn within(fitted: &FittedPrompt, limit: usize) {
        assert_eq!(fitted.bytes.limit, limit);
        assert_eq!(fitted.bytes.total, fitted.text.len());
        assert!(
            fitted.text.len() <= limit - prompt_fit::LANGUAGE_ROOM,
            "{} bytes past the limit of {limit}: {:?}",
            fitted.text.len(),
            fitted.bytes
        );
        assert_eq!(
            fitted.bytes.sections.values().sum::<usize>(),
            fitted.bytes.total,
            "{:?}",
            fitted.bytes
        );
        // The language's instruction is counted when it is added, and the
        // whole stays within the limit.
        let language = crate::domain::language::Language {
            tag: "ja".into(),
            source: crate::domain::language::LanguageSource::Repository,
        };
        let with = fitted.clone().with_language(Some(&language));
        assert_eq!(with.bytes.total, with.text.len());
        assert!(with.bytes.total <= limit, "{}", with.bytes.total);
        assert!(with.bytes.sections.contains_key("language"));
    }

    fn big(what: &str, bytes: usize) -> String {
        format!("{what} ").repeat(bytes / (what.len() + 1) + 1)
    }

    fn goal_of(id: i64, bytes: usize) -> Goal {
        Goal::restore(GoalRecord {
            priority: Default::default(),
            id: GoalId::new(id),
            title: big("goal title", 2_000),
            description: big("goal description", bytes),
            acceptance: big("goal acceptance", bytes),
            constraints: big("goal constraints", bytes),
            doc: Some("docs/plans/goal.md".into()),
            status: GoalStatus::Open,
            closed_at: None,
            verdict: None,
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap()
    }

    fn goal_tasks(n: i64) -> Vec<GoalTask> {
        (1..=n)
            .map(|id| GoalTask {
                id: TaskId::new(id),
                title: big("a goal task's title", 500),
                status: TaskStatus::Completed,
                priority: crate::domain::Priority::Normal,
                priority_source: crate::domain::PrioritySource::Goal,
            })
            .collect()
    }

    fn asked(id: i64, bytes: usize) -> Ask {
        serde_json::from_value(json!({
            "id": id, "kind": "planner_question", "task_id": null, "run_id": null,
            "question": big("question", bytes), "options": [], "answer": big("answer", bytes),
            "asked_by": "planner", "reason_category": "scope", "created_at": 0,
            "answered_at": 1, "closed_at": null,
        }))
        .unwrap()
    }

    fn event_of(id: i64, bytes: usize) -> RunEvent {
        RunEvent {
            id: crate::domain::EventId::new(id),
            task_id: Some(TaskId::new(7)),
            goal_id: None,
            run_id: None,
            kind: "turn_finished".into(),
            payload: json!({"message": big("evidence", bytes)}),
            created_at: String::new(),
            actor: None,
        }
    }

    fn big_task(id: i64, bytes: usize) -> Task {
        Task::restore(TaskRecord {
            goal_priority: None,
            id: TaskId::new(id),
            title: big("title", 5_000),
            description: big("description", bytes),
            acceptance: big("acceptance", bytes),
            verification_commands: vec![big("cargo test", bytes)],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            status: TaskStatus::Draft,
            goal_id: Some(GoalId::new(1)),
            context: big("context", bytes),
            created_at: String::new(),
            updated_at: String::new(),
            worker: crate::domain::worker::Worker::CLAUDE_INTERACTIVE,
            named_mode: None,
            wait_for_build: false,
        })
        .unwrap()
    }

    /// The goal review of a goal with many tasks, each with a long receipt
    /// (job 24 of production had 50 tasks in 200,200 bytes), many notes and
    /// earlier reviews, and a goal past its own limit stays within
    /// [`GOAL_REVIEW_PROMPT_LIMIT`]: the tasks that landed come first, the
    /// newest first, the rest are stubs with how to read them, and the
    /// required goal is cut and said so.
    #[test]
    fn a_goal_review_prompt_of_many_large_tasks_stays_within_its_limits() {
        let tasks: Vec<Value> = (1..=200)
            .map(|id| {
                let mut task = json!({"id": id, "title": format!("task {id}"), "status": "completed",
                    "description": big("description", 3_000), "acceptance": big("acceptance", 1_000)});
                // The even ones landed, with a receipt of 10 KB.
                if id % 2 == 0 {
                    task["landed"] = json!({"run_id": RUN, "summary": big("summary", 10_000)});
                }
                task
            })
            .collect();
        let material = GoalReviewMaterial {
            goal: json!({"id": 7, "description": big("goal", 50_000)}),
            tasks,
            follow_ups: (1..=100)
                .map(|id| json!({"task_id": 1000 + id, "reason": big("why", 2_000)}))
                .collect(),
            events: (1..=200)
                .map(|id| json!({"kind": "observation", "at": id, "payload": big("note", 1_000)}))
                .collect(),
            previous: (1..=50)
                .map(|id| json!({"id": id, "summary": big("review", 3_000)}))
                .collect(),
            gaps_in_a_row: 1,
            repo_root: Path::new("/repo"),
        };
        let fitted = goal_review_prompt(&material);
        within(&fitted, GOAL_REVIEW_PROMPT_LIMIT);
        let text = &fitted.text;
        let bytes = &fitted.bytes;
        // Every section left something out, and says how to read it.
        for section in ["goal", "tasks", "follow_ups", "events", "previous"] {
            assert!(
                bytes.omitted.get(section).is_some_and(|n| *n > 0),
                "{section}: {bytes:?}"
            );
        }
        assert!(
            bytes.over_limit.as_deref().unwrap().starts_with("goal: "),
            "{bytes:?}"
        );
        assert!(text.contains("read it whole with `dagq goal show 7 --full`"));
        assert!(text.contains("The tasks left out, in summary:"));
        assert!(text.contains("tasks left out by this section's limit:"));
        assert!(text.contains("`dagq show ID --full` for each, and `dagq events --full --task ID --kind integration_receipt`"));
        assert!(text.contains("notes and edits (the oldest) left out by this section's limit"));
        assert!(text.contains("To read them: `dagq events --full --all --goal 7`."));
        assert!(
            text.contains(
                "To read them: `dagq events --full --goal 7 --kind goal_review_finished`."
            )
        );
        // The newest landed task is in full; the oldest one that did not
        // land is left out to a stub.
        assert!(
            text.contains(r#""id":200,"#),
            "the newest landed task is shown"
        );
        assert!(text.contains(r#"{"id":1,"landed":false,"status":"completed","title":"task 1"}"#));
        // A task past its own limit is cut and names how to read it.
        assert!(text.contains("bytes left out; read it whole with `dagq show ID --full`"));
        // The instructions and the schema are whole.
        assert!(text.contains("Answer with one JSON object and nothing else"));
        for read in GOAL_REVIEW_READS {
            let form = read.replace("ID", "7");
            let named = text.contains(&form) || text.contains(read);
            assert!(named, "{read} is not named");
        }
    }

    /// The run review of a task whose title, acceptance and required
    /// subagents are huge stays within [`RUN_REVIEW_PROMPT_LIMIT`] and
    /// names where in the review material the acceptance is whole.
    #[test]
    fn a_run_review_prompt_of_a_huge_acceptance_stays_within_its_limits() {
        let task = big_task(7, 100_000);
        let subagents = format!(
            "\nRequired review subagents: ... in /runs/run/review-subagents-1.json\n{}{}",
            big("- agent (changed: src/a.rs)\n", 50_000),
            super::super::review::SUBAGENTS_INSTRUCTION
        );
        let fitted = review_prompt(
            &task,
            &run_on(Provider::Claude, WorkerMode::Headless),
            "/runs/run/review.md",
            Some(&subagents),
        );
        within(&fitted, RUN_REVIEW_PROMPT_LIMIT);
        let bytes = &fitted.bytes;
        for section in ["title", "acceptance", "subagents"] {
            assert_eq!(bytes.omitted.get(section), Some(&1), "{section}: {bytes:?}");
        }
        assert!(
            bytes
                .over_limit
                .as_deref()
                .unwrap()
                .starts_with("acceptance: ")
        );
        assert!(fitted.text.contains(
            "read it whole in the review material at /runs/run/review.md, its section Acceptance"
        ));
        assert!(fitted.text.contains("in /runs/run/review-subagents-1.json"));
        assert!(
            fitted.text.contains(
                "every agent and the paths that selected it are in the file the list names"
            )
        );
        // How to run the agents and report them is never cut.
        assert!(
            fitted
                .text
                .ends_with(super::super::review::SUBAGENTS_INSTRUCTION)
        );
        // A small task is not cut.
        let small = review_prompt(
            &task_with_evidence(7, Vec::new()),
            &run_on(Provider::Claude, WorkerMode::Headless),
            "r",
            None,
        );
        assert!(
            small.bytes.omitted.is_empty() && small.bytes.over_limit.is_none(),
            "{:?}",
            small.bytes
        );
        within(&small, RUN_REVIEW_PROMPT_LIMIT);
    }

    /// The recovery job of a headless run whose last turns, alert facts,
    /// history, processes, worktree and ended material are huge stays
    /// within [`RECOVERY_PROMPT_LIMIT`] (the largest in production had
    /// 46,559 bytes of turns): what is cut names the file in the run
    /// directory it is in, or says the job cannot read it.
    #[test]
    fn a_recovery_prompt_of_huge_turns_and_facts_stays_within_its_limits() {
        let task = big_task(7, 50_000);
        let facts = json!({"reason": "idle_process", "processes": big("pid 1 idle", 100_000)});
        let screen = big("turn 3: ended", 200_000);
        let history: Vec<Value> = (1..=100).map(|id| json!({"id": id, "kind": "recovery_finished", "diagnosis": big("diagnosis", 1_000)})).collect();
        let processes: Vec<ProcessInfo> = (1..=100)
            .map(|pid| ProcessInfo {
                pid,
                ppid: 1,
                elapsed_secs: 10,
                cpu_ms: None,
                cwd: Some("/runs/run/worktree".into()),
                command: big("cargo test", 300),
            })
            .collect();
        let status = big(" M src/a.rs", 50_000);
        for (run, ended) in [
            (run_on(Provider::Claude, WorkerMode::Headless), None),
            (
                run(7, RunStatus::Failed, None),
                Some(big("Verification log", 100_000)),
            ),
        ] {
            let fitted = recovery_prompt(
                &task,
                &run,
                1,
                &RecoveryMaterial {
                    alert: RecoveryAlert::IdleProcess,
                    ended: ended.clone(),
                    facts: &facts,
                    workspace: "ws",
                    screen: &screen,
                    processes: Ok(processes.clone()),
                    git_status: &status,
                    head: SHA,
                    receipt_commit: None,
                    history: &history,
                    allowed: &["wait", "stop_processes"],
                },
            )
            .unwrap();
            within(&fitted, RECOVERY_PROMPT_LIMIT);
            let (text, bytes) = (&fitted.text, &fitted.bytes);
            for section in [
                "task",
                "facts",
                "screen",
                "processes",
                "git_status",
                "history",
            ] {
                assert!(
                    bytes.omitted.get(section).is_some_and(|n| *n > 0),
                    "{section}: {bytes:?}"
                );
            }
            assert!(
                bytes.over_limit.as_deref().unwrap().contains("task: "),
                "{bytes:?}"
            );
            assert!(
                text.contains(
                    "the task as the run claimed it is in prompt.txt of the run directory"
                )
            );
            assert!(text.contains(NOT_READABLE));
            assert!(text.contains("of them (the oldest) left out by this section's limit"));
            assert!(text.contains("read the files of the worktree at /runs/run/worktree"));
            assert!(text.contains("Answer with one JSON object and nothing else"));
            if ended.is_some() {
                assert_eq!(bytes.omitted.get("ended"), Some(&1));
                assert!(text.contains("the run directory has the receipt, the verification logs"));
            } else {
                assert!(text.contains(
                    "each turn's whole output is in turns/turn-NNNNNN.jsonl of the run directory"
                ));
            }
        }
    }

    /// The planner opened for a revise of a proposal with many long tasks
    /// and reasons stays within [`RUNTIME_PLANNER_PROMPT_LIMIT`].
    #[test]
    fn a_runtime_planner_prompt_of_many_reasons_stays_within_its_limits() {
        let tasks: Vec<Task> = (1..=500)
            .map(|id| task(id, &big("title", 2_000), TaskStatus::Submitted))
            .collect();
        let reasons: Vec<String> = (1..=100)
            .map(|n| big(&format!("reason {n}"), 5_000))
            .collect();
        let fitted = runtime_planner_prompt(
            Path::new("/q/queue.db"),
            ProposalId::new(3),
            &tasks,
            &reasons,
        )
        .unwrap();
        within(&fitted, RUNTIME_PLANNER_PROMPT_LIMIT);
        let (text, bytes) = (&fitted.text, &fitted.bytes);
        assert!(
            bytes.omitted["tasks"] > 0 && bytes.omitted["reasons"] > 0,
            "{bytes:?}"
        );
        assert!(text.contains("tasks (the oldest) left out by this section's limit"));
        assert!(text.contains("To read them: `dagq proposal show 3`."));
        assert!(text.contains("more reasons left out by this section's limit. To read them: `dagq events --full --task ID --kind plan_review_finished` for a task of proposal 3."));
        // The first reasons are kept, cut to their own limit.
        assert!(text.contains("- reason 1 reason 1"));
        assert!(text.contains("dagq submit --proposal 3"));
    }

    /// The planner opened for a large bundle of follow_up drafts with a
    /// huge source task, receipt, goals and an answer stays within
    /// [`DRAFT_PLANNER_PROMPT_LIMIT`].
    #[test]
    fn a_draft_planner_prompt_of_a_large_bundle_stays_within_its_limits() {
        let members: Vec<(DraftTarget, usize)> = (10..60)
            .map(|id| {
                (
                    DraftTarget {
                        task: big_task(id, 20_000),
                        origin: DraftOrigin::FollowUp,
                        material: json!({"source_run_id": RUN, "source_task_id": 3, "index": id}),
                        planners: 0,
                    },
                    1,
                )
            })
            .collect();
        let key = BundleKey::of(
            DraftOrigin::FollowUp,
            &members[0].0.material,
            members[0].0.task.id(),
        );
        let source = big_task(3, 50_000);
        let receipt = json!({"summary": big("summary", 50_000), "follow_ups": (0..100).map(|n| json!({"title": format!("f{n}"), "description": big("d", 1_000)})).collect::<Vec<_>>()});
        let goals: Vec<(Goal, bool, Vec<GoalTask>)> = (1..=10)
            .map(|id| (goal_of(id, 20_000), false, goal_tasks(500)))
            .collect();
        let answer = asked(9, 20_000);
        let fitted = draft_planner_prompt(&DraftPlannerMaterial {
            db: Path::new("/q/queue.db"),
            key: &key,
            members: &members,
            source: Some(&source),
            receipt: Some(&receipt),
            goals: &goals,
            answer: Some(&answer),
        })
        .unwrap();
        within(&fitted, DRAFT_PLANNER_PROMPT_LIMIT);
        let (text, bytes) = (&fitted.text, &fitted.bytes);
        for section in ["drafts", "origin", "goals", "answer"] {
            assert!(
                bytes.omitted.get(section).is_some_and(|n| *n > 0),
                "{section}: {bytes:?}"
            );
        }
        assert!(
            bytes.over_limit.as_deref().unwrap().starts_with("drafts: "),
            "{bytes:?}"
        );
        assert!(text.contains("## Drafts left out"));
        assert!(text.contains("`dagq show ID --full` for each"));
        assert!(text.contains(&format!(
            "read it whole with `dagq events --full --run {RUN} --kind integration_receipt`"
        )));
        assert!(text.contains("## Goals left out"));
        assert!(text.contains("read it whole with `dagq goal show 1 --full`"));
        assert!(text.contains("read ask 9 whole with `dagq asks --all`"));
        assert!(text.contains("Apply this answer as step 3 says."));
        assert!(text.contains("## What to do"));
    }

    /// The planner opened for a finding whose evidence is huge (finding 44
    /// of production had 270,669 bytes of it) stays within
    /// [`FINDING_PLANNER_PROMPT_LIMIT`], keeping the newest evidence.
    #[test]
    fn a_finding_planner_prompt_of_huge_evidence_stays_within_its_limits() {
        let view = FindingView {
            finding: crate::domain::Finding {
                id: crate::domain::FindingId::new(4),
                kind: "conflict".into(),
                target: "queue".into(),
                task_id: None,
                run_id: None,
                goal_id: None,
                subject: big("subject", 5_000),
                summary: big("summary", 5_000),
                detail: big("detail", 50_000),
                impact: crate::domain::Impact::Normal,
                first_seen_at: 0,
                last_seen_at: 0,
                occurrences: 500,
                evidence: Vec::new(),
                status: crate::domain::FindingStatus::Open,
                status_reason: None,
                proposal_id: None,
                propose_reason: Some(big("why", 5_000)),
                propose_requested_at: None,
                recorded_by: "observer".into(),
                updated_at: 0,
            },
            proposal_status: None,
            open_asks: Vec::new(),
            evidence_events: Some((1..=500).map(|id| event_of(id, 5_000)).collect()),
        };
        let asks: Vec<Ask> = (1..=100).map(|id| asked(id, 2_000)).collect();
        let goal = goal_of(5, 20_000);
        let siblings = goal_tasks(500);
        let answer = asked(200, 20_000);
        let fitted = finding_planner_prompt(&FindingPlannerMaterial {
            db: Path::new("/q/queue.db"),
            finding: &view,
            attempt: 1,
            asks: &asks,
            goal: Some(&goal),
            goal_closed: false,
            siblings: &siblings,
            answer: Some(&answer),
        })
        .unwrap();
        within(&fitted, FINDING_PLANNER_PROMPT_LIMIT);
        let (text, bytes) = (&fitted.text, &fitted.bytes);
        for section in ["finding", "evidence", "asks", "goals", "answer"] {
            assert!(
                bytes.omitted.get(section).is_some_and(|n| *n > 0),
                "{section}: {bytes:?}"
            );
        }
        assert!(
            bytes
                .over_limit
                .as_deref()
                .unwrap()
                .starts_with("finding: "),
            "{bytes:?}"
        );
        assert!(text.contains("evidence events (the oldest) left out by this section's limit"));
        assert!(text.contains("`dagq findings 4 --full`, or one event with `dagq events --full --all --after <its ID - 1> --limit 1`"));
        // The newest evidence is kept, the oldest left out.
        assert!(text.contains(r#""id":500,"#));
        assert!(!text.contains(r#""id":1,"kind":"turn_finished""#));
        assert!(text.contains("asks (the oldest) left out by this section's limit"));
        assert!(text.contains("To read them: `dagq asks --all`."));
        assert!(text.contains("To read them: `dagq goal show 5 --full`."));
        assert!(text.contains("## What to do"));
    }

    /// The planner opened for a planning request that refers to many huge
    /// tasks, runs, events, asks and findings, with a huge note, goals and
    /// asks, stays within [`REQUEST_PLANNER_PROMPT_LIMIT`]. Production had
    /// no request planner yet: this input measures what the sections'
    /// limits add up to.
    #[test]
    fn a_request_planner_prompt_of_huge_references_stays_within_its_limits() {
        let request = crate::domain::plan_request::PlanRequest {
            id: crate::domain::RequestId::new(6),
            text: "plan it".into(),
            note: Some(big("note", 50_000)),
            refs: Vec::new(),
            requested_by: "inbox".into(),
            requested_by_id: "inbox".into(),
            status: crate::domain::plan_request::RequestStatus::Open,
            status_reason: None,
            proposals: Vec::new(),
            planners: 0,
            created_at: 0,
            updated_at: 0,
        };
        let receipt = json!({"summary": big("summary", 20_000), "follow_ups": []});
        let mut refs = Vec::new();
        for n in 1..=10 {
            refs.push(RequestRefMaterial::Task {
                task: Box::new(big_task(n, 20_000)),
                receipt: Some(receipt.clone()),
            });
            refs.push(RequestRefMaterial::Ask(asked(n, 20_000)));
            refs.push(RequestRefMaterial::Event(event_of(100 + n, 20_000)));
            refs.push(RequestRefMaterial::Run {
                run: RunId::new(RUN).unwrap(),
                task: Some(TaskId::new(n)),
                receipt: Some(receipt.clone()),
            });
        }
        let goals: Vec<(Goal, bool, Vec<GoalTask>)> = (1..=10)
            .map(|id| (goal_of(id, 20_000), false, goal_tasks(500)))
            .collect();
        let asks: Vec<Ask> = (1..=100).map(|id| asked(id, 2_000)).collect();
        let answer = asked(300, 20_000);
        let fitted = request_planner_prompt(&RequestPlannerMaterial {
            db: Path::new("/q/queue.db"),
            request: &request,
            handed: "The person's words are in /q/planners/1/request.md.",
            attempt: 1,
            refs: &refs,
            goals: &goals,
            asks: &asks,
            answer: Some(&answer),
        })
        .unwrap();
        within(&fitted, REQUEST_PLANNER_PROMPT_LIMIT);
        let (text, bytes) = (&fitted.text, &fitted.bytes);
        for section in ["note", "refs", "goals", "asks", "answer"] {
            assert!(
                bytes.omitted.get(section).is_some_and(|n| *n > 0),
                "{section}: {bytes:?}"
            );
        }
        assert!(text.contains("read it whole with `dagq requests 6`"));
        assert!(text.contains("references left out by this section's limit"));
        assert!(text.contains("read it whole with `dagq show 1 --full` and `dagq events --full --task 1 --kind integration_receipt`"));
        assert!(text.contains("`dagq events --full --all --after 100 --limit 1`"));
        assert!(text.contains(&format!(
            "`dagq events --full --run {RUN} --kind integration_receipt`"
        )));
        assert!(text.contains("## Goals left out"));
        assert!(text.contains("Apply this answer as step 3 says."));
        // What the largest input takes, for the limit's reason in
        // docs/design/supervisor-lifecycle/prompt.md.
        assert!(bytes.total > 60_000, "{bytes:?}");
    }
}
