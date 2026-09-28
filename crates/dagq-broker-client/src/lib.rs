//! The client of the dagq resource broker, `dagq-broker-client` ([Broker],
//! ADR-t827-1): the worker's MCP server (`mcp`) and a CLI for a person's
//! diagnosis, tests and smokes. For now the binary answers `--version`; the
//! HTTP client, `health` and the MCP server come in the next tasks.
//!
//! [Broker]: https://github.com/hisamekms/dagq/blob/main/docs/design/broker.md

use std::io::Write;

/// The name of the binary.
pub const NAME: &str = "dagq-broker-client";

/// The build this client names itself by in `--version`; dagq uses the
/// client only when it is its own build.
pub const BUILD: &str = env!("CARGO_PKG_VERSION");

/// The protocol version this client speaks.
pub const PROTOCOL_VERSION: u32 = dagq_broker_protocol::PROTOCOL_VERSION;

/// Run the command line `args` (without the program name), writing to `out`.
/// `Err` is the message for stderr, and the exit status is then 2.
pub fn run(args: &[String], out: &mut impl Write) -> Result<(), String> {
    let text = match args {
        [flag] if flag == "--version" || flag == "-V" => format!("{NAME} {BUILD}"),
        [flag] if flag == "--help" || flag == "-h" => usage(),
        _ => return Err(format!("{NAME}: unknown arguments {args:?}\n{}", usage())),
    };
    writeln!(out, "{text}").map_err(|error| format!("{NAME}: write the output: {error}"))
}

fn usage() -> String {
    format!("Usage: {NAME} --version")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_with(args: &[&str]) -> (Result<(), String>, String) {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let mut out = Vec::new();
        let result = run(&args, &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn version_names_the_build() {
        for flag in ["--version", "-V"] {
            let (result, out) = run_with(&[flag]);
            assert_eq!(result, Ok(()));
            assert_eq!(out, format!("dagq-broker-client {BUILD}\n"));
        }
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn help_and_unknown_arguments() {
        let (result, out) = run_with(&["-h"]);
        assert_eq!(result, Ok(()));
        assert!(out.starts_with("Usage: dagq-broker-client"));
        let (result, out) = run_with(&["mcp"]);
        assert!(result.unwrap_err().contains("unknown arguments"));
        assert!(out.is_empty());
    }
}
