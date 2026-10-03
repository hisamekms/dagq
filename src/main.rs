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
use dagq::application::queue_reads::{self as reads, QueueRead};
use dagq::domain::queue_service::UseCase;
use dagq::{
    application::{PlanRequestStore, TaskStore},
    domain::{
        ActorContext, ActorRole, AskId, AskKind, AskReason, AuthorizationError, Capability,
        EventId, FindingId, FindingTarget, GoalEdit, GoalId, GoalVerdict, LeaseToken, NewAsk,
        NewFinding, NewGoal, NewNote, NewTask, NoteTarget, PlannerId, PlannerOrigin, PlannerOwner,
        ProposalId, RequestId, Resource, RunId, SessionRole, StaticPolicy, Submission, TaskEdit,
        TaskId, TaskStatus, worker::WorkerMode,
    },
    infrastructure::{adapters::path_text, location::QueueLocation, sqlite::SqliteQueue},
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

#[derive(Subcommand, Clone)]
enum Command {
    /// Initialize the queue, creating its directory if needed. An existing queue is checked, never migrated.
    Init,
    /// Apply the migrations this binary knows and the queue lacks; opening a queue never does (ADR-0045).
    /// A breaking migration is refused while a supervisor, run or wrapper uses the queue, and the
    /// database is copied to `backups/` first.
    Migrate {
        /// Report the schema and what would be applied, without changing anything.
        #[arg(long)]
        check: bool,
    },
    /// Replace this dagq binary and hand the queue's live supervisor over to the new one without
    /// waiting for its sessions (ADR-0045): build (or take) the binary, check its --version and a
    /// start on a throwaway queue, apply the queue's compatible migrations, put it in place by a
    /// rename that keeps the old one as <name>.previous, and ask the supervisor to exec it. A
    /// failed handoff puts the old binary back.
    Install {
        /// A checkout to build (`cargo build --release --locked -p dagq -p dagq-broker-client`), or a
        /// built binary; the dagq-broker-client beside it goes in place beside dagq. Default: build
        /// the main checkout of the repository of the working directory.
        #[arg(long, conflicts_with_all = ["rollback", "release"])]
        from: Option<PathBuf>,
        /// Install a release of crates.io instead (ADR-t618-1): `cargo install --locked
        /// dagq@<VERSION>` under the queue's update/release directory, then the same check, swap
        /// and handoff. Without VERSION, the newest release.
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
        /// cmux executable: stops an in-cmux supervisor for the drain, and its restart uses it.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
        /// Claude Code executable the restarted supervisor uses after a drain.
        #[arg(long)]
        claude: Option<PathBuf>,
        /// Codex CLI the restarted supervisor's Codex workers use after a drain.
        #[arg(long)]
        codex: Option<PathBuf>,
        /// Plugin directory of the restarted `up` after a drain.
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
        /// Install a checkout of dagq's source without first running its e2e (`cargo test --locked
        /// --test e2e -- --ignored`, ADR-t963-1 decision 1), which otherwise must pass before
        /// anything is replaced. For a person in a hurry; a built binary, `--rollback` and
        /// `--release` have no e2e.
        #[arg(long)]
        skip_e2e: bool,
        /// A shell command in place of the e2e (tests).
        #[arg(long, hide = true)]
        e2e_command: Option<String>,
        /// Seconds the e2e may run before it counts as failed.
        #[arg(long, hide = true, default_value_t = 1800)]
        e2e_timeout: u64,
        /// Milliseconds between two looks at the supervisors asked to hand off (tests shorten
        /// it, task 1048).
        #[arg(long, hide = true, default_value_t = 500)]
        poll_ms: u64,
    },
    /// Show which queue this directory resolves to, without opening it.
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
        /// How urgently the supervisor should claim it: interrupt (ahead of every other ready
        /// task), urgent (a defect stopping the operation), high (groundwork other work needs
        /// soon), normal, or low (later). A ready task it waits for inherits it.
        #[arg(long, default_value = "normal", value_parser = PRIORITIES)]
        priority: String,
        /// The kind of change the task makes (ADR-t980-1), as a label of the repository's own (a
        /// lowercase slug of letters, digits, '-' and '_', not `unknown` or `all`); `stats`, `kpi`
        /// and `forecast` group the runs by it. When dagq.toml has `[tasks] changes`, one of them,
        /// and `lint` and `submit` refuse a task without one. Omitted: none.
        #[arg(long)]
        change: Option<String>,
        /// The agent the worker runs on (ADR-t813-2): claude or codex. Omitted: claude.
        #[arg(long, value_parser = PROVIDERS)]
        provider: Option<String>,
        /// Run the worker non-interactively, one call per turn (ADR-t813-1), and store that mode.
        /// Omitted (with no --interactive): the provider's default, headless for Claude
        /// (ADR-t1340-1) and for codex (its only mode); the task follows a later change of it.
        #[arg(long, conflicts_with = "interactive")]
        headless: bool,
        /// Run the worker in Claude's interactive session in the cmux terminal (Claude only:
        /// refused with --provider codex), for a task a person wants to watch or step into.
        #[arg(long)]
        interactive: bool,
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
        /// Skip plan review (recorded as a review_bypassed event).
        #[arg(long)]
        bypass_review: bool,
    },
    /// Return a ready or submitted task to draft.
    Draft { id: i64 },
    /// Submit draft tasks for plan review as one proposal owned by this planner session: TASKs,
    /// and with --goal a goal and its draft tasks. The tasks become submitted, which no claim
    /// takes; plan review makes them ready. Prints the proposal.
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
        /// Open finding the proposal remedies; repeatable. It becomes proposed with the proposal
        /// (ADR-0044 decision 19). A planner the runtime opened for a finding links that one
        /// without it.
        #[arg(long = "finding")]
        findings: Vec<i64>,
    },
    /// Check the fixed rules of a plan (plan review's mechanical checks): dependency cycles,
    /// dependencies on completed, canceled or unsubmitted draft tasks and on abandoned goals, no
    /// verification (with or without declared paths), invalid path globs, blank acceptance and
    /// titles repeated within the set. Lints TASKs and the members of each --proposal. Prints
    /// {"tasks", "violations"}, each violation {"code", "task_id", "reason"}; none: an empty list.
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
    /// Cancel a draft, submitted or ready task. Does not satisfy its dependents.
    Cancel {
        id: i64,
        /// Record the task as a duplicate of this one (ADR-0046): another task that exists and is
        /// not canceled; a completed one means it is already implemented there. `show`, `list`
        /// and `stats` report it.
        #[arg(long = "duplicate-of")]
        duplicate_of: Option<i64>,
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
    /// Move a draft or ready task to an open goal, or out of its goal with --none.
    SetGoal {
        /// Draft or ready task to move.
        task: i64,
        /// Open goal to join; omit it and pass --none to leave the current goal.
        #[arg(required_unless_present = "none", conflicts_with = "none")]
        goal: Option<i64>,
        /// Remove the task from its goal.
        #[arg(long)]
        none: bool,
    },
    /// Replace the paths a draft or ready task may change (`add --paths`), or remove the limit with --none.
    SetPaths {
        /// Draft or ready task.
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
    /// Replace fields of a draft or submitted task. User or inbox may replace only --verify
    /// on an in_progress task after its latest run ended and no live run remains. Each given
    /// field replaces the old value; `task_edited` records old and new values and actor.
    #[command(group = clap::ArgGroup::new("field").multiple(true).required(true))]
    Edit {
        /// Draft or submitted task, or an eligible in_progress task for --verify only.
        task: i64,
        #[arg(long, group = "field")]
        title: Option<String>,
        #[arg(long, group = "field")]
        description: Option<String>,
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
        /// The agent the worker runs on (`add --provider`): claude or codex. Without --headless or
        /// --interactive, the worker takes that provider's default mode (headless for both) and
        /// the task names none.
        #[arg(long, group = "field", value_parser = PROVIDERS)]
        provider: Option<String>,
        /// Run the worker non-interactively (`add --headless`), named on the task.
        #[arg(long, group = "field", conflicts_with = "interactive")]
        headless: bool,
        /// Run the worker in the agent's interactive session (`add --interactive`; Claude only).
        #[arg(long, group = "field")]
        interactive: bool,
    },
    /// Give a draft or ready task another priority (`add --priority`); it takes effect at the
    /// next claim and never stops a running run.
    SetPriority {
        /// Draft or ready task.
        task: i64,
        /// interrupt, urgent, high, normal or low.
        #[arg(value_parser = PRIORITIES)]
        level: String,
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
    /// Record a change mark (ADR-0051 decision 12): a change the KPIs should be compared
    /// before and after (a setting, the operation, the host). With --retract, record that an
    /// earlier mark (a `dagq mark` or a `[run.env]` change) was none. Prints the mark. Not for
    /// the observer or a job.
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
    /// List the change marks oldest first by when they took effect (ADR-0051 decision 12): the
    /// recorded ones (supervisor start, handoff and stop, `[run.env]` changes, `dagq mark` and
    /// its retractions) and the ones derived from the claims (`derived:dagq_version`,
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
    /// Record a planning request for a planner of the runtime's (ADR-t1394-1), or decline one.
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
    /// Record a finding (ADR-0044 decision 18), or resolve or dismiss one.
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
    /// Full-text search of tasks (title, description, acceptance, context), goals (title,
    /// description, acceptance, constraints), notes and the messages of landed commits, in every
    /// status (ADR-0046). QUERY is words (all must match; `"..."` for a phrase) with FTS5's AND,
    /// OR, NOT and parentheses; any substring of 3 or more characters matches, including in
    /// Japanese, and shorter terms must all be present. Prints {"hits", "total"}, best first: per
    /// hit its kind, id (a task, goal or note event ID, or a commit SHA), status, title and the
    /// matching field with an excerpt marking the match with « ».
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
    /// The tasks most related to TASK (any status, drafts included), scored by fixed rules
    /// (ADR-0046 decision 4): declared paths that overlap; file names, test names (snake_case of
    /// three or more words, `--test NAME`), ADR numbers and task numbers in the texts and landed
    /// commit messages; follow-ups of the same run; the same goal; and how strongly the search
    /// index matches TASK's title against the other task. A clue many tasks share counts less.
    /// Prints {"task_id", "related", "total"}, best first: per task its id, status, title, score,
    /// the clues that scored it ({"clue", "value", "weight"}) and `duplicate_of` when it was
    /// canceled as a duplicate.
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
    /// `effective_priority`, then most `unblocks`, then lowest ID); does not claim.
    Candidates,
    /// Show the unfinished tasks' dependencies: per task its direct predecessors (`depends_on`),
    /// its goal dependencies (`goal_dependencies`), what it still waits for (`ready_after`: unfinished
    /// predecessors, then `{"goal": ID}` for goals not closed as achieved), the tasks it blocks
    /// directly (including those waiting for its open goal) and how many it releases transitively
    /// (`unblocks`), its `priority` and the `effective_priority` it inherits from the ready tasks
    /// waiting for it; `candidates` in claim order and the `critical` chain. With `--format d2`
    /// or `svg`, the near-term dependency diagram instead (ADR-0077): the d2 source, or the SVG the
    /// host's `d2 --layout=tala` draws from it.
    Graph {
        /// Only this goal's tasks and candidates; counts still span every goal.
        #[arg(long = "goal")]
        goal_id: Option<i64>,
        /// `json` (the dependency view), `d2` (the diagram's source) or `svg` (drawn by d2 and TALA
        /// from PATH).
        #[arg(long, default_value = "json", value_parser = ["json", "d2", "svg"])]
        format: String,
        /// Write the d2 source or the SVG to this file instead of stdout.
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
        /// Maximum number of runs waiting for a person's answer outside the
        /// --parallel slots (ADR-0062); 0 keeps every run in its slot.
        /// Without it, `max_waiting` of `[supervisor]` in dagq.toml, else 4.
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
        /// cmux executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
        /// Codex CLI the Codex workers start (ADR-t813-2); a bare name is resolved on PATH. When it
        /// is not found, the supervisor goes on and starts Codex tasks with non-interactive Claude
        /// (provider_switched, reason executable_missing).
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        /// Write this start's JSON Lines log (supervise-<UTC time>-<pid>.jsonl)
        /// into this directory (created if missing) instead of the queue's
        /// logs/; messages also go to stderr.
        #[arg(long)]
        log_dir: Option<PathBuf>,
        /// Start the observer job (`observe`) when this many seconds passed
        /// since the last one started or finished; 0 disables the observer.
        /// Default 3600, or 0 with --once.
        #[arg(long)]
        observe_interval: Option<u64>,
        /// Also run the daily observation of the last 24 hours once a day.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        observe_daily: bool,
        /// Start the throughput reviews (ADR-t996-1): each hour one whose landings the runtime's rules
        /// find off (an hour that meets none starts no agent), each day one of yesterday and each week
        /// one of the ISO week before, saved under the queue's reports/reviews/ and told to the inbox.
        /// Default true, or false with --once.
        #[arg(long, action = clap::ArgAction::Set)]
        throughput_review: Option<bool>,
        /// Write the KPI reports of each finished day and ISO week under the queue's reports/
        /// on the first pass after local midnight (ADR-0051 decision 20).
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        report_daily: bool,
        /// Record the forecast of every open task and goal as one event when a plan review
        /// passes, on a change mark, after a landing that moved it and once a day (ADR-0070).
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
        /// Continue the registered supervisor with this token after it
        /// exec'd this binary (ADR-0045 decision 10); set by the handoff.
        #[arg(long, hide = true)]
        handoff_token: Option<String>,
        /// How `up` started this process (launchd or in_cmux), for its start mark (ADR-0051
        /// decision 10); set by `up`.
        #[arg(long, hide = true)]
        mode: Option<String>,
        /// Register with the automatic update on: build and install the runtime of every landing
        /// on main that changes it (ADR-0045 decision 17). `up --auto-update` starts it so.
        #[arg(long)]
        auto_update: bool,
        /// Seconds between two looks at main for the automatic update.
        #[arg(long, default_value_t = 30)]
        update_interval: u64,
        /// A shell command the automatic update runs in place of `cargo build --release --locked -p
        /// dagq -p dagq-broker-client` (tests); it must leave the binary at
        /// $CARGO_TARGET_DIR/release/dagq (and the client beside it, when there is one).
        #[arg(long, hide = true)]
        update_build_command: Option<String>,
        /// A shell command the automatic update runs in place of its e2e gate (`cargo test --locked
        /// --test e2e -- --ignored`, ADR-t963-1 decision 1; tests).
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
        /// Milliseconds between two heartbeats of the registration and the leases (tests; 2000
        /// by default, task 1048).
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
    /// The automatic update's job (ADR-0045 decision 17), which the supervisor starts: build
    /// main's COMMIT in the queue's update checkout, check it, put it in place of --to like
    /// `install` and watch the supervisor take it; on a failure put the old binary back, start the
    /// supervisor again when it is gone, and open the `update_failed` ask.
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
        /// A shell command in place of the e2e the build passes before it is put in place (`cargo
        /// test --locked --test e2e -- --ignored`, ADR-t963-1 decision 1).
        #[arg(long)]
        e2e_command: Option<String>,
        /// Seconds the e2e may run before it counts as failed.
        #[arg(long, default_value_t = 1800)]
        e2e_timeout: u64,
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
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
        /// Milliseconds between two looks at the handoff and at the new supervisor's heartbeat
        /// (tests shorten it, task 1048).
        #[arg(long, hide = true, default_value_t = 500)]
        poll_ms: u64,
    },
    /// The release update's job (ADR-t618-1 decision 5), which a supervisor of a release build
    /// starts: `cargo install` RELEASE under the queue's update directory, check it, put it in
    /// place of --to like `install` and watch the supervisor take it; on a failure put the old
    /// binary back, start the supervisor again when it is gone, and open the `update_failed` ask.
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
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
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
        /// Only bring the installed claude-dagq plugin to RELEASE, the binary being it already
        /// (ADR-t618-2 decision 4).
        #[arg(long)]
        plugin_only: bool,
    },
    /// Run the observer job once: headless Claude under DAGQ_ROLE=observer reads stats past the
    /// cursor, the open findings, the latest notes, the open asks and the graph, and writes
    /// findings and blocked asks on them only (ADR-0044 decision 4). Records observe_started / observe_finished and saves the new cursor.
    /// When no event but the observer's own came since the last observation, starts no agent and records
    /// observe_finished with outcome skipped (unless --since is given). The agent loads no MCP server.
    Observe {
        /// List the past observations instead, newest first: the events each read, the findings and asks it
        /// wrote, how long it took and whether it was skipped.
        #[arg(long)]
        history: bool,
        /// With --history, how many observations to list.
        #[arg(long, requires = "history", default_value_t = dagq::observer::HISTORY_LIMIT)]
        limit: usize,
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
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
        /// cmux executable for workspace listing and inbox notifications; bare names resolve
        /// on PATH. If not found, neither listing nor notification is attempted.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
    },
    /// Run the throughput review once (ADR-t996-1): for the last whole hour (--mode hourly), yesterday
    /// (daily) or the ISO week before this one (weekly). An hour the runtime's rules find unremarkable starts
    /// no agent and records throughput_review_finished with outcome skipped. Otherwise a headless agent
    /// under DAGQ_ROLE=throughput-review-job, which may only read, follows the weekly review of the dagq
    /// skill's reference/kpi.md: on Claude by default, or on Codex in its read-only sandbox when
    /// [roles.throughput_review] of dagq.toml says provider = "codex" (the supervisor passes the
    /// provider it routed the review to). The review is saved under <queue dir>/reports/reviews/ and its
    /// conclusion reaches the inbox as throughput_review_reported (next: report the review). A weekly
    /// next move becomes a finding marked for a proposal. A review that fails or cannot start reaches the
    /// inbox as its throughput_review_finished (next: check the failed review), except one whose Codex
    /// could not be used (provider_unusable), which the supervisor reviews again on the other provider.
    /// The prompt carries a summary of the inputs; the whole is input.json in the review's directory.
    /// The agent loads no MCP server; the supervisor starts this on its timer.
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
        /// name is resolved on PATH outside cmux's shims.
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        /// What the job starts with, as the supervisor routed it (an actor launch as JSON);
        /// `[roles.throughput_review]` of dagq.toml without.
        #[arg(long, hide = true)]
        launch: Option<String>,
        /// Codex's home, whose rollouts name the model (tests); Codex's own without.
        #[arg(long, hide = true)]
        codex_home: Option<PathBuf>,
        /// The role names its provider: a Codex job that cannot use Codex records
        /// provider_unusable for the supervisor to start it again on the other provider.
        #[arg(long, hide = true)]
        switchable: bool,
        /// No provider can run the job (--no-claude, Codex not usable): a period that needs a
        /// review records its failure with this reason.
        #[arg(long, hide = true)]
        unavailable: Option<String>,
    },
    /// Start the queue's runtime: a launchd-resident supervisor and the inbox's cmux workspace. Idempotent; a live supervisor of another build is handed over to this binary without waiting for its sessions (or drained when it cannot take a handoff), after the queue's compatible migrations. Opens no planner (`plan` does) and forgets the resident planner's record.
    Up {
        /// Never start Claude. Run workers on Codex and handle unsupported roles manually.
        #[arg(long)]
        no_claude: bool,
        /// Maximum number of runs the supervisor executes at once. Passed
        /// to the supervisor only when given; without it, `parallel` of
        /// `[supervisor]` in the main checkout's dagq.toml, else 4.
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        parallel: Option<u16>,
        /// Maximum number of runs the supervisor keeps waiting for a
        /// person's answer outside the --parallel slots (ADR-0062); 0 keeps
        /// every run in its slot. Passed only when given; without it,
        /// `max_waiting` of `[supervisor]` in dagq.toml, else 4.
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
        /// Run the supervisor in the cmux workspace `[<repo>]supervisor`
        /// instead of under launchd: no socket password needed, and nothing
        /// restarts it if it stops.
        #[arg(long)]
        in_cmux: bool,
        /// Do not wait for a supervisor that cannot take a handoff to drain:
        /// stop with an error instead when any run is still in flight.
        #[arg(long)]
        no_wait: bool,
        /// Seconds a supervisor asked to hand off may take to come back
        /// under this binary (it finishes a validation or landing in
        /// progress first).
        #[arg(long, default_value_t = 1800)]
        handoff_timeout: u64,
        /// Claude Code plugin directory the inbox session loads (`claude --plugin-dir`).
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
        /// Checkout of the repository; defaults to the working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// cmux executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
        /// Codex CLI the supervisor's Codex workers start (ADR-t813-2); a bare name is resolved on
        /// PATH and fixed on the supervisor like --claude. When it is not found, `up` goes on and
        /// the supervisor starts Codex tasks with non-interactive Claude
        /// (provider_switched, reason executable_missing).
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        /// Have the supervisor build and install the runtime of every landing on main that changes
        /// it, in the queue's own checkout, and hand itself over to it (ADR-0045 decision 17). Kept
        /// on the registration; an `up` without it turns it off.
        #[arg(long)]
        auto_update: bool,
    },
    /// Open a new planner session in a cmux workspace `[<repo>]planner#<id>`, next to any planner
    /// already open; every call opens another. Prints the planner, its workspace and directory.
    Plan {
        /// Claude Code plugin directory the planner session loads (`claude --plugin-dir`).
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
        /// Checkout the planner works in; defaults to the working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// cmux executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
        /// Claude Code executable; a bare name is resolved on PATH.
        #[arg(long, default_value = "claude")]
        claude: PathBuf,
    },
    /// List the planner sessions not closed, each with its state (opening, working, idle, exited,
    /// lost, closed), whether it is alive and since when it is idle. Reads only.
    Planners {
        /// Include closed planners.
        #[arg(long)]
        all: bool,
        /// cmux executable, used to look for each planner's workspace.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
    },
    /// A run's session, named by its run id or its task id (the task's
    /// latest run): read its screen, or send it a key of a fixed set or the
    /// answer of an answered ask; or close the workspaces ended runs left
    /// open (ADR-t1228-1). Each is recorded with its actor.
    Run {
        #[command(subcommand)]
        command: RunCommand,
    },
    /// A planner's session, named by its planner id: read its screen, or
    /// send it a key of a fixed set or the answer of an answered
    /// `planner_question` (ADR-t1228-1). Each is recorded with its actor.
    Planner {
        #[command(subcommand)]
        command: PlannerCommand,
    },
    /// Stop the queue's supervisor: unload its launchd agent so it drains and is not restarted, or signal and close the workspace of an in-cmux one. Leaves the inbox and planner workspaces open.
    Down {
        /// Wait until the supervisor's registration is gone or its process exited.
        #[arg(long)]
        wait: bool,
        /// Kill the supervisor after the unload and drop its registration.
        #[arg(long, conflicts_with = "wait")]
        force: bool,
        /// cmux executable, used to close an in-cmux supervisor's workspace.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
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
    /// List supervisors, unfinished runs, what waits for a person (attention), the open asks and the event cursor, without changing anything.
    Status {
        /// Only the attention addressed to this role: inbox gets all of it, planner none.
        #[arg(long, value_parser = ROLES)]
        role: Option<String>,
    },
    /// Register a question for a person about a task or one of its runs; prints the ask.
    /// An open ask of the same task, run and kind is returned instead (`created: false`).
    /// A new ask sends one `cmux notify` to the inbox workspace (`notified`, or `notify_error`).
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
        /// Why a person is needed (ADR-0047 decision 41): scope (the acceptance, the scope, an ADR or a goal's decision), discard (whether to throw work away) or recovery_failed.
        /// Authentication and cost asks are the runtime's, one per queue.
        /// A question that fits none of them is no ask: decide it yourself, or leave it as a note (`dagq note`).
        #[arg(long = "because", required = true, value_parser = ["scope", "discard", "recovery_failed", "authentication", "cost"])]
        because: Option<String>,
        /// What a worker_question left undecided (ADR-t947-2); repeat for several, the first the
        /// primary one (what stopped you first), the rest what must be decided with it. Required
        /// for a worker_question and refused on any other kind. One of: discard_work (throw away
        /// or redo the work), adr_conflict (the task contradicts an accepted ADR, a design or a
        /// person's decision), acceptance_conflict (two criteria cannot both hold),
        /// acceptance_infeasible (a fact keeps a criterion from being met), out_of_scope_change
        /// (a change outside the task's paths or description is needed), task_overlap (another
        /// task or a landed change overlaps), precondition_missing (what the work starts from is
        /// not there yet), host_environment (the host's tools or settings block it),
        /// design_choice (an implementation choice, yours to decide) or other; the heavier first
        /// when two came at once. A code outside the list is kept as given.
        #[arg(long = "topic")]
        topics: Vec<String>,
        /// The option you recommend (ADR-t451-1 decision 1): one of the ask's --option texts (or
        /// `propose` / `dismiss`, which the runtime adds to a blocked ask about a finding).
        /// Required on a blocked ask (ADR-t451-1 decision 2), optional on every other kind; the
        /// inbox shows it to the person next to the options.
        #[arg(long = "recommend")]
        recommend: Option<String>,
        /// How sure you are of the judgement behind the ask: high or low (ADR-t451-1 decision 1).
        #[arg(long, value_parser = ["high", "low"])]
        confidence: Option<String>,
        /// Task the ask is about. Only a blocked ask, or a planner_question about a finding, may
        /// name neither a task nor a run.
        #[arg(long = "task", conflicts_with = "run")]
        task_id: Option<i64>,
        /// Run the ask is about (its task is implied).
        #[arg(long)]
        run: Option<String>,
        /// Finding a blocked ask raises; one ask per finding stays open (ADR-0044 decision 23).
        /// A blocked ask about a finding also offers `propose` and `dismiss`, which the runtime
        /// applies to the finding. A planner_question names the finding its planner was opened
        /// for (decision 19).
        #[arg(long)]
        finding: Option<i64>,
        /// Planning request a planner_question is about: the one its planner was opened for
        /// (ADR-t1394-1 decision 7).
        #[arg(long, conflicts_with_all = ["task_id", "run", "finding"])]
        request: Option<i64>,
        /// cmux executable, used to notify the inbox; a bare name is resolved on PATH.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
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
        /// Every event kind, not only attention.
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
    /// with the reason the events show (idle, waiting_ask, background, after_receipt,
    /// waiting_integration, integrating, no_supervisor, unknown). Reads only.
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
        /// cmux executable, used to list the workspaces for `workspace_mismatch`; a bare name is
        /// resolved on PATH.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
    },
    /// KPIs of the flow, rework, people's load, infrastructure, improvements and sessions per day
    /// or ISO week (ADR-0051), split by the task's change (ADR-t980-1; `unknown` without one),
    /// by the areas of
    /// what the run landed when dagq.toml has `[areas]` (ADR-t980-1), and --by the claim's
    /// attributes, each next to the previous period (and a day's 7-day median), judged
    /// against the `[kpi.targets]` of dagq.toml and host.toml. --compare splits at a mark (its
    /// event id) or a time, or compares two windows A..B,C..D, and lists every other mark in and
    /// between them. Reads only; JSON.
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
    /// When the open tasks (ready and in progress, outside a draft goal) and the open goals are
    /// likely to finish if the plan flows as it is now (ADR-0070): the p50 and p90 of a seeded
    /// simulation over the dependencies, the claim order, the slots and each change's landed runs
    /// (the whole distribution for a change with fewer than `[kpi]` `min_samples`), with what it
    /// assumed. New tasks are not added. Reads only and records nothing; JSON.
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
    /// Write the KPI report of a day or ISO week (ADR-0051 decision 21) as the supervisor writes it
    /// daily: `dagq kpi --period P --at <the period>` with the build, the time and the top open
    /// findings, as JSON and as one self-contained HTML page (no script, CSS, font or image from
    /// anywhere else), under the queue's reports/ (daily/YYYY-MM-DD, weekly/YYYY-Www; today's and
    /// this week's are named .partial), then index.html and the [report] retention of host.toml.
    /// Prints the paths written. Changes no queue state.
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
    Doctor {
        /// Include each run's lease, processes, heartbeats and paths, and every supervisor field.
        #[arg(long)]
        full: bool,
    },
    /// The queue's resource broker in dagq's own Podman machine: status, start, stop.
    Broker {
        #[command(subcommand)]
        command: BrokerCommand,
    },
    /// The queue service: the host process that opens the queue for the callers given its
    /// socket and a token, with use cases (ask, show, note) authorized on its side. `up` starts
    /// it before the supervisor, the supervisor starts it again, and `down` stops it.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Bind the queue to the repository containing the working directory (or
    /// --repo) after the repository moved; the one command that changes the
    /// binding. Refused while a supervisor runs. Pass --db for a queue still
    /// in its old directory; `move_to` names where the repository now looks for it.
    Rebind {
        /// Checkout of the repository to bind to; defaults to the working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Mark one unfinished run interrupted once its processes and supervisor are gone; keeps its worktree and workspace and leaves other runs alone.
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
        /// The Codex CLI a Codex worker's turns call (ADR-t813-3).
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        /// Reopen the session of a `needs_session` run the supervisor resumes.
        #[arg(long)]
        resume: bool,
        /// The cmux a wrapper refused its session closes its own workspace
        /// with (task 806).
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
        /// Started by the supervisor in the background, without a
        /// workspace or a terminal (ADR-t1404-1).
        #[arg(long)]
        background: bool,
    },
    /// Record a SessionStart (`open`) or SessionEnd (`close`) of an inbox or planner session:
    /// the plugin's hook passes its stdin, and the session's environment names its kind
    /// (ADR-0048 decision 6). Only the session spans are written.
    #[command(hide = true)]
    SessionEvent {
        #[arg(value_parser = ["open", "close"])]
        event: String,
        /// The run whose session reports, when it is no inbox or planner
        /// session: what the caller is authorized on (task 734). Without
        /// it, the caller's `DAGQ_RUN_ID`. A run's session records no span.
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
        /// The model its agent starts with (ADR-0079 decision 7), given
        /// with `--effort`.
        #[arg(long, requires = "effort")]
        model: Option<String>,
        #[arg(long, requires = "model")]
        effort: Option<String>,

        /// The cmux a wrapper refused its session closes its own workspace
        /// with (task 806).
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
        /// The planner's agent runs one call per turn (ADR-t1394-2): the
        /// wrapper needs no terminal for it.
        #[arg(long)]
        headless: bool,
        /// The supervisor started this wrapper in the background, without
        /// a workspace (ADR-t1404-1 decision 8).
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
        /// Ask for a proposal to remedy it, with the reason (ADR-0044 decision 19).
        #[arg(long)]
        propose: Option<String>,
    },
    /// Mark a finding resolved: the problem no longer occurs.
    Resolve {
        id: i64,
        #[arg(long)]
        reason: String,
    },
    /// Mark a finding dismissed: nobody will remedy it. It still counts occurrences.
    Dismiss {
        id: i64,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Subcommand, Clone)]
enum RequestCommand {
    /// Record a person's words as a planning request (only the inbox, at a person's word, and a
    /// person at a plain terminal): the supervisor opens a planner of the runtime's for it, which
    /// submits a proposal (the request becomes proposed) or declines it. Prints the request.
    #[command(group = clap::ArgGroup::new("words").required(true))]
    Add {
        /// The person's own words, not a summary.
        #[arg(long, group = "words")]
        text: Option<String>,
        /// A file holding the person's own words.
        #[arg(long, group = "words")]
        file: Option<PathBuf>,
        /// What the inbox adds, kept apart from the person's words.
        #[arg(long)]
        note: Option<String>,
        /// What it refers to: ask:N, task:N, run:ID, event:N, finding:N or goal:N; repeatable.
        #[arg(long = "ref")]
        refs: Vec<String>,
    },
    /// Decline an open request with the reason (only the planner opened for it): nothing will be
    /// planned of it. The inbox is told.
    Decline {
        id: i64,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Subcommand, Clone)]
enum ServiceCommand {
    /// Report whether the queue service runs and answers, its pid, build, API version and
    /// socket, and its attention, without changing anything.
    Status,
    /// Start the queue service of this binary unless one of its build answers, replacing one
    /// of another build; `up` does this before the supervisor.
    Start {
        /// cmux the service notifies the inbox of a new ask through.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
    },
    /// Stop the queue service; `down` does this after the supervisor.
    Stop,
    /// Run the queue service in the foreground until SIGINT or SIGTERM (what `start`, `up`
    /// and the supervisor run in the background). A second one for the same queue is refused.
    Serve {
        /// cmux the service notifies the inbox of a new ask through.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
    },
}

#[derive(Subcommand, Clone)]
enum BrokerCommand {
    /// Report dagq's Podman machine, the broker's image, the queue's container and its health on
    /// 127.0.0.1, without changing anything.
    Status {
        /// podman executable; defaults to podman on PATH.
        #[arg(long)]
        podman: Option<PathBuf>,
    },
    /// Make the queue's broker run: init and start dagq's own Podman machine with the fewest
    /// resources when needed (never another machine), build the image from the material this
    /// binary embeds when missing (never a checkout's files), run the
    /// container publishing on 127.0.0.1 only, and wait for its health. Idempotent.
    Start {
        /// Port on 127.0.0.1; defaults to the queue's last one, else a free one.
        #[arg(long)]
        port: Option<u16>,
        /// podman executable; defaults to podman on PATH.
        #[arg(long)]
        podman: Option<PathBuf>,
    },
    /// Stop the queue's broker container, then dagq's machine when no container runs on it.
    Stop {
        /// podman executable; defaults to podman on PATH.
        #[arg(long)]
        podman: Option<PathBuf>,
    },
    /// Print the tail of the queue's broker container's podman logs as {"container", "tail",
    /// "stdout", "stderr"}. Reads only: starts no machine and makes no container, and fails
    /// with podman_missing, machine_missing, machine_stopped or container_missing instead.
    Logs {
        /// Lines from the end.
        #[arg(long, default_value_t = dagq::application::broker_admin::DEFAULT_LOG_TAIL)]
        tail: u32,
        /// podman executable; defaults to podman on PATH.
        #[arg(long)]
        podman: Option<PathBuf>,
    },
    /// Print the broker's audit lines (<queue dir>/broker/audit/<YYYY-MM-DD>.jsonl, UTC) oldest
    /// first as {"entries", "skipped", "dropped"}: each entry is the line as the broker wrote
    /// it, skipped counts broken or cut lines, dropped the older matches past --limit. Reads the
    /// files only; the queue DB is not touched.
    Audit {
        /// Only this run's lines.
        #[arg(long)]
        run: Option<String>,
        /// Only this task's lines.
        #[arg(long)]
        task: Option<u64>,
        /// Only lines at or after this UTC time (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS[.fff]Z).
        #[arg(long)]
        since: Option<String>,
        /// Only lines before this UTC time.
        #[arg(long)]
        until: Option<String>,
        /// The latest lines kept.
        #[arg(long, default_value_t = dagq::application::broker_admin::DEFAULT_AUDIT_LIMIT)]
        limit: usize,
    },
}

#[derive(Subcommand, Clone)]
enum RunCommand {
    /// Print the last lines of the screen of the run's session (at most
    /// 200), recorded as `screen_read` without its text. A headless run has
    /// no screen: the reply names its turns' directory instead.
    Screen {
        /// Run ID, or a task ID for the task's latest run.
        run: String,
        /// How many lines, from the bottom; more than 200 is cut to 200.
        #[arg(long, default_value_t = dagq::application::screen::DEFAULT_LINES)]
        lines: usize,
        /// cmux executable.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
    },
    /// Type into the run's session one or more keys of the set (enter,
    /// escape, up, down, 1-9, or exit alone for `/exit`), or the answer of
    /// an answered ask on that run as `answer to ask <id>: <answer>`; no
    /// other text. Recorded as `screen_input_sent`. A headless run is
    /// refused.
    Send {
        /// Run ID, or a task ID for the task's latest run.
        run: String,
        /// A key to send; repeat for several, sent in order.
        #[arg(long = "key", conflicts_with = "answer")]
        keys: Vec<String>,
        /// The answered ask whose answer is typed.
        #[arg(long)]
        answer: Option<i64>,
        /// cmux executable.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
    },
    /// List, or with --apply close, the workspaces ended runs (integrated, succeeded, failed,
    /// interrupted) left open that cmux still lists, as the supervisor's sweep would; it needs
    /// no supervisor. Without RUN or --task: every run the sweep takes. RUN or --task also takes
    /// the run a triage left (triage by hand), once no live lease, wrapper or agent is behind it.
    /// Never closes a live or waiting run's workspace, nor the inbox's, the supervisor's or a
    /// planner's. Lists only (a dry run) unless --apply.
    CloseWorkspaces {
        /// Run ID: only this run's workspaces; refused unless it ended.
        #[arg(conflicts_with = "task")]
        run: Option<String>,
        /// Task ID: the workspaces of every ended run of this task.
        #[arg(long)]
        task: Option<i64>,
        /// Close them; without it, only list what would be closed.
        #[arg(long)]
        apply: bool,
        /// cmux executable that lists and closes the workspaces.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
    },
}

#[derive(Subcommand, Clone)]
enum PlannerCommand {
    /// Print the last lines of the screen of the planner's session (at
    /// most 200), recorded as `screen_read` without its text.
    Screen {
        planner: i64,
        /// How many lines, from the bottom; more than 200 is cut to 200.
        #[arg(long, default_value_t = dagq::application::screen::DEFAULT_LINES)]
        lines: usize,
        /// cmux executable.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
    },
    /// Type into the planner's session one or more keys of the set (enter,
    /// escape, up, down, 1-9, or exit alone for `/exit`), or the answer of
    /// an answered `planner_question` that goes to this planner; no other
    /// text. Recorded as `screen_input_sent`.
    Send {
        planner: i64,
        /// A key to send; repeat for several, sent in order.
        #[arg(long = "key", conflicts_with = "answer")]
        keys: Vec<String>,
        /// The answered ask whose answer is typed.
        #[arg(long)]
        answer: Option<i64>,
        /// cmux executable.
        #[arg(long, default_value = "cmux")]
        cmux: PathBuf,
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
    },
    /// Open a draft goal so the supervisor may claim its ready tasks.
    Ready { id: i64 },
    /// List goals with their status and task counts by status.
    List,
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
        Command::Locate
        | Command::List { .. }
        | Command::Show { .. }
        | Command::Candidates
        | Command::Graph { out: None, .. }
        | Command::Status { .. }
        | Command::Asks { .. }
        | Command::Events { .. }
        | Command::Timeline { .. }
        | Command::Stats { .. }
        | Command::Kpi { .. }
        | Command::Forecast { .. }
        | Command::Doctor { .. }
        | Command::Broker {
            command:
                BrokerCommand::Status { .. } | BrokerCommand::Logs { .. } | BrokerCommand::Audit { .. },
        }
        | Command::Service {
            command: ServiceCommand::Status,
        }
        | Command::Notes { .. }
        | Command::Marks { .. }
        | Command::Findings { .. }
        | Command::Requests { .. }
        | Command::Search { .. }
        | Command::Related { .. }
        | Command::Proposal {
            command: ProposalCommand::List { .. } | ProposalCommand::Show { .. },
        }
        | Command::Planners { .. }
        | Command::Lint { .. }
        | Command::Goal {
            command: GoalCommand::List | GoalCommand::Show { .. },
        }
        | Command::Observe { history: true, .. } => queue(C::QueueRead),
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
        | Command::Broker {
            command: BrokerCommand::Start { .. } | BrokerCommand::Stop { .. },
        }
        | Command::Service {
            command:
                ServiceCommand::Start { .. } | ServiceCommand::Stop | ServiceCommand::Serve { .. },
        }
        | Command::Plan { .. }
        | Command::Supervise { .. }
        | Command::Observe { history: false, .. }
        | Command::ThroughputReview { .. }
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
        Command::Ready { id, bypass_review } => one(
            if *bypass_review {
                C::TaskReadyBypassReview
            } else {
                C::TaskReady
            },
            task_resource(*id),
        ),
        Command::Draft { id }
        | Command::Edit { task: id, .. }
        | Command::SetGoal { task: id, .. }
        | Command::SetPaths { task: id, .. }
        | Command::SetPriority { task: id, .. }
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
            GoalCommand::List | GoalCommand::Show { .. } => queue(C::QueueRead),
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
            ..
        } => one(
            C::AskOpen,
            match (
                kind.as_deref().unwrap_or_default().parse::<AskKind>(),
                run.as_deref().map(RunId::new).transpose(),
            ) {
                (Ok(kind), Ok(run)) => Resource::NewAsk {
                    kind,
                    run,
                    task: task_id.map(TaskId::new),
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
        Command::Broker {
            command: BrokerCommand::Start { .. } | BrokerCommand::Stop { .. },
        } => Operation::Broker,
        Command::Service {
            command:
                ServiceCommand::Start { .. } | ServiceCommand::Stop | ServiceCommand::Serve { .. },
        } => Operation::QueueService,
        Command::Plan { .. } => Operation::Plan,
        Command::Supervise { .. } => Operation::Supervise,
        // Starting a job of the supervisor's timer, as `observe` (ADR-t996-1).
        Command::Observe { history: false, .. } | Command::ThroughputReview { .. } => {
            Operation::Observe
        }
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
                command
                @ (RunCommand::Screen { run: target, .. } | RunCommand::Send { run: target, .. }),
        } => {
            let resource = dagq::application::screen::RunTarget::parse(target)
                .map_or(Resource::Unresolved, |target| target.resource());
            match command {
                RunCommand::Screen { .. } => Operation::ReadScreen(resource),
                _ => Operation::SendToScreen(resource),
            }
        }
        Command::Planner { command } => match command {
            PlannerCommand::Screen { planner, .. } => {
                Operation::ReadScreen(Resource::Planner(PlannerId::new(*planner)))
            }
            PlannerCommand::Send { planner, .. } => {
                Operation::SendToScreen(Resource::Planner(PlannerId::new(*planner)))
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
        Command::Add { .. }
        | Command::Edit { .. }
        | Command::Submit { .. }
        | Command::Draft { .. }
        | Command::Ready { .. }
        | Command::Cancel { .. }
        | Command::Dependency { .. }
        | Command::SetGoal { .. }
        | Command::SetPaths { .. }
        | Command::SetPriority { .. }
        | Command::Proposal {
            command: ProposalCommand::Withdraw { .. },
        } => true,
        Command::Goal { command } => {
            !matches!(command, GoalCommand::List | GoalCommand::Show { .. })
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
/// queue `db` names, opened only then (task 1151); a command allowed opens
/// nothing here, and reads keep the queue read-only (ADR-0073 decisions 5,
/// 7 and 18).
fn check_access(actor: &ActorContext, command: &Command, db: Option<&Path>) -> Result<()> {
    if authorized_in_application(command) {
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
/// observer and the headless jobs always got.
fn error_json(error: &anyhow::Error) -> Value {
    // A broker step's failure carries its code (ADR-t827-3).
    if let Some(failure) = error.downcast_ref::<dagq::application::broker::BrokerFailure>() {
        return json!({"error": format!("{error:#}"), "broker": failure.to_json()});
    }
    // A client-mode dagq's failure carries the service's code or its own
    // (goal 82's stage (3)).
    if let Some(failure) =
        error.downcast_ref::<dagq::infrastructure::queue_service::ClientFailure>()
    {
        return json!({"error": failure.message, "queue_service": {"code": failure.code}});
    }
    let Some(denied) = error.downcast_ref::<AuthorizationError>() else {
        return json!({"error": format!("{error:#}")});
    };
    let message = if denied.role == ActorRole::Observer {
        OBSERVER_DENIED.to_owned()
    } else if denied.role.is_headless_job() {
        REVIEWER_DENIED.to_owned()
    } else {
        denied.to_string()
    };
    json!({
        "error": message,
        "denied": {
            "role": denied.role,
            "capability": denied.capability,
            "reason": denied.reason.as_str(),
        },
    })
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
        Command::Candidates => QueueRead::Candidates,
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
            cmux: _,
        } => QueueRead::Stats(reads::StatsRead {
            since,
            until,
            goal: goal_id,
            full,
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
            command: GoalCommand::List,
        } => QueueRead::GoalList,
        Command::Goal {
            command: GoalCommand::Show { id, full },
        } => QueueRead::GoalShow(reads::GoalShowRead { id, full }),
        Command::Lint { tasks, proposals } => QueueRead::Lint(reads::LintRead { tasks, proposals }),
        Command::Observe {
            history: true,
            limit,
            ..
        } => QueueRead::ObserveHistory(reads::ObserveHistoryRead { limit }),
        _ => return None,
    })
}

/// The use case and params of the queue service a client-mode `dagq`
/// sends `command` as (goal 82's stage (3), ADR-t1233-1 decision 7): the
/// same options the service reads back as the command line's. `None` for
/// a command the service has no use case for. A `--cmux` is the service's
/// own: an ask notifies the inbox through it, and `stats` lists the
/// workspaces with it.
fn client_request(command: &Command) -> Result<Option<(UseCase, Value)>> {
    if let Some(read) = queue_read(command) {
        return Ok(Some(read.request()));
    }
    Ok(Some(match command {
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
            // A request's planner runs on the host, never as a client.
            request: None,
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
            (UseCase::Ask, params)
        }
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
            command: FindingCommand::Dismiss { id, reason },
        } => (UseCase::FindingDismiss, json!({"id": id, "reason": reason})),
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
                "--db {} names a queue to open, and a client-mode dagq (DAGQ_SERVICE_SOCKET is \
                 set) opens none: run the command without it",
                db.display()
            ),
        )
        .into());
    }
    // `locate` says where the commands go, without the queue's path: the
    // plugin's `--resolve` runs it first.
    if matches!(cli.command, Command::Locate) {
        return Ok(json!({
            "client_mode": true,
            "socket": client.socket,
            "db": null,
            "db_exists": null,
            "note": "dagq runs in client mode: its commands go to the queue service at the \
                     socket, and it opens no queue",
        }));
    }
    let Some((use_case, params)) = client_request(&cli.command)? else {
        return Err(ClientFailure::new(
            "no_use_case",
            "this command is not one of the queue service's use cases, and a client-mode dagq \
             (DAGQ_SERVICE_SOCKET is set) opens no queue to run it (docs/design/queue-service.md \
             lists the use cases)",
        )
        .into());
    };
    client.call(use_case, params)
}

fn execute(cli: Cli) -> Result<Value> {
    // A worker's or a job's `dagq` goes to the queue service, which
    // authorizes it for the principal of its token: neither `DAGQ_ROLE`
    // nor the queue's path is read (goal 82's stage (3)).
    if let Some(client) =
        dagq::infrastructure::queue_service::Client::from_env(|name| env::var(name).ok())
    {
        return client_mode(&client, &cli);
    }
    // Who runs the command (ADR-t728-1 decision 4): no `DAGQ_ROLE` is the
    // user, and a value that is no role stops it before anything is read.
    let actor = ActorContext::from_env(|name| env::var(name).ok())?;
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
        let mut queue = SqliteQueue::init(&db)?.with_generators(generators);
        if let Some(common_dir) = &common_dir {
            queue.bind_repository(common_dir)?;
        }
        return Ok(json!({
            "db": db,
            "schema_version": queue.schema_version()?,
            "source": location.source,
            "git_common_dir": common_dir,
        }));
    }
    if let Command::Migrate { check } = cli.command {
        if check {
            return Ok(serde_json::to_value(SqliteQueue::schema(&db)?)?);
        }
        let report = SqliteQueue::migrate(
            &db,
            Some(&dagq::infrastructure::adapters::process_alive),
            generators.clock.now(),
        )?;
        let mut value = serde_json::to_value(report)?;
        // A landing recorded without its message (ADR-0046 decision 3).
        if let Ok(mut queue) = SqliteQueue::open(&db) {
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
        cmux,
        claude,
        codex,
        plugin_dir,
        skip_e2e,
        e2e_command,
        e2e_timeout,
        poll_ms,
    } = cli.command
    {
        use dagq::application::install::{E2eGate, E2eSettings, InstallOptions, Source};
        use dagq::infrastructure::{
            adapters::{Cmux, executable},
            launchd::Launchctl,
        };
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
        let cmux = executable(&cmux).unwrap_or(cmux);
        let mut restart = vec!["--cmux".to_owned(), path_text(&cmux)?];
        if let Some(claude) = claude {
            restart.extend(["--claude".to_owned(), path_text(&executable(&claude)?)?]);
        }
        if let Some(codex) = codex {
            let codex = dagq::infrastructure::codex::executable(&codex).unwrap_or(codex);
            restart.extend(["--codex".to_owned(), path_text(&codex)?]);
        }
        if let Some(plugin_dir) = plugin_dir {
            restart.extend(["--plugin-dir".to_owned(), path_text(&plugin_dir)?]);
        }
        // A checkout of dagq's source passes its e2e before it is put in
        // place, unless a person skips it (ADR-t963-1 decision 1).
        let e2e = match &source {
            Source::Checkout(checkout)
                if !skip_e2e && dagq::infrastructure::adapters::is_dagq_source(checkout) =>
            {
                // A command in place of the e2e (tests) has no broker's
                // e2e: the host's podman is not checked for it
                // (ADR-t1162-1).
                let podman = match &e2e_command {
                    Some(_) => None,
                    None => Some(dagq::application::install::PodmanCheck {
                        executable: None,
                        lock_home: dagq::infrastructure::broker_podman::machine_lock_home()?,
                        reconnect: dagq::application::broker::RECONNECT,
                    }),
                };
                E2eGate::Run(E2eSettings {
                    command: e2e_command,
                    timeout: Duration::from_secs(e2e_timeout),
                    cmux: Some(cmux.clone()),
                    run_env_root: Some(checkout.clone()),
                    queue_dir: Some(location.queue_dir.clone()),
                    scratch: dagq::application::update::UpdatePaths::under(&location.queue_dir).e2e,
                    log: location
                        .log_dir
                        .join(format!("install-{}.e2e.log", generators.clock.now())),
                    podman,
                    utc_offset_secs: dagq::infrastructure::clock::local_utc_offset(
                        generators.clock.now(),
                    ),
                    lock: dagq::application::install::e2e_lock_path(&location.queue_dir),
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
                &Cmux { executable: cmux },
                &Launchctl { uid: current_uid() },
                &dagq::infrastructure::release_update::CurlIndex::default(),
                &executable(&cargo).unwrap_or(cargo),
                Some(release.as_str()).filter(|version| !version.is_empty()),
                &options,
            );
        }
        return one_shot.install(
            &location,
            &Cmux { executable: cmux },
            &Launchctl { uid: current_uid() },
            &options,
        );
    }
    // A repository queue already resolved the working directory; `--repo`
    // overrides it for a `--db` queue used from elsewhere or a moved checkout.
    let checkout = |repo: Option<PathBuf>| repo.unwrap_or_else(|| cwd.clone());
    // `rebind` is the one command that runs on a queue bound elsewhere.
    if let Command::Rebind { repo } = cli.command {
        return one_shot.rebind(&db, &checkout(repo));
    }
    // `doctor` reports the schema even of a queue this binary cannot open
    // (ADR-0045 decision 5), so it opens the queue itself.
    if let Command::Doctor { full } = cli.command {
        return one_shot.doctor(&db, full, common_dir.as_deref());
    }
    // The broker's container needs no queue state, only its paths.
    if let Command::Service { command } = cli.command {
        use dagq::infrastructure::adapters::executable;
        return match command {
            ServiceCommand::Status => Ok(dagq::compose::queue_service_status(&db)),
            ServiceCommand::Start { cmux } => dagq::compose::queue_service_start(
                &db,
                &env::current_exe()?,
                &executable(&cmux).unwrap_or(cmux),
            ),
            ServiceCommand::Stop => dagq::compose::queue_service_stop(&db),
            ServiceCommand::Serve { cmux } => {
                dagq::infrastructure::queue_service::serve(
                    &dagq::infrastructure::queue_service::ServeOptions {
                        db: db.clone(),
                        // A missing cmux fails only an ask's notification.
                        cmux: executable(&cmux).unwrap_or(cmux),
                        generators: generators.clone(),
                        stop: install_stop_signal()?,
                        poll: Duration::from_millis(50),
                    },
                )
            }
        };
    }
    if let Command::Broker { command } = cli.command {
        return match command {
            BrokerCommand::Status { podman } => {
                dagq::compose::broker_status(&location, podman.as_deref())
            }
            BrokerCommand::Start { port, podman } => dagq::compose::broker_start(
                &location,
                &dagq::compose::BrokerStartOptions {
                    port,
                    podman,
                    cwd: cwd.clone(),
                },
            ),
            BrokerCommand::Stop { podman } => {
                dagq::compose::broker_stop(&location, podman.as_deref())
            }
            BrokerCommand::Logs { tail, podman } => {
                dagq::compose::broker_logs(&location, podman.as_deref(), tail)
            }
            BrokerCommand::Audit {
                run,
                task,
                since,
                until,
                limit,
            } => {
                let millis = |text: Option<String>| -> Result<Option<i64>> {
                    text.map(|text| {
                        let time = dagq::watch::event_time(&text)?;
                        dagq::domain::stats::rfc3339_millis(&time)
                            .with_context(|| format!("not a UTC time: {text}"))
                    })
                    .transpose()
                };
                dagq::compose::broker_audit(
                    &location,
                    &dagq::application::broker_admin::AuditQuery {
                        run,
                        task,
                        since: millis(since)?,
                        until: millis(until)?,
                        limit: Some(limit),
                    },
                )
            }
        };
    }
    // `report` and `graph --out` write files but no queue state.
    let mut queue = if reads_only(&cli.command)
        || matches!(cli.command, Command::Report { .. } | Command::Graph { .. })
    {
        SqliteQueue::open_read_only(&db)?
    } else {
        SqliteQueue::open(&db)?
    }
    .with_generators(generators.clone());
    if let Some(common_dir) = &common_dir {
        queue.assert_repository(common_dir)?;
    }
    // The repository's set of changes holds the tasks the planning
    // commands register, edit and submit (ADR-t980-1); lint reads it
    // itself (`compose::read_queue`).
    if matches!(
        cli.command,
        Command::Add { .. } | Command::Edit { .. } | Command::Submit { .. }
    ) {
        let changes = dagq::compose::task_changes(&queue)?;
        queue = queue.with_changes(changes);
    }
    // The planning commands run as the caller through the application,
    // which authorizes each before it changes the queue (task 732).
    macro_rules! planning {
        () => {
            dagq::application::commands::planning::Planning::new(&mut queue, &actor, &StaticPolicy)
        };
    }
    // The asks, answers, notes, marks and findings, likewise (task 733).
    // Only `ask` notifies; the others never reach the backend.
    let no_cmux = dagq::infrastructure::adapters::Cmux {
        executable: PathBuf::from("cmux"),
    };
    let mut dialogue_store;
    macro_rules! dialogue {
        ($cmux:expr) => {{
            dialogue_store = dagq::infrastructure::dialogue::DialogueQueue {
                queue: &mut queue,
                checkout: &cwd,
                cmux: $cmux,
            };
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
        use dagq::infrastructure::adapters::{Cmux, executable};
        // `stats` lists the workspaces with its cmux; a missing one leaves
        // only `workspace_mismatch` unjudged.
        let cmux = match &cli.command {
            Command::Stats { cmux, .. } => {
                executable(cmux).ok().map(|executable| Cmux { executable })
            }
            _ => None,
        };
        return dagq::compose::read_queue(&mut queue, &db, &one_shot, cmux.as_ref(), &read);
    }
    Ok(match cli.command {
        Command::Init
        | Command::Locate
        | Command::Rebind { .. }
        | Command::Migrate { .. }
        | Command::Install { .. }
        | Command::Doctor { .. }
        | Command::Broker { .. }
        | Command::Service { .. } => {
            unreachable!()
        }
        // Answered above as reads.
        Command::List { .. }
        | Command::Candidates
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
        | Command::Observe { history: true, .. } => unreachable!("a read is answered above"),
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
        } => serde_json::to_value(
            planning!().add(NewTask {
                title,
                description,
                acceptance,
                verification_commands,
                dependencies: dependencies.into_iter().map(TaskId::new).collect(),
                goal_dependencies: goal_dependencies.into_iter().map(GoalId::new).collect(),
                goal_id: goal_id.map(GoalId::new),
                context,
                required_evidence: required_evidence
                    .iter()
                    .map(|name| name.parse())
                    .collect::<Result<_, _>>()?,
                paths,
                priority: priority.parse()?,
                change: change.map(|change| change.parse()).transpose()?,
                provider: provider.map(|provider| provider.parse()).transpose()?,
                worker_mode: match (headless, interactive) {
                    (true, _) => Some(WorkerMode::Headless),
                    (_, true) => Some(WorkerMode::Interactive),
                    _ => None,
                },
            })?,
        )?,
        Command::Show { id, full, events } => {
            let detail = queue.show(TaskId::new(id))?;
            if full {
                serde_json::to_value(detail)?
            } else {
                dagq::view::task_detail(&detail, events)
            }
        }
        Command::Ready { id, bypass_review } => {
            serde_json::to_value(planning!().ready(TaskId::new(id), bypass_review)?)?
        }
        Command::Submit {
            tasks,
            goals,
            proposal,
            findings,
        } => {
            use dagq::application::lifecycle::{
                CMUX_WORKSPACE_ENV, PLANNER_ID_ENV, PLANNER_ORIGIN_ENV,
            };
            let origin = match env::var(PLANNER_ORIGIN_ENV) {
                Ok(origin) if !origin.is_empty() => origin.parse()?,
                _ => PlannerOrigin::Person,
            };
            // A planner of the runtime's whose wrapper runs in the
            // background has no workspace of cmux's: its handle, which its
            // record keeps in place of one (ADR-t1404-1 decision 10),
            // makes it the proposal's owner.
            let workspace_id = env::var(CMUX_WORKSPACE_ENV)
                .ok()
                .filter(|id| !id.trim().is_empty())
                .or_else(|| {
                    let planner = env::var(PLANNER_ID_ENV).ok()?.trim().parse().ok()?;
                    queue
                        .planner(PlannerId::new(planner))
                        .ok()?
                        .workspace_id
                        .filter(|id| dagq::domain::background_wrapper::is_background(id))
                });
            serde_json::to_value(planning!().submit(
                Submission {
                    tasks: tasks.into_iter().map(TaskId::new).collect(),
                    goals: goals.into_iter().map(GoalId::new).collect(),
                    proposal: proposal.map(ProposalId::new),
                    owner: PlannerOwner {
                        origin,
                        workspace_id,
                    },
                },
                &findings.into_iter().map(FindingId::new).collect::<Vec<_>>(),
            )?)?
        }
        Command::Proposal { command } => match command {
            ProposalCommand::List { all } => json!({"proposals": queue.proposals(all)?}),
            ProposalCommand::Show { id } => {
                serde_json::to_value(queue.show_proposal(ProposalId::new(id))?)?
            }
            ProposalCommand::Withdraw { id } => {
                serde_json::to_value(planning!().withdraw(ProposalId::new(id))?)?
            }
        },
        Command::Draft { id } => serde_json::to_value(planning!().draft(TaskId::new(id))?)?,
        Command::Cancel { id, duplicate_of } => serde_json::to_value(
            planning!().cancel(TaskId::new(id), duplicate_of.map(TaskId::new))?,
        )?,
        Command::Dependency { command } => {
            use dagq::application::commands::planning::Dependency;
            // clap requires a predecessor or --goal; --goal wins.
            let on = |predecessor: Option<i64>, goal: Option<i64>| match (predecessor, goal) {
                (_, Some(goal)) => Dependency::Goal(GoalId::new(goal)),
                (Some(predecessor), None) => Dependency::Task(TaskId::new(predecessor)),
                (None, None) => unreachable!("clap requires a predecessor or --goal"),
            };
            serde_json::to_value(match command {
                DependencyCommand::Add {
                    task,
                    predecessor,
                    goal,
                } => planning!().add_dependency(TaskId::new(task), on(predecessor, goal))?,
                DependencyCommand::Remove {
                    task,
                    predecessor,
                    goal,
                } => planning!().remove_dependency(TaskId::new(task), on(predecessor, goal))?,
            })?
        }
        Command::Goal { command } => match command {
            GoalCommand::Add {
                title,
                description,
                acceptance,
                constraints,
                doc,
                draft,
            } => serde_json::to_value(planning!().add_goal(NewGoal {
                title,
                description,
                acceptance,
                constraints,
                doc,
                draft,
            })?)?,
            GoalCommand::Ready { id } => {
                serde_json::to_value(planning!().ready_goal(GoalId::new(id))?)?
            }
            GoalCommand::List | GoalCommand::Show { .. } => {
                unreachable!("a read is answered above")
            }
            GoalCommand::Edit {
                id,
                title,
                description,
                acceptance,
                constraints,
                doc,
            } => serde_json::to_value(planning!().edit_goal(
                GoalId::new(id),
                GoalEdit {
                    title,
                    description,
                    acceptance,
                    constraints,
                    doc,
                },
            )?)?,
            GoalCommand::Close { id, verdict } => serde_json::to_value(
                planning!().close_goal(GoalId::new(id), verdict.parse::<GoalVerdict>()?)?,
            )?,
            GoalCommand::Review { id } => planning!().review_goal(GoalId::new(id))?,
        },
        Command::SetGoal {
            task,
            goal,
            none: _,
        } => serde_json::to_value(planning!().set_goal(TaskId::new(task), goal.map(GoalId::new))?)?,
        Command::SetPaths {
            task,
            paths,
            none: _,
        } => serde_json::to_value(planning!().set_paths(TaskId::new(task), paths)?)?,
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
        } => {
            // A list flag replaces the list; its --no- flag empties it.
            let replaced =
                |values: Vec<String>, none: bool| (none || !values.is_empty()).then_some(values);
            let required_evidence = replaced(required_evidence, no_evidence)
                .map(|names| names.iter().map(|name| name.parse()).collect())
                .transpose()?;
            serde_json::to_value(planning!().edit(
                TaskId::new(task),
                TaskEdit {
                    title,
                    description,
                    acceptance,
                    verification_commands: replaced(verification_commands, no_verify),
                    required_evidence,
                    paths: replaced(paths, no_paths),
                    context,
                    change: change.map(|change| change.parse()).transpose()?,
                    provider: provider.map(|provider| provider.parse()).transpose()?,
                    worker_mode: match (headless, interactive) {
                        (true, _) => Some(WorkerMode::Headless),
                        (_, true) => Some(WorkerMode::Interactive),
                        _ => None,
                    },
                },
            )?)?
        }
        Command::SetPriority { task, level } => {
            serde_json::to_value(planning!().set_priority(TaskId::new(task), level.parse()?)?)?
        }
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
            serde_json::to_value(dialogue!(&no_cmux).note(NewNote {
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
        } => dialogue!(&no_cmux).mark(match retract {
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
        } => serde_json::to_value(dialogue!(&no_cmux).record_finding(NewFinding {
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
        } => {
            serde_json::to_value(dialogue!(&no_cmux).resolve_finding(FindingId::new(id), &reason)?)?
        }
        Command::Finding {
            command: FindingCommand::Dismiss { id, reason },
        } => {
            serde_json::to_value(dialogue!(&no_cmux).dismiss_finding(FindingId::new(id), &reason)?)?
        }
        Command::Request {
            command:
                RequestCommand::Add {
                    text,
                    file,
                    note,
                    refs,
                },
        } => {
            let text = match (text, file) {
                (Some(text), _) => text,
                (None, Some(file)) => {
                    let file = cwd.join(file);
                    std::fs::read_to_string(&file)
                        .with_context(|| format!("read {}", file.display()))?
                }
                (None, None) => unreachable!("clap requires --text or --file"),
            };
            let request = dagq::domain::plan_request::NewPlanRequest {
                text,
                note,
                refs: refs
                    .iter()
                    .map(|value| dagq::domain::plan_request::RequestRef::parse(value))
                    .collect::<Result<_, _>>()?,
            };
            serde_json::to_value(
                dagq::application::commands::requests::Requests::new(
                    &mut queue,
                    &actor,
                    &StaticPolicy,
                )
                .record(&request)?,
            )?
        }
        Command::Request {
            command: RequestCommand::Decline { id, reason },
        } => serde_json::to_value(
            dagq::application::commands::requests::Requests::new(&mut queue, &actor, &StaticPolicy)
                .decline(RequestId::new(id), &reason)?,
        )?,
        Command::Requests { id: Some(id), .. } => {
            json!({"requests": [queue.plan_request(RequestId::new(id))?]})
        }
        Command::Requests { id: None, all } => json!({"requests": queue.plan_requests(all)?}),
        Command::Graph {
            goal_id,
            format,
            out: Some(out),
        } => {
            anyhow::ensure!(format != "json", "--out needs --format d2 or svg");
            let (text, tasks) =
                dagq::compose::graph_diagram(&queue, goal_id.map(GoalId::new), &format)?;
            let out = cwd.join(out);
            std::fs::write(&out, &text).with_context(|| format!("write {}", out.display()))?;
            json!({
                "format": format,
                "out": out,
                "tasks": tasks,
            })
        }
        Command::Ask {
            command: Some(AskCommand::Close { id }),
            ..
        } => serde_json::to_value(dialogue!(&no_cmux).close(AskId::new(id))?)?,
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
            cmux,
        } => {
            use dagq::infrastructure::adapters::{Cmux, executable};
            // A missing cmux fails only the notification, not the ask.
            let cmux = Cmux {
                executable: executable(&cmux).unwrap_or(cmux),
            };
            dialogue!(&cmux).ask(NewAsk {
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
        // The user's own answer or the inbox's delegated one, which the
        // application records from the actor (ADR-t728-3 decision 2).
        Command::Answer { id, text } => {
            serde_json::to_value(dialogue!(&no_cmux).answer(AskId::new(id), &text)?)?
        }
        Command::Watch {
            after,
            timeout,
            until_attention,
            interval,
            role: r,
        } => dagq::watch::watch(
            &db,
            &dagq::watch::WatchOptions {
                after: after.map(EventId::new),
                timeout: (!until_attention)
                    .then(|| Duration::from_secs(timeout.unwrap_or(DEFAULT_WATCH_TIMEOUT_SECS))),
                interval: Duration::from_secs(interval),
                role: parse_role(r)?,
            },
        )?,
        Command::Supervise {
            no_claude,
            repo,
            parallel,
            max_waiting,
            max_load,
            once,
            cmux,
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
            use dagq::infrastructure::adapters::{Cmux, executable};
            let cmux = executable(&cmux)?;
            let mut options = SuperviseOptions {
                no_claude,
                stop: install_stop_signal()?,
                // A one-shot pass observes only when asked to.
                observe_interval: Duration::from_secs(observe_interval.unwrap_or(if once {
                    0
                } else {
                    3600
                })),
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
                    cmux: Some(cmux.clone()),
                    cargo: update_cargo,
                },
                parallel: parallel.map(usize::from),
                max_waiting: max_waiting.map(usize::from),
                max_load: dagq::domain::claim_hold::resolve_max_load(
                    max_load,
                    dagq::infrastructure::clock::logical_cores(),
                ),
                user_config: dagq::infrastructure::language::user_config_file(),
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
                // A supervisor at work keeps the queue's service (ADR-t1233-4
                // decision 2); a one-shot pass does not.
                queue_service: (!once).then(|| dagq::compose::QueueServiceOptions {
                    executable: env::current_exe().unwrap_or_else(|_| PathBuf::from("dagq")),
                    cmux: cmux.clone(),
                    interval: dagq::application::supervise::QUEUE_SERVICE_INTERVAL,
                    start_timeout: dagq::application::supervise::QUEUE_SERVICE_START_TIMEOUT,
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
                &Cmux { executable: cmux },
                &if no_claude {
                    claude
                } else {
                    executable(&claude)?
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
            cmux,
            claude,
            codex,
            auto_update,
        } => {
            use dagq::application::lifecycle::{QUEUE_ENV, ROLE_ENV, UpEnvironment, UpOptions};
            use dagq::infrastructure::adapters::{SOCKET_PASSWORD_ENV, claude_global_config};
            use dagq::infrastructure::{
                adapters::{Cmux, SystemProcesses, executable},
                launchd::Launchctl,
            };
            let environment = UpEnvironment {
                role: env::var(ROLE_ENV).ok(),
                queue: env::var_os(QUEUE_ENV).map(PathBuf::from),
                path: env::var("PATH").context("PATH is unset")?,
                socket_password: env::var(SOCKET_PASSWORD_ENV)
                    .ok()
                    .filter(|password| !password.is_empty()),
                config_home: env::var(dagq::application::CONFIG_HOME_ENV)
                    .ok()
                    .filter(|home| !home.is_empty()),
                current_exe: env::current_exe()?,
                claude_config: claude_global_config(
                    env::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
                    env::var("HOME").ok().as_deref(),
                ),
                user_config: dagq::infrastructure::language::user_config_file(),
                restart: env::var_os(dagq::lifecycle::UP_RESTART_ENV).is_some(),
            };
            let options = UpOptions {
                no_claude,
                parallel,
                max_waiting,
                runtime_planners,
                max_load,
                in_cmux,
                no_wait,
                plugin_dir,
                cmux: executable(&cmux)?,
                claude: if no_claude {
                    claude
                } else {
                    executable(&claude)?
                },
                codex: dagq::infrastructure::codex::executable(&codex).unwrap_or(codex),
                startup_timeout: Duration::from_secs(30),
                handoff_timeout: Duration::from_secs(handoff_timeout),
                auto_update,
                queue_service: true,
                poll: Duration::from_millis(500),
            };
            one_shot.up(
                &location,
                &checkout(repo),
                &Cmux {
                    executable: options.cmux.clone(),
                },
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
            cmux,
            claude,
            codex,
            plugin_dir,
            handoff_timeout,
            watch_timeout,
            poll_ms,
        } => {
            use dagq::infrastructure::adapters::executable;
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
                    cmux: executable(&cmux).unwrap_or(cmux),
                    claude: executable(&claude).unwrap_or(claude),
                    codex: dagq::infrastructure::codex::executable(&codex).unwrap_or(codex),
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
            cmux,
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
                    cmux: executable(&cmux).unwrap_or(cmux),
                    claude: executable(&claude).unwrap_or(claude),
                    codex: dagq::infrastructure::codex::executable(&codex).unwrap_or(codex),
                    plugin_dir,
                    handoff_timeout: Duration::from_secs(handoff_timeout),
                    watch_timeout: Duration::from_secs(watch_timeout),
                    plugin_only,
                },
            )?
        }
        Command::Plan {
            plugin_dir,
            repo,
            cmux,
            claude,
        } => {
            use dagq::infrastructure::adapters::{Cmux, executable};
            one_shot.plan(
                &location,
                &checkout(repo),
                &Cmux {
                    executable: executable(&cmux)?,
                },
                &dagq::compose::PlanOptions {
                    claude: executable(&claude)?,
                    plugin_dir,
                    runner: env::current_exe()?,
                    user_config: dagq::infrastructure::language::user_config_file(),
                },
            )?
        }
        Command::Planners { all, cmux } => {
            use dagq::infrastructure::adapters::{Cmux, executable};
            one_shot.planners_of(
                &queue,
                &db,
                &Cmux {
                    executable: executable(&cmux)?,
                },
                all,
            )?
        }
        Command::Run { command } => {
            use dagq::application::screen::{self, RunTarget, ScreenPorts, Sending};
            use dagq::infrastructure::adapters::{ClaudeCode, Cmux, executable};
            match command {
                RunCommand::Screen { run, lines, cmux } => screen::run_screen(
                    &mut queue,
                    &Cmux {
                        executable: executable(&cmux)?,
                    },
                    &RunTarget::parse(&run)?,
                    lines,
                )?,
                RunCommand::Send {
                    run,
                    keys,
                    answer,
                    cmux,
                } => {
                    let sending = Sending::parse(&keys, answer)?;
                    screen::run_send(
                        &mut queue,
                        &ScreenPorts {
                            cmux: &Cmux {
                                executable: executable(&cmux)?,
                            },
                            // Only Claude Code has a session with a screen.
                            signals: &ClaudeCode {
                                executable: PathBuf::from("claude"),
                            },
                        },
                        &RunTarget::parse(&run)?,
                        &sending,
                    )?
                }
                RunCommand::CloseWorkspaces {
                    run,
                    task,
                    apply,
                    cmux,
                } => {
                    use dagq::application::workspace_cleanup::{
                        CleanupTarget, close_ended_workspaces,
                    };
                    use dagq::infrastructure::adapters::SystemProcesses;
                    let target = match (run, task) {
                        (Some(run), _) => CleanupTarget::Run(RunId::new(run)?),
                        (None, Some(task)) => CleanupTarget::Task(TaskId::new(task)),
                        (None, None) => CleanupTarget::All,
                    };
                    serde_json::to_value(close_ended_workspaces(
                        &mut queue,
                        &Cmux {
                            executable: executable(&cmux)?,
                        },
                        &SystemProcesses,
                        &*generators.clock,
                        &actor,
                        &target,
                        apply,
                    )?)?
                }
            }
        }
        Command::Planner { command } => {
            use dagq::application::screen::{self, ScreenPorts, Sending};
            use dagq::infrastructure::adapters::{ClaudeCode, Cmux, executable};
            match command {
                PlannerCommand::Screen {
                    planner,
                    lines,
                    cmux,
                } => screen::planner_screen(
                    &mut queue,
                    &Cmux {
                        executable: executable(&cmux)?,
                    },
                    PlannerId::new(planner),
                    lines,
                )?,
                PlannerCommand::Send {
                    planner,
                    keys,
                    answer,
                    cmux,
                } => {
                    let sending = Sending::parse(&keys, answer)?;
                    screen::planner_send(
                        &mut queue,
                        &ScreenPorts {
                            cmux: &Cmux {
                                executable: executable(&cmux)?,
                            },
                            signals: &ClaudeCode {
                                executable: PathBuf::from("claude"),
                            },
                        },
                        PlannerId::new(planner),
                        &sending,
                    )?
                }
            }
        }
        Command::Down { wait, force, cmux } => {
            use dagq::application::lifecycle::DownOptions;
            use dagq::infrastructure::{
                adapters::{Cmux, SystemProcesses, executable},
                launchd::Launchctl,
            };
            // cmux is only needed to close an in-cmux supervisor's
            // workspace, so a queue without one still goes down when cmux
            // is not installed; the unresolved name then fails only there.
            one_shot.down(
                &location,
                &Cmux {
                    executable: executable(&cmux).unwrap_or(cmux),
                },
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
        Command::Report {
            period,
            at,
            out,
            print,
        } => one_shot.report_of(
            &queue,
            &db,
            period.parse().map_err(anyhow::Error::msg)?,
            at,
            out.as_deref(),
            print.is_some(),
        )?,
        Command::Observe {
            history: false,
            since,
            dry_run,
            daily,
            timeout,
            claude,
            cmux,
            ..
        } => {
            use dagq::infrastructure::adapters::{ClaudeCode, executable};
            use dagq::observer::{ObserveMode, ObserveOptions};
            // A dry run starts nothing, so it needs no Claude Code.
            let executable = if dry_run {
                claude
            } else {
                executable(&claude)?
            };
            let provider = ClaudeCode { executable };
            dagq::observer::observe(
                &db,
                &provider,
                &provider,
                &ObserveOptions {
                    mode: if daily {
                        ObserveMode::Daily
                    } else {
                        ObserveMode::Hourly
                    },
                    cmux: Some(cmux),
                    since: since.map(EventId::new),
                    dry_run,
                    timeout: Duration::from_secs(timeout),
                    dagq: env::current_exe()?,
                    user_config: dagq::infrastructure::language::user_config_file(),
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
            unavailable,
        } => {
            use dagq::domain::{Provider, actor_model::ActorLaunch};
            use dagq::infrastructure::{
                adapters::{ClaudeCode, executable},
                codex::Codex,
            };
            let launch: ActorLaunch = match launch {
                Some(launch) => serde_json::from_str(&launch).context("parse --launch")?,
                None => dagq::throughput_review::configured_launch(&db)?,
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
                    executable: resolved(claude, executable),
                }),
                Provider::Codex => Box::new(Codex {
                    executable: resolved(codex, dagq::infrastructure::codex::executable),
                    home: codex_home,
                }),
            };
            dagq::throughput_review::review(
                &db,
                provider.as_ref(),
                &dagq::throughput_review::ReviewOptions {
                    mode: mode.parse()?,
                    at,
                    dry_run,
                    timeout: Duration::from_secs(timeout),
                    dagq: env::current_exe()?,
                    user_config: dagq::infrastructure::language::user_config_file(),
                    utc_offset,
                    launch: Some(launch),
                    switchable,
                    unavailable,
                },
            )?
        }
        Command::Recover { run } => one_shot.recover(&db, &RunId::new(run)?)?,
        Command::Session {
            run,
            lease,
            claude,
            codex,
            resume,
            cmux,
            background,
        } => dagq::compose::session(
            &db,
            &RunId::new(run)?,
            &LeaseToken::new(lease),
            &claude,
            &codex,
            resume,
            &cmux,
            background,
        )?,
        Command::SessionEvent { event, .. } => {
            use dagq::{application::SessionRegistry, domain::sessions::SessionHook};
            let mut input = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
                .context("read the hook input")?;
            let input: Value = serde_json::from_str(&input).context("parse the hook input")?;
            match SessionHook::from_hook(&event, &input, |name| env::var(name).ok())
                .map_err(anyhow::Error::msg)?
            {
                Some(hook) => queue.record_session_hook(&hook)?,
                None => json!({"recorded": false, "reason": "not an inbox or planner session"}),
            }
        }
        Command::PlannerSession {
            planner,
            claude,
            plugin_dir,
            model,
            effort,
            cmux,
            headless,
            background,
        } => dagq::compose::planner_session(
            &db,
            dagq::domain::PlannerId::new(planner),
            &claude,
            plugin_dir.as_deref(),
            model.as_deref().zip(effort.as_deref()),
            &cmux,
            dagq::compose::PlannerEntry {
                headless,
                background,
            },
        )?,
    })
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
        Command::Observe { history: false, .. } => Some(("observe", location.log_dir.clone())),
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
/// the registration it takes over already has `up`'s mode.
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

fn main() -> ExitCode {
    let result = run(env::args_os().collect()).and_then(|value| {
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
            if let Some(kept) = dagq::application::install::KeptBinary::of(&error) {
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
mod tests {
    use super::*;

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
            ("broker start", &[]),
            ("service start", &[]),
            ("service stop", &[]),
            ("service serve", &[]),
            ("broker stop", &[]),
            ("plan", &[]),
            ("supervise", &[]),
            ("throughput-review", &[]),
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
            ("run send", &["1", "--key", "enter"]),
            ("planner screen", &["1"]),
            ("planner send", &["1", "--answer", "2"]),
            ("add", &["t"]),
            ("draft", &["1"]),
            ("edit", &["1", "--title", "t"]),
            ("set-goal", &["1", "1"]),
            ("set-paths", &["1", "--none"]),
            ("set-priority", &["1", "high"]),
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
            ("request add", &["--text", "plan it", "--ref", "task:1"]),
            ("request decline", &["1", "--reason", "r"]),
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

    /// The subcommands `DAGQ_COMMANDS` leaves out, each for a reason: the
    /// ones that only read, and the ones with a form that reads, which a
    /// rule on the name would deny as well (the CLI refuses the other
    /// form). A subcommand on neither list fails
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
        "broker status",
        "broker logs",
        "broker audit",
        "service status",
        // Forms that read: `graph` without `--out` and `observe --history`.
        "graph",
        "observe",
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
}
