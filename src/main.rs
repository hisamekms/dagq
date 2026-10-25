use std::{
    env,
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use dagq::application::commands::operations::{HookSession, Operation, WorkspaceScope};
use dagq::application::queue_commands::QueueCommand;
use dagq::application::queue_reads::{self as reads, QueueRead};
use dagq::application::queue_writes::QueueWrite;
use dagq::domain::queue_service::UseCase;
use dagq::{
    application::TaskReads,
    domain::{
        ActorContext, ActorRole, AskId, AskKind, AskReason, AuthorizationError, Capability,
        EventId, FindingId, FindingTarget, GoalId, LeaseToken, NewAsk, NewFinding, NewNote,
        NoteTarget, PlannerId, PlannerOrigin, PlannerOwner, ProposalId, RequestId, Resource, RunId,
        SessionRole, StaticPolicy, TaskId, TaskStatus,
    },
    infrastructure::{adapters::path_text, location::QueueLocation},
};

#[derive(Parser)]
#[command(
    version = dagq::VERSION,
    about = "Manage a local dependency-aware task queue (JSON output)"
)]
struct Cli {
    /// Queue database path. Without it, the queue of the repository containing
    /// the working directory is used: $XDG_DATA_HOME/dagq/<hash>/queue.db.
    #[arg(long)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

// ADR-t2159-1 decision 6. What it is is on its field: a doc comment here would be the help of
// a command that has none of its own.
#[derive(clap::Args, Clone)]
struct RetiredCmux {
    /// `--cmux <PATH>`, which the command lines of an earlier binary still carry (the launchd
    /// plist of a registered supervisor, the argv it execs at a handoff, the jobs and wrappers
    /// it started): accepted, left out of the help and ignored, as dagq calls no cmux.
    #[arg(long = "cmux", hide = true, value_name = "PATH")]
    _ignored: Option<PathBuf>,
}

#[derive(Subcommand, Clone)]
enum Command {
    /// Initialize the queue, creating its directory if needed. An existing queue is checked, never migrated.
    /// Its output recommends the review's settings (`recommended`): the built-ins on and off by
    /// default, and `dagq setup-review` to write the project's rules for them; the toolchain
    /// profiles declared or suggested and their proposals, the container's image, registries and
    /// worker cache among them (`toolchains`); and the templates of [[telemetry.commands]]
    /// for the tools its files show that no profile knows, npm/pnpm, go, pytest, gradle and make
    /// (`telemetry_commands`), never cargo, whose rules the Rust profile holds. It writes no
    /// dagq.toml and starts no agent.
    Init,
    // ADR-t2259-1 decision 7.
    /// Generate the review's settings of this repository with a coding agent, once, and write
    /// them to the main checkout's working tree only when you approve them. The agent reads the
    /// project (its instructions, documents, tests, languages and tools) in a temporary checkout
    /// of the main checkout's HEAD under the queue's directory, under
    /// DAGQ_ROLE=setup-review-job, which may only read the queue, and writes there dagq.toml's
    /// built-in tables ([review.subagents."builtin:<name>"]) and program tables
    /// ([review.programs.<name>]), .dagq/review/builtin-<name>.md (the project's rules for a
    /// built-in agent) and .dagq/review/programs/*.sh (fast checks that run no build or test). The
    /// settings the checkout commits now are its input, so running it again updates them. Before
    /// you see the diff, dagq refuses a generation that changed any other file or any other table
    /// of dagq.toml, a dagq.toml the runtime cannot read, and a program script that does not take
    /// its root from the working directory's Git work tree; it applies nothing when the working
    /// tree has changes not committed to a file the generation changes. It asks on the terminal
    /// after showing the diff (--yes approves beforehand, a person's alone: the inbox and every
    /// other actor are refused it); without approval nothing is written.
    /// It commits and pushes nothing and changes nothing of the queue, and removes the temporary
    /// checkout whatever happened. The job runs on [roles.setup_review]'s provider, Claude by
    /// default. Only a person and the inbox run it.
    SetupReview {
        /// Approve beforehand: write the changes that pass the checks without asking.
        #[arg(long)]
        yes: bool,
        /// Seconds the agent may run before it is killed.
        #[arg(long, default_value_t = 1800)]
        timeout: u64,
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
        /// Codex CLI executable, for [roles.setup_review]'s provider = "codex"; a bare name is
        /// resolved on PATH outside a terminal's shims.
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        /// Codex's home (tests); Codex's own without.
        #[arg(long, hide = true)]
        codex_home: Option<PathBuf>,
    },
    // ADR-0045.
    /// Apply the migrations this binary knows and the queue lacks; opening a queue never does. A
    /// breaking migration is refused while a supervisor, run or wrapper uses the queue, and the
    /// database is copied to `backups/` first.
    Migrate {
        /// Report the schema and what would be applied, without changing anything.
        #[arg(long)]
        check: bool,
    },
    // ADR-0045.
    /// Replace this dagq binary and hand the queue's live supervisor over to the new one without
    /// waiting for its sessions: build (or take) the binary, check its --version and a start on a
    /// throwaway queue, apply the queue's compatible migrations, put it in place by a rename that
    /// keeps the old one as <name>.previous, and ask the supervisor to exec it. A failed handoff
    /// puts the old binary back.
    Install {
        /// A checkout to build (`cargo build --release --locked -p dagq`), or a built binary.
        /// Default: build the main checkout of the repository of the working directory.
        #[arg(long, conflicts_with_all = ["rollback", "release"])]
        from: Option<PathBuf>,
        // ADR-t618-1.
        /// Install a release of crates.io instead: `cargo install --locked dagq@<VERSION>` under
        /// the queue's update/release directory, then the same check, swap and handoff. Without
        /// VERSION, the newest release.
        #[arg(long, num_args = 0..=1, default_missing_value = "", conflicts_with = "rollback", value_name = "VERSION")]
        release: Option<String>,
        /// The cargo that installs a release (tests give a stub).
        #[arg(long, hide = true, default_value = "cargo")]
        cargo: PathBuf,
        /// The binary to replace. Default: this one.
        #[arg(long)]
        to: Option<PathBuf>,
        /// Put <name>.previous back in place instead, the same way.
        #[arg(long)]
        rollback: bool,
        /// When the new binary brings a breaking migration: drain the supervisor (wait for its
        /// runs), migrate with a backup, and start it again with the new binary's `up`.
        #[arg(long)]
        allow_breaking: bool,
        /// Seconds the supervisor may take to come back under the new binary.
        #[arg(long, default_value_t = 1800)]
        handoff_timeout: u64,
        #[command(flatten)]
        cmux: RetiredCmux,
        /// Claude Code executable the restarted supervisor uses after a drain, or after a failed
        /// watch of the handed-over supervisors.
        #[arg(long)]
        claude: Option<PathBuf>,
        /// Codex CLI the restarted supervisor's Codex workers use after a drain, or after a failed
        /// watch of the handed-over supervisors.
        #[arg(long)]
        codex: Option<PathBuf>,
        /// Plugin directory of the restarted `up` after a drain, or after a failed watch of the
        /// handed-over supervisors.
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
        // ADR-t963-1 decision 1.
        /// Install a checkout of dagq's source without first running its e2e (`cargo test --locked
        /// --test e2e -- --ignored`), which otherwise must pass before anything is replaced. For a
        /// person in a hurry; a built binary, `--rollback` and `--release` have no e2e.
        #[arg(long)]
        skip_e2e: bool,
        /// A shell command in place of the e2e (tests).
        #[arg(long, hide = true)]
        e2e_command: Option<String>,
        /// Seconds the e2e may run before it counts as failed.
        #[arg(long, hide = true, default_value_t = 1800)]
        e2e_timeout: u64,
        // task 1048.
        /// Milliseconds between two looks at the supervisors asked to hand off (tests shorten
        /// it).
        #[arg(long, hide = true, default_value_t = 500)]
        poll_ms: u64,
        // ADR-0073 decision 13.
        /// Seconds each supervisor handed over may take to heartbeat on under the new binary
        /// before the install puts the old one back and tells the inbox (tests shorten it).
        #[arg(long, hide = true, default_value_t = dagq::application::update::WATCH_TIMEOUT.as_secs())]
        watch_timeout: u64,
    },
    /// Show which queue this directory resolves to, without opening it: its database, log
    /// directory, LaunchAgent label and plist path (given even before `up` writes it).
    Locate,
    /// Register a draft task; verification commands are stored, not executed.
    Add {
        title: String,
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long, default_value = "")]
        acceptance: String,
        #[arg(long = "verify")]
        verification_commands: Vec<String>,
        #[arg(long = "depends-on")]
        dependencies: Vec<i64>,
        /// Goal the task waits for until it is closed as achieved; repeatable. Never the
        /// task's own goal.
        #[arg(long = "depends-on-goal")]
        goal_dependencies: Vec<i64>,
        /// Open goal the task belongs to.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
        /// Why the task exists and what to read first; shown to the worker.
        #[arg(long, default_value = "")]
        context: String,
        /// Receipt check validation requires to be passed with evidence; repeatable.
        /// A receipt without it parks the run as needs_session (evidence_missing).
        #[arg(long = "evidence", value_parser = ["tests", "e2e", "subagent_review"])]
        required_evidence: Vec<String>,
        /// Glob of the paths the task may change, from the repository root; repeatable.
        /// `*` and `?` stay inside one segment, a `**` segment spans any depth. Validation
        /// parks a run changing anything else as needs_session (scope_violation) and
        /// integrate refuses to land it. Omitted: no limit.
        #[arg(long = "paths")]
        paths: Vec<String>,
        // ADR-t1639-1.
        /// How urgently the supervisor should claim it: interrupt (ahead of every other ready
        /// task), urgent (a defect stopping the operation), high (groundwork other work needs
        /// soon), normal, or low (later). A ready task it waits for inherits it. Omitted: the task
        /// inherits its goal's priority (normal without a goal) and follows a later change of it.
        #[arg(long, value_parser = PRIORITIES)]
        priority: Option<String>,
        // ADR-t980-1.
        /// The kind of change the task makes, as a label of the repository's own (a lowercase slug
        /// of letters, digits, '-' and '_', not `unknown` or `all`); `stats`, `kpi` and `forecast`
        /// group the runs by it. When dagq.toml has `[tasks] changes`, one of them, and `lint` and
        /// `submit` refuse a task without one. Omitted: none.
        #[arg(long)]
        change: Option<String>,
        // ADR-t813-2.
        /// The agent the worker runs on: claude or codex. Omitted: claude.
        #[arg(long, value_parser = PROVIDERS)]
        provider: Option<String>,
        // ADR-t813-1, ADR-t1340-1.
        /// Run the worker non-interactively, one call per turn, and store that mode. Omitted: the
        /// provider's default, headless for Claude and for codex (its only mode); the task
        /// follows a later change of it.
        #[arg(long, conflicts_with = "interactive")]
        headless: bool,
        // ADR-t1433-2.
        /// Refused: the interactive worker was retired and every worker runs headless. Read
        /// a run's turns with `run log --follow`; its questions come as asks for `answer`.
        #[arg(long)]
        interactive: bool,
        // ADR-t1632-1.
        /// Claim the task only once the supervisor's own build (its build identifier's commit)
        /// contains the landed commits of every task it --depends-on: for a task whose work
        /// needs the fixed binary to carry them. Until then `status` lists it under
        /// `claim_deferrals` (reason not_in_build).
        #[arg(long)]
        wait_for_build: bool,
        // ADR-t1487-1.
        /// Whether the run implements (implementation) or investigates a premise of the plan by
        /// experiment, prototype, build or measurement (spike), independent of --change. A
        /// spike's receipt must carry its result (`spike_result`), or validation parks the run
        /// as needs_session (spike_result_missing). Omitted: implementation.
        #[arg(long, value_parser = EXECUTION_CLASSES)]
        execution_class: Option<String>,
    },
    /// List one page of tasks, newest first: unfinished ones unless --status or --all says otherwise.
    /// Prints {"tasks", "next", "total"}; pass `next` to --before for the following page (null: none).
    List {
        /// Only these statuses (comma-separated, any of them): draft, submitted, ready, in_progress,
        /// completed, canceled.
        #[arg(long, value_delimiter = ',', conflicts_with = "all")]
        status: Vec<String>,
        /// Include completed and canceled tasks.
        #[arg(long)]
        all: bool,
        /// Only tasks of this goal.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
        /// Page size.
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
        /// Start the page at this task ID (the previous page's `next`); lists IDs up to it.
        #[arg(long)]
        before: Option<i64>,
        /// Include description, acceptance, context, verification commands and timestamps.
        #[arg(long)]
        full: bool,
    },
    /// Show a task, its latest run and its latest events; long texts are cut
    /// to 300 characters (ending in `…`, with `truncated: true`).
    Show {
        id: i64,
        /// Print every run, event payload and process, and the texts in full.
        #[arg(long)]
        full: bool,
        /// How many of the latest events to show without --full.
        #[arg(long, default_value_t = dagq::view::DEFAULT_EVENTS, conflicts_with = "full")]
        events: usize,
    },
    /// Make a task ready (dependencies may still block execution). Plan review readies submitted
    /// tasks; by hand a draft or submitted task needs --bypass-review. Without it only an
    /// in-progress task whose runs all failed or were interrupted returns to ready (a retry).
    Ready {
        id: i64,
        /// Skip plan review (recorded as a review_bypassed event). A follow-up without a current
        /// membership judgement is refused, as submit refuses it.
        #[arg(long, conflicts_with = "inherit")]
        bypass_review: bool,
        /// Retry an in-progress task whose latest run failed or was interrupted, carrying that
        /// run's branch (its commits on top of its base) over to the next run, without plan
        /// review: user or inbox only, refused while a recovery job holds the run.
        #[arg(long, requires = "reason")]
        inherit: bool,
        /// Why the run is carried over (with --inherit), recorded in its triage_finished event.
        #[arg(long, requires = "inherit")]
        reason: Option<String>,
    },
    /// Return a ready or submitted task to draft.
    Draft { id: i64 },
    /// Submit draft tasks for plan review as one proposal owned by this planner session: TASKs,
    /// and with --goal a goal and its draft tasks. The tasks become submitted, which no claim
    /// takes; plan review makes them ready. A follow-up draft without a current membership
    /// judgement is refused. Prints the proposal.
    #[command(group = clap::ArgGroup::new("members").multiple(true).required(true))]
    Submit {
        /// Draft task to submit; repeatable.
        #[arg(group = "members")]
        tasks: Vec<i64>,
        /// Open or draft goal to submit with its draft tasks; repeatable.
        #[arg(long = "goal", group = "members")]
        goals: Vec<i64>,
        /// Submit this proposal again after plan review sent it back, with the drafts it holds.
        #[arg(long, group = "members")]
        proposal: Option<i64>,
        // ADR-0044 decision 19.
        /// Open finding the proposal remedies; repeatable. It becomes proposed with the
        /// proposal. A planner the runtime opened for a finding links that one without it.
        #[arg(long = "finding")]
        findings: Vec<i64>,
        /// The replacement plan (JSON) a planner of a replanning operation's request submits its
        /// proposal with: how the parked task's acceptance items, saved work, verification and
        /// successors go to the proposal's child tasks. Refused for any other proposal; such a
        /// proposal is refused without it.
        #[arg(long)]
        replacement: Option<PathBuf>,
    },
    /// Check the fixed rules of a plan (plan review's mechanical checks): dependency cycles,
    /// dependencies on completed, canceled or unsubmitted draft tasks and on abandoned goals, no
    /// verification (with or without declared paths), invalid path globs, blank acceptance,
    /// titles repeated within the set, and the problems of a replacement plan a proposal of a
    /// replanning request went with. Lints TASKs and the members of each --proposal. Prints
    /// {"tasks", "violations", "hints"}, each violation {"code", "task_id", "reason"}; none: an
    /// empty list. Hints refuse nothing: per task, the sections {"path", "heading", "terms"} of
    /// the documents dagq.toml's [tasks] related_docs names that hold the task's code identifiers
    /// and paths while its description and context name neither the document nor the section,
    /// with "omitted" past the limit and any "unreadable" document; none without the key.
    #[command(group = clap::ArgGroup::new("targets").multiple(true).required(true))]
    Lint {
        /// Task to check; repeatable.
        #[arg(group = "targets")]
        tasks: Vec<i64>,
        /// Proposal whose tasks to check; repeatable.
        #[arg(long = "proposal", group = "targets")]
        proposals: Vec<i64>,
    },
    /// Read proposals (the goals and tasks submitted together for plan review), or withdraw one.
    Proposal {
        #[command(subcommand)]
        command: ProposalCommand,
    },
    // ADR-t947-4.
    /// Cancel a draft, submitted or ready task, recording why. Does not satisfy its dependents.
    Cancel {
        id: i64,
        /// Why it is dropped, chosen by where its content is now: duplicate, already_done,
        /// absorbed or re_registered (each with --duplicate-of), superseded, not_worth,
        /// decision_moot, not_repo_work, stale, or other (with --note). Needed unless
        /// --duplicate-of alone says it; a code not listed is recorded as it is. answered_cancel is
        /// the runtime's, and unrecorded (how statistics count a cancel without a reason) is never
        /// recorded.
        #[arg(long)]
        reason: Option<String>,
        // ADR-0046.
        /// The task that holds its content: another task that exists and is not canceled.
        /// Alone, it records `duplicate`, or `already_done` when that task is completed; only
        /// those two make the pair a duplicate for `related`, `search`, `stats` and `kpi`. `show` and
        /// `list` report it with the reason, and `show` of that task lists the tasks naming it
        /// with their reasons.
        #[arg(long = "duplicate-of")]
        duplicate_of: Option<i64>,
        /// A few words on why, recorded with the reason; needed with --reason other.
        #[arg(long)]
        note: Option<String>,
    },
    /// Manage prerequisites; TASK depends on PREDECESSOR, or with --goal on a goal
    /// that must be closed as achieved first.
    Dependency {
        #[command(subcommand)]
        command: DependencyCommand,
    },
    /// Manage goals: the higher-level problems that groups of tasks solve.
    Goal {
        #[command(subcommand)]
        command: GoalCommand,
    },
    /// Record or correct a follow-up membership judgement against its source goal acceptance.
    ///
    /// `judge-follow-up TASK --classification required|out_of_scope|undecided --reason TEXT`.
    /// required needs at least one --acceptance-item, and required and out_of_scope at least one
    /// --evidence; the reason and each item and reference must not be blank. out_of_scope into a
    /// destination goal above low needs at least one --destination-acceptance-item. undecided may
    /// record why it could not be decided. A judgement is appended and the latest one is the
    /// current one.
    JudgeFollowUp {
        /// The follow_up task judged, in any status.
        task: i64,
        /// required, out_of_scope or undecided.
        #[arg(long)]
        classification: String,
        /// An item of the source goal's acceptance the follow_up bears on; repeatable. required needs one or more.
        #[arg(long = "acceptance-item")]
        acceptance_items: Vec<String>,
        /// Why; for undecided, why it could not be decided.
        #[arg(long)]
        reason: String,
        /// A reference the judgement rests on; repeatable. required and out_of_scope need one or more.
        #[arg(long = "evidence")]
        evidence: Vec<String>,
        /// For out_of_scope, the open goal other than the source goal the follow_up moves to; required belongs to the source goal and takes no other.
        #[arg(long = "destination-goal")]
        destination_goal: Option<i64>,
        /// For out_of_scope, an item of the destination goal's acceptance that needs the follow_up;
        /// repeatable. A destination above low needs one or more, and the judgement is refused
        /// without: a goal whose acceptance does not need it loses to the low deferral goal of the
        /// same tag (`goal list --tag T`). A low destination needs none. Kept on the judgement's
        /// row; plan review checks it against the destination's acceptance.
        #[arg(long = "destination-acceptance-item")]
        destination_acceptance_items: Vec<String>,
        /// The source goal, found from the evidence, when the registration did not record one; later judgements use the same source goal.
        #[arg(long = "source-goal")]
        source_goal: Option<i64>,
        /// The id of the latest judgement this one corrects; needed to change between required and out_of_scope (going back to undecided is refused).
        #[arg(long)]
        corrects: Option<i64>,
    },
    /// Move a draft, submitted or ready task to an open goal, or out of its goal with --none.
    SetGoal {
        /// Draft, submitted or ready task to move.
        task: i64,
        /// Open goal to join; omit it and pass --none to leave the current goal.
        #[arg(required_unless_present = "none", conflicts_with = "none")]
        goal: Option<i64>,
        /// Remove the task from its goal.
        #[arg(long)]
        none: bool,
    },
    /// Replace the paths a draft, submitted or ready task may change (`add --paths`), or remove the limit with --none.
    SetPaths {
        /// Draft, submitted or ready task.
        task: i64,
        /// Glob of a path the task may change; repeatable. Replaces every glob it had.
        #[arg(
            long = "paths",
            required_unless_present = "none",
            conflicts_with = "none"
        )]
        paths: Vec<String>,
        /// Declare no paths: runs may change anything.
        #[arg(long)]
        none: bool,
    },
    /// Replace fields of a draft or submitted task; a ready task is refused until it goes back
    /// (`draft`, or plan review's reopen to submitted). User or inbox may replace only --verify on an in_progress task after its latest run ended and
    /// no live run remains. Each given field replaces the old value; `task_edited` records old
    /// and new values and actor. A run already started keeps its prompt; the edit takes effect
    /// from the next claim.
    #[command(group = clap::ArgGroup::new("field").multiple(true).required(true))]
    Edit {
        /// Draft or submitted task, or an eligible in_progress task for --verify only.
        task: i64,
        #[arg(long, group = "field")]
        title: Option<String>,
        /// New description; a blank one is refused.
        #[arg(long, group = "field")]
        description: Option<String>,
        /// New acceptance criteria; a blank one is refused.
        #[arg(long, group = "field")]
        acceptance: Option<String>,
        /// Why the task exists and what to read first.
        #[arg(long, group = "field")]
        context: Option<String>,
        /// Verification command; repeatable. Replaces every command the task had.
        #[arg(long = "verify", group = "field", conflicts_with = "no_verify")]
        verification_commands: Vec<String>,
        /// Remove every verification command.
        #[arg(long, group = "field")]
        no_verify: bool,
        /// Required receipt check (`add --evidence`); repeatable. Replaces every check.
        #[arg(
            long = "evidence",
            group = "field",
            conflicts_with = "no_evidence",
            value_parser = ["tests", "e2e", "subagent_review"]
        )]
        required_evidence: Vec<String>,
        /// Require no receipt check.
        #[arg(long, group = "field")]
        no_evidence: bool,
        /// Glob of a path the task may change (`add --paths`); repeatable. Replaces every glob.
        #[arg(long = "paths", group = "field", conflicts_with = "no_paths")]
        paths: Vec<String>,
        /// Declare no paths: runs may change anything.
        #[arg(long, group = "field")]
        no_paths: bool,
        /// The kind of change the task makes (`add --change`): a lowercase label, one of
        /// `[tasks] changes` of dagq.toml when it names them.
        #[arg(long, group = "field")]
        change: Option<String>,
        /// The agent the worker runs on (`add --provider`): claude or codex. Without --headless,
        /// the worker takes that provider's default mode (headless for both) and the task names
        /// none.
        #[arg(long, group = "field", value_parser = PROVIDERS)]
        provider: Option<String>,
        /// Run the worker non-interactively (`add --headless`), named on the task.
        #[arg(long, group = "field", conflicts_with = "interactive")]
        headless: bool,
        // ADR-t1433-2.
        /// Refused, as `add --interactive` is: the interactive worker was retired.
        #[arg(long, group = "field")]
        interactive: bool,
        /// Claim it only once the supervisor's build contains its dependencies' landings
        /// (`add --wait-for-build`).
        #[arg(long, group = "field", conflicts_with = "no_wait_for_build")]
        wait_for_build: bool,
        /// Withdraw --wait-for-build: claim it as soon as its dependencies are completed.
        #[arg(long, group = "field")]
        no_wait_for_build: bool,
        /// Whether the run implements or investigates (`add --execution-class`): implementation
        /// or spike.
        #[arg(long, group = "field", value_parser = EXECUTION_CLASSES)]
        execution_class: Option<String>,
    },
    // ADR-t1639-1, ADR-t1850-1.
    /// Give a draft, submitted, ready or in-progress task a priority of its own (`add --priority`),
    /// or with --inherit let it inherit its goal's again. It takes effect at the next claim, or for
    /// an in-progress task at the next resume or recovery job of its run (the user and the inbox
    /// only), and never stops a running run.
    #[command(group = clap::ArgGroup::new("setting").required(true))]
    SetPriority {
        /// Draft, submitted, ready or in-progress task.
        task: i64,
        /// interrupt, urgent, high, normal or low.
        #[arg(value_parser = PRIORITIES, group = "setting")]
        level: Option<String>,
        /// Clear the task's own priority: it inherits its goal's (normal without a goal).
        #[arg(long, group = "setting")]
        inherit: bool,
    },
    // ADR-t1540-1.
    /// Give a draft a revisit time: when it comes, the runtime opens a planner for the draft
    /// again, even one kept by a keep_draft answer or one a person added, with the last decision
    /// about it in its prompt. Setting it again changes it; --clear removes it.
    #[command(group = clap::ArgGroup::new("when").required(true))]
    Revisit {
        /// Draft task.
        task: i64,
        /// The time, in RFC 3339 (`2026-10-04T12:00:00Z`, `2026-10-04T21:00:00+09:00`).
        #[arg(long, group = "when")]
        at: Option<String>,
        /// What the planner should look at then (shown in its prompt and by `show`).
        #[arg(long, requires = "at")]
        note: Option<String>,
        /// Remove the draft's revisit time.
        #[arg(long, group = "when")]
        clear: bool,
    },
    /// Record a note (an `observation` run event) on a task, a run or a goal.
    #[command(group = clap::ArgGroup::new("target").required(true))]
    Note {
        #[arg(long, group = "target")]
        task: Option<i64>,
        #[arg(long, group = "target")]
        run: Option<String>,
        #[arg(long, group = "target")]
        goal: Option<i64>,
        #[arg(long)]
        text: String,
        /// Lowercase slug classifying the note (default: note).
        #[arg(long)]
        kind: Option<String>,
    },
    /// List notes oldest first: the latest --limit, or the next --limit after --since.
    /// Prints {"notes", "cursor"}; pass `cursor` to --since for the notes recorded later.
    Notes {
        /// Only notes on this goal and on its tasks and their runs.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
        /// Only notes on this task and its runs.
        #[arg(long = "task")]
        task_id: Option<i64>,
        /// Event id (a previous `cursor`): only notes recorded after it.
        #[arg(long)]
        since: Option<i64>,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
    },
    // ADR-0051 decision 12.
    /// Record a change mark: a change the KPIs should be compared before and after (a setting,
    /// the operation, the host). With --retract, record that an earlier mark (a `dagq mark` or
    /// a `[run.env]` change) was none. Prints the mark. Not for the observer or a job.
    #[command(group = clap::ArgGroup::new("mark").required(true))]
    Mark {
        /// Short name of the change, e.g. "parallel 4→3" or "host arm64".
        #[arg(group = "mark")]
        label: Option<String>,
        /// What changed and why.
        #[arg(long, conflicts_with = "retract")]
        note: Option<String>,
        /// When the change took effect, for a mark made afterwards: an event id, `@<unix
        /// seconds>` or an RFC 3339 time. The mark itself is recorded now.
        #[arg(long, conflicts_with = "retract")]
        at: Option<dagq::domain::stats::Cursor>,
        /// Event id of the mark to retract.
        #[arg(long, group = "mark")]
        retract: Option<i64>,
    },
    // ADR-0051 decision 12.
    /// List the change marks oldest first by when they took effect: the recorded ones
    /// (supervisor start, handoff and stop, `[run.env]` changes, `dagq mark` and its
    /// retractions) and the ones derived from the claims (`derived:dagq_version`,
    /// `derived:claude_version`, `derived:codex_version`, `derived:parallel`,
    /// `derived:toolchain`). Reads only.
    Marks {
        /// Event id, `@<unix seconds>` or an RFC 3339 time: only marks after it.
        #[arg(long)]
        since: Option<dagq::domain::stats::Cursor>,
        /// Event id, `@<unix seconds>` or an RFC 3339 time: only marks at or before it.
        #[arg(long)]
        until: Option<dagq::domain::stats::Cursor>,
    },
    // ADR-t1394-1.
    /// Record a planning request for a planner of the runtime's, or decline one.
    Request {
        #[command(subcommand)]
        command: RequestCommand,
    },
    /// List the planning requests, oldest first: the open ones unless --all, or the one ID names.
    /// Prints {"requests"}.
    Requests {
        /// Only this request, whatever its status.
        id: Option<i64>,
        /// Include the proposed, declined and exhausted ones.
        #[arg(long)]
        all: bool,
    },
    // ADR-t1879-1.
    /// Hold a task so that the runtime claims it, resumes its run or starts a recovery job of
    /// it no more until a person releases it (only a person at a plain terminal and the
    /// inbox). TARGET is a task ID, or a run ID for its task. The task keeps its status; a
    /// turn of a run already going is not stopped. Prints {"hold", "state"}.
    Hold {
        target: String,
        /// Why, in the person's words.
        #[arg(long)]
        reason: String,
        /// When to remind a person of the hold (`@<unix seconds>` or an RFC 3339 time); passing
        /// it releases nothing.
        #[arg(long)]
        until: Option<dagq::domain::stats::Cursor>,
    },
    /// Release the hold of a task (TARGET: a task ID, or a run ID for its task), refused while
    /// the step that went when it was held still goes (hold_not_settled). A run that stopped
    /// (failed or interrupted) goes on by the recovery result the hold kept when there is one
    /// and neither --provider nor --model is given, and ends otherwise. A waiting run goes on
    /// in its session, and ends when it has no session or is given another --provider. When
    /// the run ends, the next run carries its branch (and its uncommitted work, saved as a
    /// commit) over. Prints {"hold", "continuation", "decision", ...}.
    Release {
        target: String,
        /// The provider of the task's later turns and runs.
        #[arg(long)]
        provider: Option<String>,
        /// The model of the task's later turns and runs (one the provider takes; codex takes
        /// none).
        #[arg(long)]
        model: Option<String>,
    },
    // ADR-t1521-1, ADR-t1521-2.
    /// Ask to replan a long-running run (a person's request, or a worker's proposal on its own
    /// run): the runtime diagnoses it and, only when it replans, parks the run at a safe boundary
    /// and saves its work before a planner of the runtime's makes the replacement.
    Replan {
        #[command(subcommand)]
        command: ReplanCommand,
    },
    /// List the replanning operations, oldest first: the open ones unless --all, or the one ID
    /// names, each with the copy of its task's conditions, its diagnosis and its outcome; the
    /// one ID names also with its latest replacement plan and that plan's problems now.
    /// Prints {"replans"}.
    Replans {
        /// Only this operation, whatever its status.
        id: Option<i64>,
        /// Include the ended ones.
        #[arg(long)]
        all: bool,
    },
    // ADR-t1728-1.
    /// Ask for an eval of a review agent, which the supervisor runs, or read the rounds.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    // ADR-t1920-1.
    /// The landing branch's CI as the supervisor's [ci_watch] reads it.
    Ci {
        #[command(subcommand)]
        command: CiCommand,
    },
    // ADR-0044 decision 18.
    /// Record a finding, or resolve or dismiss one.
    Finding {
        #[command(subcommand)]
        command: FindingCommand,
    },
    /// List findings: the open and proposed ones unless --all or --status says otherwise, larger
    /// impact first (then more occurrences, then the latest seen). Each has its proposal's status
    /// and its open asks. Prints {"findings"}.
    #[command(group = clap::ArgGroup::new("target"))]
    Findings {
        /// Only this finding, whatever its status.
        id: Option<i64>,
        /// Include resolved and dismissed findings.
        #[arg(long, conflicts_with = "status")]
        all: bool,
        /// Only these statuses (comma-separated): open, proposed, resolved, dismissed.
        #[arg(long, value_delimiter = ',', value_parser = FINDING_STATUSES)]
        status: Vec<String>,
        /// Only these kinds (comma-separated).
        #[arg(long = "kind", value_delimiter = ',')]
        kinds: Vec<String>,
        /// Only findings on this task.
        #[arg(long, group = "target")]
        task: Option<i64>,
        /// Only findings on this run.
        #[arg(long, group = "target")]
        run: Option<String>,
        /// Only findings on this goal.
        #[arg(long, group = "target")]
        goal: Option<i64>,
        /// Only findings on the queue as a whole.
        #[arg(long, group = "target")]
        queue: bool,
        /// Include the evidence events in full.
        #[arg(long)]
        full: bool,
    },
    // ADR-0046.
    /// Full-text search of tasks (title, description, acceptance, context), goals (title,
    /// description, acceptance, constraints), notes and the messages of landed commits, in every
    /// status. QUERY is words (all must match; `"..."` for a phrase) with FTS5's AND, OR, NOT and
    /// parentheses; any substring of 3 or more characters matches, including in Japanese, and
    /// shorter terms must all be present. Prints {"hits", "total"}, best first: per hit its kind,
    /// id (a task, goal or note event ID, or a commit SHA), status, title and the matching field
    /// with an excerpt marking the match with « ».
    Search {
        query: String,
        /// Only these statuses (comma-separated): a task's (draft, submitted, ready, in_progress,
        /// completed, canceled) or a goal's (draft, open, achieved, abandoned); a note or commit
        /// has the status of its task or goal.
        #[arg(long, value_delimiter = ',')]
        status: Vec<String>,
        /// Only these kinds (comma-separated): task, goal, note, commit.
        #[arg(long = "kind", value_delimiter = ',', value_parser = ["task", "goal", "note", "commit"])]
        kinds: Vec<String>,
        /// Only this goal, its tasks and their notes and commits.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
        /// Include every searched field in full and the bm25 score.
        #[arg(long)]
        full: bool,
    },
    // ADR-0046 decision 4.
    /// The tasks most related to TASK (any status, drafts included), scored by fixed rules:
    /// declared paths that overlap; file names, test names (snake_case of three or more words,
    /// `--test NAME`), ADR numbers and task numbers in the texts and landed commit messages;
    /// follow-ups of the same run; the same goal; and how strongly the search index matches
    /// TASK's title against the other task. A clue many tasks share counts less. Prints
    /// {"task_id", "related", "total"}, best first: per task its id, status, title, score, the
    /// clues that scored it ({"clue", "value", "weight"}) and `duplicate_of` when it was canceled
    /// as a duplicate.
    Related {
        task_id: i64,
        /// Only these statuses (comma-separated): draft, submitted, ready, in_progress,
        /// completed, canceled.
        #[arg(long, value_delimiter = ',')]
        status: Vec<TaskStatus>,
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
    },
    /// List ready tasks whose prerequisites are all completed, whose goal dependencies are all
    /// closed as achieved and whose goal is not a draft, in claim order (highest
    /// `effective_priority`, then most `unblocks`, then lowest ID) less those whose claim the
    /// supervisor last recorded as deferred (`candidates`); the deferred ones apart with the
    /// recorded reason, files, runs and since (`deferred`), and what holds every claim now as the
    /// supervisors recorded it, or `no_supervisor` (`held`). The deferrals are the supervisor's
    /// last judgment, kept while it does not judge again (full slots, a hold), the class and
    /// investigation limits' among them: a Spike held while Spikes fill `[supervisor]
    /// max_spikes` shows `spike_limit`, and one that would open an investigation past
    /// `max_investigations` shows `investigation_limit` once the Spike limit lets Spikes go.
    /// The ready tasks that never enter the order are counted by why, with their lowest IDs
    /// (`excluded`: `draft_goal`, `dependencies`, `goal_dependencies`). Does not claim.
    Candidates {
        /// Keep the deferred tasks in `candidates`: the claim rule's order alone, with no class
        /// aging applied.
        #[arg(long)]
        ignore_deferrals: bool,
    },
    // ADR-0077.
    /// Show the unfinished tasks' dependencies: per task its direct predecessors (`depends_on`),
    /// its goal dependencies (`goal_dependencies`), what it still waits for (`ready_after`:
    /// unfinished predecessors, then `{"goal": ID}` for goals not closed as achieved), the tasks it
    /// blocks directly (including those waiting for its open goal) and how many it releases
    /// transitively (`unblocks`), its `priority` and the `effective_priority` it inherits from the
    /// ready tasks waiting for it; `candidates` in claim order less the deferred tasks (as
    /// `candidates` shows them, the class and investigation limits' deferrals included),
    /// `deferred` and the `critical` chain. With `--format d2` or `svg`,
    /// the near-term dependency diagram instead: the d2 source, or the SVG the host's `d2
    /// --layout=tala` draws from it.
    Graph {
        /// Only this goal's tasks and candidates; counts still span every goal. With `d2` or
        /// `svg`, only the tasks drawn for being in progress or of high priority are narrowed to
        /// the goal: its critical chain and the prerequisites one level up are drawn even outside
        /// it, each in its own goal's frame.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
        /// `json` (the dependency view), `d2` (the diagram's source) or `svg` (drawn by d2 and TALA
        /// from PATH).
        #[arg(long, default_value = "json", value_parser = ["json", "d2", "svg"])]
        format: String,
        /// Write the d2 source or the SVG to this file instead of stdout, and print
        /// `{"format", "out", "tasks"}` (the IDs of the tasks drawn). Refused with `--format json`.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Run and monitor tasks in parallel until interrupted. Run this in a dedicated terminal.
    Supervise {
        /// Never start Claude. Run workers on Codex and handle unsupported roles manually.
        #[arg(long)]
        no_claude: bool,
        /// Checkout of the repository whose `main` becomes the base commit;
        /// defaults to the working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Maximum number of runs executing at once. Without it, `parallel`
        /// of `[supervisor]` in the main checkout's dagq.toml (read again
        /// each pass), else 4.
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        parallel: Option<u16>,
        // ADR-0062.
        /// Maximum number of runs waiting for a person's answer outside the
        /// --parallel slots; 0 keeps every run in its slot. Without it,
        /// `max_waiting` of `[supervisor]` in dagq.toml, else 4.
        #[arg(long)]
        max_waiting: Option<u16>,
        /// Claim no new run while the host's 1-minute load average is above
        /// this; the runs in flight go on. 0 disables the hold. Without it,
        /// twice the host's logical CPUs (16 when they cannot be read).
        #[arg(long)]
        max_load: Option<f64>,
        /// Exit once no run is active and no task can be claimed, instead of
        /// waiting for new work.
        #[arg(long)]
        once: bool,
        #[command(flatten)]
        cmux: RetiredCmux,
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
        // ADR-t813-2.
        /// Codex CLI the Codex workers start; a bare name is resolved on PATH. When it is not
        /// found, the supervisor goes on and starts Codex tasks with non-interactive Claude
        /// (provider_switched, reason executable_missing).
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        /// Write this start's JSON Lines log (supervise-<UTC time>-<pid>.jsonl)
        /// into this directory (created if missing) instead of the queue's
        /// logs/; messages also go to stderr. Logs older than 14 days are pruned from it
        /// at start. A directory that cannot be made leaves the logs on stderr alone.
        #[arg(long)]
        log_dir: Option<PathBuf>,
        /// Start the observer job (`observe`) when this many seconds passed
        /// since the last one started or finished; 0 disables the observer.
        /// Default 10800 (3 hours), or 0 with --once.
        #[arg(long)]
        observe_interval: Option<u64>,
        /// Also run the daily observation of the last 24 hours once a day.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        observe_daily: bool,
        // ADR-t996-1, ADR-t1172-1.
        /// Start the throughput reviews: each hour one of the last hour, each day one of yesterday
        /// and each week one of the ISO week before, saved under the queue's reports/reviews/ and
        /// told to the inbox but for an hour the runtime's rules find quiet. Default true, or false
        /// with --once.
        #[arg(long, action = clap::ArgAction::Set)]
        throughput_review: Option<bool>,
        // ADR-0051 decision 20.
        /// Write the KPI reports of each finished day and ISO week under the queue's reports/
        /// on the first pass after local midnight.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        report_daily: bool,
        // ADR-0070.
        /// Record the forecast of every open task and goal as one event when a plan review
        /// passes, on a change mark, after a landing that moved it and once a day.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        forecast_snapshots: bool,
        /// Record the host's load (load average, CPU and memory per kind of process, memory,
        /// swap, pageouts) every this many seconds under the queue's host/metrics-YYYYMMDD.csv;
        /// 0 records none.
        #[arg(long, default_value_t = dagq::domain::host_metrics::DEFAULT_INTERVAL_SECS)]
        host_metrics_interval: u64,
        /// Keep the host's load files of this many local days, today's included; 0 removes
        /// none.
        #[arg(long, default_value_t = dagq::domain::host_metrics::DEFAULT_RETENTION_DAYS)]
        host_metrics_retention_days: u32,
        /// Maximum number of planners the runtime opens at once for drafts, findings and
        /// proposals plan review sent back (apart from --parallel; planners a person opened do
        /// not count). Without it, `runtime_planners` of `[supervisor]` in the main checkout's
        /// dagq.toml (read again each pass), else 1.
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        runtime_planners: Option<u16>,
        /// Seconds a planner may take to submit a proposal sent back to it before the inbox is
        /// told.
        #[arg(long, default_value_t = 3600)]
        planner_timeout: u64,
        /// Claude Code plugin directory the planners the runtime opens load.
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
        // ADR-0045 decision 10.
        /// Continue the registered supervisor with this token after it exec'd
        /// this binary; set by the handoff.
        #[arg(long, hide = true)]
        handoff_token: Option<String>,
        // ADR-0051 decision 10.
        /// How this process was started, for its registration's mode and its start mark;
        /// the LaunchAgent `up` writes gives `launchd`, whether `up` or launchd starts it.
        /// `in_cmux` comes only from the argv of a supervisor an earlier binary
        /// started in the retired in-cmux mode.
        #[arg(long, hide = true)]
        mode: Option<String>,
        // ADR-0045 decision 17.
        /// Register with the automatic update on: build and install the runtime of every landing
        /// on main that changes it. `up --auto-update` starts it so.
        #[arg(long)]
        auto_update: bool,
        /// Seconds between two looks at main for the automatic update.
        #[arg(long, default_value_t = 30)]
        update_interval: u64,
        /// A shell command the automatic update runs in place of `cargo build --release --locked -p
        /// dagq` (tests); it must leave the binary at $CARGO_TARGET_DIR/release/dagq.
        #[arg(long, hide = true)]
        update_build_command: Option<String>,
        // ADR-t963-1 decision 1.
        /// A shell command the automatic update runs in place of its e2e gate (`cargo test --locked
        /// --test e2e -- --ignored`; tests).
        #[arg(long, hide = true)]
        update_e2e_command: Option<String>,
        /// Seconds the automatic update's e2e gate may run before it counts as failed.
        #[arg(long, hide = true)]
        update_e2e_timeout: Option<u64>,
        /// The cargo the release update's job installs a release with (tests give a stub).
        #[arg(long, hide = true)]
        update_cargo: Option<PathBuf>,
        /// Milliseconds the automatic update's job waits between two looks at the handoff and
        /// at the new supervisor's heartbeat (tests; the job's default is 500).
        #[arg(long, hide = true)]
        update_poll_ms: Option<u64>,
        // task 1048.
        /// Milliseconds between two heartbeats of the registration and the leases (tests; 2000
        /// by default).
        #[arg(long, hide = true)]
        heartbeat_interval_ms: Option<u64>,
        /// Milliseconds between two looks for work while no run is active (tests; 2000 by
        /// default).
        #[arg(long, hide = true)]
        idle_poll_ms: Option<u64>,
        /// Milliseconds between two passes while a run or a job is active (tests; 1000 by
        /// default).
        #[arg(long, hide = true)]
        tick_ms: Option<u64>,
    },
    // ADR-0045 decision 17.
    /// The automatic update's job, which the supervisor starts: build main's COMMIT in the queue's
    /// update checkout, check it, put it in place of --to like `install` and watch the supervisor
    /// take it; on a failure put the old binary back, start the supervisor again when it is gone,
    /// and open the `update_failed` ask.
    #[command(hide = true)]
    AutoUpdate {
        #[arg(long)]
        commit: String,
        /// The supervisor that started the job.
        #[arg(long)]
        token: String,
        /// The fixed binary to replace.
        #[arg(long)]
        to: PathBuf,
        /// A checkout of the repository.
        #[arg(long)]
        repo: PathBuf,
        /// Where the build's output is appended.
        #[arg(long)]
        log: PathBuf,
        #[arg(long)]
        build_command: Option<String>,
        // ADR-t963-1 decision 1.
        /// A shell command in place of the e2e the build passes before it is put in place (`cargo
        /// test --locked --test e2e -- --ignored`).
        #[arg(long)]
        e2e_command: Option<String>,
        /// Seconds the e2e may run before it counts as failed.
        #[arg(long, default_value_t = 1800)]
        e2e_timeout: u64,
        #[command(flatten)]
        cmux: RetiredCmux,
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
        /// Seconds the supervisor may take to exec the new binary.
        #[arg(long, default_value_t = 1800)]
        handoff_timeout: u64,
        /// Seconds the new supervisor may take to heartbeat on.
        #[arg(long, default_value_t = 60)]
        watch_timeout: u64,
        // task 1048.
        /// Milliseconds between two looks at the handoff and at the new supervisor's heartbeat
        /// (tests shorten it).
        #[arg(long, hide = true, default_value_t = 500)]
        poll_ms: u64,
    },
    // ADR-t618-1 decision 5.
    /// The release update's job, which a supervisor of a release build starts: `cargo install`
    /// RELEASE under the queue's update directory, check it, put it in place of --to like
    /// `install` and watch the supervisor take it; on a failure put the old binary back, start
    /// the supervisor again when it is gone, and open the `update_failed` ask.
    #[command(hide = true)]
    ReleaseUpdate {
        #[arg(long)]
        release: String,
        /// The supervisor that started the job.
        #[arg(long)]
        token: String,
        /// The binary to replace.
        #[arg(long)]
        to: PathBuf,
        /// Where cargo's output is appended.
        #[arg(long)]
        log: PathBuf,
        #[arg(long, default_value = "cargo")]
        cargo: PathBuf,
        #[command(flatten)]
        cmux: RetiredCmux,
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
        /// Seconds the supervisor may take to exec the new binary.
        #[arg(long, default_value_t = 1800)]
        handoff_timeout: u64,
        /// Seconds the new supervisor may take to heartbeat on.
        #[arg(long, default_value_t = 60)]
        watch_timeout: u64,
        // ADR-t618-2 decision 4.
        /// Only bring the installed claude-dagq plugin to RELEASE, the binary being it
        /// already.
        #[arg(long)]
        plugin_only: bool,
    },
    // ADR-0044 decision 4, ADR-t1222-1.
    /// Run the observer job once: a headless agent under DAGQ_ROLE=observer (Claude by default, or
    /// Codex in its read-only sandbox when [roles.observer] of dagq.toml says provider = "codex";
    /// the supervisor passes the provider it routed the observer to) reads stats past the cursor,
    /// the open findings, the latest notes, the open asks and the graph, and writes findings and
    /// blocked asks on them only, through the queue service. Records observe_started /
    /// observe_finished and saves the new cursor. observe_finished counts the failures in a row
    /// (consecutive_failures); the second reaches the inbox (next: check the failed observer). When
    /// no event but the observer's own came since the last observation, starts no agent and records
    /// observe_finished with outcome skipped (unless --since is given). The agent loads no MCP
    /// server.
    Observe {
        /// List the past observations instead, newest first: the events each read, the findings and asks it
        /// wrote, how long it took, whether it was skipped and what its prompt came to.
        #[arg(long, group = "observe_read")]
        history: bool,
        /// Read the whole input of the observation whose directory is named (`dir` of --history) instead:
        /// what its prompt left out. Without --section, lists its sections with their bytes.
        #[arg(long, group = "observe_read", value_name = "OBSERVATION")]
        input: Option<String>,
        /// With --input, the section to read: a path of keys joined by `.` (stats.runs, kpi.breaches).
        #[arg(long, requires = "input")]
        section: Option<String>,
        /// With --input, the first item of a list section to read.
        #[arg(long, requires = "input", default_value_t = 0)]
        offset: usize,
        /// With --history, how many observations to list; with --input, how many items of a list
        /// section to read (at most 24000 bytes of them).
        #[arg(long, requires = "observe_read")]
        limit: Option<usize>,
        /// Event id to read stats past; defaults to the cursor the last observe saved
        /// (<queue dir>/observer/cursor), or with --daily the last event 24 hours ago.
        #[arg(long)]
        since: Option<i64>,
        /// Print the prompt instead of starting the agent.
        #[arg(long)]
        dry_run: bool,
        /// The daily observation: trends over the last 24 hours; leaves the cursor alone.
        #[arg(long)]
        daily: bool,
        /// Seconds the agent may run before it is killed.
        #[arg(long, default_value_t = 1800)]
        timeout: u64,
        /// Claude Code executable, for an observer on Claude; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
        /// Codex CLI executable, for `[roles.observer]`'s `provider = "codex"`; a bare name is
        /// resolved on PATH outside a terminal's shims.
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        #[command(flatten)]
        cmux: RetiredCmux,
        /// What the agent starts with, as the supervisor routed it (an actor launch as JSON);
        /// `[roles.observer]` of dagq.toml without.
        #[arg(long, hide = true)]
        launch: Option<String>,
        /// Codex's home, whose rollouts name the model (tests); Codex's own without.
        #[arg(long, hide = true)]
        codex_home: Option<PathBuf>,
        /// The role names its provider: a Codex observation that cannot use Codex records
        /// provider_unusable for the supervisor to start it again on the other provider.
        #[arg(long, hide = true)]
        switchable: bool,
        /// [provider_fallback] jobs is false: a Claude observation of a role that names its
        /// provider that cannot use Claude records provider_unusable too, for the supervisor to
        /// start it again on Claude once Claude's hold ends.
        #[arg(long, hide = true)]
        no_provider_fallback: bool,
        /// No provider can run the observation (--no-claude, Codex not usable): one that is not
        /// skipped records its finish as an error with this reason.
        #[arg(long, hide = true)]
        unavailable: Option<String>,
    },
    // ADR-t996-1, ADR-t1172-1.
    /// Run the throughput review once: for the last whole hour (--mode hourly), yesterday (daily)
    /// or the ISO week before this one (weekly). Every hour is reviewed: the rules the runtime's
    /// judgment of the hour met head its review, and only an hour that met one reaches the inbox. A
    /// headless agent under DAGQ_ROLE=throughput-review-job, which may only read, follows the
    /// weekly review of the dagq skill's reference/kpi.md: on Claude by default, or on Codex in its
    /// read-only sandbox when [roles.throughput_review] of dagq.toml says provider = "codex" (the
    /// supervisor passes the provider it routed the review to). The review is saved under <queue
    /// dir>/reports/reviews/ and its conclusion reaches the inbox as throughput_review_reported
    /// (next: report the review), but for a quiet hour's, which is only saved. A weekly next move
    /// becomes a finding marked for a proposal. A review that fails or cannot start reaches the
    /// inbox as its throughput_review_finished (next: check the failed review), except one whose
    /// Codex could not be used (provider_unusable), which the supervisor reviews again on the other
    /// provider. The prompt carries a summary of the inputs; the whole is input.json in the
    /// review's directory. The agent loads no MCP server; the supervisor starts this on its timer.
    ThroughputReview {
        #[arg(long, default_value = "hourly", value_parser = ["hourly", "daily", "weekly"])]
        mode: String,
        /// The unix second whose latest finished period is reviewed; now by default.
        #[arg(long)]
        at: Option<i64>,
        /// The time zone the hours, days and weeks begin in, in seconds east of UTC; the host's by
        /// default.
        #[arg(long, allow_hyphen_values = true)]
        utc_offset: Option<i64>,
        /// Print the prompt instead of starting the agent, whatever the rules made of the hour.
        #[arg(long)]
        dry_run: bool,
        /// Seconds the agent may run before it is killed.
        #[arg(long, default_value_t = 1800)]
        timeout: u64,
        /// Claude Code executable, for a review on Claude; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
        /// Codex CLI executable, for `[roles.throughput_review]`'s `provider = "codex"`; a bare
        /// name is resolved on PATH outside a terminal's shims.
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        /// What the job starts with, as the supervisor routed it (an actor launch as JSON);
        /// `[roles.throughput_review]` of dagq.toml without. Its executable that cannot be
        /// resolved (gone since the supervisor found it) is tried as given, so the review
        /// records its finish as the provider not usable (`executable_missing`) instead of the
        /// command failing with no record of the period.
        #[arg(long, hide = true)]
        launch: Option<String>,
        /// Codex's home, whose rollouts name the model (tests); Codex's own without.
        #[arg(long, hide = true)]
        codex_home: Option<PathBuf>,
        /// The role names its provider: a Codex job that cannot use Codex records
        /// provider_unusable for the supervisor to start it again on the other provider.
        #[arg(long, hide = true)]
        switchable: bool,
        /// [provider_fallback] jobs is false: a Claude job of a role that names its provider
        /// that cannot use Claude records provider_unusable too, for the supervisor to start the
        /// period again on Claude once Claude's hold ends.
        #[arg(long, hide = true)]
        no_provider_fallback: bool,
        /// No provider can run the job (--no-claude, Codex not usable): a period that needs a
        /// review records its failure with this reason.
        #[arg(long, hide = true)]
        unavailable: Option<String>,
    },
    /// Start the queue's runtime: a launchd-resident supervisor. Idempotent; a live supervisor of another build is handed over to this binary without waiting for its sessions (or drained when it cannot take a handoff), after the queue's compatible migrations. Opens no inbox (`dagq inbox` does, in your own terminal) and no planner (the runtime does), and forgets the workspaces an earlier binary recorded.
    Up {
        /// Never start Claude. Run workers on Codex and handle unsupported roles manually.
        #[arg(long)]
        no_claude: bool,
        /// Maximum number of runs the supervisor executes at once. Passed
        /// to the supervisor only when given; without it, `parallel` of
        /// `[supervisor]` in the main checkout's dagq.toml, else 4.
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        parallel: Option<u16>,
        // ADR-0062.
        /// Maximum number of runs the supervisor keeps waiting for a person's
        /// answer outside the --parallel slots; 0 keeps every run in its slot.
        /// Passed only when given; without it, `max_waiting` of `[supervisor]`
        /// in dagq.toml, else 4.
        #[arg(long)]
        max_waiting: Option<u16>,
        /// Maximum number of planners the supervisor's runtime opens at
        /// once (drafts, findings and revised proposals). Passed only when
        /// given; without it, `runtime_planners` of `[supervisor]` in
        /// dagq.toml, else 1.
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        runtime_planners: Option<u16>,
        /// The supervisor claims no new run while the host's 1-minute load
        /// average is above this; the runs in flight go on. 0 disables the
        /// hold. Passed only when given; without it, the supervisor holds
        /// at twice the host's logical CPUs.
        #[arg(long)]
        max_load: Option<f64>,
        /// Retired: refused with the way to move a supervisor still running
        /// in a cmux workspace to launchd (`down --wait`, then `up`).
        #[arg(long, hide = true)]
        in_cmux: bool,
        /// Do not wait for a supervisor that cannot take a handoff to drain:
        /// stop with an error instead while it still leases a run, at any
        /// stage (review, revise, resume, e2e, landing).
        #[arg(long)]
        no_wait: bool,
        /// Seconds a supervisor asked to hand off may take to come back
        /// under this binary (it finishes a validation or landing in
        /// progress first).
        #[arg(long, default_value_t = 1800)]
        handoff_timeout: u64,
        /// Claude Code plugin directory the runtime's planners load (`claude --plugin-dir`).
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
        /// Checkout of the repository; defaults to the working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        #[command(flatten)]
        cmux: RetiredCmux,
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
        // ADR-t813-2.
        /// Codex CLI the supervisor's Codex workers start; a bare name is resolved on PATH and
        /// fixed on the supervisor like --claude. When it is not found, `up` goes on and the
        /// supervisor starts Codex tasks with non-interactive Claude (provider_switched, reason
        /// executable_missing).
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        // ADR-0045 decision 17.
        /// Have the supervisor build and install the runtime of every landing on main that changes
        /// it, in the queue's own checkout, and hand itself over to it. Kept on the registration;
        /// an `up` without it turns it off.
        #[arg(long)]
        auto_update: bool,
    },
    // ADR-t2159-1 decision 2, ADR-t1181-1 decision 1.
    /// Open the queue's inbox in this terminal: start the inbox's agent (Claude Code with the
    /// inbox's settings, the plugin and the inbox's prompt, DAGQ_ROLE=inbox and DAGQ_QUEUE) in the
    /// foreground in place of this process, from the repository's main checkout, after recording
    /// inbox_opened. Its provider and model are [roles.inbox] of dagq.toml, overridden for this
    /// start by --provider and --model. Refused inside an inbox (DAGQ_ROLE=inbox), whatever queue.
    Inbox {
        /// Claude Code plugin directory the inbox loads (`claude --plugin-dir`); without it the
        /// installed claude-dagq plugin, which must be enabled.
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
        /// Checkout of the repository; defaults to the working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
        /// Codex CLI executable, for a Codex inbox; a bare name is resolved on PATH. A Codex
        /// inbox is not opened yet: it is refused with why until its records are implemented.
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        /// The inbox's provider for this start (claude or codex) over [roles.inbox]'s; without
        /// either, Claude.
        #[arg(long, value_parser = ["claude", "codex"])]
        provider: Option<String>,
        /// The inbox's model for this start, a name of its provider's, over [roles.inbox]'s.
        #[arg(long)]
        model: Option<String>,
    },
    // ADR-t1394-1.
    /// Refused: planners a person opens were abolished. Ask the inbox for a plan instead; it
    /// records a planning request (`request add`) the runtime opens a planner for. Prints the
    /// guidance and opens nothing.
    Plan {
        /// What an older `dagq plan` took (`--plugin-dir`, `--repo`, `--cmux`, `--claude`):
        /// accepted and ignored, so the refusal shows its guidance.
        #[arg(hide = true, trailing_var_arg = true, allow_hyphen_values = true)]
        ignored: Vec<String>,
    },
    /// List the planner sessions not closed, each with its state (opening, working, idle, exited,
    /// lost, closed), whether it is alive and since when it is idle. Reads only.
    Planners {
        /// Include closed planners.
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        cmux: RetiredCmux,
    },
    // ADR-t1228-1.
    /// A run's session, named by its run id or its task id (the task's latest
    /// run): read its screen, or send it a key of a fixed set or the answer of
    /// an answered ask; or close the workspaces ended runs left open. Each is
    /// recorded with its actor.
    Run {
        #[command(subcommand)]
        command: RunCommand,
    },
    // ADR-t1433-2, ADR-t1533-1.
    /// A planner's session, named by its planner id: `screen` says it has no
    /// screen and where its turns are, and `send` is refused; `log` prints a
    /// background planner's log; `request` hands a headless planner of the
    /// runtime's a follow-up request as its next turn, recorded with its actor.
    Planner {
        #[command(subcommand)]
        command: PlannerCommand,
    },
    /// Stop the queue's supervisor: unload its launchd agent so it drains and is not restarted, or signal one an earlier binary registered in the retired in-cmux mode, whose terminal is left to a person. Leaves the inbox alone.
    Down {
        /// Wait until the supervisor's registration is gone or its process exited.
        #[arg(long)]
        wait: bool,
        /// Kill the supervisor after the unload and drop its registration.
        #[arg(long, conflicts_with = "wait")]
        force: bool,
        #[command(flatten)]
        cmux: RetiredCmux,
    },
    /// Land a validated run on main: rebase, re-validate, squash into one commit, complete the task, then push main to origin.
    Integrate {
        /// Task whose run awaits integration or comes back from a session.
        #[arg(required_unless_present = "next", conflicts_with = "next")]
        id: Option<i64>,
        /// Land the oldest run awaiting integration instead of naming a task.
        #[arg(long)]
        next: bool,
        /// Checkout of the repository to land in; defaults to the working directory.
        /// Must be the repository the queue is bound to.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Do not push the landing branch to its remote (recorded as push_skipped).
        #[arg(long)]
        no_push: bool,
    },
    /// Write the review material of the task's run awaiting integration or a session to <run_dir>/review.md and report its path and diff size; the diff itself is only in the file.
    Review {
        /// Task whose run awaits integration or comes back from a session.
        id: i64,
    },
    /// List supervisors, unfinished runs, what waits for a person (attention), the open asks, the held tasks and the event cursor, without changing anything.
    /// The cursor is read before the state, so `watch --after <cursor>` misses no change after it
    /// (it may repeat one).
    Status {
        /// Only the attention addressed to this role: inbox gets all of it, planner none.
        #[arg(long, value_parser = ROLES)]
        role: Option<String>,
    },
    /// Register a question for a person about a task or one of its runs; prints the ask.
    /// An open ask of the same task, run and kind is returned instead (`created: false`).
    /// Nothing is notified here: the inbox's `watch --role inbox` tells the person of the new ask.
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Ask {
        #[command(subcommand)]
        command: Option<AskCommand>,
        #[arg(long, required = true, value_parser = ["approve_landing", "answer_prompt", "decide", "worker_question", "planner_question", "blocked"])]
        kind: Option<String>,
        #[arg(long, required = true)]
        question: Option<String>,
        /// A choice to offer; repeat for several.
        #[arg(long = "option")]
        options: Vec<String>,
        // ADR-0047 decision 41.
        /// Why a person is needed: scope (the acceptance, the scope, an ADR or a goal's decision),
        /// discard (whether to throw work away) or recovery_failed. Authentication and cost asks
        /// are the runtime's, one per queue. A question that fits none of them is no ask: decide it
        /// yourself, or leave it as a note (`dagq note`).
        #[arg(long = "because", required = true, value_parser = ["scope", "discard", "recovery_failed", "authentication", "cost"])]
        because: Option<String>,
        // ADR-t947-2.
        /// What a worker_question left undecided; repeat for several, the first the primary one
        /// (what stopped you first), the rest what must be decided with it. Required for a
        /// worker_question and refused on any other kind. One of: discard_work (throw away or
        /// redo the work), adr_conflict (the task contradicts an accepted ADR, a design or a
        /// person's decision), acceptance_conflict (two criteria cannot both hold),
        /// acceptance_infeasible (a fact keeps a criterion from being met), out_of_scope_change
        /// (a change outside the task's paths or description is needed), task_overlap (another
        /// task or a landed change overlaps), precondition_missing (what the work starts from is
        /// not there yet), host_environment (the host's tools or settings block it),
        /// design_choice (an implementation choice, yours to decide) or other; the heavier first
        /// when two came at once. A code outside the list is kept as given.
        #[arg(long = "topic")]
        topics: Vec<String>,
        // ADR-t451-1 decisions 1 and 2.
        /// The option you recommend: one of the ask's --option texts (or `propose` / `dismiss`,
        /// which the runtime adds to a blocked ask about a finding). Required on a blocked ask,
        /// optional on every other kind; the inbox shows it to the person next to the options.
        #[arg(long = "recommend")]
        recommend: Option<String>,
        // ADR-t451-1 decision 1.
        /// How sure you are of the judgement behind the ask: high or low.
        #[arg(long, value_parser = ["high", "low"])]
        confidence: Option<String>,
        /// Task the ask is about. Only a blocked ask, or a planner_question about a finding, may
        /// name neither a task nor a run.
        #[arg(long = "task", conflicts_with = "run")]
        task_id: Option<i64>,
        /// Run the ask is about (its task is implied).
        #[arg(long)]
        run: Option<String>,
        // ADR-0044 decisions 19 and 23.
        /// Finding a blocked ask raises; one ask per finding stays open. A blocked ask about a
        /// finding also offers `propose` and `dismiss`, which the runtime applies to the
        /// finding. A planner_question names the finding its planner was opened for.
        #[arg(long)]
        finding: Option<i64>,
        // ADR-t1394-1 decision 7.
        /// Planning request a planner_question is about: the one its planner was opened
        /// for.
        #[arg(long, conflicts_with_all = ["task_id", "run", "finding"])]
        request: Option<i64>,
        #[command(flatten)]
        cmux: RetiredCmux,
    },
    /// Write the answer of an open ask; the inbox then sees ask_answered unless the runtime applies it.
    Answer {
        id: i64,
        #[arg(long)]
        text: String,
    },
    /// List asks nobody closed, oldest first.
    Asks {
        /// Only the unanswered ones.
        #[arg(long)]
        open: bool,
        /// Only the ones this role acts on: inbox answers open asks and reads the answers, planner none.
        #[arg(long, value_parser = ROLES)]
        role: Option<String>,
        /// Include closed asks.
        #[arg(long)]
        all: bool,
    },
    /// Print the run events after a cursor, oldest first: attention events only unless --all. Reads only.
    Events {
        /// Event id to read past (the `cursor` of `status`, `events` or `watch`).
        #[arg(long, default_value_t = 0)]
        after: i64,
        /// Maximum number of events returned; the cursor then points at the last one.
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
        /// Every event kind, not only attention. Without it and --kind, the other filters
        /// narrow the attention events only.
        #[arg(long)]
        all: bool,
        /// Every field (run_id included) and the whole payload instead of the compact form.
        #[arg(long)]
        full: bool,
        /// Only this run's events.
        #[arg(long)]
        run: Option<String>,
        /// Only this task's events.
        #[arg(long)]
        task: Option<i64>,
        /// Only this goal's events.
        #[arg(long)]
        goal: Option<i64>,
        /// Only events of this kind, attention or not; repeat for several.
        #[arg(long)]
        kind: Vec<String>,
        /// Only events at or after this UTC time (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS[.fff]Z).
        #[arg(long)]
        since: Option<String>,
        /// Only events before this UTC time.
        #[arg(long)]
        until: Option<String>,
    },
    /// A run's events oldest first and the gaps between them of at least --gap seconds, each
    /// with the reason the events show (idle, held, waiting_ask, background, after_receipt,
    /// waiting_integration, integrating, e2e, waiting_e2e, no_supervisor, unknown). Reads only.
    Timeline {
        run: String,
        /// The shortest gap reported, in seconds.
        #[arg(long, default_value_t = dagq::domain::timeline::DEFAULT_GAP_SECS, value_parser = clap::value_parser!(i64).range(1..))]
        gap: i64,
        /// Every field and the whole payload of each event instead of the compact form.
        #[arg(long)]
        full: bool,
    },
    /// Block until an attention event after the cursor arrives or the supervisors' health changes; returns empty on timeout, or with --until-attention only when one came. Reads only, never integrates.
    Watch {
        /// Event id to wait past; defaults to the newest event now.
        #[arg(long)]
        after: Option<i64>,
        /// Seconds to wait before returning with no events (default 600).
        #[arg(long, conflicts_with = "until_attention")]
        timeout: Option<u64>,
        /// Wait with no timeout: return only when an attention event arrives or the supervisors'
        /// health changes. An error reading the queue still ends it (non-zero), never retried.
        #[arg(long)]
        until_attention: bool,
        /// Seconds between reads of the queue.
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..))]
        interval: u64,
        /// Wake only for the attention addressed to this role: inbox for all of it and the
        /// supervisors' health, except update_installed and an hourly throughput review alone,
        /// which come back with the next events that wake it; planner never.
        #[arg(long, value_parser = ROLES)]
        role: Option<String>,
        #[command(flatten)]
        cmux: RetiredCmux,
    },
    /// Per-run times in seconds (work, validate, wait_to_land, startup) and counts, per-goal and
    /// overall count/total/median, and alerts over thresholds, derived from run events. The latest
    /// 50 finished runs unless --full; pass `next_cursor` to --since for only the runs finished later.
    /// Each run also carries its title, claim/validation/landing times, integrate attempts and
    /// deferrals, the landings that broke it, and its resumes.
    Stats {
        /// Event id (a previous `next_cursor`), `@<unix seconds>` or an RFC 3339 time: only runs
        /// that finished after it.
        #[arg(long)]
        since: Option<dagq::domain::stats::Cursor>,
        /// Event id, `@<unix seconds>` or an RFC 3339 time: only runs that finished at or before
        /// it; `next_cursor` goes no further.
        #[arg(long)]
        until: Option<dagq::domain::stats::Cursor>,
        /// Only runs of tasks in this goal.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
        /// Every finished run instead of the latest 50 (or the next 50 past --since).
        #[arg(long)]
        full: bool,
        /// Instead of the stats, the parallel run of the ledger (stage 2 of the measurement's
        /// rebuild): per run today's work, wait_to_land, startup and land_phases beside the
        /// ledger's (its push apart), each difference in seconds with its reasons; the fields of
        /// no run (jobs, sessions, ...) beside those of the ledger's rows; the host's load of the
        /// node rows beside today's CSV over the window and kpi's last 7 days; and what ended in
        /// the window (from the start without --since) against the ledger's rows: missing, not
        /// final, push_pending, dirty, and up_to_date per row.
        #[arg(long)]
        ledger: bool,
        #[command(flatten)]
        cmux: RetiredCmux,
    },
    // ADR-0051, ADR-t980-1.
    /// KPIs of the flow, rework, people's load, infrastructure, improvements and sessions per day
    /// or ISO week, split by the task's change (`unknown` without one), by the areas of what the
    /// run landed when dagq.toml has `[areas]`, and --by the claim's attributes, each next to the
    /// previous period (and a day's 7-day median), judged against the `[kpi.targets]` of
    /// dagq.toml and host.toml. --compare splits at a mark (its event id) or a time, or compares
    /// two windows A..B,C..D, and lists every other mark in and between them. Reads only; JSON.
    Kpi {
        #[arg(long, default_value = "day", value_parser = ["day", "week"])]
        period: String,
        /// How many periods to list, the latest last.
        #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(u16).range(1..=400))]
        last: u16,
        /// Event id, `@<unix seconds>` or an RFC 3339 time: the latest period is the one that
        /// holds it (now without).
        #[arg(long, conflicts_with_all = ["since", "until"])]
        at: Option<dagq::domain::stats::Cursor>,
        /// One window from this cursor instead of the periods.
        #[arg(long)]
        since: Option<dagq::domain::stats::Cursor>,
        /// One window up to this cursor instead of the periods.
        #[arg(long)]
        until: Option<dagq::domain::stats::Cursor>,
        /// List only these changes' strata (`unknown`: the tasks without a change); a comparison's
        /// change summary is made for them (every change it saw without).
        #[arg(long = "change", value_parser = parse_kpi_change)]
        changes: Vec<String>,
        /// List only these areas' strata (`unknown`: the runs without an area, `other`: files no
        /// area of `[areas]` matches); a comparison's area summary is made for them (every area
        /// it saw without).
        #[arg(long = "area", value_parser = parse_kpi_area)]
        areas: Vec<String>,
        /// Also split the runs by these attributes of the claim.
        #[arg(long, value_parser = ["change", "area", "build", "parallel", "slot", "load", "toolchain", "claude", "provider", "route", "codex", "group", "model", "effort", "nature"])]
        by: Vec<String>,
        /// Add cross strata of --by axes and selected --area / --change values.
        #[arg(long)]
        cross: bool,
        /// A mark's event id or a time to compare before and after, or two windows A..B,C..D.
        #[arg(long)]
        compare: Option<dagq::domain::kpi::CompareSpec>,
        /// Days on each side of a --compare at a mark or a time.
        #[arg(long, default_value_t = dagq::domain::kpi::DEFAULT_WINDOW_DAYS, value_parser = clap::value_parser!(i64).range(1..=365))]
        window: i64,
        /// Only the runs, asks and findings of tasks in this goal.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
    },
    // ADR-0070.
    /// When the open tasks (ready and in progress, outside a draft goal) and the open goals are
    /// likely to finish if the plan flows as it is now: the p50 and p90 of a seeded simulation
    /// over the dependencies, the claim order, the slots and each change's landed runs (the whole
    /// distribution for a change with fewer than `[kpi]` `min_samples`), with what it assumed.
    /// New tasks are not added. Reads only and records nothing; JSON.
    Forecast {
        /// Only this task, and its goal.
        #[arg(long = "task")]
        task_id: Option<i64>,
        /// Only this goal and its tasks.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
        /// Simulate this many slots instead of the live supervisors' `parallel` together.
        #[arg(long, value_parser = clap::value_parser!(u16).range(0..=256))]
        parallel: Option<u16>,
        /// Simulation trials.
        #[arg(long, default_value_t = dagq::domain::forecast::DEFAULT_TRIALS as u32, value_parser = clap::value_parser!(u32).range(1..=100_000))]
        trials: u32,
    },
    // ADR-0051 decision 21.
    /// Write the KPI report of a day or ISO week as the supervisor writes it daily: `dagq kpi
    /// --period P --at <the period>` with the build, the time and the top open findings, as JSON
    /// and as one self-contained HTML page (no script, CSS, font or image from anywhere else),
    /// under the queue's reports/ (daily/YYYY-MM-DD, weekly/YYYY-Www; today's and this week's are
    /// named .partial), then index.html and the [report] retention of host.toml. Prints the paths
    /// written. Changes no queue state.
    Report {
        #[arg(long, default_value = "day", value_parser = ["day", "week"])]
        period: String,
        /// Event id, `@<unix seconds>` or an RFC 3339 time: the period that holds it (now
        /// without).
        #[arg(long)]
        at: Option<dagq::domain::stats::Cursor>,
        /// Write under this directory instead of the queue's reports/.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print the report as JSON instead of writing any file.
        #[arg(long, value_parser = ["json"], conflicts_with = "out")]
        print: Option<String>,
    },
    /// Report every unfinished run and supervisor, one line's worth each, without changing state.
    /// Also reports the host's settings, among them the Claude credential host.toml chooses for
    /// a container's worker (`claude_credential`: its kind, file, presence and mode, never its
    /// value; with no kind chosen, which to choose: a long-lived token for one's own
    /// subscription used alone, an API key for an organization, a team, a shared host or work).
    /// `toolchains` shows the toolchain profiles dagq.toml declares and the paths each applies
    /// to (none while it declares none: then no language's knowledge reads the output), and
    /// `marks` the project's own marks on the verification's output ([verify_marks.<name>]: its
    /// name, the class a line of its shape gives, and the shape).
    /// `command_rules` shows the rules that classify the commands when the ledger and a run's
    /// work breakdown are made, in the order they are tried, each with its source: dagq.toml
    /// ([[telemetry.commands]], with its line) first, then each declared profile's
    /// (profile:<name>), then the runtime's common rules (common), which a run's work breakdown
    /// classifies by too; with the [telemetry] command_detail and failed_tests in use.
    /// `agents` shows the review's agents read from the landing branch; its `builtins` start with
    /// builtin:acceptance, the whole review, and `without_rules` names the enabled built-ins
    /// that judge without the project's rules, the landing branch having no
    /// .dagq/review/builtin-<name>.md for them, with `setup` naming `dagq setup-review`, which
    /// writes those files.
    /// `schema` is reported even for a queue that needs `migrate` or refuses this binary; the
    /// latter has `error` in place of its supervisors and runs.
    Doctor {
        /// Include each run's lease, processes, heartbeats and paths, and every supervisor field.
        #[arg(long)]
        full: bool,
    },
    /// The queue service: the host process that opens the queue for the callers given its
    /// socket and a token, with use cases (ask, show, note) authorized on its side. `up` starts
    /// it before the supervisor, the supervisor starts it again, and `down` stops it.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// The queue broker: the host process a container's run reaches the queue service through,
    /// on a port of 127.0.0.1, with its run token checked. `up` starts it before the
    /// supervisor, the supervisor starts it again, and `down` stops it.
    QueueBroker {
        #[command(subcommand)]
        command: QueueBrokerCommand,
    },
    /// The path of a run's container to the queue broker: the relay in the dagq podman machine
    /// and the forwarder in the container. Neither opens a queue.
    Relay {
        #[command(subcommand)]
        command: RelayCommand,
    },
    // ADR-t2259-1 decision 1.
    /// A built-in program of a run's review, as its program job runs it in
    /// the run's worktree: `builtin:secrets BASE HEAD [PATH...]`. Opens no
    /// queue; exits 0 (passed), 1 (rejected) or 2 (could not check).
    #[command(hide = true)]
    ReviewProgram {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    // ADR-t1181-1 decision 2.
    /// A hook of a Codex inbox, which `dagq inbox` gives Codex in its launch arguments: reads
    /// the hook's input on stdin and prints its answer (the status at a session start, a Stop's
    /// block, the guardrail's refusal). Opens no queue itself; always exits 0.
    #[command(hide = true)]
    CodexInboxHook {
        #[arg(value_parser = ["session-start", "session-end", "stop", "pre-tool-use"])]
        hook: String,
    },
    /// Bind the queue to the repository containing the working directory (or
    /// --repo) after the repository moved; the one command that changes the
    /// binding. Refused while a supervisor runs. Pass --db for a queue still
    /// in its old directory; `move_to` names where the repository now looks for it.
    ///
    /// Every other command on a queue resolved from the repository refuses a repository other
    /// than the bound one. A live supervisor counts even when its heartbeat is old, and so does
    /// an `integrate` still landing. A worktree whose repair fails is reported in
    /// `worktrees[].error` and the rebind still succeeds; the supervisor repairs it again before
    /// removing it. `move_to` does not check whether its directory already exists.
    Rebind {
        /// Checkout of the repository to bind to; defaults to the working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Mark one unfinished run interrupted once its processes and supervisor are gone; keeps its worktree and workspace and leaves other runs alone.
    ///
    /// An `integrating` run goes back to `awaiting_integration`, and a run awaiting integration
    /// that a dead supervisor still leases only loses the lease. A live wrapper's run is the
    /// supervisor's to adopt, and a live supervisor's lease is never taken: stop it first. The
    /// task stays `in_progress`: the supervisor's recovery job decides the next step, and a
    /// retry a person decides is `ready ID` (with `--inherit` to keep the run's commits). A
    /// running supervisor recovers such runs by itself, so this is for a queue no supervisor
    /// serves.
    Recover {
        /// Run ID from `show` or `doctor`.
        run: String,
    },
    #[command(hide = true)]
    Session {
        #[arg(long)]
        run: String,
        #[arg(long)]
        lease: String,
        #[arg(long)]
        claude: PathBuf,
        // ADR-t813-3.
        /// The Codex CLI a Codex worker's turns call.
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        /// Reopen the session of a `needs_session` run the supervisor resumes.
        #[arg(long)]
        resume: bool,
        #[command(flatten)]
        cmux: RetiredCmux,
        // ADR-t1404-1.
        /// Started by the supervisor in the background, without a workspace or
        /// a terminal.
        #[arg(long)]
        background: bool,
    },
    // ADR-0048 decision 6.
    /// Record a SessionStart (`open`) or SessionEnd (`close`) of an inbox or planner session:
    /// the plugin's hook passes its stdin, and the session's environment names its kind. Only
    /// the session spans are written.
    #[command(hide = true)]
    SessionEvent {
        #[arg(value_parser = ["open", "close"])]
        event: String,
        // task 734.
        /// The run whose session reports, when it is no inbox or planner
        /// session: what the caller is authorized on. Without it, the caller's
        /// `DAGQ_RUN_ID`. A run's session records no span.
        #[arg(long)]
        run: Option<String>,
    },
    #[command(hide = true)]
    PlannerSession {
        #[arg(long)]
        planner: i64,
        #[arg(long)]
        claude: PathBuf,
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
        // ADR-0079 decision 7.
        /// The model its agent starts with, given with `--effort`.
        #[arg(long, requires = "effort")]
        model: Option<String>,
        #[arg(long, requires = "model")]
        effort: Option<String>,

        #[command(flatten)]
        cmux: RetiredCmux,
        // ADR-t1394-2.
        /// The planner's agent runs one call per turn: the wrapper needs no
        /// terminal for it.
        #[arg(long)]
        headless: bool,
        // ADR-t1404-1 decision 8.
        /// The supervisor started this wrapper in the background, without a
        /// workspace.
        #[arg(long, requires = "headless")]
        background: bool,
    },
}

/// The session roles attention is addressed to.
const ROLES: [&str; 2] = ["inbox", "planner"];
/// The seconds `watch` waits without `--timeout` or `--until-attention`.
const DEFAULT_WATCH_TIMEOUT_SECS: u64 = 600;
/// The names of the task priorities (ADR-0040 decision 4), highest first.
const PRIORITIES: [&str; 5] = ["interrupt", "urgent", "high", "normal", "low"];

/// The providers a task's worker may run on (ADR-t813-2 decision 1).
const PROVIDERS: [&str; 2] = ["claude", "codex"];

/// The execution classes of a task (ADR-t1487-1 decision 1).
const EXECUTION_CLASSES: [&str; 2] = ["implementation", "spike"];

use reads::{parse_kpi_area, parse_kpi_change};

fn parse_role(value: Option<String>) -> Result<Option<SessionRole>> {
    Ok(value.map(|value| value.parse()).transpose()?)
}

/// The statuses of a finding.
const FINDING_STATUSES: [&str; 4] = ["open", "proposed", "resolved", "dismissed"];

#[derive(Subcommand, Clone)]
enum FindingCommand {
    /// Record a finding. The open or proposed finding of the same kind, target and subject takes
    /// it instead: new evidence adds an occurrence (and reopens a resolved one), a new summary,
    /// detail, impact or --propose is written, and a record with nothing new changes nothing
    /// (`changed` is empty). Prints the finding with `created` and `changed`.
    #[command(group = clap::ArgGroup::new("target").required(true))]
    Record {
        /// A lowercase slug: stall, failure, wait, capacity, threshold, conflict_hotspot, ...
        #[arg(long)]
        kind: String,
        #[arg(long, group = "target")]
        task: Option<i64>,
        #[arg(long, group = "target")]
        run: Option<String>,
        #[arg(long, group = "target")]
        goal: Option<i64>,
        /// The queue as a whole.
        #[arg(long, group = "target")]
        queue: bool,
        /// What tells the problem apart within its target: a path, an alert, a threshold.
        #[arg(long, default_value = "")]
        subject: String,
        /// One line.
        #[arg(long)]
        summary: String,
        /// The reading of it; omitted keeps the recorded one.
        #[arg(long)]
        detail: Option<String>,
        /// high, normal or low; omitted keeps the recorded one (a new finding: normal).
        #[arg(long, value_parser = ["high", "normal", "low"])]
        impact: Option<String>,
        /// Run event that shows it; repeatable.
        #[arg(long = "evidence")]
        evidence: Vec<i64>,
        // ADR-0044 decision 19.
        /// Ask for a proposal to remedy it, with the reason.
        #[arg(long)]
        propose: Option<String>,
    },
    /// Mark a finding resolved: the problem no longer occurs.
    Resolve {
        id: i64,
        #[arg(long)]
        reason: String,
    },
    /// Mark a finding dismissed: nobody will remedy it, or (--covered-by) an open task already
    /// does. It still counts occurrences.
    Dismiss {
        id: i64,
        #[arg(long)]
        reason: String,
        // ADR-t1920-1.
        /// The open task that already fixes the failure of a ci_failure finding: that task's
        /// runs keep the finding's tests on the list of the tests that fail already.
        #[arg(long = "covered-by", value_name = "TASK")]
        covered_by: Option<i64>,
    },
}

#[derive(Subcommand, Clone)]
enum CiCommand {
    /// The tests (or failed jobs and steps) that fail already on the watched branch, each with
    /// the CI run that added it and its ci_failure finding, read from the queue only. With
    /// --task, the items of the findings that task fixes move to kept_for_task. Prints
    /// {"enabled", "workflow", "branch", "state", "watch", "checked_at", "latest_run",
    /// "failures", "kept_for_task"}.
    Failures {
        /// The task whose run gets the list: its own findings' tests are kept apart.
        #[arg(long)]
        task: Option<i64>,
    },
}

#[derive(Subcommand, Clone)]
enum RequestCommand {
    /// Record a person's words as a planning request (only the inbox, at a person's word, and a
    /// person at a plain terminal): the supervisor opens a planner of the runtime's for it, which
    /// submits a proposal (the request becomes proposed) or declines it. Prints the request.
    #[command(group = clap::ArgGroup::new("words").required(true))]
    #[command(group = clap::ArgGroup::new("notes"))]
    Add {
        /// The person's own words, not a summary; `-` reads them from stdin.
        #[arg(long, group = "words")]
        text: Option<String>,
        /// A file holding the person's own words: what it holds when read is recorded, not
        /// its path.
        #[arg(long, group = "words", alias = "file")]
        text_file: Option<PathBuf>,
        /// What the inbox adds, kept apart from the person's words; `-` reads it from stdin.
        #[arg(long, group = "notes")]
        note: Option<String>,
        /// A file holding what the inbox adds: what it holds when read is recorded.
        #[arg(long, group = "notes")]
        note_file: Option<PathBuf>,
        /// What it refers to: ask:N, task:N, run:ID, event:N, finding:N or goal:N; repeatable.
        #[arg(long = "ref")]
        refs: Vec<String>,
        /// The priority the person's words name (interrupt, urgent, high, normal or low), and
        /// only then: the runtime gives it, as the person's, to the goals the request's planner
        /// makes.
        #[arg(long, value_parser = PRIORITIES)]
        priority: Option<String>,
    },
    /// Decline an open request with the reason (only the planner opened for it): nothing will be
    /// planned of it. The inbox is told.
    Decline {
        id: i64,
        #[arg(long)]
        reason: String,
        /// For a request of a replanning operation: continue the parked run on its saved work
        /// rather than replace its task (the runtime resumes it).
        #[arg(long)]
        continue_run: bool,
    },
}

#[derive(Subcommand, Clone)]
enum ReplanCommand {
    /// Ask the runtime to replan run RUN (only a person at a plain terminal and the inbox, at a
    /// person's word): one open operation per task, which a request returns when it is on RUN or
    /// guards the task (it stays on the run it fenced); one that does not guard moves to the
    /// task's latest run, and a request on an earlier run is refused. Its planning request waits
    /// for the run's saved work. Prints {"operation", "created", "request"}.
    Request {
        #[arg(long)]
        run: String,
        /// Why, in the person's words.
        #[arg(long)]
        reason: String,
        /// An event it rests on; repeatable.
        #[arg(long = "evidence")]
        evidence: Vec<i64>,
    },
    /// Propose to replan your own run (a worker only, on its run): the runtime diagnoses it;
    /// nothing stops or changes your run here. Prints {"operation", "created", "request"}.
    Propose {
        #[arg(long)]
        run: String,
        /// Why: which conditions are independent, what the review keeps finding.
        #[arg(long)]
        reason: String,
        /// An event it rests on; repeatable.
        #[arg(long = "evidence")]
        evidence: Vec<i64>,
    },
}

/// The splits of an agent's cases (`.dagq/agents/<agent>/evals/<split>.json`).
const SPLITS: [&str; 3] = ["dev", "holdout", "production"];

#[derive(Subcommand, Clone)]
enum AgentCommand {
    /// Ask for one round of the agent's eval: the supervisor runs it on the landing branch's
    /// definition and cases, one job of the agent per run of a case, and records its scores.
    /// A built-in agent (`builtin:<name>`) is measured on the binary's definition and the cases
    /// it carries, without the project's rules or programs. A worker asks for dev of its own
    /// run only; holdout and production are a person's, the inbox's or a planner's. Prints the
    /// eval's id.
    Eval {
        /// The agent: `.dagq/agents/<agent>/`, or a built-in agent `builtin:<name>`.
        agent: String,
        /// The cases' split.
        #[arg(long, default_value = "dev", value_parser = SPLITS)]
        split: String,
        /// Each case's runs, over the case list's own k.
        #[arg(long)]
        k: Option<u32>,
        /// Only these cases of the split, by id (comma-separated or repeated).
        /// Holdout and production run their set whole and refuse a part of it unless with
        /// --rerun (a person's). Not --set, which picks a whole set of the queue's side.
        #[arg(long, value_delimiter = ',')]
        cases: Vec<String>,
        /// A production set of the queue's side (`agent cases`), by its id: run whole as
        /// production in place of the repository's production.json. Never with --cases (a set
        /// is run whole, and a part of it would be a key of its own).
        #[arg(long)]
        set: Option<i64>,
        /// Run a holdout or production set again on the same definition: a person's only, or
        /// the inbox's at a person's word.
        #[arg(long)]
        rerun: bool,
    },
    /// Ask for a production set of the agent's cases: the supervisor makes one case of each
    /// production review in the period that selected the agent, has a job of another provider
    /// label each by the rules alone, asks a person about the labels that disagree with the
    /// production verdict, and keeps the set on the queue's side for `agent eval --set`. A
    /// person's, the inbox's or a planner's. Prints the set's id.
    Cases {
        /// The agent: `.dagq/agents/<agent>/`, or a built-in agent `builtin:<name>`.
        agent: String,
        /// The reviews started at or after this instant: a UTC date (`2026-10-05`, its
        /// midnight) or an RFC 3339 time (`2026-10-05T12:00:00Z`, `...+09:00`).
        #[arg(long)]
        since: Option<String>,
        /// The reviews started before this instant, written as `--since` is.
        #[arg(long)]
        until: Option<String>,
    },
    /// Decide the label of a case of a production set, which clears its dispute: a person's,
    /// the inbox's at a person's word, or a planner's.
    Decide {
        /// The set's id.
        set: i64,
        /// The case's id in the set.
        case: String,
        /// violation or clean.
        #[arg(long)]
        verdict: String,
        /// The rule codes a violation breaks (comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        codes: Vec<String>,
        /// Why the label is right.
        #[arg(long)]
        reason: String,
    },
    /// List the production sets, newest first, with their status, digest and disputed cases.
    /// Prints {"sets"}.
    Sets {
        /// Only this agent's.
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// One production set whole: its request, the reviews left out, each case's label and
    /// its cases as `agent eval --set` runs them.
    Set { id: i64 },
    /// List the rounds of the eval, newest first, with their status and scores (a worker's
    /// own run's only). Prints {"evals"}.
    Results {
        /// Only this agent's.
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// One round of the eval whole: its request, its start or refusal, each run and its
    /// scores (a worker's own run's only).
    Result { id: i64 },
}

#[derive(Subcommand, Clone)]
enum ServiceCommand {
    /// Report whether the queue service runs and answers, its pid, build, API version and
    /// socket, and its attention, without changing anything.
    Status,
    /// Start the queue service of this binary unless one of its build answers, replacing one
    /// of another build; `up` does this before the supervisor.
    Start {
        #[command(flatten)]
        cmux: RetiredCmux,
    },
    /// Stop the queue service; `down` does this after the supervisor.
    Stop,
    /// Run the queue service in the foreground until SIGINT or SIGTERM (what `start`, `up`
    /// and the supervisor run in the background). A second one for the same queue is refused.
    Serve {
        #[command(flatten)]
        cmux: RetiredCmux,
    },
}

#[derive(Subcommand, Clone)]
enum RelayCommand {
    /// The relay of a run in the dagq podman machine: bind the run's unix socket (mode 0600,
    /// given to --owner) and pass each connection to --target as it is, both ways, until SIGINT
    /// or SIGTERM. It holds no token and judges nothing: the queue broker does.
    Serve {
        #[arg(long)]
        socket: PathBuf,
        /// The queue broker's `<host>:<port>`.
        #[arg(long)]
        target: String,
        /// The uid the socket is given to: the run's container's user.
        #[arg(long)]
        owner: Option<u32>,
    },
    /// The forwarder in a run's container: listen on 127.0.0.1:--port and pass each connection
    /// to the run's unix socket as it is, both ways, until SIGINT or SIGTERM.
    Forward {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        port: u16,
    },
    /// The queue broker's health through the run's unix socket: whether the relay and the
    /// machine's path to the host reach it.
    Probe {
        #[arg(long)]
        socket: PathBuf,
    },
    /// A worker's turn in its container: fetch main's bundle into the clone, start the
    /// forwarder, run the agent after `--` in the clone with the run token of
    /// DAGQ_SERVICE_CREDENTIAL_FILE, then write the run branch's bundle and the run's state to
    /// the run directory. Exits with the agent's code; its output is the agent's. With
    /// --facts-only, a turn of no agent that writes the clone's facts anew.
    #[command(hide = true)]
    Turn {
        #[arg(long)]
        run_dir: PathBuf,
        #[arg(long)]
        clone: PathBuf,
        /// The run's base commit.
        #[arg(long)]
        base: String,
        /// The run branch.
        #[arg(long)]
        branch: String,
        /// Run no agent: restore what the control side handed, then write the bundle, work
        /// and state of the clone as it is, for the control side to read (a facts-only turn).
        #[arg(long)]
        facts_only: bool,
        #[arg(
            last = true,
            required_unless_present = "facts_only",
            conflicts_with = "facts_only"
        )]
        agent: Vec<String>,
    },
    /// Make the sccache guard of a worker's turn in its container: a link named
    /// dagq-rustc-wrapper in --dir to this dagq, made anew unless it is one already. What the
    /// supervisor runs in the container (podman exec) before it opens the turn's sccache gate.
    #[command(hide = true)]
    Guard {
        #[arg(long)]
        dir: PathBuf,
    },
    /// One command of integrate's verification in its container: start the forwarder, give the
    /// command after `--` the proxy variables with the verification's token of --token-file
    /// and the run of DAGQ_RUN_ID, and run it in the current directory. Exits with its code;
    /// its output is the command's.
    #[command(hide = true)]
    Verify {
        #[arg(long)]
        token_file: PathBuf,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand, Clone)]
enum QueueBrokerCommand {
    /// Run the queue broker in the foreground until SIGINT or SIGTERM (what `up` and the
    /// supervisor run in the background). A second one for the same queue is refused.
    Serve,
}

#[derive(Subcommand, Clone)]
enum RunCommand {
    // ADR-t1433-3.
    /// Refused for every run, with the reason: a run's session runs in the
    /// background without a screen. Read its turns with `run log RUN
    /// [--follow]`. Nothing is read or recorded.
    Screen {
        /// Run ID, or a task ID for the task's latest run.
        run: String,
        /// Accepted and ignored: there is no screen to read.
        #[arg(long, default_value_t = dagq::application::screen::DEFAULT_LINES)]
        lines: usize,
        #[command(flatten)]
        cmux: RetiredCmux,
    },
    // task 1437.
    /// Refused for every run: no worker run takes keys or text, as every
    /// worker runs headless. Answer the run's asks with `answer`; the
    /// supervisor delivers the answer as the session's next turn.
    Send {
        /// Run ID, or a task ID for the task's latest run.
        run: String,
        /// Accepted and ignored.
        #[arg(long = "key", conflicts_with = "answer")]
        keys: Vec<String>,
        /// Accepted and ignored.
        #[arg(long)]
        answer: Option<i64>,
        #[command(flatten)]
        cmux: RetiredCmux,
    },
    // ADR-t1404-1, ADR-t1433-3.
    /// Print the output of the run's session wrapper started in the background:
    /// the `[dagq]` summary of each turn (its start, the agent's text, its
    /// tools, its outcome) in the log of the session the run started last, an
    /// ended run's too. With --follow, keep printing what the wrapper appends
    /// until it ends. A run whose session ran in a workspace an older binary
    /// opened has no log.
    Log {
        /// Run ID, or a task ID for the task's latest run.
        run: String,
        /// Only the last N lines; without it, the whole log.
        #[arg(long)]
        lines: Option<usize>,
        /// Keep printing what the wrapper appends until it ends.
        #[arg(long, short = 'f')]
        follow: bool,
    },
    // ADR-t1433-3.
    /// Refused: the runtime opens no workspace for a run any more and stops a
    /// run's background wrapper itself. Close a workspace a run opened before
    /// in your own terminal. The arguments are accepted and ignored.
    CloseWorkspaces {
        /// Accepted and ignored.
        #[arg(conflicts_with = "task")]
        run: Option<String>,
        /// Accepted and ignored.
        #[arg(long)]
        task: Option<i64>,
        /// Accepted and ignored.
        #[arg(long)]
        apply: bool,
        #[command(flatten)]
        cmux: RetiredCmux,
    },
}

#[derive(Subcommand, Clone)]
enum PlannerCommand {
    // ADR-t1433-2.
    /// Print that the planner's session has no screen and where its turns are
    /// (`turns/` of its directory): a planner of the runtime's runs headless,
    /// and the terminal of a person's planner is not read any more. Read its
    /// turns with `planner log`. Nothing is read or recorded.
    Screen {
        planner: i64,
        /// Accepted and ignored: there is no screen to read.
        #[arg(long, default_value_t = dagq::application::screen::DEFAULT_LINES)]
        lines: usize,
        #[command(flatten)]
        cmux: RetiredCmux,
    },
    // ADR-t1404-1 decision 8.
    /// Print the output of the planner's session wrapper started in the
    /// background: the `[dagq]` summary of each turn in `session.log` of its
    /// directory, a closed planner's too. With --follow, keep printing what the
    /// wrapper appends until it ends. A planner in a workspace (a person's, or
    /// one an older binary opened for the runtime) is refused: its screen is
    /// not read any more.
    Log {
        planner: i64,
        /// Only the last N lines; without it, the whole log.
        #[arg(long)]
        lines: Option<usize>,
        /// Keep printing what the wrapper appends until it ends.
        #[arg(long, short = 'f')]
        follow: bool,
    },
    // ADR-t1433-2.
    /// Refused for every planner, with the reason: nothing is typed into a
    /// planner's session. Answer its `planner_question` with `answer`, and the
    /// supervisor delivers the answer (to a headless planner as its next turn);
    /// hand a headless planner a follow-up with `planner request`. Nothing is
    /// recorded.
    Send {
        planner: i64,
        /// Accepted and ignored.
        #[arg(long = "key", conflicts_with = "answer")]
        keys: Vec<String>,
        /// Accepted and ignored.
        #[arg(long)]
        answer: Option<i64>,
        #[command(flatten)]
        cmux: RetiredCmux,
    },
    /// Hand a follow-up request to an open headless planner of the
    /// runtime's (only the inbox, at a person's word, and a person at a
    /// plain terminal): the words are written under the planner's
    /// directory and the fixed sentence pointing at them becomes its next
    /// turn, taken after a turn at work. Refused for an interactive or a
    /// person's planner, and one closed, lost, exited or asked to exit.
    /// Recorded as `planner_request_handed`. A new plan is `request add`.
    #[command(group = clap::ArgGroup::new("words").required(true))]
    Request {
        planner: i64,
        /// The person's own words, not a summary; `-` reads them from stdin.
        #[arg(long, group = "words")]
        text: Option<String>,
        /// A file holding the person's own words: what it holds when read is handed, not
        /// its path.
        #[arg(long, group = "words", alias = "file")]
        text_file: Option<PathBuf>,
        #[command(flatten)]
        cmux: RetiredCmux,
    },
}

#[derive(Subcommand, Clone)]
enum AskCommand {
    /// Mark an answered ask read. An open ask is withdrawn by answering it first.
    Close { id: i64 },
}

#[derive(Subcommand, Clone)]
enum DependencyCommand {
    Add {
        task: i64,
        #[arg(required_unless_present = "goal")]
        predecessor: Option<i64>,
        /// A goal instead of a predecessor task: TASK waits until it is closed as achieved.
        #[arg(long, conflicts_with = "predecessor")]
        goal: Option<i64>,
    },
    Remove {
        task: i64,
        #[arg(required_unless_present = "goal")]
        predecessor: Option<i64>,
        /// Remove the dependency on this goal instead of a predecessor task.
        #[arg(long, conflicts_with = "predecessor")]
        goal: Option<i64>,
    },
}

#[derive(Subcommand, Clone)]
enum ProposalCommand {
    /// The submitted and revising proposals, oldest submission first (plan review's order).
    List {
        /// Include accepted and canceled proposals.
        #[arg(long)]
        all: bool,
    },
    /// Show a proposal with its member task and goal IDs.
    Show { id: i64 },
    /// Withdraw a submitted or revising proposal without plan review: it ends as canceled, its
    /// submitted tasks return to draft, and its tasks and goals may join another proposal.
    Withdraw { id: i64 },
}

#[derive(Subcommand, Clone)]
enum GoalCommand {
    /// Register a goal; it has no state machine and no verification commands.
    Add {
        title: String,
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long, default_value = "")]
        acceptance: String,
        /// Naming, boundaries, and what not to do, shared by every task of the goal.
        #[arg(long, default_value = "")]
        constraints: String,
        /// Path of a reference document inside the repository.
        #[arg(long)]
        doc: Option<String>,
        /// Register a draft: its tasks are not candidates until `goal ready`.
        #[arg(long)]
        draft: bool,
        // ADR-t1639-1.
        /// The priority its tasks without one of their own inherit: interrupt, urgent, high,
        /// normal or low; normal when left out. A planner of a request a person gave a
        /// priority leaves it out: the runtime attaches the request's.
        #[arg(long, value_parser = PRIORITIES)]
        priority: Option<String>,
        // ADR-t1639-1.
        /// A tag naming what the goal is about; repeatable. With `[goals] tags` in dagq.toml,
        /// one of them.
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Open a draft goal so the supervisor may claim its ready tasks.
    Ready { id: i64 },
    /// List goals with their status, priority, tags and task counts by status: the open and draft
    /// goals first, then by priority (highest first), then by ID.
    List {
        /// Only the goals with this tag; repeatable, any of them.
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Show a goal, its tasks, and the kinds of its latest 10 events; long
    /// texts are cut to 300 characters (ending in `…`, with `truncated: true`).
    Show {
        id: i64,
        /// Print the texts and every event with its payload in full.
        #[arg(long)]
        full: bool,
    },
    /// Replace fields of a goal; runs already started keep their prompt.
    #[command(group = clap::ArgGroup::new("field").multiple(true).required(true))]
    Edit {
        id: i64,
        #[arg(long, group = "field")]
        title: Option<String>,
        #[arg(long, group = "field")]
        description: Option<String>,
        #[arg(long, group = "field")]
        acceptance: Option<String>,
        #[arg(long, group = "field")]
        constraints: Option<String>,
        /// New document path; an empty value clears it.
        #[arg(long, group = "field")]
        doc: Option<String>,
        // ADR-t1811-1, ADR-t1850-1.
        /// New priority of a draft or open goal; tasks without a priority of their own follow
        /// the goal's current priority in every status. It takes effect at a ready task's next
        /// claim or an in-progress task's next resume or recovery job, and never stops a running
        /// run.
        #[arg(long, group = "field", value_parser = PRIORITIES)]
        priority: Option<String>,
        // ADR-t1639-1.
        /// The tags that replace the goal's; repeatable. With `[goals] tags` in dagq.toml,
        /// each one of them.
        #[arg(long = "tag", group = "field", conflicts_with = "no_tags")]
        tags: Vec<String>,
        /// Remove every tag of the goal.
        #[arg(long, group = "field")]
        no_tags: bool,
    },
    /// Record the verdict once. `achieved` needs every task completed or canceled; `abandoned` needs no task in progress.
    Close {
        id: i64,
        #[arg(long, value_parser = ["achieved", "abandoned"])]
        verdict: String,
    },
    /// Have the supervisor's goal review judge the open goal again although its tasks did not
    /// change since its last review (after a failed review or a keep_open answer).
    Review { id: i64 },
}

/// The error of a command the observer may not run.
const OBSERVER_DENIED: &str = "observer may not change queue state";
/// The error of a command a headless job may not run.
const REVIEWER_DENIED: &str = "reviewer may not change queue state";

/// What `hold` and `release` name: digits are a task, anything else a run.
fn hold_target_resource(target: &str) -> Resource {
    match target.trim().parse::<i64>() {
        Ok(id) => task_resource(id),
        Err(_) => run_resource(target.trim()),
    }
}

/// `hold --until` in unix seconds: a time, never an event id.
fn hold_until(until: Option<dagq::domain::stats::Cursor>) -> Result<Option<i64>> {
    use dagq::domain::stats::Cursor;
    match until {
        None => Ok(None),
        Some(Cursor::Time(millis)) => Ok(Some(millis.div_euclid(1000))),
        Some(Cursor::Event(_)) => {
            bail!("--until takes a time (`@<unix seconds>` or RFC 3339), not an event id")
        }
    }
}

/// The request or proposal `replan request` and `replan propose` record.
fn new_replan(
    run: &str,
    reason: String,
    evidence: &[i64],
) -> Result<dagq::domain::task_replan::NewReplan> {
    Ok(dagq::domain::task_replan::NewReplan {
        run: RunId::new(run)?,
        reason,
        evidence: evidence.iter().copied().map(EventId::new).collect(),
    })
}

/// The words `request add` records for `option` (`--text` or `--note`), and
/// `planner request` hands as `--text`: the value itself, stdin for `-`, or
/// what `file` holds when read (ADR-t1394-1 decision 2 keeps the request as
/// recorded, so a later change of the file changes nothing). UTF-8 as given, line breaks and quotes kept; a file or
/// stdin that cannot be read, is not UTF-8 or holds nothing is refused.
fn request_words(
    option: &str,
    value: Option<String>,
    file: Option<PathBuf>,
    cwd: &Path,
) -> Result<Option<String>> {
    let (source, bytes) = match (value, file) {
        (Some(value), _) if value != "-" => return Ok(Some(value)),
        (Some(_), _) => {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes)
                .with_context(|| format!("{option} -: cannot read stdin"))?;
            (format!("{option} - (stdin)"), bytes)
        }
        (None, Some(file)) => {
            let file = cwd.join(file);
            let bytes = std::fs::read(&file)
                .with_context(|| format!("{option}-file: cannot read {}", file.display()))?;
            (format!("{option}-file {}", file.display()), bytes)
        }
        (None, None) => return Ok(None),
    };
    let words = String::from_utf8(bytes).map_err(|error| {
        anyhow::anyhow!(
            "{source} is not UTF-8 (invalid byte at offset {}); nothing was recorded",
            error.utf8_error().valid_up_to()
        )
    })?;
    if words.trim().is_empty() {
        bail!("{source} is empty; nothing was recorded");
    }
    Ok(Some(words))
}

/// A run named on the command line, or [`Resource::Unresolved`] when its
/// id cannot be read: no owner matches it (fail closed).
fn run_resource(run: &str) -> Resource {
    RunId::new(run).map_or(Resource::Unresolved, Resource::run)
}

fn task_resource(id: i64) -> Resource {
    Resource::task(TaskId::new(id))
}

/// What a command asks of the [`Authorizer`] (ADR-t728-1 decision 5): each
/// capability and the resource it acts on, as far as the command line names
/// it. Owners and statuses the command line does not carry are left
/// unknown. The match has no catch-all, so a new command must be listed.
fn requests(command: &Command) -> Vec<(Capability, Resource)> {
    use Capability as C;
    let one = |capability, resource| vec![(capability, resource)];
    let queue = |capability| vec![(capability, Resource::Queue)];
    match command {
        // Run before any actor is read (`execute`): it opens no queue and
        // carries bytes only (ADR-t2114-3).
        Command::Relay { .. } | Command::ReviewProgram { .. } | Command::CodexInboxHook { .. } => {
            Vec::new()
        }
        Command::Locate
        | Command::List { .. }
        | Command::Show { .. }
        | Command::Candidates { .. }
        | Command::Graph { out: None, .. }
        | Command::Status { .. }
        | Command::Asks { .. }
        | Command::Events { .. }
        | Command::Timeline { .. }
        | Command::Stats { .. }
        | Command::Kpi { .. }
        | Command::Forecast { .. }
        | Command::Doctor { .. }
        | Command::Service {
            command: ServiceCommand::Status,
        }
        | Command::Notes { .. }
        | Command::Marks { .. }
        | Command::Findings { .. }
        | Command::Requests { .. }
        | Command::Replans { .. }
        | Command::Search { .. }
        | Command::Related { .. }
        | Command::Proposal {
            command: ProposalCommand::List { .. } | ProposalCommand::Show { .. },
        }
        | Command::Planners { .. }
        | Command::Lint { .. }
        | Command::Goal {
            command: GoalCommand::List { .. } | GoalCommand::Show { .. },
        }
        | Command::Observe { history: true, .. }
        | Command::Observe { input: Some(_), .. } => queue(C::QueueRead),
        Command::Watch { .. } => queue(C::QueueWatch),
        Command::Graph { out: Some(_), .. } | Command::Report { .. } => queue(C::ExportFile),
        // The runtime operations, as the application names them (task 734).
        Command::Init
        | Command::Migrate { .. }
        | Command::Rebind { .. }
        | Command::Install { .. }
        | Command::AutoUpdate { .. }
        | Command::ReleaseUpdate { .. }
        | Command::Up { .. }
        | Command::Down { .. }
        | Command::Inbox { .. }
        | Command::Service {
            command:
                ServiceCommand::Start { .. } | ServiceCommand::Stop | ServiceCommand::Serve { .. },
        }
        | Command::QueueBroker { .. }
        | Command::Plan { .. }
        | Command::Supervise { .. }
        | Command::Observe { history: false, .. }
        | Command::ThroughputReview { .. }
        | Command::SetupReview { .. }
        | Command::Integrate { .. }
        | Command::Review { .. }
        | Command::Recover { .. }
        | Command::Session { .. }
        | Command::PlannerSession { .. }
        | Command::SessionEvent { .. }
        | Command::Run { .. }
        | Command::Planner { .. } => operation(command, |name| env::var(name).ok())
            .map(|operation| operation.request())
            .into_iter()
            .collect(),
        Command::Add { goal_id, .. } => one(
            C::TaskWrite,
            goal_id.map_or(Resource::Queue, |goal| Resource::Goal(GoalId::new(goal))),
        ),
        Command::Ready {
            id,
            bypass_review,
            inherit,
            ..
        } => one(
            if *bypass_review || *inherit {
                C::TaskReadyBypassReview
            } else {
                C::TaskReady
            },
            task_resource(*id),
        ),
        Command::JudgeFollowUp { task, .. } => one(C::FollowUpJudge, task_resource(*task)),
        Command::Draft { id }
        | Command::Edit { task: id, .. }
        | Command::SetGoal { task: id, .. }
        | Command::SetPaths { task: id, .. }
        | Command::SetPriority { task: id, .. }
        | Command::Revisit { task: id, .. }
        | Command::Dependency {
            command:
                DependencyCommand::Add { task: id, .. } | DependencyCommand::Remove { task: id, .. },
        } => one(C::TaskWrite, task_resource(*id)),
        Command::Cancel { id, .. } => one(C::TaskCancel, task_resource(*id)),
        Command::Submit { proposal, .. } => one(
            C::ProposalSubmit,
            proposal.map_or(Resource::Queue, |id| Resource::Proposal {
                id: ProposalId::new(id),
                owner: None,
            }),
        ),
        Command::Proposal {
            command: ProposalCommand::Withdraw { id },
        } => one(
            C::ProposalWithdraw,
            Resource::Proposal {
                id: ProposalId::new(*id),
                owner: None,
            },
        ),
        Command::Goal { command } => match command {
            GoalCommand::Add { .. } => queue(C::GoalWrite),
            GoalCommand::Edit { id, .. } => one(C::GoalWrite, Resource::Goal(GoalId::new(*id))),
            GoalCommand::Ready { id } => one(C::GoalReady, Resource::Goal(GoalId::new(*id))),
            GoalCommand::Close { id, .. } => one(C::GoalClose, Resource::Goal(GoalId::new(*id))),
            GoalCommand::Review { id } => {
                one(C::GoalReviewRequest, Resource::Goal(GoalId::new(*id)))
            }
            GoalCommand::List { .. } | GoalCommand::Show { .. } => queue(C::QueueRead),
        },
        Command::Note {
            task, run, goal, ..
        } => one(
            C::NoteWrite,
            match (task, run, goal) {
                (Some(task), _, _) => task_resource(*task),
                (_, Some(run), _) => run_resource(run),
                (_, _, Some(goal)) => Resource::Goal(GoalId::new(*goal)),
                _ => Resource::Unresolved,
            },
        ),
        Command::Mark { .. } => queue(C::MarkWrite),
        Command::Agent { command } => match command {
            AgentCommand::Eval {
                split, rerun, set, ..
            } => dagq::domain::authorization::eval_request_needs(
                split != "dev" || set.is_some(),
                *rerun,
            )
            .into_iter()
            .map(|capability| (capability, Resource::AgentEval { run: None }))
            .collect(),
            AgentCommand::Results { .. }
            | AgentCommand::Result { .. }
            | AgentCommand::Sets { .. }
            | AgentCommand::Set { .. } => one(C::EvalRead, Resource::AgentEval { run: None }),
            AgentCommand::Cases { .. } | AgentCommand::Decide { .. } => {
                one(C::EvalCases, Resource::AgentEval { run: None })
            }
        },
        Command::Request { command } => match command {
            RequestCommand::Add { .. } => queue(C::RequestRecord),
            RequestCommand::Decline { id, .. } => one(
                C::RequestDecline,
                Resource::Request {
                    id: RequestId::new(*id),
                    planner: None,
                },
            ),
        },
        // The task of a run named is read from the queue.
        Command::Hold { target, .. } | Command::Release { target, .. } => {
            one(C::TaskHold, hold_target_resource(target))
        }
        // The task of the run is read from the queue.
        Command::Replan { command } => match command {
            ReplanCommand::Request { run, .. } => one(C::ReplanRequest, run_resource(run)),
            ReplanCommand::Propose { run, .. } => one(C::ReplanPropose, run_resource(run)),
        },
        // The list of the tests that fail already (ADR-t1920-1): not for
        // a worker or a job, whose prompt carries it.
        Command::Ci {
            command: CiCommand::Failures { .. },
        } => queue(C::CiRead),
        Command::Finding { command } => match command {
            FindingCommand::Record {
                task, run, goal, ..
            } => one(
                C::FindingRecord,
                match (task, run, goal) {
                    (Some(task), _, _) => task_resource(*task),
                    (_, Some(run), _) => run_resource(run),
                    (_, _, Some(goal)) => Resource::Goal(GoalId::new(*goal)),
                    _ => Resource::Queue,
                },
            ),
            FindingCommand::Resolve { id, .. } => {
                one(C::FindingResolve, Resource::Finding(FindingId::new(*id)))
            }
            FindingCommand::Dismiss { id, .. } => {
                one(C::FindingDismiss, Resource::Finding(FindingId::new(*id)))
            }
        },
        // The threshold crossings the observer raises to the inbox, each on
        // its finding (ADR-0044 decision 23).
        Command::Ask {
            command: None,
            kind: Some(kind),
            finding: Some(finding),
            ..
        } if kind == AskKind::Blocked.as_str() => {
            one(C::FindingAsk, Resource::Finding(FindingId::new(*finding)))
        }
        Command::Ask {
            command: Some(AskCommand::Close { id }),
            ..
        } => one(
            C::AskClose,
            Resource::Ask {
                id: AskId::new(*id),
                run: None,
            },
        ),
        Command::Ask {
            command: None,
            kind,
            task_id,
            run,
            request,
            ..
        } => one(
            C::AskOpen,
            match (
                kind.as_deref().unwrap_or_default().parse::<AskKind>(),
                run.as_deref().map(RunId::new).transpose(),
            ) {
                // The request's planner is read from the queue.
                (Ok(kind), Ok(run)) => Resource::NewAsk {
                    kind,
                    run,
                    task: task_id.map(TaskId::new),
                    request: request.map(RequestId::new),
                    planner: None,
                },
                _ => Resource::Unresolved,
            },
        ),
        Command::Answer { id, .. } => one(
            C::AskAnswer,
            Resource::Ask {
                id: AskId::new(*id),
                run: None,
            },
        ),
    }
}

/// The runtime operation `command` is (task 734), with what it acts on as
/// far as the command and `env`, the caller's environment, name it; `None`
/// for the commands the application authorizes elsewhere or that only read.
fn operation(command: &Command, env: impl Fn(&str) -> Option<String>) -> Option<Operation> {
    let run = |run: &str| RunId::new(run).ok();
    Some(match command {
        Command::Init => Operation::Init,
        Command::Migrate { .. } => Operation::Migrate,
        Command::Rebind { .. } => Operation::Rebind,
        Command::Install { .. } => Operation::Install,
        Command::AutoUpdate { .. } | Command::ReleaseUpdate { .. } => Operation::AutoUpdate,
        Command::Up { .. } => Operation::Up,
        Command::Down { .. } => Operation::Down,
        Command::Inbox { .. } => Operation::OpenInbox,
        Command::Service {
            command:
                ServiceCommand::Start { .. } | ServiceCommand::Stop | ServiceCommand::Serve { .. },
        }
        | Command::QueueBroker { .. } => Operation::QueueService,
        Command::Plan { .. } => Operation::Plan,
        Command::Supervise { .. } => Operation::Supervise,
        // Starting a job of the supervisor's timer, as `observe` (ADR-t996-1).
        Command::Observe {
            history: false,
            input: None,
            ..
        }
        | Command::ThroughputReview { .. } => Operation::Observe,
        Command::SetupReview { yes, .. } => Operation::SetupReview { yes: *yes },
        Command::Integrate { id, next, .. } => {
            Operation::Integrate(id.filter(|_| !next).map(TaskId::new))
        }
        Command::Review { id } => Operation::Review(TaskId::new(*id)),
        Command::Recover { run: id } => Operation::Recover(run(id)),
        Command::Run {
            command: RunCommand::CloseWorkspaces { run: id, task, .. },
        } => Operation::CloseWorkspaces(match (id, task) {
            (Some(id), _) => WorkspaceScope::Run(run(id)),
            (None, Some(task)) => WorkspaceScope::Task(TaskId::new(*task)),
            (None, None) => WorkspaceScope::All,
        }),
        Command::Session { run: id, .. } => Operation::Session(run(id)),
        Command::PlannerSession { planner, .. } => {
            Operation::PlannerSession(PlannerId::new(*planner))
        }
        Command::Run {
            command:
                command @ (RunCommand::Screen { run: target, .. }
                | RunCommand::Log { run: target, .. }
                | RunCommand::Send { run: target, .. }),
        } => {
            let resource = dagq::application::screen::RunTarget::parse(target)
                .map_or(Resource::Unresolved, |target| target.resource());
            match command {
                // A background session's log is read as its screen is.
                RunCommand::Screen { .. } | RunCommand::Log { .. } => {
                    Operation::ReadScreen(resource)
                }
                _ => Operation::SendToScreen(resource),
            }
        }
        Command::Planner { command } => match command {
            PlannerCommand::Screen { planner, .. } | PlannerCommand::Log { planner, .. } => {
                Operation::ReadScreen(Resource::Planner(PlannerId::new(*planner)))
            }
            PlannerCommand::Send { planner, .. } => {
                Operation::SendToScreen(Resource::Planner(PlannerId::new(*planner)))
            }
            PlannerCommand::Request { planner, .. } => {
                Operation::RequestPlanner(PlannerId::new(*planner))
            }
        },
        // The span the hook would record decides; a session with none is
        // a run's.
        Command::SessionEvent { run: named, .. } => Operation::SessionEvent(
            match dagq::domain::sessions::hook_kind(
                env(dagq::application::lifecycle::SESSION_KIND_ENV).as_deref(),
                env(dagq::domain::actor::ROLE_ENV).as_deref(),
                env(dagq::application::lifecycle::PLANNER_ORIGIN_ENV).as_deref(),
            ) {
                Some(dagq::domain::sessions::INBOX) => HookSession::Queue,
                // A planner workspace opened before `DAGQ_ACTOR_ID` names
                // no planner as its actor, so its span is the queue's.
                Some(_) => env(dagq::application::lifecycle::PLANNER_ID_ENV)
                    .filter(|_| {
                        env(dagq::domain::actor::ACTOR_ID_ENV)
                            .is_some_and(|id| !id.trim().is_empty())
                    })
                    .and_then(|id| id.trim().parse().ok())
                    .map_or(HookSession::Queue, |id| {
                        HookSession::Planner(PlannerId::new(id))
                    }),
                None => HookSession::Run(
                    named
                        .clone()
                        .or_else(|| env(dagq::domain::actor::RUN_ID_ENV))
                        .as_deref()
                        .and_then(run),
                ),
            },
        ),
        _ => return None,
    })
}

/// The split `--split` names.
fn split_named(name: &str) -> Result<dagq::domain::agent_eval::Split> {
    dagq::domain::agent_eval::Split::ALL
        .into_iter()
        .find(|split| split.as_str() == name)
        .with_context(|| format!("unknown split {name}"))
}

/// The commands that only read the queue. They open it on a read-only
/// connection (ADR-0045 decision 18).
fn reads_only(command: &Command) -> bool {
    requests(command)
        .iter()
        .all(|(capability, _)| *capability == Capability::QueueRead)
}

/// The task, run or goal a finding command names, if any.
fn finding_target(
    task: Option<i64>,
    run: Option<String>,
    goal: Option<i64>,
) -> Result<Option<FindingTarget>> {
    Ok(match (task, run, goal) {
        (Some(task), _, _) => Some(FindingTarget::Task(TaskId::new(task))),
        (_, Some(run), _) => Some(FindingTarget::Run(RunId::new(run)?)),
        (_, _, Some(goal)) => Some(FindingTarget::Goal(GoalId::new(goal))),
        _ => None,
    })
}

/// The planning commands, which the application authorizes for every
/// role before it changes the queue, with the owners and statuses read
/// from it, and records what it refuses (task 732).
fn authorized_in_application(command: &Command) -> bool {
    match command {
        Command::JudgeFollowUp { .. }
        | Command::Add { .. }
        | Command::Edit { .. }
        | Command::Submit { .. }
        | Command::Draft { .. }
        | Command::Ready { .. }
        | Command::Cancel { .. }
        | Command::Dependency { .. }
        | Command::SetGoal { .. }
        | Command::SetPaths { .. }
        | Command::SetPriority { .. }
        | Command::Revisit { .. }
        | Command::Proposal {
            command: ProposalCommand::Withdraw { .. },
        } => true,
        Command::Goal { command } => {
            !matches!(command, GoalCommand::List { .. } | GoalCommand::Show { .. })
        }
        // The asks, answers, notes, marks and findings (task 733).
        Command::Ask { .. }
        | Command::Answer { .. }
        | Command::Note { .. }
        | Command::Mark { .. }
        | Command::Finding { .. } => true,
        // The planning requests (ADR-t1394-1), with the planner a decline
        // needs read from the queue.
        Command::Request { .. } => true,
        // The replanning requests and proposals (ADR-t1521-2), with the
        // task of the run read from the queue.
        Command::Replan { .. } => true,
        // The hold of a task and its release (ADR-t1879-1), with the task
        // of a run named read from the queue.
        Command::Hold { .. } | Command::Release { .. } => true,
        // The eval's requests and reads (ADR-t1728-1), with the run a
        // worker's request is on and the round a read names.
        Command::Agent { .. } => true,
        // The runtime operations, authorized in `execute` before they run
        // (task 734).
        _ => operation(command, |_| None).is_some(),
    }
}

/// The commands the application does not authorize, which change no state
/// (reads, `watch`, `graph --out`, `report`), by the [`StaticPolicy`] for
/// every role (default deny, task 859): a worker, a wrapper or the
/// integrator reads but does not watch or export, and the observer and the
/// supervisor's headless jobs (ADR-0027, each job by its own role and the
/// legacy `reviewer` as a review job) keep their limits (ADR-0044
/// decision 4). A refusal is recorded as `authorization_denied` on the
/// queue `db` names, opened only then (task 1151). `request add` also uses
/// this check before reading its input; Requests::record still authorizes
/// before recording. A command allowed opens
/// nothing here, and reads keep the queue read-only (ADR-0073 decisions 5,
/// 7 and 18).
fn check_access(actor: &ActorContext, command: &Command, db: Option<&Path>) -> Result<()> {
    // `request add` must reject before reading words from a file or stdin.
    // Keep Requests::record's application check too; a refusal here returns
    // immediately, so only this Gate records it.
    let request_add = matches!(
        command,
        Command::Request {
            command: RequestCommand::Add { .. }
        }
    );
    if authorized_in_application(command) && !request_add {
        return Ok(());
    }
    let gate = dagq::application::commands::Gate {
        actor,
        authorizer: &StaticPolicy,
    };
    let log = CommandDenials { db, actor };
    for (capability, resource) in requests(command) {
        gate.authorize(&log, capability, &resource)?;
    }
    Ok(())
}

/// The queue a refused command names (`--db` or the repository of the
/// working directory), resolved only to record the refusal; with no queue
/// there, or one this binary cannot open, the refusal goes unrecorded and
/// is still a refusal.
struct CommandDenials<'a> {
    db: Option<&'a Path>,
    actor: &'a ActorContext,
}

impl dagq::application::commands::DenialLog for CommandDenials<'_> {
    fn record_denial(&self, payload: Value) -> Result<()> {
        let cwd = env::current_dir().context("working directory is unavailable")?;
        let location = QueueLocation::resolve(self.db, &cwd)?;
        dagq::infrastructure::denials::QueueDenials {
            db: &location.db,
            actor: self.actor,
        }
        .record_denial(payload)
    }
}

/// The error JSON of a failed command. A refusal carries what was refused
/// (`denied`: the role, the capability and why), and keeps the message the
/// observer and the headless jobs always got; a client-mode dagq's refusal
/// by the service's policy is printed the same, with the service's code.
fn error_json(error: &anyhow::Error) -> Value {
    // A client-mode dagq's failure carries the service's code or its own
    // (goal 82's stage (3)).
    if let Some(failure) =
        error.downcast_ref::<dagq::infrastructure::queue_service::ClientFailure>()
    {
        let mut printed =
            json!({"error": failure.message, "queue_service": {"code": failure.code}});
        // The outage the supervisor recorded of a service the host's `dagq`
        // did not reach (ADR-t3102-3 decision 2).
        if let Some(outage) = &failure.outage {
            printed["queue_service"]["outage"] = json!(outage);
        }
        if let Some(denied) = &failure.denied {
            if let Some(message) = denied.role.parse().ok().and_then(role_denied_message) {
                printed["error"] = json!(message);
            }
            printed["denied"] = json!({
                "role": denied.role,
                "capability": denied.capability,
                "reason": denied.reason,
            });
        }
        return printed;
    }
    let Some(denied) = error.downcast_ref::<AuthorizationError>() else {
        return json!({"error": format!("{error:#}")});
    };
    json!({
        "error": role_denied_message(denied.role).map_or_else(|| denied.to_string(), str::to_owned),
        "denied": {
            "role": denied.role,
            "capability": denied.capability,
            "reason": denied.reason.as_str(),
        },
    })
}

/// The message every refusal of `role` prints in place of the refusal's
/// own: the observer's and the headless jobs'.
fn role_denied_message(role: ActorRole) -> Option<&'static str> {
    if role == ActorRole::Observer {
        Some(OBSERVER_DENIED)
    } else if role.is_headless_job() {
        Some(REVIEWER_DENIED)
    } else {
        None
    }
}

/// Who the events this command writes record (ADR-t728-1 decision 4): the
/// caller, except the processes of the control plane the runtime starts in
/// an actor's environment: the supervisor, and the session wrappers and
/// hooks, whose events are the wrapper's rather than the session's.
fn event_actor(actor: &ActorContext, command: &Command) -> ActorContext {
    match command {
        Command::Supervise { .. } => {
            ActorContext::instance(ActorRole::Supervisor, std::process::id())
        }
        Command::Session { run, .. } => ActorContext::instance(ActorRole::Wrapper, run),
        Command::PlannerSession { planner, .. } => {
            ActorContext::instance(ActorRole::Wrapper, format_args!("planner:{planner}"))
        }
        // Named like the wrapper of its session: the run's, or the id of
        // an inbox or planner session.
        Command::SessionEvent { .. } => match actor.run_id() {
            Some(run) => ActorContext::instance(ActorRole::Wrapper, run),
            None => ActorContext::instance(ActorRole::Wrapper, actor.actor_id()),
        },
        _ => actor.clone(),
    }
}

/// The read of the queue `command` is, as the queue service answers it too
/// (ADR-t1233-5 decision 1): `None` for any other command.
fn queue_read(command: &Command) -> Option<QueueRead> {
    Some(match command.clone() {
        Command::List {
            status,
            all,
            goal_id,
            limit,
            before,
            full,
        } => QueueRead::List(reads::ListRead {
            status,
            all,
            goal: goal_id,
            limit,
            before,
            full,
        }),
        Command::Candidates { ignore_deferrals } => {
            QueueRead::Candidates(reads::CandidatesRead { ignore_deferrals })
        }
        Command::Graph {
            goal_id,
            format,
            out: None,
        } => QueueRead::Graph(reads::GraphRead {
            goal: goal_id,
            format,
        }),
        Command::Status { role } => QueueRead::Status(reads::RoleRead { role }),
        Command::Asks { open, role, all } => QueueRead::Asks(reads::AsksRead { open, role, all }),
        Command::Events {
            after,
            limit,
            all,
            full,
            run,
            task,
            goal,
            kind,
            since,
            until,
        } => QueueRead::Events(reads::EventsRead {
            after,
            limit,
            all,
            full,
            run,
            task,
            goal,
            kind,
            since,
            until,
        }),
        Command::Timeline { run, gap, full } => {
            QueueRead::Timeline(reads::TimelineRead { run, gap, full })
        }
        Command::Stats {
            since,
            until,
            goal_id,
            full,
            ledger,
            cmux: _,
        } => QueueRead::Stats(reads::StatsRead {
            since,
            until,
            goal: goal_id,
            full,
            ledger,
            git: None,
        }),
        Command::Kpi {
            period,
            last,
            at,
            since,
            until,
            changes,
            areas,
            by,
            cross,
            compare,
            window,
            goal_id,
        } => QueueRead::Kpi(reads::KpiRead {
            period,
            last,
            at,
            since,
            until,
            changes,
            areas,
            by,
            cross,
            compare,
            window,
            goal: goal_id,
            git: None,
        }),
        Command::Forecast {
            task_id,
            goal_id,
            parallel,
            trials,
        } => QueueRead::Forecast(reads::ForecastRead {
            task: task_id,
            goal: goal_id,
            parallel,
            trials,
        }),
        Command::Notes {
            goal_id,
            task_id,
            since,
            limit,
        } => QueueRead::Notes(reads::NotesRead {
            goal: goal_id,
            task: task_id,
            since,
            limit,
        }),
        Command::Marks { since, until } => QueueRead::Marks(reads::MarksRead { since, until }),
        Command::Findings {
            id,
            all,
            status,
            kinds,
            task,
            run,
            goal,
            queue,
            full,
        } => QueueRead::Findings(reads::FindingsRead {
            id,
            all,
            status,
            kinds,
            task,
            run,
            goal,
            queue,
            full,
        }),
        Command::Search {
            query,
            status,
            kinds,
            goal_id,
            limit,
            full,
        } => QueueRead::Search(reads::SearchRead {
            query,
            status,
            kinds,
            goal: goal_id,
            limit,
            full,
        }),
        Command::Related {
            task_id,
            status,
            limit,
        } => QueueRead::Related(reads::RelatedRead {
            task: task_id,
            status: status
                .iter()
                .map(|status| status.as_str().to_owned())
                .collect(),
            limit,
        }),
        Command::Goal {
            command: GoalCommand::List { tags },
        } => QueueRead::GoalList(reads::GoalListRead { tag: tags }),
        Command::Goal {
            command: GoalCommand::Show { id, full },
        } => QueueRead::GoalShow(reads::GoalShowRead { id, full }),
        Command::Lint { tasks, proposals } => QueueRead::Lint(reads::LintRead { tasks, proposals }),
        Command::Observe {
            history: true,
            limit,
            ..
        } => QueueRead::ObserveHistory(reads::ObserveHistoryRead {
            limit: limit.unwrap_or(dagq::application::observer::HISTORY_LIMIT),
        }),
        Command::Observe {
            input: Some(observation),
            section,
            offset,
            limit,
            ..
        } => QueueRead::ObserveInput(reads::ObserveInputRead {
            observation,
            section,
            offset,
            limit: limit.unwrap_or(dagq::application::observer::INPUT_PAGE),
        }),
        Command::Requests { id, all } => QueueRead::Requests(reads::RecordsRead { id, all }),
        Command::Replans { id, all } => QueueRead::Replans(reads::RecordsRead { id, all }),
        Command::Ci {
            command: CiCommand::Failures { task },
        } => QueueRead::CiFailures(reads::CiFailuresRead { task }),
        Command::Planners { all, cmux: _ } => QueueRead::Planners(reads::PlannersRead { all }),
        _ => return None,
    })
}

/// The host's dialogue or operation command `command` is, as the queue
/// service's use case takes it and as the command line runs it on the DB
/// (`compose::run_command`, ADR-t3102-2 decision 6 (4)): `None` for any
/// other command. A path is made absolute from the working directory, what
/// clap or the command line refuses before it runs is refused here, and the
/// words of `planner request` are read (`--text-file`, `-`), so it is built
/// once for a command line.
fn queue_command(command: &Command) -> Result<Option<QueueCommand>> {
    use dagq::application::queue_commands as commands;
    let cwd = || env::current_dir().context("working directory is unavailable");
    let run = |run: &String| commands::RunParams { run: run.clone() };
    let planner = |planner: &i64| commands::PlannerParams { planner: *planner };
    Ok(Some(match command {
        Command::Answer { id, text } => QueueCommand::Answer(commands::AnswerParams {
            id: *id,
            text: text.clone(),
        }),
        Command::Ask {
            command: Some(AskCommand::Close { id }),
            ..
        } => QueueCommand::AskClose(commands::AskCloseParams { id: *id }),
        Command::Hold {
            target,
            reason,
            until,
        } => QueueCommand::Hold(commands::HoldParams {
            target: target.clone(),
            reason: reason.clone(),
            until: hold_until(*until)?,
        }),
        Command::Release {
            target,
            provider,
            model,
        } => QueueCommand::Release(commands::ReleaseParams {
            target: target.clone(),
            provider: provider.clone(),
            model: model.clone(),
            // Read and kept here before it is sent (`call_service`).
            work: None,
        }),
        Command::Replan {
            command:
                ReplanCommand::Request {
                    run,
                    reason,
                    evidence,
                },
        } => QueueCommand::ReplanRequest(commands::ReplanRequestParams {
            run: run.clone(),
            reason: reason.clone(),
            evidence: evidence.clone(),
        }),
        // The checkout's Git is read here, as none runs in the service; a
        // caller the policy does not grant `rebind` reads none and sends
        // none, and the service refuses and records it.
        Command::Rebind { repo } => {
            let git_common_dir = if granted_by_its_role(Capability::QueueAdmin) {
                Some(dagq::compose::repository_common_dir(&rebind_checkout(
                    repo.as_deref(),
                )?)?)
            } else {
                None
            };
            QueueCommand::Rebind(commands::RebindParams {
                repository_queue_dir: git_common_dir
                    .as_deref()
                    .and_then(dagq::compose::repository_queue_dir),
                git_common_dir,
            })
        }
        Command::Recover { run: id } => QueueCommand::Recover(run(id)),
        Command::Run { command } => match command {
            RunCommand::Screen { run: id, .. } => QueueCommand::RunScreen(run(id)),
            RunCommand::Send { run: id, .. } => QueueCommand::RunSend(run(id)),
            RunCommand::Log { run: id, .. } => QueueCommand::RunLog(run(id)),
            RunCommand::CloseWorkspaces { .. } => return Ok(None),
        },
        Command::Planner { command } => match command {
            PlannerCommand::Screen { planner: id, .. } => QueueCommand::PlannerScreen(planner(id)),
            PlannerCommand::Send { planner: id, .. } => QueueCommand::PlannerSend(planner(id)),
            PlannerCommand::Log { planner: id, .. } => QueueCommand::PlannerLog(planner(id)),
            PlannerCommand::Request {
                planner: id,
                text,
                text_file,
                cmux: _,
            } => QueueCommand::PlannerRequest(commands::PlannerRequestParams {
                planner: *id,
                // A caller the policy does not grant it is refused before a
                // file or stdin is read: it sends no words, and the service
                // refuses and records it before it reads the params' text.
                text: if granted_by_its_role(Capability::PlannerRequest) {
                    request_words("--text", text.clone(), text_file.clone(), &cwd()?)?
                        .expect("clap requires --text or --text-file")
                } else {
                    String::new()
                },
            }),
        },
        Command::Report {
            period,
            at,
            out,
            print,
        } => QueueCommand::Report(commands::ReportParams {
            period: period.clone(),
            at: at.map(dagq::domain::stats::Cursor::text),
            out: out
                .as_ref()
                .map(|out| cwd().map(|cwd| cwd.join(out)))
                .transpose()?,
            print: print.is_some(),
            // Read and drawn here before it is sent (`call_service`).
            diagram: None,
            checkout: None,
            areas: None,
        }),
        Command::Graph {
            goal_id,
            format,
            out: Some(_),
        } => {
            anyhow::ensure!(format != "json", "--out needs --format d2 or svg");
            QueueCommand::GraphExport(commands::GraphExportParams { goal: *goal_id })
        }
        _ => return Ok(None),
    }))
}

/// Whether the role the caller's environment names (none: the user's) is
/// granted `capability` by the [`StaticPolicy`]: only what spares the
/// client a read the service would refuse anyway, which decides on its own
/// side for the principal of the request. A role that does not read is
/// granted nothing.
fn granted_by_its_role(capability: Capability) -> bool {
    ActorContext::from_env(|name| env::var(name).ok())
        .is_ok_and(|actor| dagq::domain::authorization::grants(actor.role()).contains(&capability))
}

/// What the command line prints of `answer`, the answer of the
/// [`queue_command`] of `command`, whether the service or the DB answered
/// it: a log's background session is printed (and followed) from its file,
/// and the diagram of `graph --out` is drawn by the host's d2 (`svg`) and
/// written, as the command line printed and wrote them; any other answer
/// as it is.
fn finish_command(command: &Command, answer: Value) -> Result<Value> {
    match command {
        // The queue is bound; the run worktrees it lists get their Git link
        // from the new repository here (ADR-t3102-3 decision 5).
        Command::Rebind { repo } => {
            dagq::compose::repair_rebound_worktrees(&rebind_checkout(repo.as_deref())?, answer)
        }
        Command::Run {
            command: RunCommand::Log { lines, follow, .. },
        }
        | Command::Planner {
            command: PlannerCommand::Log { lines, follow, .. },
        } => {
            let session = serde_json::from_value(answer["session"].clone())
                .context("the log's answer names no background session")?;
            print_session_log(
                &dagq::application::session_log::SessionLog { session },
                *lines,
                *follow,
            )
        }
        Command::Graph {
            format,
            out: Some(out),
            ..
        } => {
            let source = answer["source"]
                .as_str()
                .context("the diagram's answer holds no d2 source")?;
            let text = dagq::compose::graph_export_text(source, format)?;
            let out = env::current_dir()
                .context("working directory is unavailable")?
                .join(out);
            std::fs::write(&out, &text).with_context(|| format!("write {}", out.display()))?;
            Ok(json!({
                "format": format,
                "out": out,
                "tasks": answer["tasks"],
            }))
        }
        _ => Ok(answer),
    }
}

/// The checkout `rebind` binds the queue to: a repository queue already
/// resolved the working directory; `--repo` overrides it for a `--db` queue
/// used from elsewhere or a moved checkout.
fn rebind_checkout(repo: Option<&Path>) -> Result<PathBuf> {
    let cwd = env::current_dir().context("working directory is unavailable")?;
    Ok(match repo {
        Some(repo) => cwd.join(repo),
        None => cwd,
    })
}

/// What `client` answers `command`, sent as the use case and params of
/// [`client_request`], printed as [`finish_command`] prints it: a `report`
/// and a `release` first ask what to read, and read it with Git and draw
/// here before they are sent ([`dagq::compose::prepare_command`]), for the
/// queue at `db` when the caller knows it; so do `stats` and `kpi`
/// ([`dagq::compose::call_stats_read`], sent again while the service finds
/// the Git reads older than the queue's events).
fn call_service(
    client: &dagq::infrastructure::queue_service::Client,
    db: Option<&Path>,
    command: &Command,
    use_case: UseCase,
    params: Value,
) -> Result<Value> {
    let params = match command {
        Command::Report { .. } | Command::Release { .. } => {
            let queued = queue_command(command)?.context("the command is one of the service's")?;
            dagq::compose::prepare_command(queued, db, |first| {
                let (use_case, params) = first.request();
                client.call(use_case, params)
            })?
            .request()
            .1
        }
        Command::Stats { .. } | Command::Kpi { .. } => {
            let read = queue_read(command).context("the command is one of the service's")?;
            return finish_command(
                command,
                dagq::compose::call_stats_read(&read, |sent| {
                    let (use_case, params) = sent.request();
                    client.call(use_case, params)
                })?,
            );
        }
        _ => params,
    };
    finish_command(command, client.call(use_case, params)?)
}

/// The use case and params of the queue service a client-mode `dagq`
/// sends `command` as (goal 82's stage (3), ADR-t1233-1 decision 7): the
/// same options the service reads back as the command line's. `None` for
/// a command the service has no use case for. A `--cmux` ([`RetiredCmux`])
/// is not sent.
/// The planner a proposal submitted on the DB directly belongs to: the
/// origin the turn's environment names (a person's when it names none), and
/// the background handle of the planner it names (ADR-t1404-1 decision
/// 10). The terminal's variables name none (ADR-t2159-1 decision 1). The
/// queue service finds it from the caller's principal instead
/// ([`dagq::application::queue_writes::principal_owner`]).
fn environment_owner(queue: &dagq::infrastructure::sqlite::SqliteQueue) -> Result<PlannerOwner> {
    use dagq::application::lifecycle::{PLANNER_ID_ENV, PLANNER_ORIGIN_ENV};
    let origin = match env::var(PLANNER_ORIGIN_ENV) {
        Ok(origin) if !origin.is_empty() => origin.parse()?,
        _ => PlannerOrigin::Person,
    };
    let workspace_id = env::var(PLANNER_ID_ENV)
        .ok()
        .and_then(|planner| planner.trim().parse().ok())
        .and_then(|planner| queue.planner(PlannerId::new(planner)).ok())
        .and_then(|planner| planner.workspace_id)
        .filter(|id| dagq::domain::background_wrapper::is_background(id));
    Ok(PlannerOwner {
        origin,
        workspace_id,
    })
}

/// The planning command `command` is, as the queue service runs it too
/// (ADR-t3102-3 decision 3): `None` for any other command, and for `ready
/// --inherit`. The words of `request add` are read here, from its files
/// (relative to the working directory) or stdin, so they are read once
/// and the service gets what they held.
fn queue_write(command: &Command) -> Result<Option<QueueWrite>> {
    use dagq::application::queue_writes as writes;
    Ok(Some(match command.clone() {
        Command::Add {
            title,
            description,
            acceptance,
            verification_commands,
            dependencies,
            goal_dependencies,
            goal_id,
            context,
            required_evidence,
            paths,
            priority,
            change,
            provider,
            headless,
            interactive,
            wait_for_build,
            execution_class,
        } => QueueWrite::Add(writes::AddWrite {
            title,
            description,
            acceptance,
            verify: verification_commands,
            depends_on: dependencies,
            depends_on_goal: goal_dependencies,
            goal: goal_id,
            context,
            evidence: required_evidence,
            paths,
            priority,
            change,
            provider,
            headless,
            interactive,
            wait_for_build,
            execution_class,
        }),
        Command::Edit {
            task,
            title,
            description,
            acceptance,
            context,
            verification_commands,
            no_verify,
            required_evidence,
            no_evidence,
            paths,
            no_paths,
            change,
            provider,
            headless,
            interactive,
            wait_for_build,
            no_wait_for_build,
            execution_class,
        } => QueueWrite::Edit(writes::EditWrite {
            task,
            title,
            description,
            acceptance,
            context,
            verify: verification_commands,
            no_verify,
            evidence: required_evidence,
            no_evidence,
            paths,
            no_paths,
            change,
            provider,
            headless,
            interactive,
            wait_for_build,
            no_wait_for_build,
            execution_class,
        }),
        Command::Submit {
            tasks,
            goals,
            proposal,
            findings,
            replacement,
        } => QueueWrite::Submit(writes::SubmitWrite {
            tasks,
            goal: goals,
            proposal,
            finding: findings,
            replacement: replacement
                .map(|path| {
                    std::fs::read_to_string(&path)
                        .with_context(|| format!("read the replacement plan {}", path.display()))
                })
                .transpose()?,
        }),
        Command::Cancel {
            id,
            reason,
            duplicate_of,
            note,
        } => QueueWrite::Cancel(writes::CancelWrite {
            id,
            reason,
            duplicate_of,
            note,
        }),
        Command::Dependency { command } => match command {
            DependencyCommand::Add {
                task,
                predecessor,
                goal,
            } => QueueWrite::DependencyAdd(writes::DependencyWrite {
                task,
                predecessor,
                goal,
            }),
            DependencyCommand::Remove {
                task,
                predecessor,
                goal,
            } => QueueWrite::DependencyRemove(writes::DependencyWrite {
                task,
                predecessor,
                goal,
            }),
        },
        Command::SetGoal { task, goal, .. } => {
            QueueWrite::SetGoal(writes::SetGoalWrite { task, goal })
        }
        Command::SetPaths { task, paths, .. } => {
            QueueWrite::SetPaths(writes::SetPathsWrite { task, paths })
        }
        Command::SetPriority { task, level, .. } => {
            QueueWrite::SetPriority(writes::SetPriorityWrite { task, level })
        }
        Command::Revisit { task, at, note, .. } => {
            QueueWrite::Revisit(writes::RevisitWrite { task, at, note })
        }
        Command::JudgeFollowUp {
            task,
            classification,
            acceptance_items,
            reason,
            evidence,
            destination_goal,
            destination_acceptance_items,
            source_goal,
            corrects,
        } => QueueWrite::JudgeFollowUp(writes::JudgeFollowUpWrite {
            task,
            classification,
            acceptance_item: acceptance_items,
            reason,
            evidence,
            destination_goal,
            destination_acceptance_item: destination_acceptance_items,
            source_goal,
            corrects,
        }),
        Command::Draft { id } => QueueWrite::Draft(writes::IdWrite { id }),
        Command::Ready { inherit: true, .. } => return Ok(None),
        Command::Ready {
            id, bypass_review, ..
        } => QueueWrite::Ready(writes::ReadyWrite { id, bypass_review }),
        Command::Goal { command } => match command {
            GoalCommand::Add {
                title,
                description,
                acceptance,
                constraints,
                doc,
                draft,
                priority,
                tags,
            } => QueueWrite::GoalAdd(writes::GoalAddWrite {
                title,
                description,
                acceptance,
                constraints,
                doc,
                draft,
                priority,
                tag: tags,
            }),
            GoalCommand::Edit {
                id,
                title,
                description,
                acceptance,
                constraints,
                doc,
                priority,
                tags,
                no_tags,
            } => QueueWrite::GoalEdit(writes::GoalEditWrite {
                id,
                title,
                description,
                acceptance,
                constraints,
                doc,
                priority,
                tag: tags,
                no_tags,
            }),
            GoalCommand::Close { id, verdict } => {
                QueueWrite::GoalClose(writes::GoalCloseWrite { id, verdict })
            }
            GoalCommand::Ready { id } => QueueWrite::GoalReady(writes::IdWrite { id }),
            GoalCommand::Review { id } => QueueWrite::GoalReview(writes::IdWrite { id }),
            GoalCommand::List { .. } | GoalCommand::Show { .. } => return Ok(None),
        },
        Command::Proposal {
            command: ProposalCommand::Withdraw { id },
        } => QueueWrite::ProposalWithdraw(writes::IdWrite { id }),
        Command::Request {
            command:
                RequestCommand::Add {
                    text,
                    text_file,
                    note,
                    note_file,
                    refs,
                    priority,
                },
        } => {
            // A role that may not record a request reads no file and no
            // stdin: the service refuses it, and records the refusal, for
            // the principal it is.
            // A role that does not read (none it knows) counts as one that
            // may not.
            let granted = ActorContext::from_env(|name| env::var(name).ok()).is_ok_and(|actor| {
                dagq::domain::authorization::grants(actor.role())
                    .contains(&Capability::RequestRecord)
            });
            if !granted {
                return Ok(Some(QueueWrite::RequestAdd(writes::RequestAddWrite {
                    text: None,
                    note: None,
                    refs,
                    priority,
                })));
            }
            if text.as_deref() == Some("-") && note.as_deref() == Some("-") {
                bail!("--text - and --note - both read stdin; give one of them as a file");
            }
            let cwd = env::current_dir().context("working directory is unavailable")?;
            let text = request_words("--text", text, text_file, &cwd)?
                .expect("clap requires --text or --text-file");
            let note = request_words("--note", note, note_file, &cwd)?;
            QueueWrite::RequestAdd(writes::RequestAddWrite {
                text: Some(text),
                note,
                refs,
                priority,
            })
        }
        Command::Request {
            command:
                RequestCommand::Decline {
                    id,
                    reason,
                    continue_run,
                },
        } => QueueWrite::RequestDecline(writes::RequestDeclineWrite {
            id,
            reason,
            continue_run,
        }),
        _ => return Ok(None),
    }))
}

fn client_request(command: &Command) -> Result<Option<(UseCase, Value)>> {
    if let Some(read) = queue_read(command) {
        return Ok(Some(read.request()));
    }
    if let Some(write) = queue_write(command)? {
        return write.request().map(Some);
    }
    if let Some(command) = queue_command(command)? {
        return Ok(Some(command.request()));
    }
    Ok(Some(match command {
        Command::Mark {
            label,
            note,
            at,
            retract,
        } => (
            UseCase::Mark,
            json!({"label": label, "note": note, "at": at.map(|at| at.text()),
                   "retract": retract}),
        ),
        Command::Show { id, full, events } => (
            UseCase::Show,
            json!({"id": id, "full": full, "events": events}),
        ),
        Command::Note {
            task,
            run,
            goal,
            text,
            kind,
        } => (
            UseCase::Note,
            json!({"task": task, "run": run, "goal": goal, "text": text, "kind": kind}),
        ),
        Command::Ask {
            command: None,
            kind,
            question,
            options,
            because,
            topics,
            recommend,
            confidence,
            task_id,
            run,
            finding,
            request,
            cmux: _,
        } => {
            let mut params = json!({"kind": kind, "question": question, "options": options,
                "because": because, "topics": topics, "task_id": task_id, "run_id": run,
                "finding_id": finding});
            // Only when given: a service of an older build refuses a field it
            // does not know (ADR-t451-1 decision 1).
            for (key, value) in [("recommend", recommend), ("confidence", confidence)] {
                if let Some(value) = value {
                    params[key] = json!(value);
                }
            }
            // A planner's ask on its request (ADR-t3102-1 decision 2).
            if let Some(request) = request {
                params["request_id"] = json!(request);
            }
            (UseCase::Ask, params)
        }
        Command::Agent { command } => match command {
            AgentCommand::Eval {
                agent,
                split,
                k,
                cases,
                set,
                rerun,
            } => {
                if set.is_some() && !cases.is_empty() {
                    bail!(dagq::application::agent_eval::SET_WITH_CASES);
                }
                let mut params = json!({"agent": agent, "split": split, "k": k,
                       "cases": (!cases.is_empty()).then_some(cases), "rerun": rerun});
                // Only when given: a service of an older build refuses a
                // field it does not know (ADR-t451-1 decision 1).
                if let Some(set) = set {
                    params["set"] = json!(set);
                }
                (UseCase::AgentEval, params)
            }
            AgentCommand::Cases {
                agent,
                since,
                until,
            } => (
                UseCase::AgentCases,
                json!({"agent": agent, "since": since, "until": until}),
            ),
            AgentCommand::Decide {
                set,
                case,
                verdict,
                codes,
                reason,
            } => (
                UseCase::AgentDecide,
                json!({"set": set, "case": case, "verdict": verdict, "codes": codes, "reason": reason}),
            ),
            AgentCommand::Sets { agent, limit } => {
                (UseCase::AgentSets, json!({"agent": agent, "limit": limit}))
            }
            AgentCommand::Set { id } => (UseCase::AgentSet, json!({"id": id})),
            AgentCommand::Results { agent, limit } => (
                UseCase::AgentResults,
                json!({"agent": agent, "limit": limit}),
            ),
            AgentCommand::Result { id } => (UseCase::AgentResult, json!({"id": id})),
        },
        // A worker's proposal on its own run (ADR-t1521-2 decision 3).
        Command::Replan {
            command:
                ReplanCommand::Propose {
                    run,
                    reason,
                    evidence,
                },
        } => (
            UseCase::ReplanPropose,
            json!({"run": run, "reason": reason, "evidence": evidence}),
        ),
        Command::Proposal {
            command: ProposalCommand::List { all },
        } => (UseCase::ProposalList, json!({"all": all})),
        Command::Proposal {
            command: ProposalCommand::Show { id },
        } => (UseCase::ProposalShow, json!({"id": id})),
        Command::Finding {
            command:
                FindingCommand::Record {
                    kind,
                    task,
                    run,
                    goal,
                    queue: _,
                    subject,
                    summary,
                    detail,
                    impact,
                    evidence,
                    propose,
                },
        } => (
            UseCase::FindingRecord,
            json!({"kind": kind, "task": task, "run": run, "goal": goal,
                   // No target is the queue's, as the command line takes it.
                   "queue": task.is_none() && run.is_none() && goal.is_none(),
                   "subject": subject, "summary": summary, "detail": detail,
                   "impact": impact, "evidence": evidence, "propose": propose}),
        ),
        Command::Finding {
            command: FindingCommand::Resolve { id, reason },
        } => (UseCase::FindingResolve, json!({"id": id, "reason": reason})),
        Command::Finding {
            command:
                FindingCommand::Dismiss {
                    id,
                    reason,
                    covered_by,
                },
        } => (
            UseCase::FindingDismiss,
            // `covered_by` only when given: an older service refuses a
            // key it does not know.
            match covered_by {
                Some(task) => json!({"id": id, "reason": reason, "covered_by": task}),
                None => json!({"id": id, "reason": reason}),
            },
        ),
        _ => return Ok(None),
    }))
}

/// Run `cli` in client mode (goal 82's stage (3), ADR-t1233-1 decision
/// 7): as the use case of the queue service at `client`, for the principal
/// of its token, which the service authorizes on its side whatever role
/// the environment names. No queue is opened: a command that names one
/// (`--db`) or is none of the service's use cases is refused, and so is
/// every command when the service does not answer (fail closed).
fn client_mode(client: &dagq::infrastructure::queue_service::Client, cli: &Cli) -> Result<Value> {
    use dagq::infrastructure::queue_service::ClientFailure;
    if let Some(db) = &cli.db {
        return Err(ClientFailure::new(
            "queue_named",
            format!(
                "--db {} names a queue to open, and a client-mode dagq (DAGQ_SERVICE_SOCKET or \
                 DAGQ_SERVICE_URL is set) opens none: run the command without it",
                db.display()
            ),
        )
        .into());
    }
    if let Some(name) = control_side_command(&cli.command) {
        return Err(ClientFailure::new(
            "control_side_command",
            format!(
                "dagq {name} is a control-side command: a person or the inbox runs it on the \
                 host, and a client-mode dagq (DAGQ_SERVICE_SOCKET or DAGQ_SERVICE_URL is set) \
                 neither opens the queue for it nor sends it to the queue service"
            ),
        )
        .into());
    }
    // `locate` says where the commands go, without the queue's path, and
    // whether the service answers there: the plugin's `--resolve` runs it
    // first.
    if matches!(cli.command, Command::Locate) {
        use dagq::infrastructure::queue_service::ClientRoute;
        let mut located = match &client.route {
            ClientRoute::Socket(socket) => json!({
                "client_mode": true,
                "socket": socket,
                "db": null,
                "db_exists": null,
                "note": "dagq runs in client mode: its commands go to the queue service at the \
                         socket, and it opens no queue",
            }),
            ClientRoute::Broker { address, .. } => json!({
                "client_mode": true,
                "broker": address,
                "db": null,
                "db_exists": null,
                "note": "dagq runs in client mode: its commands go to the queue service through \
                         the queue broker, and it opens no queue",
            }),
        };
        if let (Value::Object(located), Value::Object(reach)) = (&mut located, client.reach()) {
            located.extend(reach);
        }
        return Ok(located);
    }
    let Some((use_case, params)) = client_request(&cli.command)? else {
        return Err(ClientFailure::new(
            "no_use_case",
            "this command is not one of the queue service's use cases, and a client-mode dagq \
             (DAGQ_SERVICE_SOCKET or DAGQ_SERVICE_URL is set) opens no queue to run it \
             (docs/design/queue-service.md lists the use cases)",
        )
        .into());
    };
    call_service(client, None, &cli.command, use_case, params)
}

/// Whether the host `dagq` of a
/// [`host_client_role`](dagq::domain::queue_service::host_client_role)
/// sends `command` to the service of a served queue: a command the service
/// answers ([`client_request`]), `watch`, whose looks it answers, and
/// `service status`, whose attention it reads.
/// `planner request` and `rebind` are ones, told here without reading the
/// words or the checkout's Git, which are read once, as it is sent.
fn host_routed(command: &Command) -> Result<bool> {
    // `request add` reads its words once, when its request is made.
    let request_add = matches!(
        command,
        Command::Request {
            command: RequestCommand::Add { .. }
        }
    );
    Ok(matches!(
        command,
        Command::Watch { .. }
            | Command::Service {
                command: ServiceCommand::Status
            }
            | Command::Rebind { .. }
            | Command::Planner {
                command: PlannerCommand::Request { .. }
            }
    ) || request_add
        || client_request(command)?.is_some())
}

/// Run `command`, a [`host_routed`] one, through the service of the queue
/// at `db`, which the caller named `named`, bound to `binding` (ADR-t3102-2
/// decision 5), as the user (no token) or the principal of the token the
/// environment names: a service that does not answer, or never ran, is a
/// failure that says so, and the DB is not opened instead (fail closed,
/// ADR-t3102-3 decision 2, ADR-t3105-1 decision 3).
fn host_client(
    db: &Path,
    named: &Path,
    binding: Option<&Path>,
    command: &Command,
) -> Result<Value> {
    // A repository queue's binding is checked on every command, as the
    // command line checks it, by the service before it answers; `rebind` is
    // the one command that runs on a queue bound elsewhere.
    let binding = binding.filter(|_| !matches!(command, Command::Rebind { .. }));
    let common_dir = binding.map(path_text).transpose()?;
    let client =
        dagq::infrastructure::queue_service::Client::host(db, common_dir.as_deref(), |name| {
            env::var(name).ok()
        });
    if let Command::Watch {
        after,
        timeout,
        until_attention,
        interval,
        role,
        cmux: _,
    } = command
    {
        return dagq::compose::watch_through_service(
            db,
            named,
            &client,
            &dagq::application::watch::WatchOptions {
                after: after.map(EventId::new),
                timeout: (!until_attention)
                    .then(|| Duration::from_secs(timeout.unwrap_or(DEFAULT_WATCH_TIMEOUT_SECS))),
                interval: Duration::from_secs(*interval),
                role: parse_role(role.clone())?,
            },
        );
    }
    // `service status` answers whether or not the service does, and opens
    // no queue either way (ADR-t3102-3 decision 2): it looks in the
    // directory of the DB a link names, whose service serves it.
    if matches!(
        command,
        Command::Service {
            command: ServiceCommand::Status
        }
    ) {
        return Ok(dagq::compose::queue_service_status(db, &client));
    }
    // The service reads the diagram and the host's d2 draws it, as the
    // command line draws it: no read starts a program on the service's side.
    if let Command::Graph {
        goal_id,
        format,
        out: None,
    } = command
        && format == "svg"
    {
        let (use_case, params) = QueueRead::Graph(reads::GraphRead {
            goal: *goal_id,
            format: "d2".to_owned(),
        })
        .request();
        return dagq::compose::graph_svg_of(client.call(use_case, params)?);
    }
    let (use_case, params) =
        client_request(command)?.context("the command is none of the service's use cases")?;
    // `status` says at once that the service does not answer, as a look of
    // a watch does (ADR-t3102-3 decision 2).
    if matches!(command, Command::Status { .. }) {
        return client.look_within(use_case, params, dagq::application::watch::LOOK_LIMIT);
    }
    call_service(&client, Some(db), command, use_case, params)
}

/// The name of `command` when it is one of the control side's, which a
/// person or the inbox runs on the host and a client-mode `dagq` refuses
/// as `control_side_command` rather than as `no_use_case`: none of the
/// service's use cases (`service status` reads the service's socket and
/// files on the host beside its one read), so a command it answers is
/// sent to it and authorized on its side.
fn control_side_command(command: &Command) -> Option<&'static str> {
    match command {
        Command::Doctor { .. } => Some("doctor"),
        Command::Service {
            command: ServiceCommand::Status,
        } => Some("service status"),
        _ => None,
    }
}

/// Run the relay or the forwarder of a run's path to the queue broker, or
/// look through it (ADR-t2114-3 decisions 2 and 3).
fn relay(command: &RelayCommand) -> Result<Value> {
    use dagq::infrastructure::broker_relay;
    match command {
        RelayCommand::Serve {
            socket,
            target,
            owner,
        } => broker_relay::serve_relay(&broker_relay::RelayOptions {
            socket: socket.clone(),
            target: target.clone(),
            owner: *owner,
            stop: install_stop_signal()?,
            poll: Duration::from_millis(50),
        }),
        RelayCommand::Forward { socket, port } => {
            broker_relay::serve_forward(&broker_relay::ForwardOptions {
                port: *port,
                socket: socket.clone(),
                stop: install_stop_signal()?,
                poll: Duration::from_millis(50),
            })
        }
        RelayCommand::Probe { socket } => dagq::infrastructure::queue_broker::health_through(
            socket,
            dagq::infrastructure::queue_broker::PROBE_TIMEOUT,
        ),
        // The turn's standard output is the agent's: nothing is printed,
        // and the exit code is the agent's.
        RelayCommand::Turn {
            run_dir,
            clone,
            base,
            branch,
            facts_only: true,
            ..
        } => {
            dagq::infrastructure::container_turn::run_facts_turn(clone, run_dir, base, branch)?;
            std::process::exit(0)
        }
        RelayCommand::Turn {
            run_dir,
            clone,
            base,
            branch,
            agent,
            ..
        } => {
            let code = dagq::infrastructure::container_turn::run_turn(
                &dagq::infrastructure::container_turn::TurnOptions {
                    run_dir: run_dir.clone(),
                    clone: clone.clone(),
                    base: base.clone(),
                    branch: branch.clone(),
                    agent: agent.clone(),
                    port: dagq::domain::broker_relay::FORWARD_PORT,
                },
                |name| env::var(name).ok(),
            )?;
            std::process::exit(code)
        }
        RelayCommand::Guard { dir } => {
            let dagq = env::current_exe().context("find this dagq")?;
            let guard = dagq::infrastructure::sccache::link_guard(dir, &dagq)?;
            Ok(json!({"guard": guard.to_string_lossy()}))
        }
        // The command's output and exit code are the verification's.
        RelayCommand::Verify {
            token_file,
            command,
        } => {
            let code = dagq::infrastructure::container_turn::run_verification(
                &dagq::infrastructure::container_turn::VerifyOptions {
                    token_file: token_file.clone(),
                    command: command.clone(),
                    port: dagq::domain::broker_relay::FORWARD_PORT,
                },
                |name| env::var(name).ok(),
            )?;
            std::process::exit(code)
        }
    }
}

/// The observer's interval for `supervise`: the given seconds, else three
/// hours (ADR-0044 decision 21), or none for `--once`.
fn resolve_observe_interval(interval: Option<u64>, once: bool) -> Duration {
    // A one-shot pass observes only when asked to.
    Duration::from_secs(interval.unwrap_or(if once { 0 } else { 10800 }))
}

fn execute(cli: Cli) -> Result<Value> {
    // The relay and the forwarder of a run's container carry bytes and open
    // no queue (ADR-t2114-3): they run whatever the environment names,
    // the forwarder in a container whose `dagq` is in client mode.
    if let Command::Relay { command } = &cli.command {
        return relay(command);
    }
    // A built-in review program reads the worktree's Git and prints its
    // findings, and its exit code is its verdict: it opens no queue.
    if let Command::ReviewProgram { name, args } = &cli.command {
        std::process::exit(dagq::infrastructure::review_secrets::run(name, args));
    }
    // A Codex inbox's hook answers on its stdin and stdout; the `dagq` it
    // runs for the status and the span reach the queue as the inbox's.
    if let Command::CodexInboxHook { hook } = &cli.command {
        let hook = dagq::application::codex_inbox::InboxHook::from_argument(hook)
            .context("unknown Codex inbox hook")?;
        std::process::exit(dagq::infrastructure::codex_inbox::run_hook(hook));
    }
    // A worker's or a job's `dagq` goes to the queue service, which
    // authorizes it for the principal of its token: neither `DAGQ_ROLE`
    // nor the queue's path is read (goal 82's stage (3)). An environment
    // that names both a socket and a broker runs nothing (fail closed).
    if let Some(client) =
        dagq::infrastructure::queue_service::Client::from_env(|name| env::var(name).ok())?
    {
        return client_mode(&client, &cli);
    }
    // A planner's hook leaves its report for the planner's wrapper, which
    // records it as its own principal (ADR-t3102-1 decision 4): the hook
    // opens no queue and reaches no service. A client-mode `dagq` (an
    // agent's) was refused above and leaves none.
    let hook_kind = match &cli.command {
        Command::SessionEvent { event, .. } => {
            let kind = session_hook_kind();
            if kind.is_some_and(|kind| kind != dagq::domain::sessions::INBOX)
                && let Some(hook) = read_session_hook(event)?
            {
                return spool_planner_hook(cli.db.as_deref(), &hook);
            }
            kind
        }
        _ => None,
    };
    // A session wrapper reads and records through the queue service as
    // the wrapper's principal, whose token the supervisor issued before it
    // started it (ADR-t3102-1 decision 4): it opens no queue, and runs as
    // no actor its environment names (the agent's, which it inherits).
    if matches!(
        cli.command,
        Command::Session { .. } | Command::PlannerSession { .. }
    ) {
        return session_wrapper(cli);
    }
    // Who runs the command (ADR-t728-1 decision 4): no `DAGQ_ROLE` is the
    // user, and a value that is no role stops it before anything is read.
    let actor = ActorContext::from_env(|name| env::var(name).ok())?;
    // The user's, the inbox's or a planner's `dagq` sends what the queue
    // service answers to the queue's service, which authorizes it for its
    // principal, and never opens the DB for it: a service that does not
    // run, or never ran, is a failure that says so (ADR-t3102-2 decisions
    // 5 and 6 (2), ADR-t3105-1 decision 3).
    if dagq::domain::queue_service::host_client_role(actor.role()) && host_routed(&cli.command)? {
        let cwd = env::current_dir().context("working directory is unavailable")?;
        let location = QueueLocation::resolve(cli.db.as_deref(), &cwd)?;
        // `service status` reads no DB, and answers where there is none.
        let status = matches!(
            cli.command,
            Command::Service {
                command: ServiceCommand::Status
            }
        );
        anyhow::ensure!(
            status || location.db.is_file(),
            "no queue at {} (use init to create it)",
            location.db.display()
        );
        // The DB a link names is its target's: its directory's service
        // serves it.
        let db = std::fs::canonicalize(&location.db).unwrap_or_else(|_| location.db.clone());
        // `rebind` records what it did in its own log as the command line
        // did (ADR-0033 decision 2), from the service's answer.
        let rebind = matches!(cli.command, Command::Rebind { .. });
        if rebind {
            install_telemetry(&cli.command, &location);
        }
        let result = host_client(
            &db,
            &location.db,
            location.git_common_dir.as_deref(),
            &cli.command,
        );
        if rebind {
            dagq::compose::trace_command("rebind", &result);
        }
        return result;
    }
    check_access(&actor, &cli.command, cli.db.as_deref())?;
    dagq::infrastructure::event_actor::set_process_actor(event_actor(&actor, &cli.command));
    let cwd = env::current_dir().context("working directory is unavailable")?;
    let location = QueueLocation::resolve(cli.db.as_deref(), &cwd)?;
    let db = location.db.clone();
    // The runtime operations are authorized before they do anything; a
    // refusal is recorded on the queue if there is one (task 734).
    if let Some(operation) = operation(&cli.command, |name| env::var(name).ok()) {
        dagq::application::commands::operations::authorize(
            &actor,
            &StaticPolicy,
            &dagq::infrastructure::denials::QueueDenials {
                db: &location.db,
                actor: &actor,
            },
            &operation,
        )?;
    }
    install_telemetry(&cli.command, &location);
    // The clock and IDs of every queue and use case this command runs.
    let generators = dagq::infrastructure::clock::system();
    let one_shot = dagq::compose::OneShot {
        user_config: dagq::infrastructure::language::user_config_file(),
        ..dagq::compose::OneShot::new(generators.clone())
    };
    // The binding is checked on every command of a repository queue; a `--db`
    // queue is bound by its first `supervise` and checked there and by `integrate`.
    let common_dir = location
        .git_common_dir
        .as_deref()
        .map(path_text)
        .transpose()?;
    if matches!(cli.command, Command::Locate) {
        let mut value = serde_json::to_value(&location)?;
        value["db_exists"] = json!(db.is_file());
        return Ok(value);
    }
    if matches!(cli.command, Command::Init) {
        location.prepare()?;
        let mut queue =
            dagq::compose::init_queue(&db, &env::current_exe()?)?.with_generators(generators);
        if let Some(common_dir) = &common_dir {
            queue.bind_repository(common_dir)?;
        }
        return Ok(json!({
            "db": db,
            "schema_version": queue.schema_version()?,
            "source": location.source,
            "git_common_dir": common_dir,
            "recommended": dagq::compose::init_recommended(
                &cwd,
                &dagq::domain::toolchain::BUILTIN,
            ),
        }));
    }
    if let Command::SetupReview {
        yes,
        timeout,
        claude,
        codex,
        codex_home,
    } = &cli.command
    {
        if !db.is_file() {
            bail!(
                "no queue at {}: run `dagq init` first; setup-review makes its temporary \
                 checkout under the queue's directory",
                db.display()
            );
        }
        use dagq::application::setup_review::Approval;
        return dagq::compose::setup_review(
            &db,
            &cwd,
            &dagq::compose::SetupReviewOptions {
                claude: claude.clone(),
                codex: codex.clone(),
                codex_home: codex_home.clone(),
                timeout: Duration::from_secs(*timeout),
                approval: if *yes { Approval::Given } else { Approval::Ask },
                dagq: env::current_exe()?,
                profiles: &dagq::domain::toolchain::BUILTIN,
            },
        );
    }
    if let Command::Migrate { check } = cli.command {
        if check {
            return Ok(serde_json::to_value(dagq::compose::queue_schema(&db)?)?);
        }
        let report =
            dagq::compose::migrate_queue(&db, &env::current_exe()?, generators.clock.now())?;
        let mut value = serde_json::to_value(report)?;
        // A landing recorded without its message (ADR-0046 decision 3).
        if let Ok(mut queue) = dagq::compose::open_queue(&db) {
            let fallback = common_dir.clone();
            value["commit_messages_filled"] =
                json!(queue.fill_commit_messages(|dir, commit| {
                    let dir = dir.map(str::to_owned).or_else(|| fallback.clone())?;
                    dagq::infrastructure::adapters::commit_message(
                        std::path::Path::new(&dir),
                        commit,
                    )
                })?);
        }
        value["db"] = json!(db);
        return Ok(value);
    }
    if let Command::Install {
        from,
        release,
        cargo,
        to,
        rollback,
        allow_breaking,
        handoff_timeout,
        cmux: _,
        claude,
        codex,
        plugin_dir,
        skip_e2e,
        e2e_command,
        e2e_timeout,
        poll_ms,
        watch_timeout,
    } = cli.command
    {
        use dagq::application::install::{E2eGate, E2eSettings, InstallOptions, Source};
        use dagq::infrastructure::{adapters::executable, launchd::Launchctl};
        let source = match (rollback, from) {
            (true, _) => Source::Rollback,
            // Replaced by the release's binary below.
            (false, _) if release.is_some() => Source::Rollback,
            (false, Some(from)) if from.is_file() => Source::Binary(from),
            (false, Some(from)) => Source::Checkout(from),
            (false, None) => {
                location.git_common_dir.as_deref().context(
                    "not in a repository: pass --from with a checkout or a built binary",
                )?;
                let checkout = dagq::infrastructure::adapters::main_checkout_of(&cwd)
                    .context("the checkout to build is unknown: pass --from with a checkout or a built binary")?;
                let dagq_source = dagq::infrastructure::adapters::is_dagq_source(&checkout);
                Source::default_checkout(checkout, dagq_source)?
            }
        };
        let restart = dagq::compose::install_restart_arguments(
            claude.as_deref(),
            codex.as_deref(),
            plugin_dir.as_deref(),
        )?;
        // A checkout of dagq's source passes its e2e before it is put in
        // place, unless a person skips it (ADR-t963-1 decision 1).
        let e2e = match &source {
            Source::Checkout(checkout)
                if !skip_e2e && dagq::infrastructure::adapters::is_dagq_source(checkout) =>
            {
                E2eGate::Run(E2eSettings {
                    command: e2e_command,
                    timeout: Duration::from_secs(e2e_timeout),
                    run_env_root: Some(checkout.clone()),
                    queue_dir: Some(location.queue_dir.clone()),
                    scratch: dagq::application::update::UpdatePaths::under(&location.queue_dir).e2e,
                    log: location
                        .log_dir
                        .join(format!("install-{}.e2e.log", generators.clock.now())),
                    utc_offset_secs: dagq::infrastructure::clock::local_utc_offset(
                        generators.clock.now(),
                    ),
                    lock: dagq::application::install::e2e_lock_path(&location.queue_dir),
                    git_env: Vec::new(),
                    ended: None,
                })
            }
            Source::Checkout(_) if skip_e2e => E2eGate::Skip,
            _ => E2eGate::NotApplicable,
        };
        let options = InstallOptions {
            source,
            target: match to {
                Some(to) => cwd.join(to),
                None => env::current_exe()?,
            },
            allow_breaking,
            restart,
            handoff_timeout: Duration::from_secs(handoff_timeout),
            poll: Duration::from_millis(poll_ms),
            e2e,
        };
        if let Some(release) = release {
            return one_shot.install_release(
                &location,
                &Launchctl { uid: current_uid() },
                &dagq::infrastructure::release_update::CurlIndex::default(),
                &executable(&cargo).unwrap_or(cargo),
                Some(release.as_str()).filter(|version| !version.is_empty()),
                &options,
                Duration::from_secs(watch_timeout),
            );
        }
        return one_shot.install(
            &location,
            &Launchctl { uid: current_uid() },
            &options,
            Duration::from_secs(watch_timeout),
        );
    }
    // A repository queue already resolved the working directory; `--repo`
    // overrides it for a `--db` queue used from elsewhere or a moved checkout.
    let checkout = |repo: Option<PathBuf>| repo.unwrap_or_else(|| cwd.clone());
    // `rebind` is the one command that runs on a queue bound elsewhere.
    if let Command::Rebind { repo } = cli.command {
        return one_shot.rebind(&db, &checkout(repo));
    }
    // `dagq inbox` opens the queue itself and puts the inbox's agent in
    // this process's place, in the foreground of its terminal (ADR-t2159-1
    // decision 2); only a failed start comes back.
    if let Command::Inbox {
        plugin_dir,
        repo,
        claude,
        codex,
        provider,
        model,
    } = cli.command
    {
        let command = one_shot.inbox(
            &location,
            &checkout(repo),
            &up_environment()?,
            &dagq::lifecycle::InboxOptions {
                plugin_dir,
                agent: claude,
                codex,
                provider: provider.as_deref().map(str::parse).transpose()?,
                model,
            },
        )?;
        let error = dagq::infrastructure::process::exec(&command);
        return Err(anyhow::Error::new(error).context(format!(
            "start the inbox's agent {}",
            command.get_program().to_string_lossy()
        )));
    }
    // `doctor` reports the schema even of a queue this binary cannot open
    // (ADR-0045 decision 5), so it opens the queue itself.
    if let Command::Doctor { full } = cli.command {
        return one_shot.doctor(&db, full, common_dir.as_deref());
    }
    // The CI watch's list reads the queue and the bound checkout's
    // `[ci_watch]` (ADR-t1920-1).
    if let Command::Ci {
        command: CiCommand::Failures { task },
    } = cli.command
    {
        return dagq::compose::ci_failures(&db, task.map(TaskId::new));
    }
    // The queue service needs no queue state, only its paths.
    if let Command::Service { command } = cli.command {
        return match command {
            // A caller the service takes for no host client reads it the
            // same, opening no queue.
            ServiceCommand::Status => Ok(dagq::compose::queue_service_status(
                &db,
                &dagq::infrastructure::queue_service::Client::host(
                    &db,
                    common_dir.as_deref(),
                    |name| env::var(name).ok(),
                ),
            )),
            ServiceCommand::Start { cmux: _ } => {
                dagq::compose::queue_service_start(&db, &env::current_exe()?)
            }
            ServiceCommand::Stop => dagq::compose::queue_service_stop(&db),
            ServiceCommand::Serve { cmux: _ } => dagq::infrastructure::queue_service::serve(
                &dagq::infrastructure::queue_service::ServeOptions {
                    db: db.clone(),
                    generators: generators.clone(),
                    reads: dagq::compose::service_reads(),
                    writes: dagq::compose::service_writes(),
                    detail: dagq::compose::service_detail(),
                    commands: dagq::compose::service_commands(),
                    stop: install_stop_signal()?,
                    poll: Duration::from_millis(50),
                    owner: dagq::infrastructure::queue_service::owner_from_env(|name| {
                        env::var(name).ok()
                    }),
                },
            ),
        };
    }
    // The queue broker needs no queue state either (ADR-t2114-1).
    if let Command::QueueBroker {
        command: QueueBrokerCommand::Serve,
    } = cli.command
    {
        return dagq::infrastructure::queue_broker::serve(
            &dagq::infrastructure::queue_broker::BrokerServeOptions {
                db: db.clone(),
                generators: generators.clone(),
                stop: install_stop_signal()?,
                poll: Duration::from_millis(50),
                owner: dagq::infrastructure::queue_service::owner_from_env(|name| {
                    env::var(name).ok()
                }),
                build: dagq::VERSION.to_owned(),
                claude_api: dagq::domain::queue_broker::CLAUDE_API_URL.to_owned(),
                host_wide: dagq::infrastructure::kpi_config::host_wide_file(),
            },
        );
    }
    // A session whose hook records no span opens no queue.
    if matches!(cli.command, Command::SessionEvent { .. }) && hook_kind.is_none() {
        return Ok(json!({"recorded": false, "reason": "not an inbox or planner session"}));
    }
    // `watch` needs the live read-only file rather than a migrated snapshot.
    // `report` and `graph --out` write files but no queue state; `run log`
    // and `planner log` read a session's log, and `run screen` / `run send`
    // only read the run (task 1437), recording nothing.
    let mut queue = if matches!(cli.command, Command::Watch { .. }) {
        dagq::compose::open_queue_watch(&db)?
    } else if reads_only(&cli.command)
        || matches!(
            cli.command,
            Command::Report { .. }
                | Command::Graph { .. }
                | Command::Run {
                    command: RunCommand::Log { .. }
                        | RunCommand::Screen { .. }
                        | RunCommand::Send { .. }
                }
                | Command::Planner {
                    command: PlannerCommand::Log { .. }
                }
        )
    {
        dagq::compose::open_queue_read_only(&db)?
    } else {
        dagq::compose::open_queue(&db)?
    }
    .with_generators(generators.clone());
    if let Some(common_dir) = &common_dir {
        queue.assert_repository(common_dir)?;
    }
    // The planning commands run as the caller through the application,
    // which authorizes each before it changes the queue (task 732).
    macro_rules! planning {
        () => {
            dagq::application::commands::planning::Planning::new(&mut queue, &actor, &StaticPolicy)
        };
    }
    // The asks, answers, notes, marks and findings, likewise (task 733).
    // None notifies: the inbox's watch tells the person of a new ask
    // (ADR-t1433-1 decision 2).
    let mut dialogue_store;
    macro_rules! dialogue {
        () => {{
            dialogue_store = dagq::infrastructure::dialogue::DialogueQueue { queue: &mut queue };
            dagq::application::commands::dialogue::Dialogue::new(
                &mut dialogue_store,
                &actor,
                &StaticPolicy,
            )
        }};
    }
    // The reads of the queue, as the queue service answers them too
    // (ADR-t1233-5 decision 1).
    if let Some(read) = queue_read(&cli.command) {
        return dagq::compose::read_queue(&mut queue, &db, &one_shot, &read);
    }
    // The planning commands, as the queue service runs them too
    // (ADR-t3102-3 decision 3): the application authorizes each for the
    // caller before it changes the queue (task 732).
    if let Some(write) = queue_write(&cli.command)? {
        return dagq::compose::write_queue(&mut queue, &actor, &write, environment_owner);
    }
    // The host's dialogue and operations, as the queue service runs them
    // too (ADR-t3102-2 decision 6 (4)).
    if let Some(command) = queue_command(&cli.command)? {
        let command = dagq::compose::prepare_command(command, Some(&db), |first| {
            dagq::compose::run_command(&mut queue, &db, first)
        })?;
        return finish_command(
            &cli.command,
            dagq::compose::run_command(&mut queue, &db, &command)?,
        );
    }
    Ok(match cli.command {
        Command::Init
        | Command::Locate
        | Command::Rebind { .. }
        | Command::Migrate { .. }
        | Command::Install { .. }
        | Command::Doctor { .. }
        | Command::Ci { .. }
        | Command::Inbox { .. }
        | Command::Service { .. }
        | Command::QueueBroker { .. }
        | Command::Relay { .. }
        | Command::ReviewProgram { .. }
        | Command::CodexInboxHook { .. }
        | Command::SetupReview { .. } => {
            unreachable!()
        }
        // Answered above as reads.
        Command::List { .. }
        | Command::Candidates { .. }
        | Command::Graph { out: None, .. }
        | Command::Status { .. }
        | Command::Asks { .. }
        | Command::Events { .. }
        | Command::Timeline { .. }
        | Command::Stats { .. }
        | Command::Kpi { .. }
        | Command::Forecast { .. }
        | Command::Notes { .. }
        | Command::Marks { .. }
        | Command::Findings { .. }
        | Command::Search { .. }
        | Command::Related { .. }
        | Command::Lint { .. }
        | Command::Observe { history: true, .. }
        | Command::Observe { input: Some(_), .. }
        | Command::Requests { .. }
        | Command::Replans { .. }
        | Command::Planners { .. } => unreachable!("a read is answered above"),
        // Answered above as writes.
        Command::Add { .. }
        | Command::Edit { .. }
        | Command::Submit { .. }
        | Command::Draft { .. }
        | Command::Ready { inherit: false, .. }
        | Command::Cancel { .. }
        | Command::Dependency { .. }
        | Command::Goal { .. }
        | Command::JudgeFollowUp { .. }
        | Command::SetGoal { .. }
        | Command::SetPaths { .. }
        | Command::SetPriority { .. }
        | Command::Revisit { .. }
        | Command::Request { .. } => unreachable!("a write is answered above"),
        // Run above as the queue service runs them.
        Command::Answer { .. }
        | Command::Ask {
            command: Some(AskCommand::Close { .. }),
            ..
        }
        | Command::Hold { .. }
        | Command::Release { .. }
        | Command::Replan {
            command: ReplanCommand::Request { .. },
        }
        | Command::Recover { .. }
        | Command::Run {
            command: RunCommand::Screen { .. } | RunCommand::Send { .. } | RunCommand::Log { .. },
        }
        | Command::Planner { .. }
        | Command::Report { .. }
        | Command::Graph { out: Some(_), .. } => unreachable!("a command is run above"),
        Command::Show { id, full, events } => {
            let detail = queue.show(TaskId::new(id))?;
            if full {
                serde_json::to_value(detail)?
            } else {
                dagq::view::task_detail(&detail, events)
            }
        }
        Command::Ready {
            id,
            inherit: true,
            reason,
            ..
        } => {
            let containers = dagq::compose::queue_run_containers(&queue, &db);
            serde_json::to_value(planning!().ready_inheriting(
                TaskId::new(id),
                reason.as_deref().unwrap_or_default(),
                &dagq::infrastructure::adapters::RunRepositories {
                    containers: &containers,
                },
            )?)?
        }
        Command::Proposal { command } => match command {
            ProposalCommand::List { all } => json!({"proposals": queue.proposals(all)?}),
            ProposalCommand::Show { id } => {
                serde_json::to_value(queue.show_proposal(ProposalId::new(id))?)?
            }
            ProposalCommand::Withdraw { .. } => unreachable!("a write is answered above"),
        },
        Command::Note {
            task,
            run,
            goal,
            text,
            kind,
        } => {
            let target = match (task, run, goal) {
                (Some(task), _, _) => NoteTarget::Task(TaskId::new(task)),
                (_, Some(run), _) => NoteTarget::Run(RunId::new(run)?),
                (_, _, goal) => NoteTarget::Goal(GoalId::new(goal.context("note needs a target")?)),
            };
            serde_json::to_value(dialogue!().note(NewNote {
                target,
                text,
                kind,
                by: actor.written_by().to_owned(),
            })?)?
        }
        Command::Mark {
            label,
            note,
            at,
            retract,
        } => dialogue!().mark(match retract {
            Some(id) => {
                dagq::application::commands::dialogue::MarkChange::Retract(EventId::new(id))
            }
            None => dagq::application::commands::dialogue::MarkChange::Record {
                label: label.unwrap_or_default(),
                note,
                at,
            },
        })?,
        Command::Finding {
            command:
                FindingCommand::Record {
                    kind,
                    task,
                    run,
                    goal,
                    queue: _,
                    subject,
                    summary,
                    detail,
                    impact,
                    evidence,
                    propose,
                },
        } => serde_json::to_value(dialogue!().record_finding(NewFinding {
            kind,
            target: finding_target(task, run, goal)?.unwrap_or(FindingTarget::Queue),
            subject,
            summary,
            detail,
            impact: impact.map(|impact| impact.parse()).transpose()?,
            evidence: evidence.into_iter().map(EventId::new).collect(),
            propose,
            by: actor.written_by().to_owned(),
        })?)?,
        Command::Finding {
            command: FindingCommand::Resolve { id, reason },
        } => serde_json::to_value(dialogue!().resolve_finding(FindingId::new(id), &reason)?)?,
        Command::Finding {
            command:
                FindingCommand::Dismiss {
                    id,
                    reason,
                    covered_by,
                },
        } => serde_json::to_value(dialogue!().dismiss_finding(
            FindingId::new(id),
            &reason,
            covered_by.map(TaskId::new),
        )?)?,
        Command::Agent { command } => {
            let evals =
                dagq::application::agent_eval::AgentEvals::new(&queue, &actor, &StaticPolicy);
            let sets =
                dagq::application::agent_cases::AgentCaseSets::new(&queue, &actor, &StaticPolicy);
            match command {
                AgentCommand::Eval {
                    agent,
                    split,
                    k,
                    cases,
                    set,
                    rerun,
                } => evals.request(&dagq::application::agent_eval::EvalRequest {
                    agent,
                    split: split_named(&split)?,
                    k,
                    cases: (!cases.is_empty()).then_some(cases),
                    set,
                    rerun,
                })?,
                AgentCommand::Results { agent, limit } => evals.list(agent.as_deref(), limit)?,
                AgentCommand::Result { id } => evals.show(id)?,
                AgentCommand::Cases {
                    agent,
                    since,
                    until,
                } => sets.request(&agent, since, until)?,
                AgentCommand::Decide {
                    set,
                    case,
                    verdict,
                    codes,
                    reason,
                } => sets.decide(set, &case, &verdict, codes, &reason)?,
                AgentCommand::Sets { agent, limit } => sets.list(agent.as_deref(), limit)?,
                AgentCommand::Set { id } => sets.show(id)?,
            }
        }
        // A worker's proposal on its own run (ADR-t1521-2 decision 3).
        Command::Replan {
            command:
                ReplanCommand::Propose {
                    run,
                    reason,
                    evidence,
                },
        } => serde_json::to_value(
            dagq::application::commands::replans::Replans::new(&mut queue, &actor, &StaticPolicy)
                .propose(&new_replan(&run, reason, &evidence)?)?,
        )?,
        Command::Ask {
            command: None,
            kind,
            question,
            options,
            because,
            topics,
            recommend,
            confidence,
            task_id,
            run,
            finding,
            request,
            cmux: _,
        } => {
            // The inbox's watch tells of the new ask (ADR-t2159-1 decision 4).
            dialogue!().ask(NewAsk {
                recommendation: recommend,
                confidence: confidence.as_deref().map(str::parse).transpose()?,
                kind: kind.unwrap_or_default().parse::<AskKind>()?,
                task_id: task_id.map(TaskId::new),
                run_id: run.map(RunId::new).transpose()?,
                question: question.unwrap_or_default(),
                options,
                // The session's role; a person at a plain terminal has none.
                asked_by: actor.written_by().to_owned(),
                reason_category: because.unwrap_or_default().parse::<AskReason>()?,
                topics,
                finding_id: finding.map(FindingId::new),
                request_id: request.map(RequestId::new),
            })?
        }
        Command::Watch {
            after,
            timeout,
            until_attention,
            interval,
            role: r,
            cmux: _,
        } => dagq::compose::watch_in(
            &db,
            &queue,
            &dagq::application::watch::WatchOptions {
                after: after.map(EventId::new),
                timeout: (!until_attention)
                    .then(|| Duration::from_secs(timeout.unwrap_or(DEFAULT_WATCH_TIMEOUT_SECS))),
                interval: Duration::from_secs(interval),
                role: parse_role(r)?,
            },
            true,
        )?,
        Command::Supervise {
            no_claude,
            repo,
            parallel,
            max_waiting,
            max_load,
            once,
            cmux: _,
            claude,
            codex,
            log_dir: _,
            observe_interval,
            observe_daily,
            throughput_review,
            report_daily,
            forecast_snapshots,
            host_metrics_interval,
            host_metrics_retention_days,
            runtime_planners,
            planner_timeout,
            plugin_dir,
            handoff_token,
            mode,
            auto_update,
            update_interval,
            update_build_command,
            update_e2e_command,
            update_e2e_timeout,
            update_cargo,
            update_poll_ms,
            heartbeat_interval_ms,
            idle_poll_ms,
            tick_ms,
        } => {
            use dagq::compose::SuperviseOptions;
            let mut options = SuperviseOptions {
                no_claude,
                stop: install_stop_signal()?,
                // A test's supervisor stops with the test's process; nothing
                // in production sets it.
                owner: dagq::infrastructure::queue_service::owner_from_env(|name| {
                    env::var(name).ok()
                }),
                observe_interval: resolve_observe_interval(observe_interval, once),
                observe_daily,
                throughput_review: throughput_review.unwrap_or(!once),
                report_daily,
                forecast_snapshots,
                generators,
                runtime_planners: runtime_planners.map(usize::from),
                planner_timeout: Duration::from_secs(planner_timeout),
                plugin_dir,
                handoff_token: handoff_token.map(LeaseToken::new),
                mode: mode.map(|mode| mode.parse()).transpose()?,
                update: dagq::application::supervise::UpdateSettings {
                    register: auto_update,
                    interval: Duration::from_secs(update_interval),
                    build_command: update_build_command,
                    e2e_command: update_e2e_command,
                    e2e_timeout: update_e2e_timeout.map(Duration::from_secs),
                    poll: update_poll_ms.map(Duration::from_millis),
                    cargo: update_cargo,
                },
                parallel: parallel.map(usize::from),
                max_waiting: max_waiting.map(usize::from),
                max_load: dagq::domain::claim_hold::resolve_max_load(
                    max_load,
                    dagq::infrastructure::clock::logical_cores(),
                ),
                max_load_source: if max_load.is_some() {
                    dagq::domain::slot_limits::SettingSource::Flag
                } else {
                    dagq::domain::slot_limits::SettingSource::Default
                },
                user_config: dagq::infrastructure::language::user_config_file(),
                claude_config_dir: dagq::infrastructure::adapters::claude_config_dir(),
                codex,
                host_metrics: (host_metrics_interval > 0).then(|| {
                    dagq::compose::HostMetricsSettings::new(
                        Duration::from_secs(host_metrics_interval),
                        host_metrics_retention_days,
                    )
                }),
                // The host's Claude Code scratchpads (task 1100).
                scratchpad_roots: None,
                // The sccache server [run.env] names, started outside any
                // sandbox (ADR-t1215-1).
                sccache: Some(dagq::compose::SccacheOptions::default()),
                // The landing branch's CI when [ci_watch] is set
                // (ADR-t1920-1); a one-shot pass does not watch it.
                ci_watch: (!once).then(dagq::compose::CiWatchOptions::default),
                // The ledger, built from what is recorded (docs/design/
                // measurement.md "台帳を作る係"); a one-shot pass does not.
                ledger: (!once).then(dagq::compose::LedgerOptions::default),
                // The environment of the runs and their resources (docs/design/
                // measurement-environment.md "環境の口"); a one-shot pass does not.
                environment: (!once).then(dagq::compose::EnvironmentOptions::default),
                // The watch of the adopted agent definitions (ADR-t1728-1
                // (g)); a one-shot pass does not watch them.
                agent_watch: (!once).then_some(dagq::application::supervise::AGENT_WATCH_CHECK),
                // A supervisor keeps the queue's service (ADR-t1233-4
                // decision 2), a one-shot pass too: it reaches the queue
                // through it (ADR-t3102-3 decision 5), and starts it when
                // it does not run.
                queue_service: Some(dagq::compose::QueueServiceOptions {
                    executable: env::current_exe().unwrap_or_else(|_| PathBuf::from("dagq")),
                    interval: dagq::application::supervise::QUEUE_SERVICE_INTERVAL,
                    start_timeout: dagq::application::supervise::QUEUE_SERVICE_START_TIMEOUT,
                    control: None,
                }),
                service_route: dagq::compose::ServiceRoute::Socket,
                // And the queue broker (ADR-t2114-1 decision 1), from the same
                // binary.
                queue_broker: (!once).then(|| dagq::compose::QueueBrokerOptions {
                    executable: env::current_exe().unwrap_or_else(|_| PathBuf::from("dagq")),
                    interval: dagq::application::supervise::QUEUE_SERVICE_INTERVAL,
                    start_timeout: dagq::application::supervise::QUEUE_SERVICE_START_TIMEOUT,
                    control: None,
                }),
                // And the relays of the runs' containers to the broker
                // (ADR-t2114-3 decision 4).
                broker_relay: (!once).then(|| dagq::compose::QueueRelayOptions {
                    interval: dagq::application::supervise::QUEUE_SERVICE_INTERVAL,
                    control: None,
                }),
                ..SuperviseOptions::new(dagq::domain::slot_limits::DEFAULT_PARALLEL, once)
            };
            // Tests shorten the heartbeat and the pauses between passes
            // (task 1048).
            if let Some(ms) = heartbeat_interval_ms {
                options.heartbeat_interval = Duration::from_millis(ms);
            }
            if let Some(ms) = idle_poll_ms {
                options.idle_poll = Duration::from_millis(ms);
            }
            if let Some(ms) = tick_ms {
                options.tick = Duration::from_millis(ms);
            }
            dagq::compose::supervise(
                &db,
                &checkout(repo),
                &dagq::infrastructure::adapters::BackgroundSessions,
                &if no_claude {
                    claude
                } else {
                    dagq::infrastructure::adapters::claude_at_entry(&claude)?
                },
                &env::current_exe()?,
                &options,
            )?
        }
        Command::Up {
            no_claude,
            parallel,
            max_waiting,
            runtime_planners,
            max_load,
            in_cmux,
            no_wait,
            handoff_timeout,
            plugin_dir,
            repo,
            cmux: _,
            claude,
            codex,
            auto_update,
        } => {
            use dagq::application::lifecycle::UpOptions;
            use dagq::infrastructure::{adapters::SystemProcesses, launchd::Launchctl};
            let environment = up_environment()?;
            let options = UpOptions {
                no_claude,
                parallel,
                max_waiting,
                runtime_planners,
                max_load,
                in_cmux,
                no_wait,
                plugin_dir,
                claude: if no_claude {
                    claude
                } else {
                    dagq::infrastructure::adapters::claude_at_entry(&claude)?
                },
                codex: dagq::infrastructure::codex::codex_at_entry(&codex).unwrap_or(codex),
                startup_timeout: Duration::from_secs(30),
                handoff_timeout: Duration::from_secs(handoff_timeout),
                auto_update,
                queue_service: true,
                queue_broker: true,
                poll: Duration::from_millis(500),
            };
            one_shot.up(
                &location,
                &checkout(repo),
                &Launchctl { uid: current_uid() },
                &SystemProcesses,
                &environment,
                &options,
            )?
        }
        Command::AutoUpdate {
            commit,
            token,
            to,
            repo,
            log,
            build_command,
            e2e_command,
            e2e_timeout,
            cmux: _,
            claude,
            codex,
            plugin_dir,
            handoff_timeout,
            watch_timeout,
            poll_ms,
        } => {
            drop(queue);
            one_shot.auto_update(
                &location,
                &dagq::compose::AutoUpdateJob {
                    commit,
                    token: LeaseToken::new(token),
                    target: to,
                    repository: repo,
                    log,
                    build_command,
                    e2e_command,
                    e2e_timeout: Duration::from_secs(e2e_timeout),
                    claude: dagq::infrastructure::adapters::claude_at_entry(&claude)
                        .unwrap_or(claude),
                    codex: dagq::infrastructure::codex::codex_at_entry(&codex).unwrap_or(codex),
                    plugin_dir,
                    handoff_timeout: Duration::from_secs(handoff_timeout),
                    watch_timeout: Duration::from_secs(watch_timeout),
                    poll: Duration::from_millis(poll_ms),
                },
            )?
        }
        Command::ReleaseUpdate {
            release,
            token,
            to,
            log,
            cargo,
            cmux: _,
            claude,
            codex,
            plugin_dir,
            handoff_timeout,
            watch_timeout,
            plugin_only,
        } => {
            use dagq::infrastructure::adapters::executable;
            drop(queue);
            one_shot.release_update(
                &location,
                &dagq::compose::ReleaseUpdateJob {
                    version: release,
                    token: LeaseToken::new(token),
                    target: to,
                    log,
                    cargo: executable(&cargo).unwrap_or(cargo),
                    claude: dagq::infrastructure::adapters::claude_at_entry(&claude)
                        .unwrap_or(claude),
                    codex: dagq::infrastructure::codex::codex_at_entry(&codex).unwrap_or(codex),
                    plugin_dir,
                    handoff_timeout: Duration::from_secs(handoff_timeout),
                    watch_timeout: Duration::from_secs(watch_timeout),
                    plugin_only,
                },
            )?
        }
        Command::Plan { .. } => bail!(dagq::application::planner::PLAN_REFUSED),
        Command::Run {
            command: RunCommand::CloseWorkspaces { .. },
        } => bail!(dagq::application::screen::CLOSE_WORKSPACES_REFUSED),
        Command::Down {
            wait,
            force,
            cmux: _,
        } => {
            use dagq::application::lifecycle::DownOptions;
            use dagq::infrastructure::{adapters::SystemProcesses, launchd::Launchctl};
            one_shot.down(
                &location,
                &Launchctl { uid: current_uid() },
                &SystemProcesses,
                &DownOptions {
                    wait,
                    force,
                    poll: Duration::from_secs(2),
                },
            )?
        }
        Command::Integrate {
            id,
            next,
            repo,
            no_push,
        } => {
            use dagq::{
                application::integrate::IntegrateTarget, infrastructure::adapters::GitRepository,
            };
            let target = match (id, next) {
                (Some(id), false) => IntegrateTarget::Task(TaskId::new(id)),
                _ => IntegrateTarget::Next,
            };
            let repo = checkout(repo);
            let remote = if no_push {
                None
            } else {
                Some(GitRepository::inspect(&repo)?)
            };
            one_shot.integrate(
                &db,
                target,
                &repo,
                remote
                    .as_ref()
                    .map(|r| r as &dyn dagq::application::MainRemote),
            )?
        }
        Command::Review { id } => dagq::compose::review(&db, TaskId::new(id))?,
        Command::Observe {
            history: false,
            input: None,
            since,
            dry_run,
            daily,
            timeout,
            claude,
            codex,
            cmux: _,
            launch,
            codex_home,
            switchable,
            no_provider_fallback,
            unavailable,
            ..
        } => {
            use dagq::application::observer::{ObserveMode, ObserveOptions};
            use dagq::domain::{Provider, actor_model::ActorLaunch};
            use dagq::infrastructure::{adapters::ClaudeCode, codex::Codex};
            let launch: ActorLaunch = match launch {
                Some(launch) => serde_json::from_str(&launch).context("parse --launch")?,
                None => dagq::compose::observer_launch(&db)?,
            };
            // A dry run starts nothing, so it needs no executable; nor does
            // an observation no provider can run, nor the provider it does
            // not start. A Claude Code that cannot be resolved fails as
            // before; a Codex one (gone since the supervisor found it) is
            // kept as given, so the observation records its finish, whose
            // start error says Codex cannot be used (task 1223).
            let resolve = !dry_run && unavailable.is_none();
            // With `[provider_fallback] jobs` off, a Claude that cannot be
            // resolved is kept as given too, so that the observation
            // records Claude's start error and waits for Claude
            // (ADR-t1857-1).
            let claude = if resolve && launch.provider == Provider::Claude {
                if switchable && no_provider_fallback {
                    dagq::infrastructure::adapters::claude_at_entry(&claude).unwrap_or_else(|error| {
                        tracing::warn!(error = %format_args!("{error:#}"), "{} could not be resolved: {error:#}", claude.display());
                        claude
                    })
                } else {
                    dagq::infrastructure::adapters::claude_at_entry(&claude)?
                }
            } else {
                claude
            };
            let claude = ClaudeCode { executable: claude };
            let codex = Codex {
                executable: if resolve && launch.provider == Provider::Codex {
                    dagq::infrastructure::codex::codex_at_entry(&codex).unwrap_or_else(|error| {
                        tracing::warn!(error = %format_args!("{error:#}"), "{} could not be resolved: {error:#}", codex.display());
                        codex
                    })
                } else {
                    codex
                },
                home: codex_home,
            };
            let (provider, signals): (
                &dyn dagq::application::AgentProvider,
                Option<&dyn dagq::application::AgentSignals>,
            ) = match launch.provider {
                Provider::Claude => (&claude, Some(&claude)),
                Provider::Codex => (&codex, None),
            };
            dagq::compose::observe(
                &db,
                provider,
                signals,
                &ObserveOptions {
                    mode: if daily {
                        ObserveMode::Daily
                    } else {
                        ObserveMode::Hourly
                    },
                    since: since.map(EventId::new),
                    dry_run,
                    timeout: Duration::from_secs(timeout),
                    dagq: env::current_exe()?,
                    user_config: dagq::infrastructure::language::user_config_file(),
                    prompt_limit: dagq::application::observer::PROMPT_LIMIT,
                    launch: Some(launch),
                    switchable,
                    fallback: !no_provider_fallback,
                    unavailable,
                },
            )?
        }
        Command::ThroughputReview {
            mode,
            at,
            utc_offset,
            dry_run,
            timeout,
            claude,
            codex,
            launch,
            codex_home,
            switchable,
            no_provider_fallback,
            unavailable,
        } => {
            use dagq::domain::{Provider, actor_model::ActorLaunch};
            use dagq::infrastructure::{adapters::ClaudeCode, codex::Codex};
            let launch: ActorLaunch = match launch {
                Some(launch) => serde_json::from_str(&launch).context("parse --launch")?,
                None => dagq::compose::throughput_review_launch(&db)?,
            };
            // A dry run starts nothing, so it needs no executable; nor does a
            // review no provider can run. One that cannot be resolved (gone
            // since the supervisor found it) is kept as given: the review
            // records its finish, whose start error says the provider
            // cannot be used (task 1220), rather than this command failing
            // with no record of the period.
            let resolve = !dry_run && unavailable.is_none();
            let resolved = |path: PathBuf, find: fn(&std::path::Path) -> Result<PathBuf>| {
                if !resolve {
                    return path;
                }
                find(&path).unwrap_or_else(|error| {
                    tracing::warn!(error = %format_args!("{error:#}"), "{} could not be resolved: {error:#}", path.display());
                    path
                })
            };
            let provider: Box<dyn dagq::application::AgentProvider> = match launch.provider {
                Provider::Claude => Box::new(ClaudeCode {
                    executable: resolved(claude, dagq::infrastructure::adapters::claude_at_entry),
                }),
                Provider::Codex => Box::new(Codex {
                    executable: resolved(codex, dagq::infrastructure::codex::codex_at_entry),
                    home: codex_home,
                }),
            };
            dagq::compose::throughput_review(
                &db,
                provider.as_ref(),
                &dagq::application::throughput_review::ReviewOptions {
                    mode: mode.parse()?,
                    at,
                    dry_run,
                    timeout: Duration::from_secs(timeout),
                    dagq: env::current_exe()?,
                    user_config: dagq::infrastructure::language::user_config_file(),
                    utc_offset,
                    launch: Some(launch),
                    switchable,
                    fallback: !no_provider_fallback,
                    unavailable,
                },
            )?
        }
        Command::Session { .. } | Command::PlannerSession { .. } => {
            unreachable!("a session wrapper opens no queue: session_wrapper runs it")
        }
        // An inbox's hook records as the inbox (ADR-t3102-1 decision 4).
        Command::SessionEvent { event, .. } => match read_session_hook(&event)? {
            Some(hook) => dagq::application::SessionRegistry::record_session_hook(&queue, &hook)?,
            None => json!({"recorded": false, "reason": "not an inbox or planner session"}),
        },
    })
}

/// The kind of span the session's hook records, as its environment names
/// it; `None` for a session it records none of (a run's).
fn session_hook_kind() -> Option<&'static str> {
    use dagq::application::lifecycle::{PLANNER_ORIGIN_ENV, SESSION_KIND_ENV};
    let env = |name: &str| env::var(name).ok();
    dagq::domain::sessions::hook_kind(
        env(SESSION_KIND_ENV).as_deref(),
        env(dagq::domain::actor::ROLE_ENV).as_deref(),
        env(PLANNER_ORIGIN_ENV).as_deref(),
    )
}

/// The report of the session hook `event` (`open` or `close`) on stdin,
/// with what the session's environment names; `None` for a session the
/// hook records no span of (a run's).
fn read_session_hook(event: &str) -> Result<Option<dagq::domain::sessions::SessionHook>> {
    // Nothing is read of a session whose span the hook does not record.
    if session_hook_kind().is_none() {
        return Ok(None);
    }
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
        .context("read the hook input")?;
    let input: Value = serde_json::from_str(&input).context("parse the hook input")?;
    dagq::domain::sessions::SessionHook::from_hook(event, &input, |name| env::var(name).ok())
        .map_err(anyhow::Error::msg)
}

/// Leave a planner's hook's report `hook` in the planner's directory of
/// the queue `db` names, for the planner's wrapper to record
/// ([`dagq::application::session::spool_hook`]). A planner session of no
/// planner (one opened before its actor was named) records nothing.
fn spool_planner_hook(
    db: Option<&Path>,
    hook: &dagq::domain::sessions::SessionHook,
) -> Result<Value> {
    let Some(id) = hook.planner_id else {
        return Ok(json!({"recorded": false, "reason": "a planner session of no planner"}));
    };
    let cwd = env::current_dir().context("working directory is unavailable")?;
    let location = QueueLocation::resolve(db, &cwd)?;
    let dir = dagq::application::planner::planner_dir(
        &dagq::infrastructure::location::planners_dir(&location.db),
        dagq::domain::PlannerId::new(id),
    );
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let stamp = format!("{:020}-{}", since.as_nanos(), std::process::id());
    let report = dagq::application::session::spool_hook(
        &dagq::infrastructure::run_files::LocalRunFiles,
        &dir,
        hook,
        &stamp,
    )?;
    Ok(json!({"recorded": false, "relayed_by": "wrapper", "planner_id": id, "report": report}))
}

/// `session` and `planner-session`: the wrapper of a run or a planner,
/// which reaches the queue of `--db` only through its service
/// ([`dagq::compose::session`], [`dagq::compose::planner_session`]).
fn session_wrapper(cli: Cli) -> Result<Value> {
    let cwd = env::current_dir().context("working directory is unavailable")?;
    let location = QueueLocation::resolve(cli.db.as_deref(), &cwd)?;
    install_telemetry(&cli.command, &location);
    let db = location.db;
    match cli.command {
        Command::Session {
            run,
            lease,
            claude,
            codex,
            resume,
            cmux: _,
            background,
        } => dagq::compose::session(
            &db,
            &RunId::new(run)?,
            &LeaseToken::new(lease),
            &claude,
            &codex,
            resume,
            background,
        ),
        Command::PlannerSession {
            planner,
            claude,
            plugin_dir,
            model,
            effort,
            cmux: _,
            headless,
            background,
        } => dagq::compose::planner_session(
            &db,
            dagq::domain::PlannerId::new(planner),
            &claude,
            plugin_dir.as_deref(),
            model.as_deref().zip(effort.as_deref()),
            dagq::compose::PlannerEntry {
                headless,
                background,
            },
        ),
        _ => unreachable!("only a session wrapper's command comes here"),
    }
}

/// The subscriber of this process's progress and diagnostic events
/// (ADR-0033): the long-running and landing processes (`supervise`,
/// `integrate`, `observe` and the session wrapper) and `up`, `down` and
/// `rebind` (on a queue that exists) keep a JSON Lines file in the
/// queue's `logs/` (a supervisor's `--log-dir` if given); every other
/// command prints its messages on stderr only.
fn install_telemetry(command: &Command, location: &QueueLocation) {
    use dagq::infrastructure::telemetry::Telemetry;
    let file = match command {
        Command::Supervise { log_dir, .. } => Some((
            "supervise",
            log_dir.clone().unwrap_or_else(|| location.log_dir.clone()),
        )),
        Command::Integrate { .. } => Some(("integrate", location.log_dir.clone())),
        Command::Observe {
            history: false,
            input: None,
            ..
        } => Some(("observe", location.log_dir.clone())),
        Command::ThroughputReview { .. } => Some(("throughput-review", location.log_dir.clone())),
        Command::Session { .. } => Some(("session", location.log_dir.clone())),
        Command::PlannerSession { .. } => Some(("planner-session", location.log_dir.clone())),
        Command::AutoUpdate { .. } => Some(("auto-update", location.log_dir.clone())),
        Command::ReleaseUpdate { .. } => Some(("release-update", location.log_dir.clone())),
        // Only on a queue that exists: a `logs/` made for one that does not
        // would leave a directory where the queue is to be moved or made.
        Command::Up { .. } | Command::Down { .. } | Command::Rebind { .. }
            if !location.db.is_file() =>
        {
            None
        }
        Command::Up { .. } => Some(("up", location.log_dir.clone())),
        Command::Down { .. } => Some(("down", location.log_dir.clone())),
        Command::Rebind { .. } => Some(("rebind", location.log_dir.clone())),
        _ => None,
    };
    let telemetry = match file {
        Some((process, dir)) => Telemetry::open(&dir, process),
        None => Telemetry::stderr(),
    };
    telemetry.install();
}

/// What `up` and `dagq inbox` read from the environment of the process.
fn up_environment() -> Result<dagq::lifecycle::UpEnvironment> {
    use dagq::application::lifecycle::{QUEUE_ENV, ROLE_ENV};
    Ok(dagq::lifecycle::UpEnvironment {
        role: env::var(ROLE_ENV).ok(),
        queue: env::var_os(QUEUE_ENV).map(PathBuf::from),
        path: env::var("PATH").context("PATH is unset")?,
        config_home: env::var(dagq::application::CONFIG_HOME_ENV)
            .ok()
            .filter(|home| !home.is_empty()),
        current_exe: env::current_exe()?,
        claude_config: dagq::infrastructure::adapters::claude_global_config(
            env::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
            env::var("HOME").ok().as_deref(),
        ),
        user_config: dagq::infrastructure::language::user_config_file(),
        restart: env::var_os(dagq::lifecycle::UP_RESTART_ENV).is_some(),
        owner: dagq::infrastructure::queue_service::owner_from_env(|name| env::var(name).ok()),
    })
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();

extern "C" fn request_stop(signal: libc::c_int) {
    if let Some(stop) = STOP.get() {
        stop.store(true, Ordering::SeqCst);
    }
    // SAFETY: restoring the default disposition is async-signal-safe, so a
    // second signal terminates the process the usual way.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
    }
}

/// The first SIGINT/SIGTERM asks the supervisor to stop claiming and drain its
/// active runs; the second one terminates it (leases then go stale).
fn install_stop_signal() -> Result<Arc<AtomicBool>> {
    let stop = STOP
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone();
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the handler only stores an atomic and resets the disposition.
        let previous =
            unsafe { libc::signal(signal, request_stop as extern "C" fn(libc::c_int) as usize) };
        anyhow::ensure!(previous != libc::SIG_ERR, "install signal handler");
    }
    Ok(stop)
}

/// The options a handoff drops from this process's arguments: its own
/// token, and `--mode`, which the handed-off binary may predate (the
/// handoff probe asks only for `--handoff-token`) and does not need, since
/// the registration it takes over already has its mode.
const HANDOFF_DROPPED: [&str; 2] = ["--handoff-token", "--mode"];

/// The options of `supervise` a handed-off binary may predate (`--codex`,
/// ADR-t813-2): each is kept only when the binary takes it, as
/// [`takes_option`] finds, so a rollback to an older binary still starts.
const HANDOFF_OPTIONAL: [&str; 1] = ["--codex"];

/// The arguments of this process with `--handoff-token <token>` in place
/// of any it had and without `--mode`, nor the options of `dropped`: the
/// `supervise` the handed-off binary runs.
fn handoff_arguments(arguments: &[OsString], token: &str, dropped: &[&str]) -> Vec<OsString> {
    let mut kept = Vec::with_capacity(arguments.len() + 2);
    let mut skip = false;
    let dropped: Vec<&str> = HANDOFF_DROPPED.iter().chain(dropped).copied().collect();
    for argument in arguments {
        if std::mem::take(&mut skip) {
            continue;
        }
        if dropped.iter().any(|option| argument == *option) {
            skip = true;
            continue;
        }
        if argument.to_str().is_some_and(|text| {
            dropped
                .iter()
                .any(|option| text.starts_with(&format!("{option}=")))
        }) {
            continue;
        }
        kept.push(argument.clone());
    }
    kept.push("--handoff-token".into());
    kept.push(token.into());
    kept
}

/// Whether `binary`'s `supervise` takes `option` (with a value): clap
/// refuses an unknown argument before it prints the help.
fn takes_option(binary: &str, option: &str) -> bool {
    std::process::Command::new(binary)
        .args(["supervise", option, "probe", "--help"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Run the command; a supervisor asked to hand off (ADR-0045 decision 10)
/// execs the requested binary here, under this pid, once every connection
/// of the loop is closed. When the exec itself fails, this binary takes its
/// registration back and supervises on, so the one asking sees the version
/// it did not ask for.
fn run(mut arguments: Vec<OsString>) -> Result<Value> {
    loop {
        let value = execute(Cli::parse_from(&arguments))?;
        if value["outcome"] != "handoff" {
            return Ok(value);
        }
        let (Some(binary), Some(token)) = (value["binary"].as_str(), value["token"].as_str())
        else {
            bail!("a handoff without a binary or a token: {value}");
        };
        let dropped: Vec<&str> = HANDOFF_OPTIONAL
            .into_iter()
            .filter(|option| {
                arguments.iter().any(|argument| {
                    argument.to_str().is_some_and(|text| {
                        text == *option || text.starts_with(&format!("{option}="))
                    })
                }) && !takes_option(binary, option)
            })
            .collect();
        // This binary takes every option: it goes on with them when the
        // exec fails.
        let handed = handoff_arguments(&arguments, token, &dropped);
        arguments = handoff_arguments(&arguments, token, &[]);
        tracing::info!("supervisor {token} execs {binary}");
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new(binary).args(&handed[1..]).exec();
        tracing::error!(
            error = %error,
            "supervisor {token} could not exec {binary}: {error}; it goes on with this binary"
        );
    }
}

use dagq::view::RAW_STDOUT;

/// Print `log` to stdout as `run log` / `planner log` do, following it
/// with `follow`; nothing is left for the JSON output.
fn print_session_log(
    log: &dagq::application::session_log::SessionLog,
    lines: Option<usize>,
    follow: bool,
) -> Result<Value> {
    use dagq::application::session_log::{FOLLOW_INTERVAL, LogPorts, print};
    use dagq::infrastructure::{adapters::SystemProcesses, run_files::LocalRunFiles};
    print(
        &LogPorts {
            files: &LocalRunFiles,
            processes: &SystemProcesses,
        },
        log,
        lines,
        follow.then_some(FOLLOW_INTERVAL),
        &mut io::stdout().lock(),
    )?;
    Ok(json!({ RAW_STDOUT: "" }))
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().collect();
    // cargo's `RUSTC_WRAPPER` in a process given `[run.env]`, not the CLI
    // (ADR-t2086-1).
    if args
        .first()
        .is_some_and(|argv0| dagq::domain::sccache::invoked_as_guard(argv0))
    {
        let status = dagq::infrastructure::sccache::run_guard(&args[1..]);
        return ExitCode::from(u8::try_from(status).unwrap_or(1));
    }
    let result = run(args).and_then(|value| {
        let mut stdout = io::stdout().lock();
        if let Some(text) = value
            .as_object()
            .filter(|object| object.len() == 1)
            .and_then(|object| object.get(RAW_STDOUT))
            .and_then(Value::as_str)
        {
            stdout.write_all(text.as_bytes())?;
            return Ok(());
        }
        serde_json::to_writer_pretty(&mut stdout, &value)?;
        writeln!(stdout)?;
        Ok(())
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // An install that kept the new binary for some supervisors
            // reports what it did as well (ADR-t632-1).
            // So does one whose handoff or watch failed and told the inbox
            // (ADR-0073 decision 14), in place of the install's report.
            if let Some(failed) = dagq::application::update::InstallFailed::of(&error) {
                let mut stdout = io::stdout().lock();
                let _ = serde_json::to_writer_pretty(&mut stdout, &failed.report);
                let _ = writeln!(stdout);
            } else if let Some(kept) = dagq::application::install::KeptBinary::of(&error) {
                let mut stdout = io::stdout().lock();
                let _ = serde_json::to_writer_pretty(&mut stdout, &kept.report);
                let _ = writeln!(stdout);
            }
            // So does an `up` whose handoff some supervisors did not take.
            if let Some(partial) = dagq::application::lifecycle::PartialHandoff::of(&error) {
                let mut stdout = io::stdout().lock();
                let _ = serde_json::to_writer_pretty(&mut stdout, &partial.report);
                let _ = writeln!(stdout);
            }
            // The command's own failure, in its log file too; stderr gets
            // the error JSON below as always.
            tracing::error!(
                target: "dagq::telemetry::exit",
                error = %format_args!("{error:#}"),
                "dagq exited with an error: {error:#}"
            );
            eprintln!("{}", error_json(&error));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod emitted_text;
#[cfg(test)]
mod history_citation;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_interval_defaults_to_three_hours() {
        assert_eq!(
            resolve_observe_interval(None, false),
            Duration::from_secs(10800)
        );
    }

    #[test]
    fn observer_interval_defaults_to_disabled_for_once() {
        assert_eq!(resolve_observe_interval(None, true), Duration::ZERO);
    }

    #[test]
    fn observer_interval_preserves_explicit_values() {
        for once in [false, true] {
            for seconds in [0, 60, 3600] {
                assert_eq!(
                    resolve_observe_interval(Some(seconds), once),
                    Duration::from_secs(seconds)
                );
            }
        }
    }

    #[test]
    fn a_handoff_replaces_the_token_and_drops_the_mode() {
        let arguments: Vec<OsString> = [
            "dagq",
            "supervise",
            "--mode",
            "in_cmux",
            "--handoff-token",
            "old",
            "--parallel",
            "3",
            "--mode=launchd",
            "--handoff-token=older",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        assert_eq!(
            handoff_arguments(&arguments, "new", &[]),
            [
                "dagq",
                "supervise",
                "--parallel",
                "3",
                "--handoff-token",
                "new"
            ]
            .map(OsString::from)
        );
    }

    /// An option the handed-off binary does not take (`--codex` of a
    /// binary before it) is dropped with its value, in either spelling.
    #[test]
    fn a_handoff_drops_an_option_the_binary_does_not_take() {
        let arguments: Vec<OsString> = [
            "dagq",
            "supervise",
            "--codex",
            "/bin/codex",
            "--claude",
            "/bin/claude",
            "--codex=/bin/codex",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        assert_eq!(
            handoff_arguments(&arguments, "new", &["--codex"]),
            [
                "dagq",
                "supervise",
                "--claude",
                "/bin/claude",
                "--handoff-token",
                "new"
            ]
            .map(OsString::from)
        );
        assert!(!takes_option("/nonexistent/dagq", "--codex"));
    }

    /// Each command a role's Claude settings deny by its policy needs only
    /// the capabilities the table lists for it, so a role granted none of
    /// them is refused it by the CLI as well.
    #[test]
    fn the_denied_commands_need_what_the_table_says() {
        // Clap's parser of every command needs more than a test thread's
        // stack, as `main` gives it.
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(denied_commands_need_what_the_table_says)
            .unwrap()
            .join()
            .unwrap();
    }

    fn denied_commands_need_what_the_table_says() {
        use dagq::application::execution::DAGQ_COMMANDS;
        let samples: &[(&str, &[&str])] = &[
            ("watch", &[]),
            ("report", &[]),
            ("init", &[]),
            ("migrate", &[]),
            ("rebind", &[]),
            ("install", &[]),
            (
                "auto-update",
                &[
                    "--commit", "c", "--token", "t", "--to", "/t", "--repo", "/r", "--log", "/l",
                ],
            ),
            (
                "release-update",
                &[
                    "--release",
                    "1.0.0",
                    "--token",
                    "t",
                    "--to",
                    "/t",
                    "--log",
                    "/l",
                ],
            ),
            ("up", &[]),
            ("down", &[]),
            ("inbox", &[]),
            ("service start", &[]),
            ("service stop", &[]),
            ("service serve", &[]),
            ("queue-broker serve", &[]),
            ("plan", &[]),
            ("supervise", &[]),
            ("throughput-review", &[]),
            ("setup-review", &[]),
            ("setup-review", &["--yes"]),
            ("integrate", &["1"]),
            ("recover", &["r1"]),
            ("run close-workspaces", &["--apply"]),
            ("run close-workspaces", &["r1"]),
            ("run close-workspaces", &["--task", "1"]),
            ("review", &["1"]),
            (
                "session",
                &["--run", "r1", "--lease", "l", "--claude", "/c"],
            ),
            ("planner-session", &["--planner", "1", "--claude", "/c"]),
            ("session-event", &["open"]),
            ("run screen", &["r1"]),
            ("run log", &["r1", "--follow"]),
            ("run send", &["1", "--key", "enter"]),
            ("planner screen", &["1"]),
            ("planner log", &["1", "--lines", "5"]),
            ("planner send", &["1", "--answer", "2"]),
            ("planner request", &["1", "--text", "t"]),
            ("add", &["t"]),
            ("draft", &["1"]),
            ("edit", &["1", "--title", "t"]),
            (
                "judge-follow-up",
                &["1", "--classification", "required", "--reason", "r"],
            ),
            ("set-goal", &["1", "1"]),
            ("set-paths", &["1", "--none"]),
            ("set-priority", &["1", "high"]),
            ("revisit", &["1", "--at", "2026-10-04T12:00:00Z"]),
            ("revisit", &["1", "--clear"]),
            ("dependency", &["add", "1", "2"]),
            ("cancel", &["1"]),
            ("ready", &["1"]),
            ("ready", &["1", "--bypass-review"]),
            ("submit", &["1"]),
            ("proposal withdraw", &["1"]),
            ("goal add", &["g"]),
            ("goal edit", &["1", "--title", "g"]),
            ("goal ready", &["1"]),
            ("goal close", &["1", "--verdict", "achieved"]),
            ("goal review", &["1"]),
            ("note", &["--text", "n", "--task", "1"]),
            ("mark", &["m"]),
            (
                "finding record",
                &["--kind", "k", "--summary", "s", "--queue"],
            ),
            ("finding resolve", &["1", "--reason", "r"]),
            ("finding dismiss", &["1", "--reason", "r"]),
            (
                "finding dismiss",
                &["1", "--reason", "r", "--covered-by", "2"],
            ),
            ("ci failures", &[]),
            ("ci failures", &["--task", "1"]),
            ("request add", &["--text", "plan it", "--ref", "task:1"]),
            ("agent eval", &["adr-rules"]),
            (
                "agent eval",
                &["adr-rules", "--split", "holdout", "--rerun"],
            ),
            ("agent results", &["--agent", "adr-rules"]),
            ("agent result", &["1"]),
            ("agent eval", &["adr-rules", "--set", "3"]),
            ("agent cases", &["adr-rules", "--since", "2026-10-05"]),
            (
                "agent decide",
                &["3", "prod-r1-a1", "--verdict", "clean", "--reason", "r"],
            ),
            ("agent sets", &[]),
            ("agent set", &["3"]),
            ("request decline", &["1", "--reason", "r"]),
            ("replan request", &["--run", "r1", "--reason", "r"]),
            (
                "replan propose",
                &["--run", "r1", "--reason", "r", "--evidence", "3"],
            ),
            (
                "ask",
                &[
                    "--run",
                    "r1",
                    "--kind",
                    "worker_question",
                    "--question",
                    "q",
                    "--because",
                    "scope",
                    "--topic",
                    "task_overlap",
                ],
            ),
            (
                "ask",
                &[
                    "--kind",
                    "blocked",
                    "--finding",
                    "1",
                    "--question",
                    "q",
                    "--because",
                    "scope",
                ],
            ),
            ("ask close", &["1"]),
            ("answer", &["1", "--text", "yes"]),
            ("hold", &["7", "--reason", "r"]),
            ("release", &["7"]),
        ];
        for (command, needs) in DAGQ_COMMANDS {
            let forms: Vec<_> = samples.iter().filter(|(of, _)| of == command).collect();
            assert!(!forms.is_empty(), "no sample of {command}");
            for (_, args) in forms {
                let argv: Vec<&str> = std::iter::once("dagq")
                    .chain(command.split(' '))
                    .chain(args.iter().copied())
                    .collect();
                let cli =
                    Cli::try_parse_from(&argv).unwrap_or_else(|error| panic!("{argv:?}: {error}"));
                let requested = requests(&cli.command);
                assert!(!requested.is_empty(), "{argv:?}");
                for (capability, _) in requested {
                    assert!(needs.contains(&capability), "{argv:?} needs {capability}");
                }
            }
        }
    }

    /// ADR-t3102-2 decisions 1 and 6: of every command of the command line
    /// (each leaf clap knows, counted, so a new one must be placed), the
    /// host `dagq` of the user, the inbox and a planner sends each to the
    /// queue service but those the ADR keeps out, each listed with why: the
    /// processes that open the DB at the end (decision 1), the ones that
    /// open no queue for the command, the wrappers' and the hooks' own
    /// route (decision 6 (3)), the control side's processes (decision 6
    /// (5)) and the migration and the diagnosis (decision 6 (6)); and
    /// `ready --inherit`, which moves a run's branch (ADR-t3102-3 decision
    /// 5), of a command the service runs otherwise. A client-mode `dagq`
    /// refuses by name (`control_side_command`) only `doctor` and `service
    /// status`, which have no use case yet, and sends every command the
    /// service answers.
    #[test]
    fn the_host_cli_sends_every_command_but_the_adr_s_exceptions() {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let outcome =
                    std::panic::catch_unwind(host_cli_sends_every_command_but_the_adr_s_exceptions);
                let _ = done.send(outcome);
            })
            .unwrap();
        if let Err(panic) = finished
            .recv_timeout(Duration::from_secs(60))
            .expect("the count of the commands did not end within 60 seconds")
        {
            std::panic::resume_unwind(panic);
        }
    }

    fn host_cli_sends_every_command_but_the_adr_s_exceptions() {
        use clap::CommandFactory;
        // A node is a command when it takes no subcommand, or runs without
        // one (`ask`); `help` is clap's.
        fn leaves(command: &clap::Command, path: &str, found: &mut Vec<String>) {
            let subs: Vec<_> = command
                .get_subcommands()
                .filter(|sub| sub.get_name() != "help")
                .collect();
            if subs.is_empty() || !command.is_subcommand_required_set() {
                found.push(path.to_owned());
            }
            for sub in subs {
                leaves(sub, &format!("{path} {}", sub.get_name()), found);
            }
        }
        let mut found = Vec::new();
        for sub in Cli::command()
            .get_subcommands()
            .filter(|sub| sub.get_name() != "help")
        {
            leaves(sub, sub.get_name(), &mut found);
        }
        const CONTROL: &str = "a process of the control side: ADR-t3102-2 decision 6 (5)";
        const DIAGNOSIS: &str =
            "the migration and the diagnosis of the queue: ADR-t3102-2 decision 6 (6)";
        const NO_QUEUE: &str = "opens no queue for the command";
        let kept: &[(&str, &str)] = &[
            (
                "init",
                "creates the DB where there is none: ADR-t3102-2 decision 1",
            ),
            (
                "service serve",
                "the queue service itself: ADR-t3102-2 decision 1",
            ),
            ("locate", NO_QUEUE),
            ("plan", "refused for every caller (ADR-t1394-1)"),
            (
                "run close-workspaces",
                "refused for every caller (ADR-t1433-3 decision 3)",
            ),
            ("relay serve", NO_QUEUE),
            ("relay forward", NO_QUEUE),
            ("relay probe", NO_QUEUE),
            ("relay turn", NO_QUEUE),
            ("relay verify", NO_QUEUE),
            ("relay guard", NO_QUEUE),
            ("review-program", NO_QUEUE),
            ("codex-inbox-hook", NO_QUEUE),
            (
                "session",
                "the wrapper's own principal: ADR-t3102-2 decision 6 (3)",
            ),
            (
                "planner-session",
                "the wrapper's own principal: ADR-t3102-2 decision 6 (3)",
            ),
            (
                "session-event",
                "a session's hook: ADR-t3102-2 decision 6 (3)",
            ),
            ("supervise", CONTROL),
            ("up", CONTROL),
            ("down", CONTROL),
            ("install", CONTROL),
            ("auto-update", CONTROL),
            ("release-update", CONTROL),
            ("inbox", CONTROL),
            ("service start", CONTROL),
            ("service stop", CONTROL),
            ("queue-broker serve", CONTROL),
            ("integrate", CONTROL),
            ("review", CONTROL),
            ("observe", CONTROL),
            ("throughput-review", CONTROL),
            ("setup-review", CONTROL),
            ("migrate", DIAGNOSIS),
            ("doctor", DIAGNOSIS),
        ];
        // The arguments each command needs to parse.
        let samples: &[(&str, &[&str])] = &[
            ("add", &["t"]),
            ("lint", &["1"]),
            (
                "ask",
                &["--kind", "decide", "--question", "q", "--because", "scope"],
            ),
            ("show", &["1"]),
            ("ready", &["1"]),
            ("draft", &["1"]),
            ("submit", &["1"]),
            ("proposal show", &["1"]),
            ("proposal withdraw", &["1"]),
            ("cancel", &["1"]),
            ("dependency add", &["1", "2"]),
            ("dependency remove", &["1", "2"]),
            ("goal add", &["g"]),
            ("goal ready", &["1"]),
            ("goal show", &["1"]),
            ("goal edit", &["1", "--title", "g"]),
            ("goal close", &["1", "--verdict", "achieved"]),
            ("goal review", &["1"]),
            (
                "judge-follow-up",
                &["1", "--classification", "required", "--reason", "r"],
            ),
            ("set-goal", &["1", "1"]),
            ("set-paths", &["1", "--none"]),
            ("edit", &["1", "--title", "t"]),
            ("set-priority", &["1", "high"]),
            ("revisit", &["1", "--clear"]),
            ("note", &["--text", "n", "--task", "1"]),
            ("mark", &["m"]),
            ("request add", &["--text", "plan it"]),
            ("request decline", &["1", "--reason", "r"]),
            ("hold", &["7", "--reason", "r"]),
            ("release", &["7"]),
            ("replan request", &["--run", "r1", "--reason", "r"]),
            ("replan propose", &["--run", "r1", "--reason", "r"]),
            ("agent eval", &["adr-rules"]),
            ("agent cases", &["adr-rules"]),
            (
                "agent decide",
                &["3", "prod-r1-a1", "--verdict", "clean", "--reason", "r"],
            ),
            ("agent set", &["3"]),
            ("agent result", &["1"]),
            (
                "finding record",
                &["--kind", "k", "--summary", "s", "--queue"],
            ),
            ("finding resolve", &["1", "--reason", "r"]),
            ("finding dismiss", &["1", "--reason", "r"]),
            ("search", &["q"]),
            ("related", &["1"]),
            (
                "auto-update",
                &[
                    "--commit", "c", "--token", "t", "--to", "/t", "--repo", "/r", "--log", "/l",
                ],
            ),
            (
                "release-update",
                &[
                    "--release",
                    "1.0.0",
                    "--token",
                    "t",
                    "--to",
                    "/t",
                    "--log",
                    "/l",
                ],
            ),
            ("run screen", &["r1"]),
            ("run send", &["r1"]),
            ("run log", &["r1"]),
            ("planner screen", &["1"]),
            ("planner log", &["1"]),
            ("planner send", &["1"]),
            ("planner request", &["1", "--text", "t"]),
            ("integrate", &["1"]),
            ("review", &["1"]),
            ("ask close", &["1"]),
            ("answer", &["1", "--text", "yes"]),
            ("timeline", &["r1"]),
            ("recover", &["r1"]),
            ("relay serve", &["--socket", "/s", "--target", "/t"]),
            ("relay forward", &["--socket", "/s", "--port", "1"]),
            ("relay probe", &["--socket", "/s"]),
            (
                "relay turn",
                &[
                    "--run-dir",
                    "/r",
                    "--clone",
                    "/c",
                    "--base",
                    "b",
                    "--branch",
                    "x",
                    "--facts-only",
                ],
            ),
            ("relay verify", &["--token-file", "/t", "--", "true"]),
            ("relay guard", &["--dir", "/d"]),
            ("review-program", &["secrets"]),
            ("codex-inbox-hook", &["stop"]),
            (
                "session",
                &["--run", "r1", "--lease", "l", "--claude", "/c"],
            ),
            ("session-event", &["open"]),
            ("planner-session", &["--planner", "1", "--claude", "/c"]),
        ];
        let mut sent = 0;
        let mut wrong = Vec::new();
        let mut refused_by_name = Vec::new();
        for leaf in &found {
            let args = samples
                .iter()
                .find(|(of, _)| of == leaf)
                .map_or(&[][..], |(_, args)| *args);
            let argv: Vec<&str> = std::iter::once("dagq")
                .chain(leaf.split(' '))
                .chain(args.iter().copied())
                .collect();
            let Ok(cli) = Cli::try_parse_from(&argv) else {
                wrong.push(format!("{leaf}: give it a sample"));
                continue;
            };
            let routed = host_routed(&cli.command).unwrap();
            let why = kept.iter().find(|(of, _)| of == leaf).map(|(_, why)| *why);
            if routed == why.is_some() {
                wrong.push(format!("{leaf}: sent {routed}, kept {why:?}"));
            }
            sent += usize::from(routed);
            // A client-mode `dagq` refuses by name before sending only a
            // command the service has no use case for.
            if control_side_command(&cli.command).is_some() {
                refused_by_name.push(leaf.as_str());
                if client_request(&cli.command).unwrap().is_some() {
                    wrong.push(format!(
                        "{leaf}: a use case refused as control_side_command"
                    ));
                }
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
        assert_eq!(refused_by_name, ["doctor", "service status"]);
        for (leaf, _) in kept {
            assert!(found.iter().any(|found| found == leaf), "no command {leaf}");
        }
        assert_eq!(found.len(), sent + kept.len(), "{found:?}");
        // The reads and the exports among a kept command's forms are sent.
        for argv in [
            &["dagq", "observe", "--history"][..],
            &["dagq", "observe", "--input", "1"],
            &["dagq", "graph", "--format", "svg", "--out", "g.svg"],
        ] {
            let cli = Cli::try_parse_from(argv).unwrap();
            assert!(host_routed(&cli.command).unwrap(), "{argv:?}");
        }
        // A sent command's form that moves a run's branch is not.
        let cli =
            Cli::try_parse_from(["dagq", "ready", "1", "--inherit", "--reason", "r"]).unwrap();
        assert!(!host_routed(&cli.command).unwrap());
    }

    /// ADR-t2159-1 decision 6: the command lines an earlier binary
    /// registered or started (the launchd plist's `supervise`, the update
    /// jobs', the wrappers', the service's, and `up`, `down`, `install` and
    /// the other commands that took a `--cmux`) still parse with their
    /// `--cmux <path>`, and no command's help shows it.
    #[test]
    fn a_registered_argv_with_a_cmux_parses_and_no_help_shows_it() {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(registered_argv_with_a_cmux_parses_and_no_help_shows_it)
            .unwrap()
            .join()
            .unwrap();
    }

    fn registered_argv_with_a_cmux_parses_and_no_help_shows_it() {
        use clap::CommandFactory;
        let cmux = ["--cmux", "/opt/homebrew/bin/cmux"];
        let plist = [
            "/opt/bin/dagq",
            "--db",
            "/q/queue.db",
            "supervise",
            "--log-dir",
            "/q/logs",
            "--cmux",
            "/opt/homebrew/bin/cmux",
            "--claude",
            "/opt/bin/claude",
            "--codex",
            "/opt/bin/codex",
            "--mode",
            "launchd",
            "--auto-update",
        ];
        assert!(matches!(
            Cli::try_parse_from(plist).unwrap().command,
            Command::Supervise {
                auto_update: true,
                ..
            }
        ));
        let argvs: [&[&str]; 20] = [
            &["up", "--parallel", "3"],
            &["down", "--wait"],
            &["install", "--allow-breaking"],
            &[
                "auto-update",
                "--commit",
                "c",
                "--token",
                "t",
                "--to",
                "/b",
                "--repo",
                "/r",
                "--log",
                "/l",
            ],
            &[
                "release-update",
                "--release",
                "0.4.0",
                "--token",
                "t",
                "--to",
                "/b",
                "--log",
                "/l",
            ],
            &["observe", "--daily"],
            &["planners"],
            &[
                "ask",
                "--kind",
                "worker_question",
                "--question",
                "q",
                "--because",
                "scope",
            ],
            &["watch", "--role", "inbox"],
            &["stats", "--full"],
            &["session", "--run", "r", "--lease", "l", "--claude", "/c"],
            &["planner-session", "--planner", "1", "--claude", "/c"],
            &["service", "start"],
            &["service", "serve"],
            &["run", "screen", "1"],
            &["run", "send", "1", "--key", "enter"],
            &["run", "close-workspaces"],
            &["planner", "screen", "1"],
            &["planner", "send", "1", "--key", "enter"],
            &["planner", "request", "1", "--text", "more"],
        ];
        for argv in argvs {
            let argv: Vec<&str> = std::iter::once("dagq")
                .chain(argv.iter().copied())
                .chain(cmux)
                .collect();
            Cli::try_parse_from(&argv).unwrap_or_else(|error| panic!("{argv:?}: {error}"));
        }
        // Every command that takes it hides it.
        fn with_cmux(command: &clap::Command, path: &str, found: &mut Vec<String>) {
            if let Some(arg) = command
                .get_arguments()
                .find(|arg| arg.get_long() == Some("cmux"))
            {
                assert!(arg.is_hide_set(), "{path} shows --cmux");
                let help = command.clone().render_long_help().to_string();
                assert!(!help.contains("--cmux"), "{path}: {help}");
                found.push(path.to_owned());
            }
            for sub in command.get_subcommands() {
                with_cmux(sub, &format!("{path} {}", sub.get_name()), found);
            }
        }
        let mut found = Vec::new();
        with_cmux(&Cli::command(), "dagq", &mut found);
        assert_eq!(found.len(), 21, "{found:?}");
    }

    /// The wrapper argv an older supervisor or planner opener gives, without
    /// `--background` and with `--cmux`, still parses into the wrapper's
    /// command, which starts in a workspace as before.
    #[test]
    fn an_older_wrapper_argv_without_background_is_accepted() {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(older_wrapper_argv_without_background_is_accepted)
            .unwrap()
            .join()
            .unwrap();
    }

    fn older_wrapper_argv_without_background_is_accepted() {
        let session = [
            "dagq", "session", "--run", "r1", "--lease", "l", "--claude", "/c", "--codex", "/x",
            "--resume", "--cmux", "/m",
        ];
        let cli = Cli::try_parse_from(session).unwrap();
        assert!(matches!(
            cli.command,
            Command::Session {
                resume: true,
                background: false,
                ..
            }
        ));
        for planner in [
            &[
                "dagq",
                "planner-session",
                "--planner",
                "1",
                "--claude",
                "/c",
                "--cmux",
                "/m",
            ][..],
            &[
                "dagq",
                "planner-session",
                "--planner",
                "1",
                "--claude",
                "/c",
                "--model",
                "m",
                "--effort",
                "e",
                "--cmux",
                "/m",
                "--headless",
            ][..],
        ] {
            let cli =
                Cli::try_parse_from(planner).unwrap_or_else(|error| panic!("{planner:?}: {error}"));
            assert!(
                matches!(
                    cli.command,
                    Command::PlannerSession {
                        planner: 1,
                        background: false,
                        ..
                    }
                ),
                "{planner:?}"
            );
        }
    }

    /// A client-mode `dagq agent ...` goes to the use case the service
    /// names for its command line, with the params the command line gives.
    #[test]
    fn an_agent_command_goes_to_the_use_case_its_command_line_names() {
        // Clap's parser needs a big stack; the join is bounded so that a
        // stalled parse fails here by name.
        let (done, finished) = std::sync::mpsc::channel();
        let parsing = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                for (argv, params) in [
                    (
                        &["dagq", "agent", "sets", "--agent", "adr-rules"][..],
                        json!({"agent": "adr-rules", "limit": 20}),
                    ),
                    (&["dagq", "agent", "set", "3"], json!({"id": 3})),
                    (
                        &["dagq", "agent", "results", "--limit", "5"],
                        json!({"agent": null, "limit": 5}),
                    ),
                    (&["dagq", "agent", "result", "3"], json!({"id": 3})),
                ] {
                    let cli = Cli::try_parse_from(argv).unwrap();
                    let (use_case, sent) = client_request(&cli.command).unwrap().unwrap();
                    assert_eq!(Some(use_case), UseCase::of_command(&argv[1..]), "{argv:?}");
                    assert_eq!(sent, params, "{argv:?}");
                }
                done.send(()).unwrap();
            })
            .unwrap();
        match finished.recv_timeout(std::time::Duration::from_secs(60)) {
            // A panic in the thread drops `done`: its join says why.
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                parsing.join().unwrap();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
                "an_agent_command_goes_to_the_use_case_its_command_line_names: \
                 the agent commands were not parsed and sent within 60s"
            ),
        }
    }

    /// Task 1561 (ADR-t1566-1 decision 3): every command the plan review
    /// prompt names to read what its limits left out is a read the plan
    /// review job's role may run, on the command line and as a client of
    /// the queue service, which authorizes it as a read.
    #[test]
    fn the_plan_review_job_may_run_each_read_its_prompt_names() {
        // Clap's parser needs more than a test thread's stack, as `main`
        // gives it.
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(plan_review_job_may_run_each_read_its_prompt_names)
            .unwrap()
            .join()
            .unwrap();
    }

    fn plan_review_job_may_run_each_read_its_prompt_names() {
        assert!(dagq::runtime::PLAN_REVIEW_ACCESS.runs_queue_cli());
        let actor = ActorContext::plan_review_job(3, 1);
        for form in dagq::application::prompt::PLAN_REVIEW_READS {
            let line = form
                .replace("--limit ID", "--limit 200")
                .replace("'<words of its title>'", "words")
                .replace("ID", "3");
            let argv: Vec<&str> = line.split(' ').collect();
            let cli =
                Cli::try_parse_from(&argv).unwrap_or_else(|error| panic!("{argv:?}: {error}"));
            let requested = requests(&cli.command);
            assert!(!requested.is_empty(), "{argv:?}");
            for (capability, _) in requested {
                assert_eq!(capability, Capability::QueueRead, "{argv:?}");
            }
            check_access(&actor, &cli.command, None)
                .unwrap_or_else(|error| panic!("{argv:?}: {error:#}"));
            assert!(
                client_request(&cli.command).unwrap().is_some(),
                "{argv:?} has no use case of the queue service"
            );
        }
    }

    /// Task 1571 (ADR-t1566-1 decision 3): every command the goal review's
    /// prompt and the prompts of the planners the runtime opens name to
    /// read what their limits left out is a read the job's or the
    /// planner's role may run on the command line; the goal review job's
    /// `dagq` runs as a client of the queue service, which has a use case
    /// for each (a planner opens the queue itself).
    #[test]
    fn the_goal_review_job_and_the_planners_may_run_each_read_their_prompts_name() {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                assert!(dagq::runtime::GOAL_REVIEW_ACCESS.runs_queue_cli());
                let readers = [
                    (
                        ActorContext::goal_review_job(3, 1),
                        dagq::application::prompt::GOAL_REVIEW_READS,
                        true,
                    ),
                    (
                        ActorContext::instance(dagq::domain::ActorRole::Planner, 3),
                        dagq::application::prompt::PLANNER_READS,
                        false,
                    ),
                ];
                for (actor, reads, client) in readers {
                    for form in reads {
                        let line = form.replace("ID", "3");
                        let argv: Vec<&str> = line.split(' ').collect();
                        let cli = Cli::try_parse_from(&argv)
                            .unwrap_or_else(|error| panic!("{argv:?}: {error}"));
                        let requested = requests(&cli.command);
                        assert!(!requested.is_empty(), "{argv:?}");
                        for (capability, _) in requested {
                            assert_eq!(capability, Capability::QueueRead, "{argv:?}");
                        }
                        check_access(&actor, &cli.command, None)
                            .unwrap_or_else(|error| panic!("{actor:?} {argv:?}: {error:#}"));
                        assert!(
                            !client || client_request(&cli.command).unwrap().is_some(),
                            "{argv:?} has no use case of the queue service"
                        );
                    }
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// The subcommands `DAGQ_COMMANDS` leaves out, each for a reason: the
    /// ones that only read; the ones with a form that reads, which a rule
    /// on the name would deny as well (the CLI refuses the other form); and
    /// the ones that open no queue (`relay`): those that only carry bytes
    /// to the queue broker, which checks what passes, and the turn the
    /// executor starts in a run's container, which runs the agent there
    /// with the run's own token. A subcommand on neither
    /// list fails
    /// [`every_subcommand_is_denied_or_left_out_on_purpose`].
    const LEFT_OUT_COMMANDS: &[&str] = &[
        // Read the queue.
        "locate",
        "list",
        "show",
        "lint",
        "proposal list",
        "proposal show",
        "goal list",
        "goal show",
        "notes",
        "marks",
        "findings",
        "requests",
        "replans",
        "search",
        "related",
        "candidates",
        "planners",
        "status",
        "asks",
        "events",
        "timeline",
        "stats",
        "kpi",
        "forecast",
        "doctor",
        "service status",
        // Forms that read: `graph` without `--out` and `observe --history` /
        // `--input`.
        "graph",
        "observe",
        // Carry bytes between a run's container and the queue broker, and
        // open no queue: the broker checks what passes (ADR-t2114-3).
        "relay serve",
        "relay forward",
        "relay probe",
        // Read the worktree's Git and print what a built-in review program
        // found, and open no queue (ADR-t2259-1).
        "review-program",
        // A worker's turn in its container (`dagq relay turn`): it opens no
        // queue, and the agent it runs is the worker's own.
        "relay turn",
        // A command of integrate's verification in its container (`dagq
        // relay verify`): it opens no queue, and runs the task's command.
        "relay verify",
        // The sccache guard of a worker's turn in its container (`dagq
        // relay guard`): it opens no queue, and makes a link to itself.
        "relay guard",
        // A Codex inbox's hook: it opens no queue, and the `dagq` it runs
        // is checked as the inbox's (ADR-t1181-1 decision 2).
        "codex-inbox-hook",
    ];

    /// Every subcommand clap knows, nested ones by their path (`goal
    /// close`), is in `DAGQ_COMMANDS` or on [`LEFT_OUT_COMMANDS`] (task
    /// 851): a new command that changes state is not left out of the
    /// Claude settings' deny rules unnoticed. A parent's entry in
    /// `DAGQ_COMMANDS` covers its subcommands (`dependency`), and a parent
    /// that only groups subcommands (`goal`) needs no entry of its own.
    #[test]
    fn every_subcommand_is_denied_or_left_out_on_purpose() {
        // Building clap's command needs more than a test thread's stack.
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(every_subcommand_is_listed)
            .unwrap()
            .join()
            .unwrap();
    }

    fn every_subcommand_is_listed() {
        use clap::CommandFactory;
        use dagq::application::execution::DAGQ_COMMANDS;
        // Each subcommand's path, and whether it only groups others (`goal`
        // runs nothing of its own; `ask` does).
        fn paths(command: &clap::Command, prefix: &str, out: &mut Vec<(String, bool)>) {
            for sub in command.get_subcommands() {
                let path = format!("{prefix}{}", sub.get_name());
                out.push((path.clone(), sub.is_subcommand_required_set()));
                paths(sub, &format!("{path} "), out);
            }
        }
        let mut found = Vec::new();
        paths(&Cli::command(), "", &mut found);
        let listed = |path: &str| {
            DAGQ_COMMANDS.iter().any(|(command, _)| *command == path)
                || LEFT_OUT_COMMANDS.contains(&path)
        };
        for (path, groups) in &found {
            // A parent's entry in DAGQ_COMMANDS denies its subcommands too;
            // a group with no entry of its own is covered by its children's.
            let covered = listed(path)
                || path.match_indices(' ').any(|(at, _)| {
                    DAGQ_COMMANDS
                        .iter()
                        .any(|(command, _)| *command == &path[..at])
                });
            assert!(
                covered || *groups,
                "`dagq {path}` is neither in DAGQ_COMMANDS nor in LEFT_OUT_COMMANDS"
            );
        }
        let all: Vec<&String> = found.iter().map(|(path, _)| path).collect();
        for path in DAGQ_COMMANDS
            .iter()
            .map(|(command, _)| *command)
            .chain(LEFT_OUT_COMMANDS.iter().copied())
        {
            assert!(
                all.iter().any(|known| *known == path),
                "no command `{path}`"
            );
            assert!(
                !(DAGQ_COMMANDS.iter().any(|(command, _)| *command == path)
                    && LEFT_OUT_COMMANDS.contains(&path)),
                "`{path}` is on both lists"
            );
        }
    }

    #[test]
    fn no_help_cites_a_dagq_adr_or_task_number() {
        // Building clap's command needs more than a test thread's stack.
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(every_help_is_plain)
            .unwrap()
            .join()
            .unwrap();
    }

    /// The short and the long help of `dagq` and of every subcommand (what
    /// `-h` and `--help` print, possible values included) state the rules
    /// in plain words: a pointer for maintainers goes in a `//` comment,
    /// which clap does not read.
    fn every_help_is_plain() {
        use clap::CommandFactory;
        fn check(command: &mut clap::Command, path: &str, found: &mut Vec<String>) {
            for help in [command.render_help(), command.render_long_help()] {
                // Joined, so that a citation wrapped over two lines is found.
                let text = help
                    .to_string()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if let Some(citation) = crate::history_citation::first_citation(&text) {
                    found.push(format!("`{path} --help`: {citation:?}"));
                }
            }
            for sub in command.get_subcommands_mut() {
                let path = format!("{path} {}", sub.get_name());
                check(sub, &path, found);
            }
        }
        let mut command = Cli::command();
        command.build();
        let mut found = Vec::new();
        check(&mut command, "dagq", &mut found);
        found.dedup();
        assert!(
            found.is_empty(),
            "the help cites dagq's ADRs or task numbers; state the rule in plain words:\n{}",
            found.join("\n")
        );
    }

    /// `watch` asks to watch the queue, `graph --out` and `report` to
    /// export it to a file, and the reads (`graph` without `--out`
    /// included) to read it: the capabilities a worker, a wrapper, the
    /// integrator and the jobs are refused or granted by the table
    /// (moved from `tests/it/cli_authorization.rs`, task 1709; the
    /// refusal's wiring and record stay there).
    #[test]
    fn watching_and_exporting_ask_their_own_capability_and_reads_ask_to_read() {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(watching_and_exporting_ask_their_own_capability)
            .unwrap()
            .join()
            .unwrap();
    }

    fn watching_and_exporting_ask_their_own_capability() {
        let forms: [(&[&str], Capability); 8] = [
            (
                &["watch", "--after", "0", "--timeout", "1"],
                Capability::QueueWatch,
            ),
            (
                &["graph", "--format", "d2", "--out", "/g.d2"],
                Capability::ExportFile,
            ),
            (&["report", "--out", "/reports"], Capability::ExportFile),
            (&["report", "--print", "json"], Capability::ExportFile),
            (&["status"], Capability::QueueRead),
            (&["show", "1"], Capability::QueueRead),
            (&["list"], Capability::QueueRead),
            (&["graph"], Capability::QueueRead),
        ];
        for (args, capability) in forms {
            let argv: Vec<&str> = std::iter::once("dagq")
                .chain(args.iter().copied())
                .collect();
            let cli =
                Cli::try_parse_from(&argv).unwrap_or_else(|error| panic!("{argv:?}: {error}"));
            assert!(!authorized_in_application(&cli.command), "{argv:?}");
            assert_eq!(
                requests(&cli.command),
                [(capability, Resource::Queue)],
                "{argv:?}"
            );
        }
    }

    /// A refusal names the role, the capability and why; the observer and
    /// every headless job keep the message they always got (ADR-0027,
    /// ADR-t728-1 decision 2; moved from `tests/it/cli_actor.rs`, task
    /// 1709).
    #[test]
    fn a_refusal_of_the_observer_or_a_job_keeps_its_old_message() {
        let refusal = |role| {
            error_json(&anyhow::Error::new(AuthorizationError {
                role,
                capability: Capability::MarkWrite,
                reason: dagq::domain::authorization::DenyReason::NotGranted,
            }))
        };
        for job in [
            ActorRole::ReviewJob,
            ActorRole::RecoveryJob,
            ActorRole::PlanReviewJob,
            ActorRole::GoalReviewJob,
            ActorRole::ThroughputReviewJob,
            ActorRole::SetupReviewJob,
        ] {
            let error = refusal(job);
            assert_eq!(error["error"], REVIEWER_DENIED, "{job:?}");
            assert_eq!(error["denied"]["role"], json!(job), "{job:?}");
            assert_eq!(error["denied"]["capability"], "mark.write");
            assert_eq!(error["denied"]["reason"], "not granted");
        }
        assert_eq!(refusal(ActorRole::Observer)["error"], OBSERVER_DENIED);
        assert_eq!(
            refusal(ActorRole::Worker)["error"],
            "worker may not mark.write (not granted)"
        );
    }

    /// A client-mode dagq prints the service's refusal of the policy as the
    /// command line prints its own, with the service's code; a failure with
    /// nothing refused prints only the code and the message.
    #[test]
    fn a_refusal_by_the_service_prints_as_a_local_one_with_its_code() {
        use dagq::domain::queue_service::ServiceDenial;
        use dagq::infrastructure::queue_service::ClientFailure;
        for role in [
            ActorRole::Worker,
            ActorRole::Observer,
            ActorRole::PlanReviewJob,
        ] {
            let refused = AuthorizationError {
                role,
                capability: Capability::MarkWrite,
                reason: dagq::domain::authorization::DenyReason::NotGranted,
            };
            let local = error_json(&anyhow::Error::new(refused.clone()));
            let served = error_json(&anyhow::Error::new(ClientFailure {
                denied: Some(Box::new(ServiceDenial::from(&refused))),
                ..ClientFailure::new("authorization_denied", refused.to_string())
            }));
            assert_eq!(served["queue_service"]["code"], "authorization_denied");
            assert_eq!(served["error"], local["error"], "{role:?}");
            assert_eq!(served["denied"], local["denied"], "{role:?}");
        }
        // A role this binary does not know keeps the service's message.
        let served = error_json(&anyhow::Error::new(ClientFailure {
            denied: Some(Box::new(ServiceDenial {
                role: "future-job".to_owned(),
                capability: "note.write".to_owned(),
                reason: "not granted".to_owned(),
            })),
            ..ClientFailure::new("authorization_denied", "future-job may not note.write")
        }));
        assert_eq!(served["error"], "future-job may not note.write");
        assert_eq!(served["denied"]["role"], "future-job");
        let served = error_json(&anyhow::Error::new(ClientFailure::new(
            "no_use_case",
            "not served",
        )));
        assert_eq!(
            served,
            json!({"error": "not served", "queue_service": {"code": "no_use_case"}})
        );
    }
}
