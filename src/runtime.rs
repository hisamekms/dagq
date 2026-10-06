//! The names the runtime's use cases had before they moved to
//! `application` (ADR-0013), kept for the tests and the CLI: the entry
//! points are [`crate::compose`], the use cases and their types are in
//! `application`, and the log subscriber is in `infrastructure::telemetry`.
pub use crate::application::{
    health::{DoctorReport, LeaseHealth, ProcessHealth, RunHealth, SupervisorHealth},
    integrate::{IntegrateTarget, integrate_verify_log, register_follow_ups},
    prompt::{
        GOAL_REVIEW_ACCESS, GoalPredecessorSummary, HEADLESS_WORKER, PLAN_REVIEW_ACCESS,
        PredecessorSummary, REVIEW_ACCESS, TRIAGE_ACCESS, WORKER_READING,
        follow_up_categories_line, inbox_prompt, prompt, review_prompt, siblings_in_progress,
        worker_question_topics_line,
    },
    rebind::REBIND_LOG,
    recording::{BACKEND_ERROR_CHARS, RecordingBackend, backend_failure_payload},
    supervise::{RunError, SUPERVISOR_HANDED_OFF},
};
pub use crate::compose::{
    CiWatchOptions, OneShot, ProcessesPort, ReleaseIndexPort, RunE2eOptions, RunFilesPort,
    SccacheOptions, SuperviseOptions, ask, ci_failures, doctor, ended_run_material, integrate,
    rebind, recover, review, session, session_in_background, session_in_background_as, stats,
    status, status_for, supervise, supervise_with_reviewer,
};
pub use crate::infrastructure::claude::{PromptKind, detect_prompt};
