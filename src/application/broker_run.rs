//! A run's use of the resource broker ([Broker] token and the worker's
//! tools, ADR-t827-2 decision 2 and ADR-t827-4 decision 1): with the mode
//! `preferred`, the supervisor issues a run's token when it starts the
//! worker (and its resume), writes the token to `<queue dir>/broker/tokens/<run id>`
//! (mode 0600, outside every mount) and the MCP configuration of the
//! broker's client to `<run dir>/broker/mcp.json`, and revokes the token
//! when the run ends. The executor hands the configuration to the agent
//! ([`crate::application::AgentProvider::broker_tools`]) when the file is
//! there: a run without it starts exactly as before.
//!
//! With the mode `required` (ADR-t838-1) the supervisor also leaves
//! [`REQUIRED_FILE`] in the run's dir before its worker or resume starts,
//! and the configuration names the run's receipt for the client's
//! `write_receipt` tool ([`RECEIPT_FILE_ENV`]). The executor reads the
//! mark: a run that has it and no configuration is never started, and one
//! with both starts refused the built-in file and command tools
//! ([`worker_broker`]).
//!
//! The token's value is never in the prompt, an environment variable's
//! value, an event or a log: the configuration names the file, and the
//! events carry the claims' `jti`, capabilities and `exp` only.
//!
//! [Broker]: ../../docs/design/broker.md

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

use crate::domain::{RunId, TaskRun, broker_usage::ToolUsage};

/// The broker's dir in a run's dir.
pub const RUN_BROKER_DIR: &str = "broker";
/// The MCP configuration in [`RUN_BROKER_DIR`].
pub const MCP_CONFIG_FILE: &str = "mcp.json";
/// The name of the client's MCP server: its tools are
/// `mcp__dagq-broker__<name>`.
pub const MCP_SERVER: &str = "dagq-broker";
/// What the worker's settings allow of the server: all its tools.
pub const MCP_TOOLS: &str = "mcp__dagq-broker";
/// The client's environment: the broker's URL and the token's file.
pub const URL_ENV: &str = "DAGQ_BROKER_URL";
pub const TOKEN_FILE_ENV: &str = "DAGQ_BROKER_TOKEN_FILE";
/// The receipt the client's `write_receipt` writes (`required` only).
pub const RECEIPT_FILE_ENV: &str = "DAGQ_RECEIPT_FILE";
/// The mark of a run whose worker must run on the broker's tools only
/// (`required`), in the run's dir beside [`RUN_BROKER_DIR`], which a revoke
/// removes: the mark stays when no token could be issued.
pub const REQUIRED_FILE: &str = "broker-required";
/// The prefix of the error of a worker that is not started because its
/// run is `required` and it could not be given the broker's tools.
pub const BROKER_REQUIRED_REFUSED: &str = "broker_required";

/// A worker not started because its run is `required` and it could not be
/// given the broker's tools (ADR-t838-1): never started without them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerRequiredRefused(pub String);

impl std::fmt::Display for BrokerRequiredRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{BROKER_REQUIRED_REFUSED}: {}", self.0)
    }
}

impl std::error::Error for BrokerRequiredRefused {}

impl BrokerRequiredRefused {
    /// Whether `error` is, or was caused by, such a refusal.
    pub fn is(error: &anyhow::Error) -> bool {
        error
            .chain()
            .any(|cause| cause.downcast_ref::<Self>().is_some())
    }
}
/// A token is issued again when less than this is left of it (4 hours).
pub const RENEW_BEFORE_SECS: u64 = 4 * 60 * 60;

/// `<run dir>/broker/mcp.json`.
pub fn mcp_config_path(run_dir: &Path) -> PathBuf {
    run_dir.join(RUN_BROKER_DIR).join(MCP_CONFIG_FILE)
}

/// The MCP configuration of `run_dir`'s worker, when the supervisor gave
/// it the broker's tools.
pub fn worker_mcp_config(run: &TaskRun) -> Option<PathBuf> {
    let path = mcp_config_path(Path::new(run.run_dir()?));
    path.is_file().then_some(path)
}

/// `<run dir>/broker-required`.
pub fn required_path(run_dir: &Path) -> PathBuf {
    run_dir.join(REQUIRED_FILE)
}

/// How `run`'s worker gets the broker's tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerBroker {
    /// None: it starts as before (`disabled`, or `preferred` without a
    /// token).
    None,
    /// `preferred`: the tools beside the built-in ones.
    Preferred(PathBuf),
    /// `required`: the tools instead of the built-in file and command
    /// tools.
    Required(PathBuf),
}

/// How `run`'s worker gets the broker's tools, from what the supervisor
/// left in its dir: a run marked `required` without its configuration is
/// an error starting with [`BROKER_REQUIRED_REFUSED`], and its worker is
/// never started with the built-in tools instead (fail closed).
pub fn worker_broker(run: &TaskRun) -> anyhow::Result<WorkerBroker> {
    let Some(run_dir) = run.run_dir().map(Path::new) else {
        return Ok(WorkerBroker::None);
    };
    let config = worker_mcp_config(run);
    // Whatever is at the mark's path marks the run: a mark the supervisor
    // could not write as a file still refuses the built-in tools.
    if !required_path(run_dir).exists() {
        return Ok(config.map_or(WorkerBroker::None, WorkerBroker::Preferred));
    }
    match config {
        Some(config) => Ok(WorkerBroker::Required(config)),
        None => Err(BrokerRequiredRefused(format!(
            "[broker] mode = \"required\" and run {} has no MCP configuration of the broker; its \
worker is not started with the built-in tools instead",
            run.id()
        ))
        .into()),
    }
}

/// The configuration Claude Code's `--mcp-config` reads: the client's
/// `mcp` on the broker's port, with the token's file (never its value),
/// and with `required` the run's receipt for `write_receipt`.
pub fn mcp_config(client: &Path, port: u16, token_file: &Path, receipt: Option<&Path>) -> Value {
    let mut env = json!({
        URL_ENV: format!("http://127.0.0.1:{port}"),
        TOKEN_FILE_ENV: token_file,
    });
    if let Some(receipt) = receipt {
        env[RECEIPT_FILE_ENV] = json!(receipt);
    }
    json!({
        "mcpServers": {
            MCP_SERVER: {
                "command": client,
                "args": ["mcp"],
                "env": env,
            }
        }
    })
}

/// Where a run's token goes: the broker's port and the client to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub client: PathBuf,
    pub port: u16,
    /// The run's receipt, which the client's `write_receipt` writes:
    /// `required` only (ADR-t838-1).
    pub receipt: Option<PathBuf>,
}

/// What an issue recorded: never the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedRun {
    pub jti: String,
    pub capabilities: Vec<String>,
    pub exp: u64,
}

impl IssuedRun {
    /// The payload of `broker_token_issued`.
    pub fn payload(&self) -> Value {
        json!({"jti": self.jti, "capabilities": self.capabilities, "exp": self.exp})
    }
}

/// A token some run holds now: an active mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldToken {
    pub jti: String,
    pub run_id: String,
    /// The `exp` of the run's token file, when it can be read.
    pub exp: Option<u64>,
}

/// Issues and revokes the runs' tokens of one queue.
pub trait RunTokens: Send + Sync {
    /// Issue `run`'s token now (UNIX seconds `now`), mark it active, put
    /// it in the run's token file and write the run's MCP configuration
    /// for `grant`.
    fn issue(&self, run: &TaskRun, grant: &Grant, now: u64) -> Result<IssuedRun>;
    /// Revoke every token of `run`: remove its active marks, its token
    /// file and `<run dir>/broker`. The `jti`s revoked.
    fn revoke(&self, run: &RunId, run_dir: Option<&Path>) -> Result<Vec<String>>;
    /// Remove the active mark `jti` only: the older token of a run whose
    /// token was issued again.
    fn retire(&self, jti: &str) -> Result<()>;
    /// The tokens held now.
    fn held(&self) -> Result<Vec<HeldToken>>;
    /// The runs with a token file, whether a mark names it or not.
    fn token_files(&self) -> Result<Vec<String>>;
    /// `run`'s calls through the broker (its audit lines) and around it
    /// (the built-in tools its dir's
    /// [`DIRECT_TOOLS_LOG`](crate::domain::broker_usage::DIRECT_TOOLS_LOG)
    /// lists), from the day of the run's start.
    fn usage(&self, run: &TaskRun) -> Result<ToolUsage>;
    /// Whether a token could be issued now for a run of the repository
    /// at `repository`: the signing key is there or can be made, the token
    /// files' dir can be written, and the repository names the committer
    /// the token carries. `required` claims and resumes only when it can
    /// (ADR-t838-1); what fails for one run alone is its grant's.
    fn ready(&self, repository: &Path) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configuration_names_the_token_file_and_not_a_token() {
        let config = mcp_config(
            Path::new("/bin/dagq-broker-client"),
            8750,
            Path::new("/q/broker/tokens/r"),
            None,
        );
        assert_eq!(
            config,
            json!({"mcpServers": {"dagq-broker": {
                "command": "/bin/dagq-broker-client",
                "args": ["mcp"],
                "env": {
                    "DAGQ_BROKER_URL": "http://127.0.0.1:8750",
                    "DAGQ_BROKER_TOKEN_FILE": "/q/broker/tokens/r",
                },
            }}})
        );
        assert_eq!(
            mcp_config_path(Path::new("/r")),
            Path::new("/r/broker/mcp.json")
        );
        // `required` names the run's receipt too, for `write_receipt`.
        let config = mcp_config(
            Path::new("/c"),
            1,
            Path::new("/t"),
            Some(Path::new("/r/receipt.json")),
        );
        assert_eq!(
            config["mcpServers"]["dagq-broker"]["env"]["DAGQ_RECEIPT_FILE"],
            "/r/receipt.json"
        );
        let issued = IssuedRun {
            jti: "j".into(),
            capabilities: vec!["fs.read".into()],
            exp: 3,
        };
        assert_eq!(
            issued.payload(),
            json!({"jti": "j", "capabilities": ["fs.read"], "exp": 3})
        );
    }
}
