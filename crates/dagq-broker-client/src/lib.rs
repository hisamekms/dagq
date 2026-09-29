//! The client of the dagq resource broker, `dagq-broker-client` ([Broker],
//! ADR-t827-1): [`BrokerClient`], the typed calls of fs, process and git
//! that return the broker's structured error as it answered, and the CLI
//! for a person's diagnosis, tests and smokes ([`cli`]), and the worker's
//! MCP server on stdio ([`mcp`], `dagq-broker-client mcp`). The crate depends on
//! `dagq-broker-protocol` only, not on dagq, so a worker's container needs
//! no dagq (ADR-t827-1 decision 1).
//!
//! [Broker]: https://github.com/hisamekms/dagq/blob/main/docs/design/broker.md

pub mod cli;
pub mod client;
pub mod http;
pub mod mcp;

pub use client::{BrokerClient, ClientError, Endpoint, TOKEN_FILE_ENV, URL_ENV};

/// The name of the binary.
pub const NAME: &str = "dagq-broker-client";

/// The build this client names itself by in `--version`; dagq uses the
/// client only when it is its own build.
pub const BUILD: &str = env!("CARGO_PKG_VERSION");

/// The protocol version this client speaks.
pub const PROTOCOL_VERSION: u32 = dagq_broker_protocol::PROTOCOL_VERSION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_speaks_protocol_1() {
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(NAME, "dagq-broker-client");
    }
}
