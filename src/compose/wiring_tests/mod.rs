//! The wiring tests of the supervisor (docs/development/testing.md
//! "判断と境界のtest", ADR-t3841-1): the production supervise
//! ([`crate::application::supervise::supervise`]) on a real
//! [`SqliteQueue`](crate::infrastructure::sqlite::SqliteQueue) in SQLite's
//! memdb, reached through the queue service's use cases in this process,
//! with stateful fakes of the ports that reach Git, the session wrappers,
//! the agent, the verification and the processes. A test checks that a
//! judgment of the supervisor reaches the queue's events, asks and status,
//! within one deadline ([`harness::LIMIT`]), and joins every thread it
//! started; no external process, Git, SQLite file or sleep of the test's
//! own is used.

mod agent;
mod git;
mod harness;
mod host;
mod service;
mod session;

mod already_satisfied;
mod ci_freshness;
mod fresh_session_decisions;
mod landing_answers;
mod main_log;
mod previous_run_decisions;
mod provider_trial;
mod review_launch;
mod task_release;
mod triage;
mod unapplied_decisions;
mod worker_trial;
