//! The dagq runtime in layers (ADR-0013):
//!
//! - [`domain`]: the aggregates (`Task`, `Goal`, `TaskRun`), their value
//!   types and the business decisions, with no I/O.
//! - [`application`]: the use cases and the ports (traits) they reach the
//!   outside through.
//! - [`infrastructure`]: the adapters that implement the ports (SQLite,
//!   Git, cmux, Claude Code, launchd, processes, files, clock and IDs).
//! - [`compose`]: the composition root, which builds the adapters and
//!   injects them into the use cases; `main` resolves the queue location,
//!   parses the CLI and prints what it returns.
//!
//! Outside the layers: [`view`] shapes the CLI's compact output;
//! [`broker_material`] and [`migration_numbers`]
//! are shared with `build.rs`; and [`runtime`] and [`lifecycle`] only
//! re-export the names the tests use from before the move.
//!
//! Across the layers, each module also belongs to one context (planning,
//! execution and landing, observation and analysis, host operation) or to
//! the shared parts every context may use; the contexts, what each owns
//! and exposes, and the rules (L1 to L8, C1 to C7, X1 to X3) are in
//! docs/design/architecture.md (ADR-t1545-1). Of [`build_id`], the
//! contexts use only the rule of the identifier and the commit it names,
//! which do no I/O; its `emit` and `compute` call Git and belong to the
//! build script.
pub mod application;
pub mod broker_material;
/// The build identifier's rule, shared with the broker's binaries.
pub use dagq_broker_protocol::build_id;
pub mod compose;
pub mod domain;
pub mod infrastructure;
pub mod lifecycle;
pub mod migration_numbers;
pub mod runtime;
pub mod view;

/// The build identifier of this binary (ADR-0045 decision 2): `X.Y.Z` for a
/// release, `X.Y.Z-dev+<commit>[.dirty]` for a development build. `dagq
/// --version` prints it, and every supervisor registration records it so
/// `up` replaces a supervisor whose identifier differs from its own in any
/// part, the commit included.
pub const VERSION: &str = env!("DAGQ_BUILD_ID");
