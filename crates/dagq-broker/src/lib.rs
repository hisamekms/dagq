//! The dagq resource broker, `dagq-broker` ([Broker], ADR-t827-1): the HTTP
//! server that does fs, process and git on behalf of a run, behind the run's
//! token. For now the binary answers `--version` and prints the health
//! answer; the server, the backends and the audit come in the next tasks.
//!
//! [Broker]: https://github.com/hisamekms/dagq/blob/main/docs/design/broker.md

use std::io::Write;

use dagq_broker_protocol::HealthResponse;

/// The name of the binary.
pub const NAME: &str = "dagq-broker";

/// The build this server names itself by, in `--version` and the health
/// answer.
pub const BUILD: &str = env!("CARGO_PKG_VERSION");

/// What a healthy server answers on `GET /v1/health`.
pub fn health() -> HealthResponse {
    HealthResponse::ok(BUILD)
}

/// Run the command line `args` (without the program name), writing to `out`.
/// `Err` is the message for stderr, and the exit status is then 2.
pub fn run(args: &[String], out: &mut impl Write) -> Result<(), String> {
    let write = |out: &mut dyn Write, text: String| {
        writeln!(out, "{text}").map_err(|error| format!("{NAME}: write the output: {error}"))
    };
    match args {
        [flag] if flag == "--version" || flag == "-V" => write(out, format!("{NAME} {BUILD}")),
        [command] if command == "health" => {
            let json = serde_json::to_string(&health())
                .map_err(|error| format!("{NAME}: serialize the health answer: {error}"))?;
            write(out, json)
        }
        [flag] if flag == "--help" || flag == "-h" => write(out, usage()),
        _ => Err(format!("{NAME}: unknown arguments {args:?}\n{}", usage())),
    }
}

fn usage() -> String {
    format!("Usage: {NAME} --version\n       {NAME} health    print the health answer as JSON")
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
            assert_eq!(out, format!("dagq-broker {BUILD}\n"));
        }
    }

    #[test]
    fn health_prints_the_health_answer() {
        let (result, out) = run_with(&["health"]);
        assert_eq!(result, Ok(()));
        let read: HealthResponse = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(read, health());
        assert_eq!(read.status, "ok");
    }

    #[test]
    fn help_and_unknown_arguments() {
        let (result, out) = run_with(&["--help"]);
        assert_eq!(result, Ok(()));
        assert!(out.starts_with("Usage: dagq-broker"));
        let (result, out) = run_with(&["serve", "--bind", "0.0.0.0:1"]);
        assert!(result.unwrap_err().contains("unknown arguments"));
        assert!(out.is_empty());
        assert!(run_with(&[]).0.is_err());
    }
}
