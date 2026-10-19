//! The ownership of the runtime's state that the module paths do not tell,
//! for the rules C1, C7 and X1 (docs/design/architecture.md): the owner of
//! each table the migrations leave, the context of each store module of
//! `src/infrastructure/`, the functions of the transactions that change the
//! state of several contexts, and the writes to another context's table
//! allowed today. The runtime does not read it; [`super::check`] matches it
//! against the code.
//!
//! For the module references of L1, L3, L6, L8 and L9 ([`super::modules`]):
//! the shared parts outside the layers each layer may reference, the
//! modules of `src/compose/` that are not a context module, and the
//! references that break a rule allowed today.
//!
//! One row per line, kept by `#[rustfmt::skip]`: until C5 moves to a Rust
//! test, `scripts/check-context-boundaries.sh` reads the context of a file
//! of `src/infrastructure/` from the `store(...)` rows of [`STORES`].

use Context::{Execution, Host, Observation, Planning, Shared};

/// A context of docs/design/architecture.md; [`Context::Shared`] is the
/// shared parts every context may write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    L3,
    L6,
    L8,
    L9,
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

/// A module reference allowed today: `reference` is the module the rule
/// names as [`super::modules::check`] prints it (`crate::view`,
/// `crate::compose::host` or `crate::compose::*`).
#[derive(Debug)]
pub(super) struct ModuleAllowed {
    pub(super) rule: Rule,
    pub(super) path: &'static str,
    pub(super) reference: &'static str,
    pub(super) fix: Fix,
    pub(super) reason: &'static str,
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
    allowed(COORDINATION, "supervisors in resume_registration", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in set_auto_update", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in set_max_load", Fix::Unregistered, MIXED_COORDINATION),
    allowed(COORDINATION, "supervisors in set_supervisor_providers", Fix::Unregistered, MIXED_COORDINATION),
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

pub(super) const MODULES_ALLOWED: &[ModuleAllowed] = &[];
