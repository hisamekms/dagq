//! The audit service's port (ADR-t4840-2 decisions 1, 2 and 13): a
//! state-changing use case writes its audit record in its own transaction
//! through [`AuditAppend`], so a record that cannot be written leaves the
//! state unchanged too, and [`append`] gives the record its order, its
//! chain and its idempotence. [`USE_CASES`] says which privileged use case
//! writes which actions, and why one writes none; [`SERVICE_ENTRIES`] and
//! [`of_control`] say which entry of the queue service runs which of them.
//! Their tests check that every entry the queue service declares has a
//! row, and every capability is a privileged use case's or ordinary.

use std::fmt;

use crate::domain::audit::Sender;
use crate::domain::audit::{
    Admission, AssuranceClaim, AuditAction, AuditRecord, Change, Principal, RecordError,
    SenderInput, admit, chain,
};
use crate::domain::authorization::Capability;
use crate::domain::queue_service::{ControlCommand, UseCase};

/// The fact written by the queue's repository-binding transaction, from
/// its before/after values, independently of operational events. The
/// control-side sender supplies its counter and the observed time.
pub fn repository_binding_change(
    sender_seq: u64,
    before: Option<&str>,
    after: &str,
    at: String,
) -> SenderInput {
    use crate::domain::audit::{Authority, Correlation, Outcome, OutcomeStatus, Resource};
    SenderInput {
        sender_seq,
        at,
        action: AuditAction::AuthzPrivileged,
        resource: Resource {
            kind: "queue".into(),
            id: "repository_binding".into(),
        },
        authority: Authority {
            capability: Some(Capability::QueueAdmin.to_string()),
            ..Authority::default()
        },
        outcome: Outcome {
            status: OutcomeStatus::Succeeded,
            reason: None,
        },
        change: Some(Change {
            before: before.map(|binding| serde_json::json!({"git_common_dir": binding})),
            after: Some(serde_json::json!({"git_common_dir": after})),
            ..Change::default()
        }),
        correlation: Correlation::default(),
        answered_via: None,
        landing: None,
        external: None,
    }
}

/// The last record of the chain: what the next one follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainHead {
    pub seq: u64,
    pub hash: String,
}

/// The audit store, inside the transaction of the caller's change of the
/// state: an implementation is the store's handle of that transaction and
/// opens none of its own, so the record commits with the change or not at
/// all. Its records are only inserted, never updated or removed.
pub trait AuditAppend {
    /// The chain's last record, `None` while it is empty.
    fn head(&mut self) -> anyhow::Result<Option<ChainHead>>;
    /// The record of the idempotence key `(sender_id, sender_seq)`.
    fn by_key(&mut self, sender_id: &str, sender_seq: u64) -> anyhow::Result<Option<AuditRecord>>;
    /// Adds `record` after the head.
    fn insert(&mut self, record: &AuditRecord) -> anyhow::Result<()>;
}

/// What the audit service takes of a sender's call from its
/// authentication, never from what the sender says (ADR-t4840-2 decision
/// 1): [`Admission`] but the chain's order.
#[derive(Debug, Clone, PartialEq)]
pub struct Caller {
    pub sender: Sender,
    pub sender_instance: String,
    pub principal: Principal,
    pub on_behalf_of: Vec<Principal>,
    pub auth: String,
    pub assurance: AssuranceClaim,
    pub evidence: std::collections::BTreeMap<String, String>,
    pub clock_source: String,
}

/// What [`append`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Appended {
    /// A new record at `seq`.
    Written { seq: u64 },
    /// The same input of the same key was admitted before, at `seq`: a
    /// sender's retry, written once.
    AlreadyWritten { seq: u64 },
}

/// Why [`append`] wrote nothing; the caller's transaction rolls back.
#[derive(Debug)]
pub enum AuditError {
    /// The key `(sender_id, sender_seq)` holds another input: the sender
    /// reused its counter.
    KeyReused {
        sender_id: String,
        sender_seq: u64,
        seq: u64,
    },
    /// The record was refused ([`admit`]).
    Refused(RecordError),
    /// The store failed.
    Store(anyhow::Error),
}

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyReused {
                sender_id,
                sender_seq,
                seq,
            } => write!(
                f,
                "the audit record {sender_id} #{sender_seq} is already seq {seq} with another input"
            ),
            Self::Refused(why) => write!(f, "the audit service refused the record: {why:?}"),
            Self::Store(error) => write!(f, "the audit store failed: {error:#}"),
        }
    }
}

impl std::error::Error for AuditError {}

/// Whether `record` was admitted from `input` and `caller`: what the
/// sender said, its change as [`Change::recorded`] keeps it, and what the
/// service took of the caller.
fn same_input(record: &AuditRecord, input: &SenderInput, caller: &Caller) -> bool {
    record.at == input.at
        && record.action == input.action
        && record.resource == input.resource
        && record.authority == input.authority
        && record.outcome == input.outcome
        && record.change == input.change.clone().map(Change::recorded)
        && record.correlation == input.correlation
        && record.answered_via == input.answered_via
        && record.landing == input.landing
        && record.external == input.external
        && record.principal == caller.principal
        && record.on_behalf_of == caller.on_behalf_of
        && record.auth == caller.auth
        && caller.assurance.chained() == Ok(record.attestation.level)
        && record.attestation.evidence == caller.evidence
}

/// Admits `input` from `caller` into the chain of `store`, in the caller's
/// transaction: after the head, with the time it was received
/// (`received_at`). The same input again under its key is written once;
/// another input under a used key is refused.
pub fn append<S: AuditAppend + ?Sized>(
    store: &mut S,
    input: SenderInput,
    caller: Caller,
    received_at: String,
) -> Result<Appended, AuditError> {
    let sender_id = format!("{}:{}", caller.sender, caller.sender_instance);
    if let Some(existing) = store
        .by_key(&sender_id, input.sender_seq)
        .map_err(AuditError::Store)?
    {
        return if same_input(&existing, &input, &caller) {
            Ok(Appended::AlreadyWritten { seq: existing.seq })
        } else {
            Err(AuditError::KeyReused {
                sender_id,
                sender_seq: input.sender_seq,
                seq: existing.seq,
            })
        };
    }
    let (seq, prev_hash) = match store.head().map_err(AuditError::Store)? {
        Some(head) => (head.seq.saturating_add(1), head.hash),
        None => (1, chain::genesis()),
    };
    let record = admit(
        input,
        Admission {
            seq,
            prev_hash,
            received_at,
            clock_source: caller.clock_source,
            sender: caller.sender,
            sender_instance: caller.sender_instance,
            principal: caller.principal,
            on_behalf_of: caller.on_behalf_of,
            auth: caller.auth,
            assurance: caller.assurance,
            evidence: caller.evidence,
        },
    )
    .map_err(AuditError::Refused)?;
    store.insert(&record).map_err(AuditError::Store)?;
    Ok(Appended::Written { seq })
}

macro_rules! privileged_use_cases {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        pub enum $name { $($(#[$vmeta])* $variant),+ }

        impl $name {
            /// Every variant, by its declaration: a variant added is here.
            pub const ALL: &[Self] = &[$(Self::$variant),+];
        }
    };
}

privileged_use_cases!(
    /// A use case of the application that holds a privilege, weakens a
    /// gate, administers the queue or allows a landing: each is a row of
    /// [`USE_CASES`].
    PrivilegedUseCase {
    /// The land answer of an approve_landing ask, a person's approval.
    ApproveLandingAnswer,
    /// Any other answer of an ask, a person's judgment.
    AskAnswer,
    /// A supervisor's judgment that allows a landing: a review's pass, an
    /// AI's land, the convergence after the limit.
    LandingJudgment,
    /// `ready` with no plan review.
    PlanReviewBypass,
    /// A change of state with a flag that goes past a refusal or a gate
    /// (`down --force`, ...).
    Force,
    /// A run's recovery by hand (`recover`).
    ManualRecover,
    /// The inbox opened without its guardrail.
    GuardrailDisable,
    /// `install`, its `--rollback` and `--release`.
    Install,
    /// The automatic update's job.
    AutoUpdate,
    /// The release update's job.
    ReleaseUpdate,
    /// `migrate`.
    Migrate,
    /// `up`.
    Up,
    /// `down`.
    Down,
    /// `dagq inbox`, opening the inbox.
    InboxOpen,
    /// `service start|stop`.
    ServiceStartStop,
    /// `queue-broker start|stop`.
    BrokerStartStop,
    /// `integrate`: a direct one's approval, the comparison and the merge.
    Integrate,
    /// The push of a landing.
    Push,
    /// A refusal of the policy, of any use case.
    Refusal,
    /// A change of the audit's scope or retention.
    AuditPolicyChange,
    /// A correction of a record.
    AuditCorrection,
    /// The reconciliation's check of the chain, main and the remote.
    AuditCheck,
    /// `setup-review --yes`, a person's approval of the review's settings
    /// written to the working tree.
    ReviewSetupApproval,
    /// `disk relax`, the disk's needs relaxed for a while.
    DiskRelax,
    /// A goal's landing mark put on or taken off.
    GoalLandingMark,
    /// The issue of a principal's token by its parent.
    TokenIssue,
    /// The team version's runners and delegates: registering, sharing and
    /// revoking a runner, a person's delegates.
    TeamRegistry,
    /// The queue's binding to a repository changed: `rebind`, and `init`
    /// binding an existing queue to the client's repository.
    Rebind,
});

/// What a privileged use case writes to the audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditDuty {
    /// These actions, in the use case's transaction.
    Writes(&'static [AuditAction]),
    /// None at stage 0, for this reason.
    NotWritten(&'static str),
}

/// One row of [`USE_CASES`]: the use case, the capability the policy
/// grants it by (`None` for one no capability names: the guardrail is how
/// the inbox is launched, a refusal is of any capability, and the
/// reconciliation and the audit's own changes are the audit's), and its
/// duty.
#[derive(Debug, Clone, Copy)]
pub struct UseCaseRow {
    pub use_case: PrivilegedUseCase,
    pub capabilities: &'static [Capability],
    pub duty: AuditDuty,
}

const fn row(
    use_case: PrivilegedUseCase,
    capabilities: &'static [Capability],
    duty: AuditDuty,
) -> UseCaseRow {
    UseCaseRow {
        use_case,
        capabilities,
        duty,
    }
}

use AuditAction as A;
use AuditDuty::{NotWritten, Writes};
use Capability as C;
use PrivilegedUseCase as U;

/// Which privileged use case writes which actions (ADR-t4840-2 decision
/// 13's stage 0): every [`PrivilegedUseCase`] once.
#[rustfmt::skip]
pub const USE_CASES: &[UseCaseRow] = &[
    row(U::ApproveLandingAnswer, &[C::AskAnswer], Writes(&[A::AskAnswered, A::AnswerApplied, A::LandingApproved])),
    row(U::AskAnswer, &[C::AskAnswer], Writes(&[A::AskAnswered, A::AnswerApplied])),
    row(U::LandingJudgment, &[C::Supervise], Writes(&[A::LandingJudged])),
    row(U::PlanReviewBypass, &[C::TaskReadyBypassReview], Writes(&[A::AuthzPrivileged])),
    row(U::Force, &[C::ServiceLifecycle], Writes(&[A::AuthzPrivileged])),
    row(U::ManualRecover, &[C::RunRecover], Writes(&[A::AuthzPrivileged])),
    row(U::GuardrailDisable, &[], Writes(&[A::AuthzPrivileged])),
    row(U::Install, &[C::BinaryInstall], Writes(&[A::BinaryInstalled])),
    row(U::AutoUpdate, &[C::BinaryInstall], Writes(&[A::BinaryInstalled])),
    row(U::ReleaseUpdate, &[C::BinaryInstall], Writes(&[A::BinaryInstalled])),
    row(U::Migrate, &[C::QueueAdmin], Writes(&[A::SchemaMigrated])),
    row(U::Up, &[C::ServiceLifecycle], Writes(&[A::ServiceLifecycle])),
    row(U::Down, &[C::ServiceLifecycle], Writes(&[A::ServiceLifecycle])),
    row(U::InboxOpen, &[C::ServiceLifecycle], NotWritten("opening the inbox starts no service and grants nothing; opening it without its guardrail is GuardrailDisable")),
    row(U::ServiceStartStop, &[C::ServiceLifecycle], Writes(&[A::ServiceLifecycle])),
    row(U::BrokerStartStop, &[C::ServiceLifecycle], NotWritten("the queue broker is a sender from stage 1; at stage 0 it records its refusals as events")),
    row(U::Integrate, &[C::IntegrationRequest, C::Land], Writes(&[A::LandingApproved, A::LandingCompared, A::LandingMerged, A::ConfigChanged])),
    row(U::Push, &[C::Push], Writes(&[A::PushAttempted, A::PushCompleted])),
    row(U::Refusal, &[], Writes(&[A::AuthzDenied])),
    row(U::AuditPolicyChange, &[], Writes(&[A::AuditPolicyChanged])),
    row(U::AuditCorrection, &[], Writes(&[A::AuditCorrected])),
    row(U::AuditCheck, &[], Writes(&[A::AuditChecked])),
    row(U::ReviewSetupApproval, &[C::ReviewSetupApprove], NotWritten("the settings reach main only through a run's landing, whose config.changed records them")),
    row(U::DiskRelax, &[C::DiskRelax], NotWritten("relaxing the disk's needs grants no authority and changes no landing; the action enum v1 has no action for it")),
    row(U::GoalLandingMark, &[C::GoalLandingMark], NotWritten("the landing mark is the team version's gate (stage 2); stage 0 records the landings it holds")),
    row(U::TokenIssue, &[C::TokenIssue], NotWritten("a token is issued only to a child the policy lets its parent start; its refusal is authz.denied")),
    row(U::Rebind, &[C::QueueAdmin], Writes(&[A::AuthzPrivileged])),
    row(U::TeamRegistry, &[C::RunnerRegister, C::RunnerRevoke, C::RunnerShare, C::DelegateWrite], NotWritten("the team version's records (stage 2); stage 0's host has no runner or delegate")),
];

/// The capabilities no privileged use case is granted by, each with why:
/// reads, planning before a run (whose code lands only through a judged
/// and merged landing), an actor's own work, and the reserved ones.
#[rustfmt::skip]
pub const ORDINARY_CAPABILITIES: &[(Capability, &str)] = &[
    (C::RunnerClaim, RUNNER), (C::RunnerHeartbeat, RUNNER), (C::RunnerReport, RUNNER),
    (C::RunnerDeclare, RUNNER), (C::RunnerList, READ), (C::ImagePublish, RUNNER),
    (C::IdentityLogin, IDENTITY), (C::CredentialPreference, IDENTITY),
    (C::AuditRead, READ), (C::QueueRead, READ), (C::QueueWatch, READ), (C::ExportFile, READ), (C::CiRead, READ),
    (C::GoalWrite, PLANNING), (C::GoalReady, PLANNING), (C::GoalClose, PLANNING),
    (C::GoalReviewRequest, PLANNING), (C::TaskWrite, PLANNING), (C::FollowUpJudge, PLANNING),
    (C::TaskVerifyEdit, PLANNING), (C::TaskReady, PLANNING), (C::ProposalSubmit, PLANNING),
    (C::ProposalWithdraw, PLANNING), (C::NoteWrite, PLANNING), (C::MarkWrite, PLANNING),
    (C::AskOpen, ACTOR), (C::SessionRun, ACTOR), (C::SessionRecord, ACTOR),
    (C::PrepareReview, ACTOR), (C::ReviewSubmit, ACTOR), (C::TriageSubmit, ACTOR),
    (C::TaskCancel, PLANNING), (C::FindingRecord, ACTOR), (C::FindingResolve, ACTOR),
    (C::FindingDismiss, ACTOR), (C::FindingAsk, ACTOR), (C::ObserveRun, ACTOR),
    (C::AskClose, PLANNING), (C::PlannerOpen, PLANNING), (C::RequestRecord, PLANNING),
    (C::RequestDecline, PLANNING), (C::ReplanRequest, PLANNING), (C::ReplanPropose, ACTOR),
    (C::ReplanResolve, PLANNING), (C::ReplanPark, PLANNING), (C::ReplanApply, PLANNING),
    (C::TaskHold, PLANNING), (C::ScreenRead, READ), (C::ScreenSend, ACTOR),
    (C::PlannerRequest, PLANNING), (C::EvalRequest, ACTOR), (C::EvalRequestHeldOut, ACTOR),
    (C::EvalRerun, ACTOR), (C::EvalRead, READ), (C::EvalCases, ACTOR), (C::ReviewSetup, ACTOR),
    (C::WorkspaceCleanup, ACTOR), (C::TelemetryReport, ACTOR), (C::QueueBrokerRecord, ACTOR),
    (C::FilesystemRead, RESERVED), (C::FilesystemWrite, RESERVED), (C::NetworkAccess, RESERVED),
    (C::SecretRead, RESERVED),
];

const READ: &str = "a read changes nothing";
const PLANNING: &str =
    "planning before a run: its code lands only through a judged, compared and merged landing";
const ACTOR: &str =
    "an actor's own work or its record, which allows no landing and weakens no gate";
const RUNNER: &str = "a runner's work on the lease the control side assigned it";
const IDENTITY: &str = "a person's own login and credential choice";
const RESERVED: &str = "reserved for a later stage and granted to no role";

/// The privileged use cases a control-side command's steps
/// ([`UseCase::Control`]) run; none for a command that only reads
/// (`observe`, `throughput-review`). A new command does not build until it
/// is placed here.
pub const fn of_control(command: ControlCommand) -> &'static [PrivilegedUseCase] {
    match command {
        ControlCommand::Integrate => &[U::Integrate, U::Push],
        ControlCommand::Review => &[U::LandingJudgment],
        ControlCommand::Observe | ControlCommand::ThroughputReview => &[],
        ControlCommand::SetupReview => &[U::ReviewSetupApproval],
        ControlCommand::Up => &[U::Up],
        ControlCommand::Down => &[U::Down, U::Force],
        ControlCommand::Inbox => &[U::InboxOpen, U::GuardrailDisable],
        ControlCommand::ServiceStart | ControlCommand::ServiceStop => &[U::ServiceStartStop],
        ControlCommand::BrokerStart | ControlCommand::BrokerStop => &[U::BrokerStartStop],
        ControlCommand::Install => &[U::Install],
        ControlCommand::AutoUpdate => &[U::AutoUpdate],
        ControlCommand::ReleaseUpdate => &[U::ReleaseUpdate],
    }
}

/// What an entry of the queue service ([`UseCase`]) is to the audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// It runs these privileged use cases.
    Privileged(&'static [PrivilegedUseCase]),
    /// Its steps are a control-side command's ([`of_control`]).
    Control,
    /// It holds no privilege, for this reason.
    Ordinary(&'static str),
}

use Entry::{Control, Ordinary, Privileged};

/// Every entry of the queue service, each once: the application's use cases
/// are reached only through them (ADR-t3102-3), so a privileged use case
/// is one of these rows. A use case added to [`UseCase`] without a row
/// fails this table's test.
#[rustfmt::skip]
pub const SERVICE_ENTRIES: &[(UseCase, Entry)] = &[
    (UseCase::Hello, Ordinary(READ)),
    (UseCase::Ask, Ordinary(ACTOR)),
    (UseCase::Show, Ordinary(READ)), (UseCase::Note, Ordinary(PLANNING)),
    (UseCase::ProposalList, Ordinary(READ)), (UseCase::ProposalShow, Ordinary(READ)),
    (UseCase::FindingRecord, Ordinary(ACTOR)), (UseCase::FindingResolve, Ordinary(ACTOR)),
    (UseCase::FindingDismiss, Ordinary(ACTOR)),
    (UseCase::List, Ordinary(READ)), (UseCase::Candidates, Ordinary(READ)),
    (UseCase::Graph, Ordinary(READ)), (UseCase::Status, Ordinary(READ)),
    (UseCase::AuditVerify, Ordinary(READ)), (UseCase::Asks, Ordinary(READ)), (UseCase::Events, Ordinary(READ)),
    (UseCase::Timeline, Ordinary(READ)), (UseCase::Stats, Ordinary(READ)),
    (UseCase::Kpi, Ordinary(READ)), (UseCase::Forecast, Ordinary(READ)),
    (UseCase::Notes, Ordinary(READ)), (UseCase::Marks, Ordinary(READ)),
    (UseCase::Findings, Ordinary(READ)), (UseCase::Search, Ordinary(READ)),
    (UseCase::Related, Ordinary(READ)), (UseCase::GoalList, Ordinary(READ)),
    (UseCase::GoalShow, Ordinary(READ)), (UseCase::Lint, Ordinary(READ)),
    (UseCase::ObserveHistory, Ordinary(READ)), (UseCase::ObserveInput, Ordinary(READ)),
    (UseCase::Requests, Ordinary(READ)), (UseCase::Replans, Ordinary(READ)),
    (UseCase::CiFailures, Ordinary(READ)), (UseCase::Planners, Ordinary(READ)),
    (UseCase::ServiceStatus, Ordinary(READ)), (UseCase::StatsInputs, Ordinary(READ)),
    (UseCase::AgentEval, Ordinary(ACTOR)), (UseCase::AgentResults, Ordinary(READ)),
    (UseCase::AgentResult, Ordinary(READ)), (UseCase::ReplanPropose, Ordinary(ACTOR)),
    (UseCase::WatchPoll, Ordinary(READ)), (UseCase::AgentCases, Ordinary(ACTOR)),
    (UseCase::AgentDecide, Ordinary(ACTOR)), (UseCase::AgentSets, Ordinary(READ)),
    (UseCase::AgentSet, Ordinary(READ)), (UseCase::TelemetryReport, Ordinary(ACTOR)),
    (UseCase::Supervisor, Privileged(&[U::LandingJudgment, U::Integrate, U::Push, U::TokenIssue])),
    (UseCase::Control, Control),
    (UseCase::SessionRun, Ordinary(ACTOR)), (UseCase::SessionRecord, Ordinary(ACTOR)),
    (UseCase::DenialRecord, Privileged(&[U::Refusal])),
    (UseCase::IssueToken, Privileged(&[U::TokenIssue])),
    (UseCase::ExchangeToken, Privileged(&[U::TokenIssue])),
    (UseCase::RunnerPoll, Ordinary(RUNNER)), (UseCase::RunnerHeartbeat, Ordinary(RUNNER)),
    (UseCase::RunnerReport, Ordinary(RUNNER)), (UseCase::RunnerRelease, Ordinary(RUNNER)),
    (UseCase::RunnerDeclare, Ordinary(RUNNER)),
    (UseCase::RunnerShare, Privileged(&[U::TeamRegistry])),
    (UseCase::RunnerList, Ordinary(READ)), (UseCase::RunnerGithub, Ordinary(RUNNER)),
    (UseCase::ImagePublisherExchange, Privileged(&[U::TokenIssue])),
    (UseCase::ImagePublish, Ordinary(RUNNER)),
    (UseCase::RunnerCode, Privileged(&[U::TeamRegistry])),
    (UseCase::RunnerEnroll, Privileged(&[U::TeamRegistry])),
    (UseCase::RunnerRefresh, Ordinary(RUNNER)),
    (UseCase::RunnerRevoke, Privileged(&[U::TeamRegistry])),
    (UseCase::CredentialPreference, Ordinary(IDENTITY)),
    (UseCase::RevokeToken, Ordinary("it takes a token away from an ended supervisor, which narrows authority")),
    (UseCase::Add, Ordinary(PLANNING)), (UseCase::Edit, Ordinary(PLANNING)),
    (UseCase::Submit, Ordinary(PLANNING)), (UseCase::Cancel, Ordinary(PLANNING)),
    (UseCase::DependencyAdd, Ordinary(PLANNING)), (UseCase::DependencyRemove, Ordinary(PLANNING)),
    (UseCase::SetGoal, Ordinary(PLANNING)), (UseCase::SetPaths, Ordinary(PLANNING)),
    (UseCase::SetPriority, Ordinary(PLANNING)), (UseCase::Revisit, Ordinary(PLANNING)),
    (UseCase::JudgeFollowUp, Ordinary(PLANNING)), (UseCase::Draft, Ordinary(PLANNING)),
    (UseCase::Ready, Privileged(&[U::PlanReviewBypass])),
    (UseCase::GoalAdd, Ordinary(PLANNING)), (UseCase::GoalEdit, Ordinary(PLANNING)),
    (UseCase::GoalClose, Ordinary(PLANNING)), (UseCase::GoalReady, Ordinary(PLANNING)),
    (UseCase::GoalReview, Ordinary(PLANNING)),
    (UseCase::GoalLandingMark, Privileged(&[U::GoalLandingMark])),
    (UseCase::DelegateAdd, Privileged(&[U::TeamRegistry])),
    (UseCase::DelegateRemove, Privileged(&[U::TeamRegistry])),
    (UseCase::DelegateList, Ordinary(READ)),
    (UseCase::ProposalWithdraw, Ordinary(PLANNING)), (UseCase::RequestAdd, Ordinary(PLANNING)),
    (UseCase::RequestDecline, Ordinary(PLANNING)), (UseCase::Mark, Ordinary(PLANNING)),
    (UseCase::DiskRelax, Privileged(&[U::DiskRelax])),
    (UseCase::Answer, Privileged(&[U::ApproveLandingAnswer, U::AskAnswer])),
    (UseCase::AskClose, Ordinary(PLANNING)),
    (UseCase::Hold, Ordinary(PLANNING)), (UseCase::Release, Ordinary(PLANNING)),
    (UseCase::ReleaseRead, Ordinary(READ)), (UseCase::ReleaseImages, Ordinary(PLANNING)),
    (UseCase::ReplanRequest, Ordinary(PLANNING)), (UseCase::ReplanResolve, Ordinary(PLANNING)),
    (UseCase::ReplanResolveRead, Ordinary(READ)),
    (UseCase::Rebind, Privileged(&[U::Rebind])),
    (UseCase::Recover, Privileged(&[U::ManualRecover])),
    (UseCase::RunScreen, Ordinary(READ)), (UseCase::RunSend, Ordinary(ACTOR)),
    (UseCase::RunLog, Ordinary(READ)), (UseCase::PlannerScreen, Ordinary(READ)),
    (UseCase::PlannerSend, Ordinary(ACTOR)), (UseCase::PlannerLog, Ordinary(READ)),
    (UseCase::PlannerRequest, Ordinary(PLANNING)), (UseCase::Report, Ordinary(READ)),
    (UseCase::ReportInputs, Ordinary(READ)), (UseCase::GraphExport, Ordinary(READ)),
    (UseCase::ReadyInherit, Ordinary(PLANNING)), (UseCase::ReadyInheritRead, Ordinary(READ)),
    (UseCase::ReadyInheritImages, Ordinary(PLANNING)),
    (UseCase::BrokerRunStatus, Ordinary(READ)), (UseCase::BrokerBinding, Ordinary(READ)),
    (UseCase::BrokerRecord, Ordinary("the queue broker's refusals and sweeps, an outside access (J) the broker sends from stage 1")),
    (UseCase::Migrate, Privileged(&[U::Migrate])),
    (UseCase::CommitMessages, Privileged(&[U::Migrate])),
    (UseCase::Init, Privileged(&[U::Rebind])),
    (UseCase::Doctor, Ordinary(READ)),
];

/// The privileged use cases no entry reaches yet: the audit's own, which
/// the audit's later tasks add as entries (`dagq audit`, the
/// reconciliation's job).
pub const AUDIT_OWN: &[PrivilegedUseCase] =
    &[U::AuditPolicyChange, U::AuditCorrection, U::AuditCheck];

/// The entry of `use_case`.
pub fn entry(use_case: UseCase) -> Option<Entry> {
    SERVICE_ENTRIES
        .iter()
        .find(|(listed, _)| *listed == use_case)
        .map(|(_, entry)| *entry)
}

/// The row of `use_case`.
pub fn duty(use_case: PrivilegedUseCase) -> Option<&'static UseCaseRow> {
    USE_CASES.iter().find(|row| row.use_case == use_case)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::audit::{
        Attestation, Authority, ChainAnchor, Correlation, Outcome, OutcomeStatus, Resource,
        chain::verify,
    };
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn repository_binding_fact_names_the_privilege_and_both_states() {
        let input = repository_binding_change(7, Some("/old/.git"), "/new/.git", "at".into());
        assert_eq!(input.action, AuditAction::AuthzPrivileged);
        assert_eq!(input.authority.capability.as_deref(), Some("queue.admin"));
        assert_eq!(input.resource.kind, "queue");
        assert_eq!(input.resource.id, "repository_binding");
        assert_eq!(input.sender_seq, 7);
        let change = input.change.unwrap();
        assert_eq!(
            change.before,
            Some(serde_json::json!({"git_common_dir": "/old/.git"}))
        );
        assert_eq!(
            change.after,
            Some(serde_json::json!({"git_common_dir": "/new/.git"}))
        );
        assert!(
            repository_binding_change(1, None, "/new/.git", "at".into())
                .change
                .unwrap()
                .before
                .is_none()
        );
    }

    #[test]
    fn every_privileged_use_case_has_one_row() {
        for use_case in PrivilegedUseCase::ALL {
            let rows = USE_CASES
                .iter()
                .filter(|row| row.use_case == *use_case)
                .count();
            assert_eq!(rows, 1, "{use_case:?} has {rows} rows of USE_CASES");
        }
        assert_eq!(USE_CASES.len(), PrivilegedUseCase::ALL.len());
        for row in USE_CASES {
            match row.duty {
                Writes(actions) => assert!(!actions.is_empty(), "{:?}", row.use_case),
                NotWritten(reason) => assert!(!reason.is_empty(), "{:?}", row.use_case),
            }
        }
    }

    #[test]
    fn every_capability_is_granted_to_a_row_or_named_ordinary_with_why() {
        let privileged: BTreeSet<&str> = USE_CASES
            .iter()
            .flat_map(|row| row.capabilities.iter().map(|c| c.as_str()))
            .collect();
        for capability in Capability::ALL {
            let ordinary = ORDINARY_CAPABILITIES
                .iter()
                .filter(|(c, _)| *c == capability)
                .count();
            let named = usize::from(privileged.contains(capability.as_str()));
            assert_eq!(
                ordinary + named,
                1,
                "{capability} must be a privileged use case's capability of USE_CASES or one row of ORDINARY_CAPABILITIES"
            );
        }
    }

    #[test]
    fn the_privileged_use_cases_of_the_design_each_write_their_action() {
        for (use_case, action) in [
            (U::ApproveLandingAnswer, A::LandingApproved),
            (U::PlanReviewBypass, A::AuthzPrivileged),
            (U::Force, A::AuthzPrivileged),
            (U::ManualRecover, A::AuthzPrivileged),
            (U::GuardrailDisable, A::AuthzPrivileged),
            (U::Install, A::BinaryInstalled),
            (U::AutoUpdate, A::BinaryInstalled),
            (U::Migrate, A::SchemaMigrated),
            (U::Up, A::ServiceLifecycle),
            (U::Down, A::ServiceLifecycle),
            (U::Push, A::PushAttempted),
            (U::Push, A::PushCompleted),
            (U::Integrate, A::LandingMerged),
            (U::Refusal, A::AuthzDenied),
        ] {
            let Some(Writes(actions)) = duty(use_case).map(|row| row.duty) else {
                panic!("{use_case:?} writes nothing");
            };
            assert!(
                actions.contains(&action),
                "{use_case:?} does not write {action}"
            );
        }
    }

    #[test]
    fn every_action_is_written_by_a_use_case() {
        let written: BTreeSet<&str> = USE_CASES
            .iter()
            .filter_map(|row| match row.duty {
                Writes(actions) => Some(actions.iter().map(|a| a.as_str())),
                NotWritten(_) => None,
            })
            .flatten()
            .collect();
        for action in AuditAction::ALL {
            assert!(
                written.contains(action.as_str()),
                "no use case writes {action}"
            );
        }
    }

    /// The values of the `string_enum!` of `name` in `source`, read as
    /// text: what the enum declares, apart from any table of this file.
    fn declared(source: &str, name: &str) -> Vec<String> {
        let start = source
            .find(&format!("string_enum!({name} {{"))
            .unwrap_or_else(|| panic!("no string_enum! of {name}"));
        let body = &source[start..];
        let body = &body[..body.find("\n});").expect("the enum ends")];
        body.lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .filter_map(|line| line.split_once("=> \"").map(|(_, rest)| rest))
            .map(|rest| rest[..rest.find('"').expect("a closed name")].to_owned())
            .collect()
    }

    /// The declared values with no row of [`SERVICE_ENTRIES`].
    fn unlisted(values: &[String]) -> Vec<String> {
        values
            .iter()
            .filter(|value| value.parse::<UseCase>().ok().and_then(entry).is_none())
            .cloned()
            .collect()
    }

    fn queue_service_source() -> String {
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/domain/queue_service.rs"
        ))
        .expect("the source of the use cases")
    }

    #[test]
    fn every_entry_of_the_queue_service_has_one_row() {
        let values = declared(&queue_service_source(), "UseCase");
        assert!(values.len() > 100, "read {} use cases", values.len());
        assert_eq!(unlisted(&values), Vec::<String>::new());
        assert_eq!(
            SERVICE_ENTRIES.len(),
            values.len(),
            "a use case listed twice"
        );
        // An entry added to the enum without a row is found.
        let mut added = values.clone();
        added.push("queue_rewrite".to_owned());
        assert_eq!(unlisted(&added), ["queue_rewrite"]);
    }

    #[test]
    fn every_privileged_use_case_is_reached_from_an_entry() {
        let control = declared(&queue_service_source(), "ControlCommand");
        let mut reached: BTreeSet<PrivilegedUseCase> = AUDIT_OWN.iter().copied().collect();
        for value in &control {
            let command: ControlCommand = value.parse().unwrap();
            reached.extend(of_control(command).iter().copied());
        }
        for (_, entry) in SERVICE_ENTRIES {
            if let Privileged(use_cases) = entry {
                assert!(!use_cases.is_empty());
                reached.extend(use_cases.iter().copied());
            }
        }
        for use_case in PrivilegedUseCase::ALL {
            assert!(reached.contains(use_case), "no entry reaches {use_case:?}");
        }
        for value in [
            "integrate",
            "review",
            "up",
            "down",
            "inbox",
            "install",
            "auto_update",
        ] {
            assert!(!of_control(value.parse().unwrap()).is_empty(), "{value}");
        }
        assert!(of_control(ControlCommand::Observe).is_empty());
    }

    /// The entries that need `queue.admin` and change the queue are
    /// privileged: the binding's change too, not only the migration.
    #[test]
    fn the_queues_administration_is_privileged_but_its_report() {
        assert_eq!(entry(UseCase::Rebind), Some(Privileged(&[U::Rebind])));
        assert_eq!(entry(UseCase::Init), Some(Privileged(&[U::Rebind])));
        assert_eq!(entry(UseCase::Migrate), Some(Privileged(&[U::Migrate])));
        assert!(matches!(entry(UseCase::Doctor), Some(Ordinary(_))));
        assert!(matches!(
            duty(U::Rebind).map(|row| row.duty),
            Some(Writes(&[A::AuthzPrivileged]))
        ));
    }

    /// A store of the records in memory, as a transaction's handle.
    #[derive(Default)]
    struct Memory(Vec<AuditRecord>);

    impl AuditAppend for Memory {
        fn head(&mut self) -> anyhow::Result<Option<ChainHead>> {
            Ok(self.0.last().map(|r| ChainHead {
                seq: r.seq,
                hash: r.hash.clone(),
            }))
        }
        fn by_key(
            &mut self,
            sender_id: &str,
            sender_seq: u64,
        ) -> anyhow::Result<Option<AuditRecord>> {
            Ok(self
                .0
                .iter()
                .find(|r| r.sender_id == sender_id && r.sender_seq == sender_seq)
                .cloned())
        }
        fn insert(&mut self, record: &AuditRecord) -> anyhow::Result<()> {
            self.0.push(record.clone());
            Ok(())
        }
    }

    fn caller(sender: Sender) -> Caller {
        Caller {
            sender,
            sender_instance: "1".to_owned(),
            principal: Principal {
                role: "user".to_owned(),
                id: "user".to_owned(),
            },
            on_behalf_of: Vec::new(),
            auth: "socket_peer".to_owned(),
            assurance: AssuranceClaim::Chained(Attestation::Enforced),
            evidence: BTreeMap::new(),
            clock_source: "host".to_owned(),
        }
    }

    fn input(sender_seq: u64, action: AuditAction, id: &str) -> SenderInput {
        SenderInput {
            sender_seq,
            at: "2026-11-13T00:00:00Z".to_owned(),
            action,
            resource: Resource {
                kind: "run".to_owned(),
                id: id.to_owned(),
            },
            authority: Authority::default(),
            outcome: Outcome {
                status: OutcomeStatus::Succeeded,
                reason: None,
            },
            change: None,
            correlation: Correlation::default(),
            answered_via: None,
            landing: None,
            external: None,
        }
    }

    #[test]
    fn append_chains_after_the_head_and_writes_a_retry_once() {
        let mut store = Memory::default();
        let at = || "2026-11-13T00:00:01Z".to_owned();
        let qs = || caller(Sender::QueueService);
        assert_eq!(
            append(&mut store, input(1, A::AskAnswered, "a"), qs(), at()).unwrap(),
            Appended::Written { seq: 1 }
        );
        assert_eq!(
            append(
                &mut store,
                input(1, A::PushAttempted, "a"),
                caller(Sender::Integrate),
                at()
            )
            .unwrap(),
            Appended::Written { seq: 2 }
        );
        assert_eq!(
            append(&mut store, input(1, A::AskAnswered, "a"), qs(), at()).unwrap(),
            Appended::AlreadyWritten { seq: 1 }
        );
        assert!(matches!(
            append(&mut store, input(1, A::AskAnswered, "b"), qs(), at()),
            Err(AuditError::KeyReused {
                seq: 1,
                sender_seq: 1,
                ..
            })
        ));
        let mut other = qs();
        other.principal.id = "inbox".to_owned();
        assert!(matches!(
            append(&mut store, input(1, A::AskAnswered, "a"), other, at()),
            Err(AuditError::KeyReused { seq: 1, .. })
        ));
        assert_eq!(
            append(&mut store, input(2, A::AnswerApplied, "a"), qs(), at()).unwrap(),
            Appended::Written { seq: 3 }
        );
        assert_eq!(store.0.len(), 3);
        assert_eq!(store.0[0].prev_hash, chain::genesis());
        assert!(verify(ChainAnchor::genesis(), &store.0).is_ok());
    }

    #[test]
    fn append_writes_nothing_it_refuses() {
        let mut store = Memory::default();
        let mut reported = caller(Sender::QueueService);
        reported.assurance = AssuranceClaim::SelfReported;
        let refused = append(
            &mut store,
            input(1, A::AskAnswered, "a"),
            reported,
            String::new(),
        );
        assert!(matches!(
            refused,
            Err(AuditError::Refused(RecordError::SelfReported))
        ));
        assert!(refused.unwrap_err().to_string().contains("refused"));
        assert!(store.0.is_empty());
    }
}
