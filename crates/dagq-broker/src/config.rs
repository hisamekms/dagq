//! [`Config`]: what `dagq-broker serve` reads from its command line.

use std::net::SocketAddr;
use std::path::PathBuf;

/// The address the server listens on when `--listen` is not given.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8750";

/// The default of `--exec-timeout-secs`.
pub const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 60;

/// The default of `--exec-max-timeout-secs`.
pub const DEFAULT_EXEC_MAX_TIMEOUT_SECS: u64 = 300;

/// The default of `--output-limit-bytes`, 1 MiB.
pub const DEFAULT_OUTPUT_LIMIT_BYTES: u64 = 1024 * 1024;

/// The limits the backends enforce on the server's side (ADR-t827-2
/// decision 8), and what `process.exec` may run and receive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub exec_timeout_secs: u64,
    pub exec_max_timeout_secs: u64,
    pub output_limit_bytes: u64,
    /// The basenames `argv[0]` may have; empty refuses every exec.
    pub exec_allow: Vec<String>,
    /// The env names a request may pass to its process.
    pub exec_env: Vec<String>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            exec_timeout_secs: DEFAULT_EXEC_TIMEOUT_SECS,
            exec_max_timeout_secs: DEFAULT_EXEC_MAX_TIMEOUT_SECS,
            output_limit_bytes: DEFAULT_OUTPUT_LIMIT_BYTES,
            exec_allow: Vec::new(),
            exec_env: Vec::new(),
        }
    }
}

/// The server's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Where to listen: loopback only, unless `container`.
    pub listen: SocketAddr,
    /// Running in the broker's container, where the address is the
    /// container's own interface and the host publishes the port on
    /// `127.0.0.1` only (ADR-t827-2 decision 1).
    pub container: bool,
    /// The queue's signing key, `<queue dir>/broker/key`.
    pub key: PathBuf,
    /// The active marks, `<queue dir>/broker/active`.
    pub active: PathBuf,
    /// Where the audit goes, `<queue dir>/broker/audit`.
    pub audit: PathBuf,
    /// The mounted roots (`<queue dir>/runs`): a token's workspace must be
    /// under one of them.
    pub roots: Vec<PathBuf>,
    pub limits: Limits,
}

impl Config {
    /// Read the arguments of `serve` (after `serve`).
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut listen = DEFAULT_LISTEN.to_owned();
        let mut container = false;
        let (mut key, mut active, mut audit) = (None, None, None);
        let mut roots = Vec::new();
        let mut limits = Limits::default();
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            if flag == "--container" {
                container = true;
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| format!("`{flag}` needs a value"))?
                .clone();
            match flag.as_str() {
                "--listen" => listen = value,
                "--key" => key = Some(PathBuf::from(value)),
                "--active" => active = Some(PathBuf::from(value)),
                "--audit" => audit = Some(PathBuf::from(value)),
                "--root" => roots.push(PathBuf::from(value)),
                "--exec-timeout-secs" => limits.exec_timeout_secs = number(flag, &value)?,
                "--exec-max-timeout-secs" => limits.exec_max_timeout_secs = number(flag, &value)?,
                "--output-limit-bytes" => limits.output_limit_bytes = number(flag, &value)?,
                "--exec-allow" => limits.exec_allow.push(value),
                "--exec-env" => limits.exec_env.push(value),
                _ => return Err(format!("unknown flag `{flag}`")),
            }
        }
        let listen: SocketAddr = listen
            .parse()
            .map_err(|_| format!("`--listen {listen}` is not an address and port"))?;
        if !container && !listen.ip().is_loopback() {
            return Err(format!(
                "`--listen {listen}` is not a loopback address; the broker listens on 127.0.0.1 only (outside its container)"
            ));
        }
        if limits.exec_timeout_secs == 0 || limits.exec_timeout_secs > limits.exec_max_timeout_secs
        {
            return Err(
                "`--exec-timeout-secs` must be at least 1 and at most `--exec-max-timeout-secs`"
                    .to_owned(),
            );
        }
        if roots.is_empty() {
            return Err("`--root` is required (the mounted runs dir)".to_owned());
        }
        if let Some(root) = roots.iter().find(|root| !root.is_absolute()) {
            return Err(format!("`--root {}` is not absolute", root.display()));
        }
        Ok(Self {
            listen,
            container,
            key: key.ok_or("`--key` is required")?,
            active: active.ok_or("`--active` is required")?,
            audit: audit.ok_or("`--audit` is required")?,
            roots,
            limits,
        })
    }
}

fn number(flag: &str, value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("`{flag} {value}` is not a number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config, String> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        Config::parse(&args)
    }

    const REQUIRED: [&str; 8] = [
        "--key",
        "/q/key",
        "--active",
        "/q/active",
        "--audit",
        "/q/audit",
        "--root",
        "/q/runs",
    ];

    fn with(extra: &[&str]) -> Result<Config, String> {
        let mut args = REQUIRED.to_vec();
        args.extend_from_slice(extra);
        parse(&args)
    }

    #[test]
    fn defaults_to_loopback_and_the_documented_limits() {
        let config = with(&[]).unwrap();
        assert_eq!(config.listen, "127.0.0.1:8750".parse().unwrap());
        assert!(config.listen.ip().is_loopback());
        assert!(!config.container);
        assert_eq!(config.key, PathBuf::from("/q/key"));
        assert_eq!(config.active, PathBuf::from("/q/active"));
        assert_eq!(config.audit, PathBuf::from("/q/audit"));
        assert_eq!(config.roots, [PathBuf::from("/q/runs")]);
        assert_eq!(config.limits, Limits::default());
        assert_eq!(config.limits.exec_timeout_secs, 60);
        assert_eq!(config.limits.exec_max_timeout_secs, 300);
        assert_eq!(config.limits.output_limit_bytes, 1_048_576);
        assert!(config.limits.exec_allow.is_empty());
    }

    #[test]
    fn refuses_a_non_loopback_address_outside_the_container() {
        for listen in ["0.0.0.0:8750", "192.168.1.10:0", "[::]:0", "10.0.0.1:1"] {
            let error = with(&["--listen", listen]).unwrap_err();
            assert!(error.contains("not a loopback address"), "{error}");
        }
        for listen in ["127.0.0.1:0", "[::1]:0", "127.0.0.2:9"] {
            assert!(with(&["--listen", listen]).is_ok(), "{listen}");
        }
        let config = with(&["--container", "--listen", "0.0.0.0:8750"]).unwrap();
        assert!(config.container);
        assert_eq!(config.listen, "0.0.0.0:8750".parse().unwrap());
    }

    #[test]
    fn reads_the_limits_and_the_allowlists() {
        let config = with(&[
            "--exec-timeout-secs",
            "5",
            "--exec-max-timeout-secs",
            "10",
            "--output-limit-bytes",
            "99",
            "--exec-allow",
            "ls",
            "--exec-allow",
            "cat",
            "--exec-env",
            "LANG",
            "--root",
            "/other/runs",
        ])
        .unwrap();
        assert_eq!(
            config.limits,
            Limits {
                exec_timeout_secs: 5,
                exec_max_timeout_secs: 10,
                output_limit_bytes: 99,
                exec_allow: vec!["ls".to_owned(), "cat".to_owned()],
                exec_env: vec!["LANG".to_owned()],
            }
        );
        assert_eq!(config.roots.len(), 2);
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        let cases: [(&[&str], &str); 8] = [
            (&["--listen", "localhost"], "not an address"),
            (&["--exec-timeout-secs", "x"], "not a number"),
            (&["--exec-timeout-secs", "0"], "at least 1"),
            (&["--exec-timeout-secs", "301"], "at most"),
            (&["--bind", "127.0.0.1:0"], "unknown flag `--bind`"),
            (&["--key"], "needs a value"),
            (&["--root", "relative"], "not absolute"),
            (&["--exec-allow"], "needs a value"),
        ];
        for (extra, expected) in cases {
            let error = with(extra).unwrap_err();
            assert!(error.contains(expected), "{extra:?}: {error}");
        }
        for missing in ["--key", "--active", "--audit", "--root"] {
            let mut args = Vec::new();
            for pair in REQUIRED.chunks(2) {
                if pair[0] != missing {
                    args.extend_from_slice(pair);
                }
            }
            let error = parse(&args).unwrap_err();
            assert!(error.contains(missing), "{missing}: {error}");
        }
    }
}
