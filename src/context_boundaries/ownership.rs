//! The ownership of the runtime's state that the module paths do not tell,
//! for the rules C1, C7 and X1 (docs/design/architecture.md): the owner of
//! each table the migrations leave, the context of each store module of
//! `src/infrastructure/`, the functions of the transactions that change the
//! state of several contexts, and the writes to another context's table
//! allowed today. The runtime does not read it; [`super::check`] matches it
//! against the code.
//!
//! For the module references of L1, L2, L3, L6, L8, L9, C5 and C8
//! ([`super::modules`]): the shared parts outside the layers each layer may
//! reference, the modules of `src/compose/` that are not a context module,
//! the context of the modules of `src/application/` their path does not
//! tell, whom each port is public to, the values lent to other contexts,
//! the other contexts' port modules each port module may name, and the
//! references that break a rule allowed today.
//!
//! One row per line, kept by `#[rustfmt::skip]`.

use Context::{Execution, Host, Observation, Planning, Shared};
use Publicity::{Internal, Partial, Public};
use To::{All, Only};

/// A context of docs/design/architecture.md; [`Context::Shared`] is the
/// shared parts every context may write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Context {
    /// 計画管理.
    Planning,
    /// 実行と着地.
    Execution,
    /// 観測と分析.
    Observation,
    /// host運用.
    Host,
    /// 共有の部品.
    Shared,
}

/// The owner of a table some migration creates.
#[derive(Clone, Debug)]
pub(super) struct Table {
    pub(super) name: &'static str,
    pub(super) owner: Context,
}

/// The context of a store module of `src/infrastructure/`: a file, or a
/// directory ending in `/` for every file under it (the longest path wins).
#[derive(Clone, Debug)]
pub(super) struct Store {
    pub(super) path: &'static str,
    pub(super) context: Context,
}

/// A function of a transaction that changes the state of several contexts
/// (X1): SQL inside every fn of this name in the file may write another
/// context's table. The rows of one ID are one transaction and name one
/// writer.
#[derive(Debug)]
pub(super) struct Transaction {
    pub(super) id: &'static str,
    pub(super) writer: Context,
    pub(super) path: &'static str,
    pub(super) function: &'static str,
    pub(super) reason: &'static str,
}

/// Where the fix of an allowed violation is.
#[derive(Debug)]
pub(super) enum Fix {
    /// The tasks that fix it, which remove the row when it is gone.
    Tasks(&'static [u32]),
    /// No task fixes it yet.
    Unregistered,
}

/// A C1 violation allowed today: `reference` is `<table> in <fn>` as
/// [`super::check`] prints it (the innermost fn, `-` outside any).
#[derive(Debug)]
pub(super) struct Allowed {
    pub(super) path: &'static str,
    pub(super) reference: &'static str,
    pub(super) fix: Fix,
    pub(super) reason: &'static str,
}

/// A layer of docs/design/architecture.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Layer {
    Domain,
    Application,
    Infrastructure,
    /// The composition root, `src/compose.rs` and `src/compose/`.
    Compose,
}

/// A rule of docs/design/architecture.md the module references break.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Rule {
    L1,
    L2,
    L3,
    L6,
    L8,
    L9,
    C5,
    C8,
}

/// A module of the crate's root outside the layers that `layers` may
/// reference as a shared part (L1, L3, L8); any other module there is an
/// outer one no layer references.
#[derive(Debug)]
pub(super) struct SharedModule {
    pub(super) module: &'static str,
    pub(super) layers: &'static [Layer],
    pub(super) reason: &'static str,
}

/// A module reference allowed today: `reference` is what the rule names as
/// [`super::modules::check`] prints it: a module for L1, L3, L6, L8 and L9
/// (`crate::view`, `crate::compose::host` or `crate::compose::*`), the
/// forbidden item for L2 (`std::process`), the port's name for C5 and the
/// path as written for C8 (`super::execution`).
#[derive(Debug)]
pub(super) struct ModuleAllowed {
    pub(super) rule: Rule,
    pub(super) path: &'static str,
    pub(super) reference: &'static str,
    pub(super) fix: Fix,
    pub(super) reason: &'static str,
}

/// Whom a port, a method of a partial port or a lent value is public to.
#[derive(Clone, Copy, Debug)]
pub(super) enum To {
    All,
    /// These contexts; never empty.
    Only(&'static [Context]),
}

/// Whom a port is public to (C5).
#[derive(Clone, Copy, Debug)]
pub(super) enum Publicity {
    Public(To),
    /// Public to no other context.
    Internal,
    /// Its [`Method`] rows' methods are public, each to its `to`; the trait
    /// counts as public, by its name, to every context one of them names,
    /// and whether a context calls only those is the review's.
    Partial,
}

/// The context of a module of `src/application/` its path does not tell:
/// a file, or a directory ending in `/` for every file under it (the
/// longest path of these rows and [`STORES`] wins). The path tells it for
/// `src/application/ports/<context>.rs`,
/// `src/application/queue_reads/<context>.rs` and
/// `src/compose/<context>.rs`, which have no row; a file with neither has
/// no context.
#[derive(Debug)]
pub(super) struct ModuleContext {
    pub(super) path: &'static str,
    pub(super) context: Context,
}

/// A port of `src/application/ports/<context>.rs`, owned by that context:
/// each pub trait of those files (but `mod.rs`) has one row.
#[derive(Debug)]
pub(super) struct Port {
    pub(super) name: &'static str,
    pub(super) publicity: Publicity,
}

/// A port defined in the file `path` of `src/application/` outside
/// `ports/`; `owner` is `None` where the path tells it
/// (`queue_reads/<context>.rs`). Each pub trait of `src/application/`
/// outside `ports/` is an [`OuterPort`] or a [`NotPort`].
#[derive(Debug)]
pub(super) struct OuterPort {
    pub(super) path: &'static str,
    pub(super) name: &'static str,
    pub(super) owner: Option<Context>,
    pub(super) publicity: Publicity,
}

/// A pub trait of `src/application/` outside `ports/` that is no port.
#[derive(Debug)]
pub(super) struct NotPort {
    pub(super) path: &'static str,
    pub(super) name: &'static str,
    pub(super) reason: &'static str,
}

/// A method of a [`Publicity::Partial`] port public to `to`, declared in
/// the trait's body; the port's other methods are internal.
#[derive(Debug)]
pub(super) struct Method {
    pub(super) port: &'static str,
    pub(super) method: &'static str,
    pub(super) to: To,
}

/// A value or a function (not a trait) `owner` lends to other contexts,
/// which `path` defines; whether only they use it is not checked.
#[derive(Debug)]
pub(super) struct Lent {
    pub(super) path: &'static str,
    pub(super) name: &'static str,
    pub(super) owner: Context,
    pub(super) to: To,
}

/// The other contexts' port modules the port module of `context` may name
/// besides its own and `shared`'s (C8).
#[derive(Debug)]
pub(super) struct PortModules {
    pub(super) context: Context,
    pub(super) names: &'static [Context],
    pub(super) reason: &'static str,
}

const fn module(path: &'static str, context: Context) -> ModuleContext {
    ModuleContext { path, context }
}

const fn port(name: &'static str, publicity: Publicity) -> Port {
    Port { name, publicity }
}

const fn outer_port(
    path: &'static str,
    name: &'static str,
    owner: Option<Context>,
    publicity: Publicity,
) -> OuterPort {
    OuterPort {
        path,
        name,
        owner,
        publicity,
    }
}

const fn not_port(path: &'static str, name: &'static str, reason: &'static str) -> NotPort {
    NotPort { path, name, reason }
}

const fn method(port: &'static str, method: &'static str, to: To) -> Method {
    Method { port, method, to }
}

const fn lent(path: &'static str, name: &'static str, owner: Context, to: To) -> Lent {
    Lent {
        path,
        name,
        owner,
        to,
    }
}

const fn module_allowed(
    rule: Rule,
    path: &'static str,
    reference: &'static str,
    fix: Fix,
    reason: &'static str,
) -> ModuleAllowed {
    ModuleAllowed {
        rule,
        path,
        reference,
        fix,
        reason,
    }
}

const fn table(name: &'static str, owner: Context) -> Table {
    Table { name, owner }
}

const fn store(path: &'static str, context: Context) -> Store {
    Store { path, context }
}

const fn transaction(
    id: &'static str,
    writer: Context,
    path: &'static str,
    function: &'static str,
    reason: &'static str,
) -> Transaction {
    Transaction {
        id,
        writer,
        path,
        function,
        reason,
    }
}

const fn allowed(
    path: &'static str,
    reference: &'static str,
    fix: Fix,
    reason: &'static str,
) -> Allowed {
    Allowed {
        path,
        reference,
        fix,
        reason,
    }
}

#[rustfmt::skip]
pub(super) const TABLES: &[Table] = &[
    table("tasks", Planning),
    table("goals", Planning),
    table("task_dependencies", Planning),
    table("task_goal_dependencies", Planning),
    table("proposals", Planning),
    table("plan_reviews", Planning),
    table("goal_reviews", Planning),
    table("planners", Planning),
    table("plan_requests", Planning),
    table("plan_request_proposals", Planning),
    table("spike_cycles", Planning), // the investigations and the goals' Spike cycles; the claim (T1) opens one
    table("replan_operations", Planning), // the replanning of a run; its guard is read by every step of the task
    table("replan_mappings", Planning),
    table("replan_replacements", Planning),
    table("replan_plans", Planning), // the replacement plans a planner of a replanning request submits
    table("task_holds", Planning), // the hold of a task; the claim, the resume and the recovery round read it
    table("draft_origins", Planning),
    table("draft_reopens", Planning),
    table("draft_revisits", Planning),
    table("draft_bundles", Planning),
    table("draft_bundle_members", Planning),
    table("follow_up_judgements", Planning),
    table("search_index", Planning), // the search index; migration triggers write it
    table("landed_commits", Planning), // the search index; migration triggers write it
    table("task_runs", Execution),
    table("run_leases", Execution),
    table("run_processes", Execution),
    table("session_workspaces", Execution),
    table("findings", Observation),
    table("ledger_rows", Observation), // the runtime's control does not read it
    table("node_samples", Observation), // the NodeSampleStore; host writes it through the port, the runtime's control does not read it
    table("node_sample_imports", Observation), // the imported CSV files, written with node_samples in one transaction
    table("session_steps", Observation), // the SessionStepStore; the runtime's control does not read it
    table("session_step_removed", Observation), // the SessionStepStore's seqs its retention removed
    table("session_resources", Observation), // the sessions' resource samples an executor measured
    table("session_owners", Observation), // the target the control side registered each session to
    table("run_resources", Observation), // the runs' resources the supervisor measured from outside
    table("supervisors", Host),
    table("queue_repository", Host),
    table("schema_floor", Host),
    table("binary_updates", Host),
    table("headless_jobs", Host),
    table("asks", Shared), // an ask is opened and applied by the context of its kind
    table("run_events", Shared), // each context writes its own kinds (src/domain/event_kind.rs)
];

#[rustfmt::skip]
pub(super) const STORES: &[Store] = &[
    store("src/infrastructure/sqlite.rs", Planning), // TaskStore
    store("src/infrastructure/proposals.rs", Planning),
    store("src/infrastructure/plan_reviews.rs", Planning),
    store("src/infrastructure/goal_reviews.rs", Planning),
    store("src/infrastructure/plan_requests.rs", Planning),
    store("src/infrastructure/investigations.rs", Planning),
    store("src/infrastructure/replans.rs", Planning),
    store("src/infrastructure/task_holds.rs", Planning),
    store("src/infrastructure/planners.rs", Planning),
    store("src/infrastructure/draft_planners.rs", Planning),
    store("src/infrastructure/finding_planners.rs", Planning),
    store("src/infrastructure/follow_up_membership.rs", Planning),
    store("src/infrastructure/search.rs", Planning),
    store("src/infrastructure/runtime_store/planning_records.rs", Planning),
    store("src/infrastructure/runtime_store/mod.rs", Execution),
    store("src/infrastructure/runtime_store/transitions.rs", Execution),
    store("src/infrastructure/runtime_store/recovery.rs", Execution),
    store("src/infrastructure/runtime_store/session_registry.rs", Execution), // mixed: SessionRegistry also writes planning's planners
    store("src/infrastructure/runtime_store/run_log.rs", Execution), // also host's InboxWatchLog
    store("src/infrastructure/runtime_store/coordination.rs", Execution), // mixed: RunCoordination, and host's SupervisorRegistry
    store("src/infrastructure/runtime_store/eval_rounds.rs", Execution),
    store("src/infrastructure/sessions.rs", Execution), // the spans of the sessions
    store("src/infrastructure/session_tokens.rs", Execution),
    store("src/infrastructure/findings.rs", Observation),
    store("src/infrastructure/ci_watch_store.rs", Observation),
    store("src/infrastructure/kpi_push.rs", Observation),
    store("src/infrastructure/runtime_store/queue_records.rs", Observation),
    store("src/infrastructure/runtime_store/ledger.rs", Observation),
    store("src/infrastructure/runtime_store/node_samples.rs", Observation), // NodeSampleStore
    store("src/infrastructure/runtime_store/session_steps.rs", Observation), // SessionStepStore
    store("src/infrastructure/runtime_store/run_resources.rs", Observation), // RunResourceStore
    store("src/infrastructure/headless_jobs.rs", Host),
    store("src/infrastructure/schema.rs", Host),
    store("src/infrastructure/asks.rs", Shared), // AskStore
    store("src/infrastructure/runtime_store/ask_store.rs", Shared), // AskStore
    store("src/infrastructure/event_actor.rs", Shared), // EventStore
];

#[rustfmt::skip]
pub(super) const TRANSACTIONS: &[Transaction] = &[
    transaction("T1", Execution, "src/infrastructure/sqlite.rs", "claim_task", "claim: no run without its owner, no in_progress without its run (ADR-0054); runtime_store::transitions calls it"),
    transaction("T1", Execution, "src/infrastructure/runtime_store/transitions.rs", "claim_for_supervisor_holding", "claim, the supervisor's side"),
    transaction("T2", Execution, "src/infrastructure/runtime_store/transitions.rs", "finish_integration", "the landing on main and the task's completion agree"),
    transaction("T2", Execution, "src/infrastructure/sqlite.rs", "complete_task", "planning's function T2 and T12 complete the task through"),
    transaction("T3", Planning, "src/infrastructure/plan_reviews.rs", "decide_plan", "a proposal and its tasks in one step"),
    transaction("T4", Execution, "src/infrastructure/runtime_store/recovery.rs", "decide_triage", "a run's failure and its task's next state in one step (ADR-0047 decision 40)"),
    transaction("T4", Execution, "src/infrastructure/sqlite.rs", "transition_task", "planning's function T4 changes the task through"),
    transaction("T4", Execution, "src/infrastructure/sqlite.rs", "cancel_task", "planning's function T4 and T12 cancel the task through, with the answer as its reason (ADR-t947-4)"),
    transaction("T5", Planning, "src/infrastructure/goal_reviews.rs", "decide_goal", "a goal's verdict and its gap tasks in one step"),
    transaction("T6", Planning, "src/infrastructure/finding_planners.rs", "submit_linking", "no double proposal of a finding"),
    transaction("T6", Planning, "src/infrastructure/finding_planners.rs", "settle_findings", "the verdict goes back to the finding"),
    transaction("T7", Planning, "src/infrastructure/draft_planners.rs", "register_follow_ups", "a landed run's follow_ups become drafts once with their origin; execution calls it, apart from T2"),
    transaction("T8", Planning, "src/infrastructure/follow_up_membership.rs", "judge_follow_up", "the judgement, the membership and the question agree (ADR-t1504-2 decisions 1, 6, 9)"),
    transaction("T9", Planning, "src/infrastructure/follow_up_membership.rs", "decide_correction", "the answer, the goal, the membership and the open questions agree (ADR-t1504-2 decision 9)"),
    transaction("T10", Execution, "src/infrastructure/runtime_store/recovery.rs", "inherit_by_hand", "a person's carry-over, once and not racing the recovery job (ADR-t1962-1)"),
    transaction("T11", Execution, "src/infrastructure/runtime_store/coordination.rs", "heartbeat", "the registration and the leases agree"),
    transaction("T12", Execution, "src/infrastructure/runtime_store/transitions.rs", "close_satisfied", "a verified already_satisfied claim's run closes without a commit and its task completes in one step (ADR-t1818-1)"),
    transaction("T12", Execution, "src/infrastructure/runtime_store/transitions.rs", "decide_satisfied", "a person's retry or cancel fails the run and moves its task in one step (ADR-t1818-1)"),
    transaction("T13", Planning, "src/infrastructure/task_holds.rs", "release_task", "a release that chooses a new run closes the hold, ends the run and readies its task in one step, and refuses before it records anything while the run settles (ADR-t1879-1)"),
];

const COORDINATION: &str = "src/infrastructure/runtime_store/coordination.rs";
const MIXED_COORDINATION: &str =
    "the store mixes execution's RunCoordination and host's SupervisorRegistry";

#[rustfmt::skip]
pub(super) const ALLOWED: &[Allowed] = &[
    allowed("src/infrastructure/finding_planners.rs", "findings in link_findings", Fix::Tasks(&[1621]), "T6: planning's store writes observation's findings in SQL, not through infrastructure::findings"),
    allowed("src/infrastructure/finding_planners.rs", "findings in apply_answer", Fix::Tasks(&[1621]), "T6: planning's store writes observation's findings in SQL, not through infrastructure::findings"),
    allowed(COORDINATION, "supervisors in register", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in set_supervisor_mode", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in deregister_supervisor", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in prune_supervisor", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in accept_handoff", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in request_handoff", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in cancel_handoff", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in take_handoff", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in resume", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in set_auto_update", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in set_max_load", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in write_providers", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in refresh_registration", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in write_slot_limits", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "queue_repository in bind_repository", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "queue_repository in rebind_repository", Fix::Unregistered, MIXED_COORDINATION),
    allowed("src/infrastructure/runtime_store/session_registry.rs", "planners in end_planner", Fix::Unregistered, "SessionRegistry (execution) writes planning's planners; split the port"),
    allowed("src/infrastructure/sqlite.rs", "schema_floor in apply_migrations", Fix::Unregistered, "planning's store writes host's schema_floor when it migrates the queue"),
];

pub(super) const SHARED_MODULES: &[SharedModule] = &[
    SharedModule {
        module: "build_id",
        layers: &[Layer::Domain, Layer::Application, Layer::Infrastructure],
        reason: "shared with build.rs; the contexts use only the rule of the identifier and the commit it names, which do no I/O",
    },
    SharedModule {
        module: "migration_numbers",
        layers: &[Layer::Application, Layer::Infrastructure],
        reason: "shared with build.rs; the numbers of the migrations, which do no I/O",
    },
];

/// The modules of `src/compose/` that are not a context module (L9): the
/// supervisor loop's, which may name the contexts.
pub(super) const COMPOSE_LOOP: &[&str] = &["supervisor"];

#[rustfmt::skip]
pub(super) const MODULES_ALLOWED: &[ModuleAllowed] = &[
    module_allowed(Rule::C5, "src/application/health/attention.rs", "DraftPlannerStore", Fix::Tasks(&[2233]), "observation's attention reads planning's DraftPlannerStore (planner_answer_route, exhausted_drafts, exhausted_findings), whose reads are not public"),
    module_allowed(Rule::C5, "src/application/health/attention.rs", "GoalReviewStore", Fix::Tasks(&[2233]), "observation's attention reads planning's internal GoalReviewStore"),
    module_allowed(Rule::C5, "src/application/health/attention.rs", "PlanReviewStore", Fix::Tasks(&[2233]), "observation's attention reads planning's internal PlanReviewStore"),
    module_allowed(Rule::C5, "src/application/lifecycle.rs", "RunRecovery", Fix::Tasks(&[2233]), "host's lifecycle reads execution's internal RunRecovery"),
    module_allowed(Rule::C5, "src/application/lifecycle.rs", "SessionRegistry", Fix::Unregistered, "host's LifecycleQueue takes execution's internal SessionRegistry"),
    module_allowed(Rule::C5, "src/application/health/report.rs", "DraftPlannerStore", Fix::Tasks(&[2233]), "observation's status passes its queue to attention, which reads planning's DraftPlannerStore"),
    module_allowed(Rule::C5, "src/application/health/report.rs", "GoalReviewStore", Fix::Unregistered, "observation's status takes planning's internal GoalReviewStore"),
    module_allowed(Rule::C5, "src/application/health/report.rs", "PlanReviewStore", Fix::Unregistered, "observation's status takes planning's internal PlanReviewStore"),
    module_allowed(Rule::C5, "src/application/supervise/host_ops/cleanup.rs", "Repository", Fix::Unregistered, "host's cleanup job takes execution's internal Repository (its reads RepositoryReads are public to observation only)"),
    module_allowed(Rule::C5, "src/application/supervise/planning/plan_review.rs", "Repository", Fix::Unregistered, "planning's conflict hotspots take execution's internal Repository (its reads RepositoryReads are public to observation only)"),
    module_allowed(Rule::C5, "src/compose/observation.rs", "Verifier", Fix::Unregistered, "observation's composition reads execution's Verifier, public to host only"),
    module_allowed(Rule::C5, "src/infrastructure/runtime_store/mod.rs", "InboxWatchLog", Fix::Unregistered, "execution's store names host's internal InboxWatchLog"),
    module_allowed(Rule::C5, "src/infrastructure/runtime_store/run_log.rs", "InboxWatchLog", Fix::Unregistered, "execution's store implements host's internal InboxWatchLog"),
    module_allowed(Rule::C5, "src/infrastructure/runtime_store/run_log.rs", "ObserverLog", Fix::Unregistered, "execution's store implements observation's internal ObserverLog"),
];

#[rustfmt::skip]
pub(super) const MODULES: &[ModuleContext] = &[
    module("src/application/commands/planning.rs", Planning),
    module("src/application/commands/requests.rs", Planning),
    module("src/application/commands/replans.rs", Planning),
    module("src/application/commands/holds.rs", Planning),
    module("src/application/planner.rs", Planning),
    module("src/application/planner_handoff.rs", Planning),
    module("src/application/planner_request.rs", Planning),
    module("src/application/prompt/planners.rs", Planning), // inbox_prompt
    module("src/application/prompt/plan_review.rs", Planning), // plan_review_prompt
    module("src/application/supervise/planning.rs", Planning),
    module("src/application/supervise/planning/", Planning),
    module("src/application/session.rs", Execution),
    module("src/application/headless_session.rs", Execution),
    module("src/application/integrate.rs", Execution),
    module("src/application/review.rs", Execution),
    module("src/application/setup_review.rs", Execution),
    module("src/application/recording.rs", Execution),
    module("src/application/run_container.rs", Execution),
    module("src/application/inherit.rs", Execution),
    module("src/application/agent_eval.rs", Execution),
    module("src/application/agent_cases.rs", Execution),
    module("src/application/telemetry_tokens.rs", Execution),
    module("src/application/step_report.rs", Execution),
    module("src/application/prompt/worker.rs", Execution), // prompt
    module("src/application/prompt/requests.rs", Execution), // resume_request
    module("src/application/prompt/run_review.rs", Execution), // recovery_prompt
    module("src/application/prompt/setup_review.rs", Execution), // setup_review_prompt
    module("src/application/health/runs.rs", Execution), // recover
    module("src/application/stats.rs", Observation),
    module("src/application/kpi.rs", Observation),
    module("src/application/ledger_check.rs", Observation),
    module("src/application/forecast.rs", Observation),
    module("src/application/report.rs", Observation),
    module("src/application/observer/", Observation),
    module("src/application/watch.rs", Observation),
    module("src/application/throughput_review.rs", Observation),
    module("src/application/ci_watch.rs", Observation),
    module("src/application/main_log.rs", Observation),
    module("src/application/main_log/", Observation),
    module("src/application/health/report.rs", Observation), // status
    module("src/application/health/attention.rs", Observation), // attention
    module("src/application/supervise/observation.rs", Observation),
    module("src/application/supervise/observation/", Observation),
    module("src/application/lifecycle.rs", Host), // up, down
    module("src/application/install.rs", Host),
    module("src/application/update.rs", Host),
    module("src/application/queue_service.rs", Host),
    module("src/application/podman_machine.rs", Host),
    module("src/application/container_image.rs", Host),
    module("src/application/queue_broker.rs", Host),
    module("src/application/broker_relay.rs", Host),
    module("src/application/sccache.rs", Host),
    module("src/application/retention.rs", Host), // the observations' directories and the backups past their retention
    module("src/application/sccache/", Host),
    module("src/application/actor_executor.rs", Host),
    module("src/application/execution.rs", Host),
    module("src/application/release_update.rs", Host),
    module("src/application/health/host.rs", Host), // supervisors, roles
    module("src/application/supervise/host_ops.rs", Host),
    module("src/application/supervise/host_ops/", Host),
];

#[rustfmt::skip]
pub(super) const PORTS: &[Port] = &[
    port("TaskReads", Public(All)), // the reads of the tasks, goals, proposals and notes
    port("TaskStore", Partial),
    port("PlanningRecords", Public(All)),
    port("DraftPlannerReads", Public(All)), // the improvements running and the findings waiting for a planner
    port("DraftPlannerStore", Partial),
    port("PlanReviewStore", Internal),
    port("GoalReviewStore", Internal),
    port("PlanRequestStore", Internal),
    port("RunReads", Public(All)),
    port("RunLog", Public(All)), // the pair of RunReads and EventStore
    port("Verifier", Partial),
    port("RepositoryReads", Public(Only(&[Observation]))), // read-only reads of main; the CI watch reads the landing branch, unresolved it leaves the check to the landing's hold
    port("Repository", Internal),
    port("CoordinationReads", Public(All)),
    port("RunCoordination", Internal),
    port("SessionReads", Public(Only(&[Observation]))), // attention reads the planners that went silent
    port("SessionRegistry", Internal),
    port("RunTransitions", Internal), // its claim_for_supervisor_holding is T1
    port("RunRecovery", Internal),
    port("EvalRounds", Internal),
    port("ReviewProgramBackend", Internal),
    port("ContainerRuntime", Public(Only(&[Host]))), // whether the container runtime can run a target now; host only wraps it in BrokerGate, which shows whether the broker runs
    port("RecordingQueue", Internal),
    port("RunFiles", Public(All)), // not classified by architecture.md; public to the contexts that use it today
    port("AgentProvider", Public(All)), // not classified by architecture.md; public to the contexts that use it today
    port("TurnReader", Internal),
    port("StepReader", Internal), // the provider's collector of a session's steps, which the session wrapper reads its turns with
    port("Transcripts", Internal),
    port("AgentSignals", Public(Only(&[Planning, Observation]))), // not classified by architecture.md; public to the contexts that use it today
    port("MainRemote", Internal),
    port("QueueRecords", Partial),
    port("ObserverLog", Internal),
    port("ObservationQueue", Internal),
    port("LedgerStore", Internal), // the ledger is read through ledger::read
    port("NodeSampleStore", Partial), // the measurement reads it; host records the host's load
    port("LedgerQueue", Internal), // what builds the ledger reads and writes
    port("SessionStepStore", Internal), // the measurement's store of the sessions' steps
    port("TelemetrySink", Public(Only(&[Execution]))), // the executor sends its sessions' steps and resource samples through it
    port("TelemetrySinks", Public(Only(&[Planning, Execution]))), // where an executor finds its target's sink: the runs' and the runtime planner's session wrappers, and the verifications
    port("SessionResourceStore", Internal), // the measurement's store of the resources an executor measured
    port("SessionOwners", Public(Only(&[Execution]))), // the control side registers the target a session is of before its executor reports
    port("RunResourceStore", Internal), // the measurement's store of the runs' resources measured from outside
    port("ResourceQueue", Internal), // what the measure of the runs' resources reads and writes
    port("ExecutionEnvironment", Public(Only(&[Execution, Host]))), // the claim records the environment a run is attached to; host's record of its load measures the node in the common units
    port("MarkLog", Internal),
    port("SupervisorReads", Public(All)),
    port("SupervisorRegistry", Partial),
    port("HeadlessJobStore", Public(Only(&[Planning, Execution, Observation]))), // to each context that starts a job
    port("QueueOpener", Public(All)),
    port("LaunchAgent", Public(All)),
    port("SccacheServer", Public(All)),
    port("ProcessControl", Public(All)),
    port("InstalledPlugin", Public(All)),
    port("InboxWatchLog", Internal),
    port("HostOpsQueue", Internal),
    port("HostMetricsFiles", Internal), // the CSV copy of the host's load
    port("SessionWrappers", Public(Only(&[Planning, Execution]))), // not classified by architecture.md; public to the contexts that use it today
    port("Clock", Public(All)),
    port("IdGenerator", Public(All)),
    port("Spawner", Public(All)),
    port("Spawned", Public(All)),
    port("AskStore", Public(All)),
    port("EventStore", Public(All)),
    port("StateStore", Public(All)),
    port("Queue", Public(All)),
];

#[rustfmt::skip]
pub(super) const OUTER_PORTS: &[OuterPort] = &[
    outer_port("src/application/inherit.rs", "InheritStore", Some(Execution), Public(Only(&[Planning]))), // ready --inherit (T10)
    outer_port("src/application/inherit.rs", "CarriedBranches", Some(Execution), Public(Only(&[Planning]))), // ready --inherit (T10)
    outer_port("src/application/inherit.rs", "ReleasedBranches", Some(Execution), Public(Only(&[Planning]))), // the release of a task's hold that ends its run for a new one (T13)
    outer_port("src/application/agent_eval.rs", "EvalStore", Some(Execution), Public(Only(&[Host]))), // through AgentEvals to the CLI and the queue service; they write requests only
    outer_port("src/application/agent_cases.rs", "CaseSetStore", Some(Execution), Public(Only(&[Host]))), // through AgentCaseSets to the CLI and the queue service; they write requests and a person's decisions only
    outer_port("src/application/commands/requests.rs", "RequestStore", Some(Planning), Internal),
    outer_port("src/application/commands/replans.rs", "ReplanStore", Some(Planning), Public(Only(&[Host]))), // through Replans to the CLI and the queue service (a worker's proposal)
    outer_port("src/application/commands/planning.rs", "PlanningStore", Some(Planning), Internal),
    outer_port("src/application/queue_writes.rs", "PlannerRecords", Some(Planning), Internal), // the planner record a submit's proposal owner is read from, on the queue service's path
    outer_port("src/application/commands/holds.rs", "HoldStore", Some(Planning), Internal), // through Holds to the CLI
    outer_port("src/application/commands/dialogue.rs", "DialogueStore", Some(Shared), Public(All)), // asks and notes
    outer_port("src/application/commands/mod.rs", "DenialLog", Some(Shared), Public(All)), // what a command refused
    outer_port("src/application/watch.rs", "AskNotifier", Some(Observation), Internal), // implemented by PushNotifier
    outer_port("src/application/watch.rs", "WatchRecord", Some(Observation), Internal),
    outer_port("src/application/watch.rs", "WatchSource", Some(Observation), Internal), // implemented by compose's ServiceWatch
    outer_port("src/application/watch.rs", "OutageNotices", Some(Observation), Internal), // implemented by compose's ServiceWatch
    outer_port("src/application/lifecycle.rs", "LifecycleQueue", Some(Host), Internal),
    outer_port("src/application/actor_executor.rs", "ActorExecutor", Some(Host), Public(All)), // the start of an actor
    outer_port("src/application/actor_executor.rs", "ExecutorEvents", Some(Host), Public(All)), // where an executor records an agent found again: the queue's events, or a session wrapper's store
    outer_port("src/application/session.rs", "WrapperStore", Some(Execution), Public(Only(&[Planning, Host]))), // what a session wrapper reads and records, a planner's too; the queue service does its operations
    outer_port("src/application/release_update.rs", "ReleaseIndex", Some(Host), Internal),
    outer_port("src/application/install.rs", "Binaries", Some(Host), Internal),
    outer_port("src/application/install.rs", "ReleaseInstaller", Some(Host), Internal),
    outer_port("src/application/queue_service.rs", "ServiceQueue", Some(Host), Internal),
    outer_port("src/application/queue_service.rs", "ServiceBackend", Some(Host), Internal),
    outer_port("src/application/queue_service.rs", "QueueServiceControl", Some(Host), Internal),
    outer_port("src/application/queue_broker.rs", "QueueBrokerControl", Some(Host), Internal), // how up, down and the supervisor start, look at and stop the broker
    outer_port("src/application/queue_broker.rs", "BrokerBackend", Some(Host), Internal), // what the broker runs a request on
    outer_port("src/application/podman_machine.rs", "PodmanMachine", Some(Host), Internal), // the podman commands on the dagq machine
    outer_port("src/application/podman_machine.rs", "MachineUsers", Some(Host), Internal), // the host-wide lock and users of the dagq machine
    outer_port("src/application/podman_machine.rs", "MachineVolumes", Some(Host), Public(Only(&[Execution]))), // the named volumes of a queue in the dagq machine; the cleanup at a run's end and the landing removes a run's build output
    outer_port("src/application/podman_machine.rs", "RunMachine", Some(Host), Public(Only(&[Execution]))), // a run's parts in the dagq machine (the machine with the run as a user, the image, the run's relay, a build output volume), made ready before each turn of a worker and each verification in the container
    outer_port("src/application/queue_broker.rs", "VerificationTokens", Some(Host), Public(Only(&[Execution]))), // how integrate issues and revokes the token of each verification
    outer_port("src/application/container_image.rs", "ImageSpecs", Some(Host), Internal), // the images of a queue as its main checkout decides them
    outer_port("src/application/container_image.rs", "ImageBuilder", Some(Host), Internal), // the podman commands on the images in the dagq machine
    outer_port("src/application/container_image.rs", "ImageRecords", Some(Host), Internal), // the queue's records of the container image
    outer_port("src/application/broker_relay.rs", "RelayControl", Some(Host), Public(Only(&[Execution]))), // the relays of the runs' containers in the dagq machine; a run's container opens its relay before it starts and closes it at its end
    outer_port("src/application/broker_relay.rs", "RelayRecords", Some(Host), Internal), // the supervisor's record of the relays, read by the gate
    outer_port("src/application/run_container.rs", "RunContainers", Some(Execution), Internal), // the containers a worker's turns run in; the wrapper starts and removes them, and every stop of a run's session or processes removes its container
    outer_port("src/application/run_container.rs", "VerificationContainers", Some(Execution), Internal), // the containers the commands of integrate's verification run in, on a clone of the commit that lands; integrate removes what each verification left
    outer_port("src/application/queue_service.rs", "ServiceAccess", Some(Host), Public(Only(&[Execution, Planning]))), // not classified by architecture.md; the supervisor issues a token at the claim and the resume, and a planner's wrapper's as it starts the wrapper
    outer_port("src/application/queue_service.rs", "TelemetryTokens", Some(Host), Public(Only(&[Execution]))), // not classified by architecture.md; the supervisor issues and revokes an executor's telemetry token per target
    outer_port("src/application/supervisor_service.rs", "ServiceCaller", Some(Host), Public(Only(&[Execution]))), // how the supervisor reaches its queue's service; it signs in before it registers (ADR-t3102-3)
    outer_port("src/application/setup_review.rs", "SetupReviewHost", Some(Execution), Internal), // the main checkout, the temporary place and the job of setup-review
    outer_port("src/application/setup_review.rs", "InitRepository", Some(Execution), Internal), // the main checkout, its dagq.toml and its files init proposes the toolchain profiles from
    outer_port("src/application/ci_watch.rs", "CiSource", Some(Observation), Internal),
    outer_port("src/application/throughput_review.rs", "ThroughputReviewSources", Some(Observation), Internal),
    outer_port("src/application/throughput_review.rs", "ThroughputReviewHost", Some(Observation), Internal),
    outer_port("src/application/observer/mod.rs", "ObserverSources", Some(Observation), Internal),
    outer_port("src/application/observer/mod.rs", "ObserverHost", Some(Observation), Internal),
    outer_port("src/application/queue_reads/planning.rs", "PlanningSources", None, Public(Only(&[Observation]))), // not classified by architecture.md; compose joins it with ObservationSources in QueueReadSources
    outer_port("src/application/queue_reads/observation.rs", "ObservationSources", None, Internal),
];

#[rustfmt::skip]
pub(super) const NOT_PORTS: &[NotPort] = &[
    not_port("src/application/telemetry_tokens.rs", "TokenLog", "a narrowing of EventStore to the record of a run's events, implemented for every EventStore"),
    not_port("src/application/main_log.rs", "MainReader", "a narrowing of RepositoryReads to its reads of main, implemented for every RepositoryReads"),
    not_port("src/application/queue_reads/mod.rs", "QueueReadSources", "a bound that joins PlanningSources and ObservationSources"),
    not_port("src/application/supervisor_service.rs", "SupervisorStore", "a bound that joins the run's ports the service runs a supervisor's operations on"),
    not_port("src/application/supervisor_service/wire.rs", "Wire", "how an operation's result crosses between the service and the supervisor, implemented for the values"),
];

#[rustfmt::skip]
pub(super) const METHODS: &[Method] = &[
    method("TaskStore", "claim", Only(&[Execution])), // T1
    method("TaskStore", "investigations", Only(&[Execution])), // the claim's investigation limit (ADR-t1487-1)
    method("DraftPlannerStore", "register_follow_ups", Only(&[Execution])), // T7
    method("Verifier", "run_env", Only(&[Host])), // the environment, for host運用's diagnosis
    method("Verifier", "run_env_programs", Only(&[Host])), // the programs, for host運用's diagnosis
    method("Verifier", "retention_config", Only(&[Host, Planning])), // [retention], for the sweeps of their ended jobs' directories (ADR-t697-1)
    method("QueueRecords", "findings", All), // the reads; record_* are internal
    method("QueueRecords", "reports_written", All),
    method("QueueRecords", "kpi_breaches_open", All),
    method("QueueRecords", "ci_watch_events", All),
    method("QueueRecords", "ci_failure_findings_of", All),
    method("QueueRecords", "ci_failure_fix_tasks", All),
    method("NodeSampleStore", "record_node_sample", Only(&[Host])), // the record of the host's load
    method("NodeSampleStore", "import_node_samples", Only(&[Host])), // the import of the CSV files
    method("NodeSampleStore", "imported_files", Only(&[Host])), // the import reads its marks
    method("NodeSampleStore", "remove_node_samples", Only(&[Host])), // the retention
    method("SupervisorRegistry", "register_supervisor", Only(&[Execution])), // the loop
    method("SupervisorRegistry", "prune_supervisor", Only(&[Execution])),
    method("SupervisorRegistry", "accept_handoff", Only(&[Execution])),
    method("SupervisorRegistry", "request_handoff", Only(&[Execution])),
    method("SupervisorRegistry", "take_handoff", Only(&[Execution])),
    method("SupervisorRegistry", "cancel_handoff", Only(&[Execution])),
    method("SupervisorRegistry", "resume_registration", Only(&[Execution])),
    method("SupervisorRegistry", "set_auto_update", Only(&[Execution])),
    method("SupervisorRegistry", "set_slot_limits", Only(&[Execution])),
    method("SupervisorRegistry", "set_max_load", Only(&[Execution])),
    method("SupervisorRegistry", "rebind_repository", Only(&[Execution])),
    method("SupervisorRegistry", "bind_repository", Only(&[Execution])),
    method("SupervisorRegistry", "assert_repository", Only(&[Execution])),
    method("SupervisorRegistry", "set_supervisor_mode", Only(&[Execution])),
];

#[rustfmt::skip]
pub(super) const LENT: &[Lent] = &[
    lent("src/application/supervise/planning.rs", "PlanningState", Planning, Only(&[Execution])), // the stop and the end of a carried-over review
    lent("src/domain/stats/conflicts.rs", "ConflictHotspot", Planning, Only(&[Execution])), // the conflict hotspots
    lent("src/domain/run.rs", "TaskRun", Execution, All), // the view of a run, as a value
    lent("src/application/supervise/execution/jobs.rs", "JobDesk", Execution, Only(&[Planning])), // jobs and providers
    lent("src/application/supervise/execution/claim_defer.rs", "DeferWatch", Execution, Only(&[Planning])), // the file of the forecast
    lent("src/application/supervise/execution/background.rs", "stop_swept_wrapper", Execution, Only(&[Host])), // the stop of a wrapper, for the sweep
    lent("src/application/supervise/execution/recheck.rs", "build_dirs", Execution, Only(&[Host])), // the declarations' build outputs, for the cleaning of free disk
    lent("src/application/supervise/execution/recheck.rs", "clear_targets", Execution, Only(&[Host])), // under recheck's lock, for the cleaning of free disk
    lent("src/application/ci_watch.rs", "known_failures", Observation, Only(&[Execution])), // to the landing's verification
    lent("src/application/ci_watch.rs", "run_known_failures", Observation, Only(&[Execution])), // to the worker's prompts and the run's whole review
    lent("src/application/supervise/observation/ci_watch.rs", "CiWatchState", Observation, Only(&[Execution])), // read held and unreadable only; supervise::ci_watch changes it
    lent("src/domain/throughput_review.rs", "UnusableFinish", Observation, Only(&[Execution])), // supervise::provider holds it back once
    lent("src/application/supervise/observation/observer.rs", "UnusableTimerJob", Observation, Only(&[Execution])), // supervise::provider holds it back once
    lent("src/application/supervise/observation/report.rs", "ReportPort", Observation, Only(&[Host])), // where the KPI push goes, for inbox_nudge
    lent("src/application/queue_broker.rs", "BrokerGate", Host, Only(&[Execution])), // whether the broker runs, through ContainerRuntime
    lent("src/application/podman_machine.rs", "MachineGate", Host, Only(&[Execution])), // whether the dagq machine can be used, through ContainerRuntime
    lent("src/application/queue_broker.rs", "VerificationToken", Host, Only(&[Execution])), // the token of one verification, revoked at its end
    lent("src/application/container_image.rs", "ImageGate", Host, Only(&[Execution])), // whether the container image can be had, through ContainerRuntime
    lent("src/application/broker_relay.rs", "RelayGate", Host, Only(&[Execution])), // whether the runs' relays are kept, through ContainerRuntime
    lent("src/application/supervise/host_ops.rs", "HostOpsState", Host, Only(&[Execution])), // free disk and sccache, read
    lent("src/application/supervise/host_ops/cleanup.rs", "CleanupWatch", Host, Only(&[Execution])), // cleaning and defer
    lent("src/application/supervise/host_ops/sccache.rs", "ensure_sccache", Host, Only(&[Execution])),
    lent("src/application/supervise/host_ops/sccache.rs", "sccache_look", Host, Only(&[Execution])),
];

#[rustfmt::skip]
pub(super) const PORT_MODULES: &[PortModules] = &[
    PortModules { context: Observation, names: &[Planning, Execution, Host], reason: "観測と分析 reads the other contexts" },
    PortModules { context: Shared, names: &[Planning, Execution, Observation, Host], reason: "Queue and StateStore join every context's ports" },
    PortModules { context: Host, names: &[Execution], reason: "host names RunLog, the pair of RunReads and EventStore" },
];
