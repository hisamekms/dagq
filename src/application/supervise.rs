//! The supervisor (ADR-0024 decision 1): execute claimed tasks in parallel,
//! validate their receipts, review accepted runs headless, land them on
//! main one at a time, triage failed ones and resume parked ones.
//!
//! Each context of the supervisor is a module with its state and the
//! submodules that change it (rule C3 of docs/design/architecture.md):
//! [`execution`] (実行と着地, with the loop), [`planning`] (計画管理),
//! [`observation`] (観測と分析) and [`host_ops`] (host運用). A state's
//! fields are private to its module, so another context's submodule cannot
//! change them; the loop calls the states' methods. What a pass of a
//! context reads of the loop is in [`contexts`].

use crate::domain::EventKind;
use crate::domain::LeaseToken;
use crate::domain::Priority;
use crate::domain::background_wrapper::StopRoute;
use crate::domain::light_slots::{self, ClaimRoom};
use crate::domain::run_phase;
use crate::domain::slot_limits::{SlotFlags, SlotLimits, SupervisorConfig};
use crate::domain::slot_order::{SlotCandidate, SlotKind, slot_order};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tracing::{error, info, warn};

use super::{
    AgentProvider, AgentSignals, AskQuery, CommandSpec, EventStore, Exhaustion, Generators,
    IdleHook, LeasedRun, MainRemote, ProcessControl, Queue, QueueOpener, Repository,
    ResumeCandidate, RunFiles, RunLog, SessionWrappers, Spawned, Spawner, Streams, TRIAGE_ASKER,
    TriageAction, Validation, Verifier, WorkerAdapters,
    actor_executor::{
        ActorExecutionSpec, ActorExecutor, ActorProgram, HeadlessProgram, HostActorExecutor,
        WorkspaceAccess,
    },
    ask, dependency_graph,
    health::{lease_health, run_health},
    integrate::{self as integration, Integration, IntegrationRequest, Integrator, check_receipt},
    naming::shell_join,
    or_none, path_text,
    prompt::{
        FittedPrompt, GoalPredecessorSummary, HEADLESS_NEVER, Inheritance, LandedTask,
        PredecessorSummary, PromptBytes, RESUME_REQUEST_LIMIT, REVISE_REQUEST_LIMIT,
        RecoveryMaterial, ResumeKind, ResumeRequest, TRIAGE_ACCESS, answer_text,
        closed_question_notice, continue_text, ended_run_material, kept_request_limit, prompt,
        recovery_instruction, recovery_prompt, restored_request, resume_request, review_prompt,
        revise_findings, revise_mismatch_request, revise_request, siblings_in_progress,
        stale_receipt_nudge, stall_nudge,
    },
    recording::{RecordingSessions, reason_of_error},
    tail, unix_seconds,
};
use crate::domain::{
    ABANDON_EXIT_FAILED, ABANDON_EXIT_REQUESTED_BEFORE, ABANDON_EXIT_SENT, ActorContext,
    AfterValidation, AskId, AskKind, AskReason, ClaimOutcome, CommitSha, ConflictDecision, EventId,
    EvidenceCheck, HEARTBEAT_TIMEOUT_SECS, IntegrationOutcome, LANDING_OPTIONS,
    MAX_RESUME_ATTEMPTS, MAX_REVISE_ATTEMPTS, NewAsk, NewHold, Provider, Reason, ReasonCode,
    Receipt, ReceiptResult, ResumedSession, ReviewDecision, ReviewVerdict, ReviseDecision,
    RunEvent, RunHistory, RunId, RunLease, RunPaths, RunPlan, RunProcess, RunStatus, SessionRole,
    TRIAGE_OPTIONS, TRIAGE_RETRY_FAILURES, TaskId, TaskRun, TaskStatus, TriageState,
    after_validation,
    claim_hold::{self, ClaimHold, HoldInputs},
    claim_spacing, decide_conflict, decide_revise, event_kind, headless_job, heartbeat_stale,
    instructions::{self, InstructionVersions},
    kpi::{CandidatesSample, ClaimPassEnd},
    marks::{RUN_ENV_CHANGED, run_env_digest},
    measure::{ClaimAttributes, ClaimSpacing, HostVersions, LoadSummary, LoadWindow},
    queue_hold::{HoldJob, Wall},
    recovery::{RecoveryAlert, RecoveryDecision, RecoveryVerdict},
    resume::{
        KILL_ONLY_RESUME_LIMIT, ResumeConfig, ResumeCount, inherits_on_exhaustion,
        uses_automatic_inherit,
    },
    run_env::RUN_ENV_PROGRAM_KINDS,
    stall::{BackgroundTask, StallConfig},
    triage_state,
    worker::Worker,
    worker_model::{WorkerSession, WorkerTrial},
};

mod contexts;
mod execution;
mod host_ops;
mod observation;
mod planning;

use self::contexts::{HostEnv, ObservationEnv, PassEnv, PlanningEnv};
pub use self::execution::*;
use self::execution::{background, claim_defer, headless, provider, recheck};
use self::host_ops::HostOpsState;
pub use self::host_ops::broker_relay::QueueRelayPort;
pub use self::host_ops::disk::CLEANUP_INTERVAL;
pub use self::host_ops::handoff::OWNER_GONE_GRACE;
pub use self::host_ops::handoff::SUPERVISOR_HANDED_OFF;
pub use self::host_ops::host_metrics::HostMetricsPort;
pub use self::host_ops::queue_broker::QueueBrokerPort;
pub use self::host_ops::queue_service::{
    QUEUE_SERVICE_INTERVAL, QUEUE_SERVICE_RESTART_WINDOW, QUEUE_SERVICE_RESTARTS,
    QUEUE_SERVICE_START_TIMEOUT, QueueServicePort,
};
pub use self::host_ops::release::{RELEASE_LOOK, ReleasePort};
pub use self::host_ops::sccache::SccachePort;
pub use self::host_ops::update::{UPDATE_INTERVAL, UpdateSettings};
use self::host_ops::{broker_relay, cleanup, disk, handoff, inbox_nudge, queue_broker, sccache};
use self::observation::ObservationState;
pub use self::observation::ci_watch::{CiSourceMaker, CiWatchFile, CiWatchPort};
pub use self::observation::forecast::{FORECAST_CHECK, ForecastPort};
pub use self::observation::ledger::{Built, FAILING_PASSES, LedgerPort, Retain, build_pass};
use self::observation::observer;
pub use self::observation::report::ReportPort;
use self::planning::PlanningState;
use self::planning::plan_review;
