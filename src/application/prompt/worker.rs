//! The worker's prompt (`prompt.txt`) and what the supervisor hands a
//! worker's session: the handoff, an answer and a recovery's instruction
//! (execution and landing).

use super::*;
use crate::domain::local_checks::LocalChecks;

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

/// One task landed on the landing branch since a run's base, as the
/// resolution request lists it: the ID and the title, no receipt summary
/// (the worker reads what landed with git).
#[derive(Debug, Clone)]
pub struct LandedTask {
    pub task_id: TaskId,
    pub title: String,
}

/// Lines of landed tasks a resolution request lists, newest first
/// (ADR-t1892-1): a resume is sent again after each landing, so the list is
/// capped and the rest is counted and left to `git log`.
pub const LANDED_TASK_LINES: usize = 20;

/// Bytes of a landed task's title its line keeps, the cut's mark included:
/// a title has no length limit in the domain, so one title could make the
/// section, and the whole request, as long as it is. The 2,000 commit
/// subjects (each a landed task's title) before 2026-10-08 took 150 bytes
/// at the median, 362 at p99 and 443 at most, so a title is cut only when
/// it is longer than any seen. A cut title keeps its start and ends with
/// [`LANDED_TITLE_MARK`]'s count of the bytes left out; the commit's
/// subject in `git log <base>..<main>` has it whole.
pub const LANDED_TITLE_BYTES: usize = 500;

/// The start of the mark a cut title ends with: `… [+<N> bytes]`, N the bytes
/// left out.
pub(super) const LANDED_TITLE_MARK: &str = "… [+";

/// Bytes the section on landed tasks takes at most, its newlines included,
/// with `base` and `main` commit IDs of 40 bytes and a branch name of at
/// most 256 bytes: [`LANDED_TASK_LINES`] lines of `- task <ID>: ` (at most
/// 29 bytes) and a title of at most [`LANDED_TITLE_BYTES`], about 10,600,
/// and the header, the line on cut titles and the line counting the rest,
/// under 1,000 together.
pub const LANDED_SECTION_BYTES: usize = 12_000;

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
                    if let Some(cut) = crate::application::health::truncate(
                        &summary.summary,
                        GOAL_TASK_SUMMARY_CHARS,
                    ) {
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
    /// The role (`user` or `inbox`) and reason of a person's retry by hand
    /// that carried it over (`ready --inherit`, ADR-t1962-1); `None` for the
    /// runtime's and the recovery job's.
    pub by_hand: Option<(String, String)>,
    /// The part of a replaced run's saved work this child of the
    /// replacement takes (ADR-t1521-2 decision 5); `None` for a retry's.
    pub replan: Option<ReplanCarried>,
}

/// What a child of an applied replacement takes of the saved work of the
/// run its replanning operation replaced, as the claim handed it over from
/// the operation's snapshot: only what the plan named for this child.
#[derive(Debug, Clone, Serialize)]
pub struct ReplanCarried {
    pub operation: crate::domain::ReplanId,
    /// The task the operation replaced.
    pub task: crate::domain::TaskId,
    pub carry: crate::domain::replan_plan::CarrySource,
    /// The ref that pins the replaced run's head.
    pub pinned_ref: String,
    /// For the uncommitted changes, the files copied into this run's
    /// directory, each with what it holds.
    pub files: Vec<(String, String)>,
}

impl Inheritance {
    /// The ref a container run's clone fetches to carry this work over:
    /// for a child of an applied replacement, the ref that pins the
    /// replaced run's saved head (its own run ref is gone with the run);
    /// otherwise the inherited run's.
    pub fn clone_ref(&self) -> String {
        match &self.replan {
            Some(replan) => replan.pinned_ref.clone(),
            None => format!("refs/dagq/runs/{}", self.run_id),
        }
    }

    /// What the next run of `previous`'s task inherits, when `previous` was
    /// ended by the retry that carries its branch over
    /// ([`crate::domain::resume::retried_with_inheritance`]): the head that
    /// retry recorded, and the summary of its receipt.
    pub fn of(files: &dyn RunFiles, previous: &TaskRun, events: &[RunEvent]) -> Option<Self> {
        if !resume::retried_with_inheritance(events) {
            return None;
        }
        let retry = events.iter().rev().find(|e| resume::is_inherit_retry(e))?;
        let inherit = &retry.payload["inherit"];
        let by_hand = resume::is_inherit_retry_by_hand(retry).then(|| {
            (
                retry.payload["by"].as_str().unwrap_or_default().to_owned(),
                retry.payload["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        });
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
            by_hand,
            replan: None,
        })
    }

    /// The prompt's section on it: start from its commit, bring it onto the
    /// current main, resolve the conflicts, verify and write the receipt.
    pub(super) fn section(&self) -> String {
        if let Some(replan) = &self.replan {
            return self.replan_section(replan);
        }
        let why = match &self.by_hand {
            Some((by, reason)) => format!(
                "{by} carried its work over by hand after it ended ({reason}), so this run starts from its work instead of from scratch."
            ),
            None => "its review passed, but its landing kept conflicting with the landing branch until its resumes were used up, so this run starts from its work instead of from scratch.".to_owned(),
        };
        format!(
            "Carried over from run {run}: {why} \
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

impl Inheritance {
    /// The prompt's section on a replaced run's saved work this child
    /// takes: bring only that onto the base, keep what this task's
    /// acceptance needs, and verify it as this task's own work, since it
    /// passed no review as such.
    fn replan_section(&self, replan: &ReplanCarried) -> String {
        use crate::domain::replan_plan::CarrySource;
        let what = match replan.carry {
            CarrySource::SnapshotDiff => format!(
                "This task takes the uncommitted changes that run had on top of its commit {head}, copied into your run directory: {files}. \
                 Apply them in your worktree (`git apply --index` for the staged diff, `git apply` for the unstaged one, and copy the untracked files to the paths named, keeping an executable one's mode with `cp -p`)",
                head = self.head,
                files = replan
                    .files
                    .iter()
                    .map(|(path, what)| format!("{path} ({what})"))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            _ => format!(
                "This task takes the commits that run made: its work is commit {head} (kept as {pinned}{branch}), its own commits {base}..{head}. \
                 Bring them onto your base, the current landing branch (for example `git cherry-pick {base}..{head}` in your worktree)",
                head = self.head,
                base = self.base,
                pinned = replan.pinned_ref,
                branch = self
                    .branch
                    .as_deref()
                    .map(|branch| format!(", branch {branch}"))
                    .unwrap_or_default(),
            ),
        };
        format!(
            "Carried over from run {run} of task {task}, which replan operation {op} replaced with this task and others: {what}, resolve the conflicts, keep only what this task's acceptance needs, rerun your checks in the worktree as above, and write the receipt for your own head. \
             That work passed no review as this task's work, so verify it as your own.\n",
            run = self.run_id,
            task = replan.task,
            op = replan.operation,
        )
    }
}

/// The other tasks a worker is told are executing alongside it: of the
/// `in_progress` tasks (ID order), those sharing the task's goal, or all of
/// them when the task has no goal; the task itself is never listed.
///
/// The supervisor claims one task at a time, so of two tasks claimed in
/// the same pass only the later one lists the earlier. A task whose run
/// waits to land or for a session is still `in_progress` and is listed.
pub fn siblings_in_progress(task: &Task, in_progress: Vec<Task>) -> Vec<Task> {
    in_progress
        .into_iter()
        .filter(|other| other.id() != task.id())
        .filter(|other| task.goal_id().is_none() || other.goal_id() == task.goal_id())
        .collect()
}

/// What a headless worker is told of its session (ADR-t813-1): each turn is
/// one non-interactive call that ends when the agent stops, and whatever
/// runs in the background then is stopped with it. It names no `/exit` and
/// no terminal: the process ends with the turn, and nobody types into it.
pub const HEADLESS_WORKER: &str = "This session is headless: each of your turns is one non-interactive call, and the turn ends when you stop. Do the whole task in this turn and end it by writing the receipt, or by an ask when you need a decision. Nobody types into this session: the answer to your ask, a review's request to revise, or a request to go on arrives as the prompt of your next turn, in the same session. Do not rely on background work: what still runs when the turn ends is stopped, so run builds, tests and waits in the foreground and wait for them to finish before you go on.\n";

/// The line in the worker prompt and every request to its session that asks
/// it to stop its own processes before the turn ends: nothing waits for an
/// `/exit`, but a process the agent detached outlives its turn (the spike
/// measured Claude's `nohup ... &`). Its last clause keeps a session from
/// signalling by name or pattern: every run session's command line holds
/// its prompt, so `pkill -f llvm-cov` from one worker ended the others'
/// sessions and `integrate`'s checks (task 359).
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
pub(super) const HEADLESS_DONE: &str = concat!(
    "Do not merge or push. Follow the repository's instructions for a worker (AGENTS.md or CLAUDE.md) as before. Do all of this in this turn. ",
    ask_rules_first!(),
    " If you need a decision, run `dagq ask --run <run> --kind worker_question --because <scope|discard> --topic <code> --question '...'` and end the turn: the answer comes as the prompt of your next turn. When done, report briefly and end the turn."
);

/// What a headless request opens with: the prompt of a resume reads as the
/// next turn of the same session.
pub(super) const HEADLESS_NEXT_TURN: &str = "dagq: this is the next turn of your session; your previous turn has ended, and anything it left running in the background was stopped.";

/// How a run's worker takes what the runtime tells it (ADR-t813-1): a
/// headless session whose every request is the prompt of its next turn, on
/// its provider. The interactive worker was retired (ADR-t1433-2): a run
/// recorded as interactive before is claimed or resumed headless, and its
/// texts are those of a headless Claude run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Route(pub(super) Provider);

impl Route {
    /// The route of `run`'s worker: the provider it runs on now (a fallback
    /// may have changed it).
    pub(crate) fn of(run: &TaskRun) -> Self {
        Self(run.actual_provider())
    }

    /// The line that asks the session to stop its own processes.
    pub(super) fn stop(self) -> &'static str {
        HEADLESS_STOP
    }

    /// The last step of a request to `run`'s session.
    pub(super) fn done(self, run: &TaskRun) -> String {
        HEADLESS_DONE.replace("<run>", run.id().as_str())
    }

    /// `first`, the opening line of a request, after the headless opening.
    pub(super) fn opening(self, first: String) -> Vec<String> {
        vec![HEADLESS_NEXT_TURN.to_owned(), first]
    }
}

/// What a headless worker on `provider` is told of its provider (the
/// spike's measures): Claude Code stops a turn's background shells when
/// the turn ends, and Codex stops a command still running when the model
/// answers. A Codex worker is also told to keep its temporary files under
/// the `TMPDIR` the runtime gives its turns (task 1290): Claude Code's own
/// scratchpad is cleaned after the run (task 1100), what a Codex worker
/// made in `/tmp` was not.
pub(super) fn headless_provider_line(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => {
            "Claude Code stops the background shells of a turn when the turn ends: do not start a build, a test or a wait with run_in_background and end the turn to wait for it.\n"
        }
        Provider::Codex => {
            "Wait for each command to finish before you answer: a command still running when you answer is stopped with the turn.\n\
             Put temporary files, throwaway repositories and any build output you keep outside the worktree under $TMPDIR (a directory the runtime made for this run and removes after it), never directly in /tmp or /private/tmp; where to build otherwise is for the repository's instructions (AGENTS.md or CLAUDE.md) to say.\n"
        }
    }
}

/// The worker's step before the receipt (ADR-t1420-1): each acceptance
/// criterion is mapped to what meets it, a criterion nothing meets is
/// fixed on the spot, and one the task cannot meet is an ask or a failed
/// receipt rather than a follow_up beside a succeeded receipt. It asks for
/// no new command: the mapping reads the diff and the receipt.
pub const ACCEPTANCE_MAP: &str = "Before the receipt, map each acceptance criterion to what meets it (a changed file, a test's name, the receipt's evidence, a document's section or a command that measures it) and fix on the spot any criterion nothing meets yet. Do not write succeeded with a criterion you cannot meet moved into follow_ups: when meeting it needs a person's decision (the acceptance or the scope changes), ask a worker_question with --because scope; when it needs work outside the task, write a failed receipt saying why. In summary, give each criterion's mapping in a short phrase.\n";

/// The receipt of a worker whose base already meets every criterion
/// (ADR-t1818-1 decision 1): `already_satisfied`, no commit, and the checks
/// run on the base as its grounds. Those are only the checks a worker may
/// run; the runtime runs the task's whole verification on main, so the
/// worker runs no gate the repository leaves to the landing.
pub const ALREADY_SATISFIED: &str = "When the base already meets every acceptance criterion and no change is needed, commit nothing and write the receipt with result already_satisfied, the base commit as commit, and base_checks: an array of {\"command\",\"result\"}, each a check you ran on the base that shows a criterion holds, with the mapping in summary. Run for it only the checks the repository's instructions let a worker run: the runtime runs the task's verification commands on main itself, as the landing runs them, before the task can close.\n";

/// The worker's check of the documents (ADR-t1428-1), right after
/// [`ACCEPTANCE_MAP`] and part of the same step: the documents that
/// describe the changed behavior, named by the task or not, are read
/// against the diff, and summary says what was updated or why nothing was.
/// The candidates include what a search of the repository for each changed
/// name finds, and summary gives the names searched, so the review can
/// search again. A document is written only when a flow, boundary,
/// invariant or promise the code does not show changed, or it disagrees
/// with the code; a name missing from it is not drift, and neither a list
/// of identifiers nor history goes in (ADR-t1942-2). It names no tool, no
/// repository path and none of a repository's own document rules, and asks
/// for no command and no document change only to show the check.
pub const DOCS_CHECK: &str = "Then check against your diff the documents on what you changed: named by the task, found as you work, or found by searching the repository for each changed name (command, flag, setting, role, path). Fix stale ones in the task's paths, file the rest as docs_drift follow_ups with path and section (one the acceptance names is a criterion above); in summary give the names searched and each path and section updated or why none was. Write to a document only when a flow, boundary, invariant or promise the code does not show changed, or the document disagrees with the code or an accepted decision; a changed name missing from it is not drift. Add no list of fields, flags, defaults or names and no history (task numbers, what was before); put what a name means in a doc comment by its definition. Touch no document only to show it.\n";

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
pub(super) fn review_line(route: Route) -> &'static str {
    match route.0 {
        Provider::Codex => {
            "Perform applicable unit tests. You have no subagent to review your change: before the receipt, read your own diff (git diff <base commit>..HEAD) as you map the acceptance criteria to it (the step below) and fix what you find, then write subagent_review as not_applicable with the reason `codex worker: no subagent review; self-reviewed the diff, the supervisor's review job reviews the commit` and what the self-review found. Record evidence or an explicit reason when not applicable.\n"
        }
        Provider::Claude => {
            "Perform applicable unit tests and subagent review. Record evidence or an explicit reason when not applicable.\n"
        }
    }
}

/// What follows a text a headless session reads as its next turn's prompt
/// (an answer, a recovery job's instruction): go on in this turn.
pub(super) const HEADLESS_GO_ON: &str = "(dagq: this is the prompt of the next turn of your session; your previous turn has ended. Go on with the task from it in this turn, and end the turn with the receipt, or with an ask when you need a decision.)";

/// `text` for a run's session, followed by [`HEADLESS_GO_ON`]. The text
/// stays first, so its own first line still says what it is.
pub(super) fn to_session(_run: &TaskRun, text: String) -> String {
    format!("{text}\n\n{HEADLESS_GO_ON}")
}

/// The text that tells a session held by a login or a usage limit to go
/// on, once a person answered the hold's ask `done`. It has no text of
/// variable length: its bytes are counted against [`NEXT_TURN_LIMIT`] like
/// the other next turns'.
pub(crate) fn continue_text(run: &TaskRun) -> FittedPrompt {
    Fit::new(NEXT_TURN_LIMIT).finish(to_session(
        run,
        crate::domain::queue_hold::CONTINUE_TEXT.to_owned(),
    ))
}

/// A request written to a file of the run directory before it was sent,
/// sent again from that file without what it took (by the next process
/// after a handoff whose `handoff.json` did not carry it, by an adopter):
/// measured anew as the one section `request` against `limit`, the whole
/// limit of its kind. One past it (written before the limits) keeps its
/// start, with the bytes left out and `file`, which has it whole.
pub(crate) fn restored_request(text: &str, limit: usize, file: &Path) -> FittedPrompt {
    Fit::new(limit).restored(
        "request",
        text,
        &format!("the whole request is in {}", file.display()),
    )
}

/// The bytes a next-turn message other than the resolution request takes
/// at most, the language's instruction included (ADR-t2072-1): the
/// revise mismatch, the stale receipt nudge, the stall nudge, an answer, a
/// recovery job's instruction and the notice of a closed question. Each is
/// its fixed steps (under 2,000 bytes) and at most one text of
/// [`NEXT_TURN_TEXT_BYTES`], with its note of what was left out and the
/// language's room. The 11 answers kept as next turns on the host on
/// 2026-10-08 took 481 bytes at most.
pub const NEXT_TURN_LIMIT: usize = 16_000;

/// The bytes of a person's or a job's text a next-turn message carries:
/// an ask's answer (also one recorded when the ask was closed) and a
/// recovery job's instruction. A person's answer sent back as a resume's
/// reason took 3,892 bytes at most on the host on 2026-10-08, so a text is
/// cut only when it is about twice as long as any seen. The worker cannot
/// read the queue, so what is cut is said to be in no file it can read.
pub const NEXT_TURN_TEXT_BYTES: usize = 8_000;

/// The bytes of why the supervisor did not accept a rewritten receipt
/// (`revise_mismatch_request`): the supervisor writes it, one sentence of
/// a commit ID and a state.
pub const NEXT_TURN_WHY_BYTES: usize = 2_000;

/// The bytes of a short value a next-turn message names: the commit a
/// receipt names (a commit ID takes 40 or 64 bytes, but the worker writes
/// it) and who closed an ask (a role and an actor ID).
pub const NEXT_TURN_NAME_BYTES: usize = 300;

/// The bytes a provider's retry or switch request (`retry_text` and
/// `switch_text` of the supervisor's provider module) takes at most
/// (ADR-t2072-1): its fixed text (under 1,000 bytes with a commit ID of
/// 64), the provider's message ([`PROVIDER_MESSAGE_BYTES`]), the name
/// ([`NEXT_TURN_NAME_BYTES`]) and text ([`UNDELIVERED_REQUEST_BYTES`]) of
/// the request the failed turn did not get to and the notes of what was
/// cut, with the language's room. The switch requests kept on the host on
/// 2026-10-08 took 21,541 bytes at most, nearly all of it that request.
/// It grows with [`RESUME_REQUEST_LIMIT`]'s section on the known failures
/// ([`KNOWN_FAILURES_LIMIT`]), which the request it carries may hold.
pub const PROVIDER_TURN_LIMIT: usize = 42_000 + KNOWN_FAILURES_LIMIT;

/// The bytes of the request a failed turn did not get to, carried by the
/// retry or switch request whatever it was (a first request, a retry or
/// switch request that failed in its turn too, one written before the
/// limits): the largest whole limit of a next turn
/// ([`RESUME_REQUEST_LIMIT`]), so a request held to its limit is carried
/// whole and one wrapped again and again stops growing here. What is cut
/// is in the request's file in the run directory's `turns/`.
pub const UNDELIVERED_REQUEST_BYTES: usize = RESUME_REQUEST_LIMIT;

/// The bytes of the message a provider's failed turn ended with, which a
/// switch request quotes: the provider writes it, usually one line. The
/// worker cannot read the queue, so what is cut is in no file it can read.
pub const PROVIDER_MESSAGE_BYTES: usize = 2_000;

/// What the handoff prompt of a new session of a run's worker carries
/// (ADR-t2080-1 decision 2 as ADR-t2852-1 amends it): why the session is
/// new, the work so far as git shows it, the records of its local checks
/// on the head's tree with how many others there are, the tests that fail already
/// on the landing branch's CI in the files the run touches, how long the
/// request may take, the summary of the receipt the worker wrote last,
/// and the request the earlier session would have been sent next (a
/// revise's, a resume's or a triage's instruction), which carries a
/// review's findings once. No transcript of the conversation: the
/// worktree and its commits hold the work. The task's prompt is the new
/// session's first part, which the session wrapper puts before it
/// ([`crate::domain::turn::new_session_prompt`]).
#[derive(Debug, Clone, Copy)]
pub struct HandoffMaterial<'a> {
    /// The previous turn's `peak_context` and `[fresh_session]
    /// peak_context_above` it was above, in tokens.
    pub peak_context: u64,
    pub threshold: u64,
    /// The resume timeout in seconds: how long after the request the
    /// supervisor waits for the receipt to be rewritten.
    pub resume_timeout: u64,
    /// `git log --oneline <base>..<branch>`: the commits so far, newest
    /// first.
    pub commits: &'a str,
    /// `git status --porcelain` of the worktree: the uncommitted changes.
    pub changes: &'a str,
    /// The tree of the branch's head; `None` when it could not be read.
    pub head_tree: Option<&'a str>,
    /// The newest records of the run's local checks judged against
    /// `head_tree` as a review judges them against its commit's
    /// (ADR-t2589-1 decision 4): those on it in the order of their files'
    /// names, and how many were of another tree or broken; and how many
    /// older ones were left out unread.
    pub checks: &'a LocalChecks,
    pub older_checks: usize,
    /// The tests that fail already on the landing branch's CI for the
    /// run's task, and the names the paths the run changes or declares
    /// give a test ([`touched_names`]).
    pub known_failures: Option<&'a KnownFailures>,
    pub touched: &'a BTreeSet<String>,
    /// The summary of the receipt the worker wrote last; `None` without
    /// one.
    pub receipt_summary: Option<&'a str>,
    /// The request the earlier session would have been sent, built and
    /// held to its own limit, and the file that holds it whole if any.
    pub request: &'a str,
    pub request_file: Option<&'a Path>,
}

/// The bytes the handoff prompt of a new session (`handoff_text`) takes
/// at most, the language's instruction included (ADR-t2080-1 decision 2,
/// held as ADR-t2072-1 holds the next turns): its fixed text (under 3,500
/// bytes with commit IDs of 64 and a run directory's longest path), the commits ([`HANDOFF_COMMITS_BYTES`]),
/// the changes ([`HANDOFF_CHANGES_BYTES`]), the local checks' records
/// ([`HANDOFF_CHECKS_BYTES`]), the tests that fail already on the landing
/// branch's CI ([`KNOWN_FAILURES_LIMIT`]), the receipt's summary
/// ([`HANDOFF_RECEIPT_BYTES`]), the request ([`HANDOFF_REQUEST_BYTES`]) and
/// the notes of what was cut, with the language's room: the sections'
/// limits take 77,500 bytes together, and the request's grows with the
/// known failures' section of a resolution request
/// ([`KNOWN_FAILURES_LIMIT`]). The new session's first turn is the task's
/// prompt ([`WORKER_PROMPT_LIMIT`]) and this.
pub const HANDOFF_LIMIT: usize = 82_000 + KNOWN_FAILURES_LIMIT;

/// The bytes of the commits' lines, the newest kept: about 100 lines of
/// `git log --oneline`. The rest is in the worktree's `git log`.
pub const HANDOFF_COMMITS_BYTES: usize = 8_000;

/// The bytes of the uncommitted changes' lines: about 100 paths of `git
/// status --porcelain`. The rest is in the worktree's `git status`.
pub const HANDOFF_CHANGES_BYTES: usize = 8_000;

/// The bytes of the local checks' lines, the newest kept: a line takes
/// about 150 bytes and its command at most [`HANDOFF_CHECK_COMMAND_BYTES`],
/// so about 50 records of usual commands fit. The rest is in the records'
/// directory.
pub const HANDOFF_CHECKS_BYTES: usize = 12_000;

/// The bytes of one record's command: cut past these, keeping its start.
pub const HANDOFF_CHECK_COMMAND_BYTES: usize = 600;

/// The bytes of the receipt's summary: a predecessor's summary took
/// 2,121 bytes at the median and 6,580 at p90 on 2026-10-08, so it is cut
/// only past p90. The rest is in the receipt the run directory keeps.
pub const HANDOFF_RECEIPT_BYTES: usize = 6_000;

/// The bytes of the request the earlier session would have been sent: the
/// largest whole limit of a next turn ([`RESUME_REQUEST_LIMIT`]), so a
/// request held to its limit is carried whole.
pub const HANDOFF_REQUEST_BYTES: usize = RESUME_REQUEST_LIMIT;

/// How every handoff prompt ([`handoff_text`]) starts.
const HANDOFF_OPENING: &str = "dagq: this run's worker goes on in a new session";

/// How the handoff names the records' directory: by the variable each
/// turn is given, whose path a container sees too.
const CHECKS_DIR: &str = "the directory `$DAGQ_LOCAL_CHECKS_DIR` names";

/// The whole limit a request kept in a run file is held to again when a
/// supervisor that did not write it sends it (an adopted revise, an
/// adopted or handed-off resume): [`HANDOFF_LIMIT`] for a handoff prompt
/// written in place of the request of its kind, `limit` for that request.
pub(crate) fn kept_request_limit(text: &str, limit: usize) -> usize {
    if text.starts_with(HANDOFF_OPENING) {
        HANDOFF_LIMIT.max(limit)
    } else {
        limit
    }
}

/// `ms` as minutes and seconds.
fn duration_text(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
}

/// The section on the local checks' records (ADR-t2852-1): only those
/// on the head's tree `head`, the newest first within
/// [`HANDOFF_CHECKS_BYTES`], with their commands as they are (the runtime
/// names no check); the others are counted, never shown, as a review's
/// material leaves them out (ADR-t2589-1 decision 4), and what each is to
/// the receipt is said (ADR-t2589-1 decisions 3 and 9, unchanged): one on
/// the final tree is a ground, one of another tree is not.
fn checks_section(
    fit: &mut Fit,
    head: Option<&str>,
    (checks, older): (&LocalChecks, usize),
) -> String {
    let read = format!("{CHECKS_DIR} holds the record whole");
    let lines: Vec<(String, bool)> = checks
        .matched
        .iter()
        .rev()
        .map(|check| {
            let (command, cut) = prompt_fit::cut_part(
                &check.command,
                HANDOFF_CHECK_COMMAND_BYTES,
                Keep::Start,
                &read,
            );
            let kind = check
                .kind
                .as_deref()
                .map_or_else(String::new, |kind| format!(", kind {kind}"));
            (
                format!(
                    "- exit {}, took {}, started {}{kind}: {command}\n",
                    check.exit_status,
                    duration_text(check.duration_ms),
                    check.started_at,
                ),
                cut,
            )
        })
        .collect();
    let mut text = match head {
        Some(head) => format!(
            "\n\nYour local checks on the tree {head} of HEAD, as the project's scripts recorded them in {CHECKS_DIR}, newest first:\n"
        ),
        None => format!(
            "\n\nYour local checks on HEAD's tree, as the project's scripts recorded them in {CHECKS_DIR}: HEAD's tree could not be read, so none is shown (`git rev-parse HEAD^{{tree}}` in your worktree prints it).\n"
        ),
    };
    if head.is_some() && lines.is_empty() {
        text.push_str("(none)\n");
    }
    let (kept, left_out) = picked_lines(
        fit,
        "checks",
        &lines,
        0..lines.len(),
        (usize::MAX, HANDOFF_CHECKS_BYTES),
    );
    for ((line, _), kept) in lines.iter().zip(&kept) {
        if *kept {
            text.push_str(line);
        }
    }
    if !left_out.is_empty() {
        text.push_str(&format!(
            "({} older records on HEAD's tree left out by this section's limit; {CHECKS_DIR} holds them all.)\n",
            left_out.len()
        ));
    }
    if checks.other_tree > 0 {
        text.push_str(&format!(
            "({} records of another tree, or whose tree changed while it ran, are not shown: they are no ground for the receipt. {CHECKS_DIR} holds them, a clue to go on from: what passed, what failed and how long it took.)\n",
            checks.other_tree
        ));
    }
    if checks.broken > 0 {
        text.push_str(&format!(
            "({} records could not be read or are over their limits.)\n",
            checks.broken
        ));
    }
    if older > 0 {
        text.push_str(&format!(
            "({older} older records were not read: past the most records a run's handoff reads.)\n"
        ));
    }
    if !checks.is_empty() || older > 0 {
        text.push_str("A record shown here ran on HEAD's tree: while nothing changes, it is a ground for the receipt as it is, so do not run it again. Once you change anything, run again on the final tree the checks it needs; which of the project's gates it needs is judged from the run's diff as before. Uncommitted changes make the work tree another tree than HEAD's.\n");
    }
    fit.section("checks", &text);
    text
}

/// The section on the tests that fail already on the landing branch's CI
/// in the files the run touches (ADR-t2852-1), the task's own all kept,
/// with how many others were left out and the file that lists them all;
/// empty without a list.
fn touched_failures_section(
    fit: &mut Fit,
    known: Option<&KnownFailures>,
    touched: &BTreeSet<String>,
) -> String {
    let Some(known) = known else {
        return String::new();
    };
    let (narrowed, left_out) = touched_known_failures(known, touched);
    let mut text = "\n\nOf the tests that fail already on the landing branch's CI, those in the files your commits change, the worktree changes or the task declares".to_owned();
    if narrowed.others.is_empty() && narrowed.fixing.is_empty() {
        text.push_str(": none\n");
    } else {
        text.push_str(":\n");
        text.push_str(&known_failures_section(
            fit,
            Some(&narrowed),
            Reader::Worker,
        ));
    }
    if left_out > 0 {
        text.push_str(&format!(
            "({left_out} more failing tests are in none of those files and are left out here; {} lists them all. Do not fix them.)\n",
            known.file
        ));
    }
    text
}

/// The handoff prompt of a new session of `run`'s worker for a large
/// context ([`HandoffMaterial`]), held to [`HANDOFF_LIMIT`]. Each part is
/// one section cut to its bytes keeping its start, with the bytes left out
/// and how to read the rest: the commits and the changes with git in the
/// worktree, the local checks' records in their directory, the known
/// failures and the request in their files, the receipt's summary in the
/// receipt. The request is required material (a cut is said in
/// `over_limit`); the fixed text is never cut. A review's findings are in
/// the request alone (ADR-t2852-1).
pub fn handoff_text(run: &TaskRun, material: &HandoffMaterial<'_>) -> FittedPrompt {
    let mut fit = Fit::new(HANDOFF_LIMIT);
    let base = run.base_commit();
    let mut text = format!(
        "{HANDOFF_OPENING}: its previous turn's context took {} tokens, above the {} tokens this repository sets, so nothing of the earlier conversation carries over. The task's prompt is above; the run, its worktree, its branch and its provider are the same. The work so far is in this worktree and its branch: go on from it rather than starting over.\n\nDo not start by reading again the instructions and documents the task's prompt names: what the task asks is above and below. Open a document only when the work needs it.\n\nThe supervisor stops this session unless you rewrite the receipt within {} seconds of this request (the resume timeout). Once your fixes are made and the checks the final tree needs have run, rewrite the receipt before any other work, such as a subagent review again.",
        material.peak_context, material.threshold, material.resume_timeout,
    );
    let commits = fit.text(
        "commits",
        material.commits.trim(),
        HANDOFF_COMMITS_BYTES,
        Keep::Start,
        &format!("`git log --oneline {base}..HEAD` in your worktree lists them all"),
    );
    fit.section("commits", &commits);
    text.push_str(&format!(
        "\n\nCommits since the base {base}, newest first:\n{}",
        or_none(&commits)
    ));
    let changes = fit.text(
        "changes",
        material.changes.trim_end(),
        HANDOFF_CHANGES_BYTES,
        Keep::Start,
        "`git status` and `git diff` in your worktree show them all",
    );
    fit.section("changes", &changes);
    text.push_str(&format!(
        "\n\nUncommitted changes in the worktree (`git status --porcelain`):\n{}",
        or_none(&changes)
    ));
    text.push_str(
        checks_section(
            &mut fit,
            material.head_tree,
            (material.checks, material.older_checks),
        )
        .trim_end_matches('\n'),
    );
    text.push_str(
        touched_failures_section(&mut fit, material.known_failures, material.touched)
            .trim_end_matches('\n'),
    );
    if let Some(summary) = material.receipt_summary {
        let read = run.receipt_path().map_or_else(
            || NOT_READABLE.to_owned(),
            |path| format!("the receipt at {path} holds the whole summary"),
        );
        let summary = fit.text(
            "receipt",
            summary.trim(),
            HANDOFF_RECEIPT_BYTES,
            Keep::Start,
            &read,
        );
        fit.section("receipt", &summary);
        text.push_str(&format!(
            "\n\nThe summary of the receipt you wrote last:\n{}",
            or_none(&summary)
        ));
    }
    let read = material.request_file.map_or_else(
        || NOT_READABLE.to_owned(),
        |path| format!("the whole of it is in {}", path.display()),
    );
    let request = fit.required("request", material.request, HANDOFF_REQUEST_BYTES, &read);
    fit.section("request", &request);
    text.push_str(&format!(
        "\n\nThe earlier session would have been sent this next; it is yours now:\n\n{request}"
    ));
    fit.finish(text)
}

/// The text that carries a person's answer to the session that asked,
/// held to [`NEXT_TURN_LIMIT`]: the answer to [`NEXT_TURN_TEXT_BYTES`].
pub(crate) fn answer_text(
    run: &TaskRun,
    ask: impl std::fmt::Display,
    answer: &str,
) -> FittedPrompt {
    let mut fit = Fit::new(NEXT_TURN_LIMIT);
    let answer = fit.text(
        "answer",
        answer,
        NEXT_TURN_TEXT_BYTES,
        Keep::Start,
        NOT_READABLE,
    );
    fit.section("answer", &answer);
    fit.finish(to_session(run, format!("answer to ask {ask}: {answer}")))
}

/// The text that carries a recovery job's `send_instruction` to the
/// session, held to [`NEXT_TURN_LIMIT`]: the instruction to
/// [`NEXT_TURN_TEXT_BYTES`].
pub(crate) fn recovery_instruction(run: &TaskRun, alert: &str, instruction: &str) -> FittedPrompt {
    let mut fit = Fit::new(NEXT_TURN_LIMIT);
    let instruction = fit.text(
        "instruction",
        instruction.trim(),
        NEXT_TURN_TEXT_BYTES,
        Keep::Start,
        NOT_READABLE,
    );
    fit.section("instruction", &instruction);
    fit.finish(to_session(
        run,
        format!(
            "dagq: the supervisor's recovery job for run {} (alert {alert}) asks: {instruction}",
            run.id(),
        ),
    ))
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

/// The bytes the whole worker prompt (`prompt.txt`) takes at most, the
/// language's instruction included (ADR-t2072-1, which brings ADR-t1566-1
/// decisions 2 to 6 to the worker): of the 1,245 runs kept on the host on
/// 2026-10-08 it took 16,109 bytes at the median, 28,065 at p90, 65,971 at
/// p99 and 124,676 at most, of which the predecessors took up to 81,949,
/// a run carried over 67,875 and a description 16,320. The limit is the
/// sum of the sections' limits below (93,000 bytes), the fixed instructions
/// (about 10,000) and the headings and notes of what was left out (about
/// 4,000; the largest input of the unit test takes 103,869 bytes), with room
/// for the language's instruction, and the section on the tests that fail
/// already on the landing branch's CI ([`KNOWN_FAILURES_LIMIT`]). A prompt
/// at p99 is not cut as a whole; how much of each section is kept is said
/// by its own limit.
pub const WORKER_PROMPT_LIMIT: usize = 112_000 + KNOWN_FAILURES_LIMIT;

/// The bytes of the task's own title, description, acceptance,
/// verification commands and declared paths: required material, cut only
/// past these (said in `over_limit`). A description took 16,320 bytes at
/// most on the host, an acceptance 2,742 and the verification commands 1,116.
pub const WORKER_TITLE_BYTES: usize = 1_000;
pub const WORKER_DESCRIPTION_BYTES: usize = 20_000;
pub const WORKER_ACCEPTANCE_BYTES: usize = 8_000;
pub const WORKER_VERIFY_BYTES: usize = 4_000;
pub const WORKER_PATHS_BYTES: usize = 4_000;

/// The bytes of the goal's title, description (3,135 at most on the
/// host), acceptance (1,636) and constraints (1,250).
pub const WORKER_GOAL_TITLE_BYTES: usize = 1_000;
pub const WORKER_GOAL_DESCRIPTION_BYTES: usize = 6_000;
pub const WORKER_GOAL_ACCEPTANCE_BYTES: usize = 4_000;
pub const WORKER_GOAL_CONSTRAINTS_BYTES: usize = 4_000;

/// The bytes of the task's context (2,750 at most on the host).
pub const WORKER_CONTEXT_BYTES: usize = 6_000;

/// The bytes of the direct predecessors' lines, in the order the queue
/// gives them, and of one's summary: the rest of a summary is the message
/// of its result commit. Of the 761 prompts with direct predecessors kept
/// on the host on 2026-10-08, their lines took 3,514 bytes at the median,
/// 13,665 at p90 and 81,882 at most; a summary took 2,121 at the median,
/// 6,580 at p90, 10,433 at p95 and 42,806 at most, and the longest summary
/// of a prompt 8,766 at p90. So the predecessors of a prompt at p90 are
/// kept whole, and a summary is cut only past p95.
pub const WORKER_PREDECESSORS_BYTES: usize = 16_000;
pub const WORKER_PREDECESSOR_SUMMARY_BYTES: usize = 10_000;

/// The count and bytes of the goals the task depended on (in the order the
/// queue gives them) with their headings, and the bytes of their completed
/// tasks' lines, the newest (the highest ID) first: a goal may hold
/// hundreds of tasks.
pub const WORKER_GOAL_PREDECESSORS: usize = 5;
pub const WORKER_GOAL_PREDECESSORS_BYTES: usize = 4_000;
pub const WORKER_GOAL_TASKS_BYTES: usize = 8_000;

/// The bytes of the sibling tasks' lines, in ID order.
pub const WORKER_SIBLINGS_BYTES: usize = 3_000;

/// The bytes of a carried-over run's receipt summary and of the reason a
/// person carried it over by hand.
pub const WORKER_INHERITED_SUMMARY_BYTES: usize = 3_000;
pub const WORKER_INHERITED_REASON_BYTES: usize = 1_000;

/// How a worker reads a predecessor's whole summary or a left-out one: the
/// landing commit's message holds the receipt's summary (ADR-t2072-1: only
/// the worktree, git and the goal doc, never a `dagq` command).
pub(super) const PREDECESSOR_READ: &str = "`git log --grep '^Dagq-Task: <id>$'` in your worktree shows each one's landing commit, whose message holds its summary";

/// One line of a predecessor (indented under a goal by `indent`), its
/// summary cut to `summary` bytes, and whether it was cut. A line too long
/// for its section is left out whole, as a long title is.
pub(super) fn predecessor_line(
    indent: &str,
    predecessor: &PredecessorSummary,
    summary: usize,
) -> (String, bool) {
    // `(not landed)`: no commit holds it.
    let read = if predecessor.result_commit.starts_with('(') {
        NOT_READABLE.to_owned()
    } else {
        format!(
            "`git show --no-patch {}` in your worktree prints the whole summary in its message",
            predecessor.result_commit
        )
    };
    let (summary, cut) = prompt_fit::cut_part(&predecessor.summary, summary, Keep::Start, &read);
    (
        format!(
            "{indent}- task {}: {}; result commit {}; summary: {summary}\n",
            predecessor.task_id, predecessor.title, predecessor.result_commit,
        ),
        cut,
    )
}

/// The lines of `lines` [`prompt_fit::pick`] keeps, taken in `order`
/// within `bytes`, in their own order, and the indices left out; counted
/// in `fit` as section `name`.
pub(super) fn picked_lines(
    fit: &mut Fit,
    name: &'static str,
    lines: &[(String, bool)],
    order: impl IntoIterator<Item = usize>,
    (count, bytes): (usize, usize),
) -> (Vec<bool>, Vec<usize>) {
    let sizes: Vec<usize> = lines.iter().map(|(line, _)| line.len()).collect();
    let kept = prompt_fit::pick(&sizes, order, count, bytes);
    let cuts: Vec<bool> = lines.iter().map(|(_, cut)| *cut).collect();
    fit.picked(name, &kept, &cuts);
    let left_out = (0..lines.len()).filter(|index| !kept[*index]).collect();
    (kept, left_out)
}

/// The Predecessor section of the worker's prompt within its limits: the
/// direct predecessors first, then the goals the task depended on with
/// their completed tasks, the newest first.
pub(super) fn predecessors_section(
    fit: &mut Fit,
    predecessors: &[PredecessorSummary],
    goal_predecessors: &[GoalPredecessorSummary],
) -> String {
    if predecessors.is_empty() && goal_predecessors.is_empty() {
        return "Predecessor tasks: none\n".to_owned();
    }
    let mut text =
        "Predecessor tasks (their changes are already in your base commit):\n".to_owned();
    let lines: Vec<(String, bool)> = predecessors
        .iter()
        .map(|p| predecessor_line("", p, WORKER_PREDECESSOR_SUMMARY_BYTES))
        .collect();
    let (kept, left_out) = picked_lines(
        fit,
        "predecessors",
        &lines,
        0..lines.len(),
        (usize::MAX, WORKER_PREDECESSORS_BYTES),
    );
    for ((line, _), kept) in lines.iter().zip(&kept) {
        if *kept {
            text.push_str(line);
        }
    }
    if !left_out.is_empty() {
        let ids: Vec<String> = left_out
            .iter()
            .map(|i| format!("task {}", predecessors[*i].task_id))
            .collect();
        text.push_str(&left_out_note("predecessor tasks", &ids, PREDECESSOR_READ));
    }
    fit.section("predecessors", &text);
    let mut goals = String::new();
    let headings: Vec<(String, bool)> = goal_predecessors
        .iter()
        .map(|goal| {
            (
                format!(
                    "- goal {} (closed as achieved): {}; its completed tasks:\n",
                    goal.goal_id, goal.title
                ),
                false,
            )
        })
        .collect();
    let (goal_kept, goals_left_out) = picked_lines(
        fit,
        "goal_predecessors",
        &headings,
        0..headings.len(),
        (WORKER_GOAL_PREDECESSORS, WORKER_GOAL_PREDECESSORS_BYTES),
    );
    let tasks: Vec<(usize, &PredecessorSummary)> = goal_predecessors
        .iter()
        .enumerate()
        .filter(|(g, _)| goal_kept[*g])
        .flat_map(|(g, goal)| goal.tasks.iter().map(move |task| (g, task)))
        .collect();
    let task_lines: Vec<(String, bool)> = tasks
        .iter()
        .map(|(_, task)| predecessor_line("  ", task, WORKER_PREDECESSOR_SUMMARY_BYTES))
        .collect();
    let mut newest: Vec<usize> = (0..tasks.len()).collect();
    newest.sort_by_key(|i| std::cmp::Reverse(tasks[*i].1.task_id));
    let (task_kept, tasks_left_out) = picked_lines(
        fit,
        "goal_predecessors",
        &task_lines,
        newest,
        (usize::MAX, WORKER_GOAL_TASKS_BYTES),
    );
    for (g, goal) in goal_predecessors.iter().enumerate() {
        if !goal_kept[g] {
            continue;
        }
        goals.push_str(&headings[g].0);
        if goal.tasks.is_empty() {
            goals.push_str("  - none\n");
        }
        for (i, (owner, _)) in tasks.iter().enumerate() {
            if *owner == g && task_kept[i] {
                goals.push_str(&task_lines[i].0);
            }
        }
    }
    if !goals_left_out.is_empty() {
        let ids: Vec<String> = goals_left_out
            .iter()
            .map(|g| format!("goal {}", goal_predecessors[*g].goal_id))
            .collect();
        goals.push_str(&left_out_note(
            "goals depended on",
            &ids,
            "their completed tasks' changes are in your base commit, and `git log` in your worktree shows each landing commit with its summary and its `Dagq-Task: <id>` line",
        ));
    }
    if !tasks_left_out.is_empty() {
        let ids: Vec<String> = tasks_left_out
            .iter()
            .map(|i| format!("task {}", tasks[*i].1.task_id))
            .collect();
        goals.push_str(&left_out_note(
            "completed tasks of the goals",
            &ids,
            PREDECESSOR_READ,
        ));
    }
    fit.section("goal_predecessors", &goals);
    text.push_str(&goals);
    text
}

/// Text of `prompt.txt` and what it takes. `goal` is the task's goal as
/// it reads at claim time, `predecessors` the task's direct dependencies,
/// `goal_predecessors` the goals it depends on (in the Predecessor section)
/// and `siblings` the other tasks executing at claim time
/// (`siblings_in_progress`), `inherited` the run a retry carries over,
/// if any, and `known_failures` the tests that fail already on the landing
/// branch's CI, a section only when there are any (ADR-t1920-1 decision 5). The Goal, Context, Predecessor and Sibling sections are always
/// present, `none` when empty, so the prompt keeps one shape whether or
/// not a task has a goal, a context, dependencies or company.
///
/// Each section is held to its limit (`WORKER_*`) within
/// [`WORKER_PROMPT_LIMIT`] (ADR-t2072-1): what a list leaves out is counted
/// and named with how the worker reads it in its worktree or with git. The
/// task's own title, description, acceptance, verification commands and
/// paths are never left out, only cut past their own limits and said in
/// `over_limit`. Within the limits the text is the same as without them.
///
/// The text is a snapshot at claim time and is never rewritten: a
/// `goal edit` or a sibling's change made while the run works reaches only
/// the prompt of a later claim.
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
    known_failures: Option<&KnownFailures>,
) -> Result<FittedPrompt> {
    let receipt = run.receipt_path().context("missing receipt path")?;
    let mut fit = Fit::new(WORKER_PROMPT_LIMIT);
    let title = fit.required("task", task.title(), WORKER_TITLE_BYTES, NOT_READABLE);
    let description = fit.required(
        "task",
        task.description(),
        WORKER_DESCRIPTION_BYTES,
        NOT_READABLE,
    );
    let acceptance = fit.required(
        "task",
        task.acceptance(),
        WORKER_ACCEPTANCE_BYTES,
        NOT_READABLE,
    );
    let verification = fit.required(
        "task",
        &serde_json::to_string_pretty(&task.verification_commands())?,
        WORKER_VERIFY_BYTES,
        NOT_READABLE,
    );
    for text in [&title, &description, &acceptance, &verification] {
        fit.section("task", text);
    }
    let inherited = inherited
        .map(|inherited| {
            let summary = fit.text(
                "inherited",
                &inherited.summary,
                WORKER_INHERITED_SUMMARY_BYTES,
                Keep::Start,
                &format!(
                    "{NOT_READABLE}; `git log {base}..{head}` in your worktree shows its work, not this summary",
                    base = inherited.base,
                    head = inherited.head
                ),
            );
            let reason = inherited.by_hand.as_ref().map(|(by, reason)| {
                let reason = fit.text(
                    "inherited",
                    reason,
                    WORKER_INHERITED_REASON_BYTES,
                    Keep::Start,
                    NOT_READABLE,
                );
                (by.clone(), reason)
            });
            let section = Inheritance {
                summary,
                by_hand: reason,
                ..inherited.clone()
            }
            .section();
            fit.section("inherited", &section);
            section
        })
        .unwrap_or_default();
    let goal = match goal {
        None => "Goal: none, this task stands alone\n".to_owned(),
        Some(goal) => {
            let read = goal.doc().map_or_else(
                || NOT_READABLE.to_owned(),
                |doc| format!("the goal doc {doc} in your worktree has the full picture"),
            );
            let mut text = |what: &str, max: usize| fit.text("goal", what, max, Keep::Start, &read);
            let section = format!(
                "Goal (the higher-level problem this task and its sibling tasks solve together):\n\
                 Goal ID: {id}\nGoal title: {title}\nGoal description:\n{description}\n\
                 Goal acceptance:\n{acceptance}\nGoal constraints:\n{constraints}\n\
                 Goal doc: {doc}\n",
                id = goal.id(),
                title = text(goal.title(), WORKER_GOAL_TITLE_BYTES),
                description = text(goal.description(), WORKER_GOAL_DESCRIPTION_BYTES),
                acceptance = text(goal.acceptance(), WORKER_GOAL_ACCEPTANCE_BYTES),
                constraints = text(goal.constraints(), WORKER_GOAL_CONSTRAINTS_BYTES),
                doc = goal
                    .doc()
                    .map(|doc| format!(
                        "{doc} (a path in the repository; read it for the full picture)"
                    ))
                    .unwrap_or_else(|| "none".to_owned()),
            );
            fit.section("goal", &section);
            section
        }
    };
    let context = if task.context().trim().is_empty() {
        "Context: none\n".to_owned()
    } else {
        let context = format!(
            "Context (why this task exists and what to read first):\n{}\n",
            fit.text(
                "context",
                task.context(),
                WORKER_CONTEXT_BYTES,
                Keep::Start,
                NOT_READABLE
            )
        );
        fit.section("context", &context);
        context
    };
    let predecessors = predecessors_section(&mut fit, predecessors, goal_predecessors);
    let siblings = if siblings.is_empty() {
        "Sibling tasks in progress: none\n".to_owned()
    } else {
        let mut text =
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n"
                .to_owned();
        let lines: Vec<(String, bool)> = siblings
            .iter()
            .map(|other| (format!("- task {}: {}\n", other.id(), other.title()), false))
            .collect();
        let (kept, left_out) = picked_lines(
            &mut fit,
            "siblings",
            &lines,
            0..lines.len(),
            (usize::MAX, WORKER_SIBLINGS_BYTES),
        );
        for ((line, _), kept) in lines.iter().zip(&kept) {
            if *kept {
                text.push_str(line);
            }
        }
        if !left_out.is_empty() {
            let ids: Vec<String> = left_out
                .iter()
                .map(|i| format!("task {}", siblings[*i].id()))
                .collect();
            text.push_str(&left_out_note("sibling tasks", &ids, NOT_READABLE));
        }
        fit.section("siblings", &text);
        text
    };
    let known_failures = known_failures_section(&mut fit, known_failures, Reader::Worker);
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
    evidence.push_str(spike_lines(task));
    // The declared scope (ADR-0029): changing anything else parks the run.
    let paths = if task.paths().is_empty() {
        String::new()
    } else {
        let declared = fit.required(
            "paths",
            &task.paths().join(", "),
            WORKER_PATHS_BYTES,
            NOT_READABLE,
        );
        fit.section("paths", &declared);
        format!(
            "Paths you may change (globs from the repository root; `*` stays in one directory, `**` spans any depth): {declared}. A commit that changes any other path is not accepted: the run waits for a session to take it out. If the task needs another path, do not change it and do not run dagq ask: write the receipt with result failed and name in summary the paths it needs and what to change there (a running task's paths cannot change; the planner registers it again with wider paths).\n",
        )
    };
    // How the session ends a step and hears back: it ends its turn and
    // reads the next turn's prompt.
    let headless = format!("{HEADLESS_WORKER}{}", headless_provider_line(route.0));
    let answer_arrives = "The answer arrives as the prompt of your next turn in this same session, as `answer to ask <id>: ...`; continue from it.";
    let after_submitting = "After writing the receipt, report the outcome briefly and end the turn. A later turn comes only if a review, a landing or a person sends the run back.";
    let (stop_word, dont_wait) = (
        "end the turn",
        "end the turn with the question in your reply",
    );
    let text = format!(
        "You are executing dagq task {task_id}, run {run_id}.\n\
         Work only in the assigned Git worktree.\n\
         {reading}\
         Implement the task, run the checks described below, and commit the result.\n\
         Do not merge, push, close the workspace, or modify the queue/runtime files.\n\
         {review}\
         Task title: {title}\nDescription:\n{description}\nAcceptance criteria:\n{acceptance}\n\
         Verification commands (integrate runs them once after rebasing onto main; that run is the verification of record for the commit):\n{verification}\n\
         {local_checks}\n\
         {evidence}{paths}{goal}{context}{predecessors}{siblings}{inherited}{known_failures}\
         Your assignment is this task only. Do not change what a sibling task owns; if you find work outside this task, record it in the receipt as follow_ups instead of doing it.\n\
         {acceptance_map}{docs_check}\
         Write a completion receipt to {receipt} using a temporary file in the same directory and atomic rename.\n\
         Receipt JSON: {{\"run_id\":\"{run_id}\",\"result\":\"succeeded, failed or already_satisfied\",\"commit\":\"full Git SHA of the branch head\",\"tests\":{{\"status\":\"passed, failed or not_applicable\",\"evidence_or_reason\":\"...\"}},\"e2e\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"subagent_review\":{{\"status\":\"...\",\"evidence_or_reason\":\"...\"}},\"summary\":\"...\",\"follow_ups\":[{{\"title\":\"...\",\"description\":\"...\",\"category\":\"...\",\"membership_proposal\":{{\"classification\":\"required, out_of_scope or undecided\",\"acceptance_items\":[\"...\"],\"reason\":\"...\"}}}}]}}\n\
         Each of tests, e2e and subagent_review needs evidence when passed and a reason when not_applicable.\n\
         {already_satisfied}\
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
        already_satisfied = ALREADY_SATISFIED,
        local_checks = local_checks("above"),
        categories = follow_up_categories_line(),
        follow_up_proposal = FOLLOW_UP_PROPOSAL,
        topics = worker_question_topics_line(),
        ask_rules_first = ASK_RULES_FIRST,
    );
    Ok(fit.finish(text))
}

/// The worker's prompt on `provider` before the task's values go in
/// (goal 113): [`prompt`] of a placeholder task with every section in it (a
/// goal, a predecessor, a goal it waited for, a sibling, a run it carries
/// over, required evidence, paths, the e2e and the known failures), each value a placeholder,
/// followed by the same task with every section empty (and the run carried
/// over by hand), so both branches of each section are in it. Its text is fixed by this binary, so its hash changes with the prompt's
/// headings and sentences and never with a task's values.
///
/// So the hash leaves out the prompt after a task's values went in (each
/// task's differs), the language instruction appended to it, and the
/// resume and revise requests, which are built apart.
pub fn worker_template(provider: Provider) -> Result<String> {
    use crate::domain::{
        GoalRecord, GoalStatus, RunRecord, TaskRecord, TaskStatus,
        worker::{Worker, WorkerMode},
    };
    const SHA: &str = "0000000000000000000000000000000000000000";
    const RUN: &str = "00000000-0000-4000-8000-000000000000";
    let value = |name: &str| format!("<{name}>");
    let record = || TaskRecord {
        goal_priority: None,
        id: TaskId::new(1),
        title: value("title"),
        description: value("description"),
        acceptance: value("acceptance"),
        verification_commands: vec![value("verification command")],
        required_evidence: vec![
            EvidenceCheck::Tests,
            EvidenceCheck::E2e,
            EvidenceCheck::SubagentReview,
        ],
        paths: vec![value("path")],
        priority: Default::default(),
        change: None,
        status: TaskStatus::InProgress,
        goal_id: Some(GoalId::new(1)),
        context: value("context"),
        created_at: String::new(),
        updated_at: String::new(),
        worker: Worker {
            provider,
            mode: WorkerMode::Headless,
        },
        named_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    };
    // A Spike, so the full prompt carries its section (ADR-t1487-1); the
    // bare one below is an implementation.
    let task = Task::restore(TaskRecord {
        execution_class: crate::domain::ExecutionClass::Spike,
        ..record()
    })?;
    let run = TaskRun::restore(RunRecord {
        id: RunId::new(RUN)?,
        task_id: task.id(),
        status: RunStatus::Claimed,
        requested_provider: provider,
        actual_provider: provider,
        worker_mode: WorkerMode::Headless,
        base_commit: CommitSha::try_from(SHA)?,
        branch: Some(value("branch")),
        worktree_path: Some(value("worktree")),
        workspace_id: None,
        receipt_path: Some(value("receipt")),
        log_path: None,
        result_commit: None,
        repo_path: None,
        run_dir: Some(value("run directory")),
        last_error: None,
        workspace_closed_at: None,
        created_at: String::new(),
    })?;
    let goal = Goal::restore(GoalRecord {
        id: GoalId::new(1),
        title: value("goal title"),
        description: value("goal description"),
        acceptance: value("goal acceptance"),
        constraints: value("goal constraints"),
        doc: Some(value("goal doc")),
        priority: Default::default(),
        tags: Vec::new(),
        status: GoalStatus::Open,
        closed_at: None,
        verdict: None,
        created_at: String::new(),
        updated_at: String::new(),
    })?;
    let predecessor = PredecessorSummary {
        task_id: TaskId::new(2),
        title: value("predecessor title"),
        result_commit: value("result commit"),
        summary: value("summary"),
    };
    let goal_predecessor = GoalPredecessorSummary {
        goal_id: GoalId::new(2),
        title: value("goal predecessor title"),
        tasks: vec![predecessor.clone()],
    };
    let inherited = Inheritance {
        run_id: run.id().clone(),
        base: run.base_commit().clone(),
        head: value("head"),
        branch: Some(value("branch")),
        receipt_path: Some(value("receipt")),
        summary: value("summary"),
        by_hand: None,
        replan: None,
    };
    let failure = KnownFailure {
        name: value("failing test"),
        url: Some(value("ci url")),
        fix_tasks: vec![TaskId::new(3)],
    };
    // A fix task's own failing test and another's, and one fixed by none.
    let known = KnownFailures {
        others: vec![
            failure.clone(),
            KnownFailure {
                fix_tasks: Vec::new(),
                ..failure.clone()
            },
        ],
        fixing: vec![failure],
        file: value("known failures file"),
    };
    let full = prompt(
        &task,
        &run,
        Some(&goal),
        &[predecessor],
        &[goal_predecessor],
        std::slice::from_ref(&task),
        Some(&inherited),
        &[value("e2e path")],
        Some(&known),
    )?
    .text;
    // The other branch of each section: `none`, and a run a person carried
    // over by hand.
    let bare = Task::restore(TaskRecord {
        required_evidence: Vec::new(),
        paths: Vec::new(),
        goal_id: None,
        context: String::new(),
        ..record()
    })?;
    let by_hand = Inheritance {
        by_hand: Some((value("by"), value("reason"))),
        ..inherited
    };
    let empty = prompt(&bare, &run, None, &[], &[], &[], Some(&by_hand), &[], None)?.text;
    Ok(format!("{full}\n{empty}"))
}

/// What the worker's prompt says to a Spike's run (ADR-t1487-1 decisions
/// 2-4): a negative or unresolved verdict is a result, the limit in the
/// description is the agent's to keep (the runtime adds no timebox), a
/// premise of the worker's route holds only when checked on it, and the
/// receipt's `spike_result` validation requires. Empty for an
/// implementation's run.
fn spike_lines(task: &Task) -> &'static str {
    if task.execution_class().is_spike() {
        SPIKE_LINES
    } else {
        ""
    }
}

pub(super) const SPIKE_LINES: &str = "Spike: this task is a Spike (execution class spike). It settles a question the plan depends on by experiment, prototype, build or measurement; its answer is the deliverable, so a negative or unresolved verdict is a result, not a failure: write result succeeded with the verdict the evidence supports, and failed only when you could not investigate at all. Keep within the limit (time, attempts) the description gives: it is an instruction to you, which the runtime does not enforce; at the limit, stop and report what you found, unresolved if it does not settle the question. A premise about permissions, headless runs or the provider holds only when you checked it on this run's own route (not a planner's, a subagent's or a person's terminal). Commit the evidence the description asks for where it says, or name where it is. Do not put the next step in follow_ups or start it yourself: write it into grounds, and a planner decides from the result. The receipt must also carry spike_result, or the run waits for a session to add it: \"spike_result\":{\"verdict\":\"holds, does_not_hold or unresolved\",\"grounds\":\"why the evidence gives this verdict, and what it means for the plan\",\"evidence\":\"where the evidence is (a committed path, a log, the receipt)\",\"conditions\":{\"commit\":\"the commit examined\",\"tools\":\"the tools and their versions\",\"provider\":\"the provider and route it ran on\",\"environment\":\"the OS, machine and settings\"},\"spent\":\"the time and attempts used against the limit\"}. Every field but spent must be non-blank.\n";

/// What the worker's prompt says of the e2e (ADR-t1233-2 decision 1): the
/// runtime runs it on the host after the review passes, so the worker does
/// not. Only when the run may need it (the task asks for it, or the
/// repository names `[e2e] paths` its diff may touch); empty otherwise.
pub(super) fn e2e_line(task: &Task, e2e_paths: &[String]) -> String {
    if e2e_paths.is_empty() && !task.required_evidence().contains(&EvidenceCheck::E2e) {
        return String::new();
    }
    "E2E: do not run the e2e yourself. When the run needs it (the task asks for it, or the diff touches the repository's dagq.toml [e2e] paths), the runtime runs it on the host after the review passes, before the run lands, and sends the run back to a session if it fails. Report `e2e` in the receipt as not_applicable with that reason.\n".to_owned()
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
