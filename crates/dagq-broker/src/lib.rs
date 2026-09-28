//! The dagq resource broker, `dagq-broker` ([Broker], ADR-t827-1): the HTTP
//! server that does fs, process and git on behalf of a run, behind the run's
//! token. `serve` listens on loopback, answers health without a token,
//! refuses everything else without a valid, active token (default deny) and
//! writes an audit line per request. The fs backend is
//! [`backends::fs::FsBackend`], the process backend
//! [`backends::process::ProcessBackend`] and the git backend
//! [`backends::git::GitBackend`].
//!
//! [Broker]: https://github.com/hisamekms/dagq/blob/main/docs/design/broker.md

pub mod audit;
pub mod backend;
pub mod backends;
pub mod config;
pub mod http;
pub mod server;

use std::io::Write;
use std::sync::Arc;

use dagq_broker_protocol::HealthResponse;

use crate::backend::Backends;
use crate::backends::fs::FsBackend;
use crate::backends::git::GitBackend;
use crate::backends::process::ProcessBackend;
use crate::config::Config;
use crate::server::Server;

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
/// `Err` is the message for stderr, and the exit status is then 2. `serve`
/// returns only on an error.
pub fn run(args: &[String], out: &mut impl Write) -> Result<(), String> {
    let write = |out: &mut dyn Write, text: String| {
        writeln!(out, "{text}")
            .and_then(|()| out.flush())
            .map_err(|error| format!("{NAME}: write the output: {error}"))
    };
    match args {
        [flag] if flag == "--version" || flag == "-V" => write(out, format!("{NAME} {BUILD}")),
        [command] if command == "health" => {
            let json = serde_json::to_string(&health())
                .map_err(|error| format!("{NAME}: serialize the health answer: {error}"))?;
            write(out, json)
        }
        [command, rest @ ..] if command == "serve" => {
            let config = Config::parse(rest).map_err(|error| format!("{NAME} serve: {error}"))?;
            let backends = Backends {
                fs: Arc::new(FsBackend::new(config.roots.clone())),
                process: Arc::new(ProcessBackend::new(config.roots.clone())),
                git: Arc::new(GitBackend::new(config.roots.clone())),
            };
            let server = Server::bind(&config, backends)
                .map_err(|error| format!("{NAME} serve: {error}"))?;
            let listening = server
                .local_addr()
                .map_err(|error| format!("{NAME} serve: {error}"))?;
            write(
                out,
                serde_json::json!({ "listening": listening.to_string(), "build": BUILD })
                    .to_string(),
            )?;
            server.serve();
            Ok(())
        }
        [flag] if flag == "--help" || flag == "-h" => write(out, usage()),
        _ => Err(format!("{NAME}: unknown arguments {args:?}\n{}", usage())),
    }
}

fn usage() -> String {
    format!(
        "Usage: {NAME} --version
       {NAME} health    print the health answer as JSON
       {NAME} serve --key FILE --active DIR --audit DIR --root DIR... [--listen ADDR:PORT]
                    [--container] [--exec-timeout-secs N] [--exec-max-timeout-secs N]
                    [--output-limit-bytes N] [--fs-limit-bytes N] [--exec-allow NAME]... [--exec-env NAME]...
                 listen (127.0.0.1:8750 by default; loopback only outside the container)
                 and print {{\"listening\":ADDR,\"build\":BUILD}} once bound"
    )
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
        assert!(out.contains("serve --key"));
        let (result, out) = run_with(&["frobnicate"]);
        assert!(result.unwrap_err().contains("unknown arguments"));
        assert!(out.is_empty());
        assert!(run_with(&[]).0.is_err());
    }

    #[test]
    fn serve_refuses_a_bad_configuration_before_listening() {
        let (result, out) = run_with(&["serve", "--listen", "0.0.0.0:0"]);
        let error = result.unwrap_err();
        assert!(error.starts_with("dagq-broker serve: "), "{error}");
        assert!(error.contains("not a loopback address"), "{error}");
        assert!(out.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("key");
        let (result, _) = run_with(&[
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--key",
            missing.to_str().unwrap(),
            "--active",
            "/a",
            "--audit",
            "/b",
            "--root",
            "/c",
        ]);
        assert!(result.unwrap_err().contains("read the key"));
    }
}
