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
//! The token's value is never in the prompt, an environment variable's
//! value, an event or a log: the configuration names the file, and the
//! events carry the claims' `jti`, capabilities and `exp` only.
//!
//! [Broker]: ../../docs/design/broker.md

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

use crate::domain::{RunId, TaskRun};

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

/// The configuration Claude Code's `--mcp-config` reads: the client's
/// `mcp` on the broker's port, with the token's file (never its value).
pub fn mcp_config(client: &Path, port: u16, token_file: &Path) -> Value {
    json!({
        "mcpServers": {
            MCP_SERVER: {
                "command": client,
                "args": ["mcp"],
                "env": {
                    URL_ENV: format!("http://127.0.0.1:{port}"),
                    TOKEN_FILE_ENV: token_file,
                },
            }
        }
    })
}

/// Where a run's token goes: the broker's port and the client to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub client: PathBuf,
    pub port: u16,
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
