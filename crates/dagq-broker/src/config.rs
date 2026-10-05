//! [`Config`]: what `dagq-broker serve` reads from its command line.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use dagq_broker_protocol::package;

/// The address the server listens on when `--listen` is not given.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8750";

/// The default of `--exec-timeout-secs`.
pub const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 60;

/// The default of `--exec-max-timeout-secs`.
pub const DEFAULT_EXEC_MAX_TIMEOUT_SECS: u64 = 300;

/// The default of `--output-limit-bytes`, 1 MiB.
pub const DEFAULT_OUTPUT_LIMIT_BYTES: u64 = 1024 * 1024;

/// The default of `--fs-limit-bytes`, 4 MiB: the most a `fs.read` answers
/// and a `fs.write` or `fs.edit` writes.
pub const DEFAULT_FS_LIMIT_BYTES: u64 = 4 * 1024 * 1024;

/// Programs that run other programs: with one of them in `--exec-allow`,
/// `process.exec` can run `git` or leave the workspace's cwd through it,
/// since the allowlist looks at `argv[0]` only. `serve` warns about them
/// (compared by basename) and does not refuse them: the throwaway
/// repository's verification needs `sh` (ADR-t827-3 decision 9).
pub const INTERPRETERS: &[&str] = &[
    // shells
    "sh", "bash", "zsh", "dash", "ksh", "mksh", "ash", "busybox", "fish", "csh", "tcsh",
    // run a command given as arguments
    "env", "xargs", "find", "nice", "nohup", "timeout", "time", "stdbuf", "setsid", "flock",
    "chroot", "sudo", "doas", "su", "script", "watch", "parallel", "make",
    // language interpreters
    "python", "python2", "python3", "perl", "ruby", "node", "deno", "bun", "php", "lua", "tclsh",
    "awk", "gawk", "mawk", "nawk",
];

/// The prefixes of env names that change what a child loads (the dynamic
/// loader's `LD_*` and macOS's `DYLD_*`), compared case-sensitively.
/// `--exec-env` refuses them: they have no legitimate use there.
pub const LOADER_ENV_PREFIXES: &[&str] = &["LD_", "DYLD_"];

/// The limits the backends enforce on the server's side (ADR-t827-2
/// decision 8), and what `process.exec` may run and receive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub exec_timeout_secs: u64,
    pub exec_max_timeout_secs: u64,
    pub output_limit_bytes: u64,
    /// The most bytes of content `fs.read` answers and `fs.write` /
    /// `fs.edit` write (and `fs.edit` reads).
    pub fs_limit_bytes: u64,
    /// The basenames `argv[0]` may have; empty refuses every exec.
    pub exec_allow: Vec<String>,
    /// The env names a request may pass to its process.
    pub exec_env: Vec<String>,
    /// The commands `package.install` may run, by name (`--package`); empty
    /// refuses every install.
    pub packages: BTreeMap<String, Vec<String>>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            exec_timeout_secs: DEFAULT_EXEC_TIMEOUT_SECS,
            exec_max_timeout_secs: DEFAULT_EXEC_MAX_TIMEOUT_SECS,
            output_limit_bytes: DEFAULT_OUTPUT_LIMIT_BYTES,
            fs_limit_bytes: DEFAULT_FS_LIMIT_BYTES,
            exec_allow: Vec::new(),
            exec_env: Vec::new(),
            packages: BTreeMap::new(),
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
    /// What `serve` says on stderr at startup, one line each (the
    /// interpreters in `--exec-allow`); it names the program only.
    pub warnings: Vec<String>,
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
                "--fs-limit-bytes" => limits.fs_limit_bytes = number(flag, &value)?,
                "--exec-allow" => limits.exec_allow.push(value),
                "--exec-env" => limits.exec_env.push(value),
                "--package" => {
                    let (name, argv) = package_command(&value)?;
                    if limits.packages.insert(name.clone(), argv).is_some() {
                        return Err(format!("`--package {name}` is given twice"));
                    }
                }
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
        if let Some(name) = limits.exec_env.iter().find(|name| {
            LOADER_ENV_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
        }) {
            return Err(format!(
                "`--exec-env {name}` changes what the child loads; `LD_*` and `DYLD_*` names are refused"
            ));
        }
        let mut warnings: Vec<String> = limits
            .exec_allow
            .iter()
            .filter(|name| is_interpreter(name))
            .map(|name| {
                format!(
                    "warning: `--exec-allow {name}` runs other programs; process.exec can run git and leave the workspace through it"
                )
            })
            .collect();
        warnings.extend(package_warnings(&limits.packages));
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
            warnings,
        })
    }
}

/// The value of `--package`, `NAME=<argv as a JSON array of strings>`,
/// checked as `dagq` checks `[broker.package]`.
fn package_command(value: &str) -> Result<(String, Vec<String>), String> {
    let (name, argv) = value
        .split_once('=')
        .ok_or_else(|| format!("`--package {value}` is not NAME=[\"PROGRAM\", ...]"))?;
    let argv: Vec<String> = serde_json::from_str(argv)
        .map_err(|_| format!("`--package {name}`: the argv is not a JSON array of strings"))?;
    package::check_command(name, &argv)?;
    Ok((name.to_owned(), argv))
}

/// One warning for each package command whose program is one of
/// [`INTERPRETERS`]; a command without a program is not read past its end
/// (`--package` refuses it, but [`Limits`] can be built without it).
fn package_warnings(packages: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    packages
        .iter()
        .filter_map(|(name, argv)| {
            let program = argv.first().filter(|program| is_interpreter(program))?;
            Some(format!(
                "warning: `--package {name}` runs {program}, which runs other programs"
            ))
        })
        .collect()
}

/// Whether `name`'s basename is one of [`INTERPRETERS`].
pub fn is_interpreter(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    INTERPRETERS.contains(&base)
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
        assert_eq!(config.limits.fs_limit_bytes, 4_194_304);
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
            "--fs-limit-bytes",
            "7",
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
                fs_limit_bytes: 7,
                exec_allow: vec!["ls".to_owned(), "cat".to_owned()],
                exec_env: vec!["LANG".to_owned()],
                packages: BTreeMap::new(),
            }
        );
        assert_eq!(config.roots.len(), 2);
    }

    #[test]
    fn warns_about_interpreters_in_the_allowlist_by_name() {
        let config = with(&[
            "--exec-allow",
            "sh",
            "--exec-allow",
            "ls",
            "--exec-allow",
            "/usr/bin/python3",
            "--exec-allow",
            "xargs",
        ])
        .unwrap();
        assert_eq!(config.warnings.len(), 3, "{:?}", config.warnings);
        for (warning, name) in config
            .warnings
            .iter()
            .zip(["sh", "/usr/bin/python3", "xargs"])
        {
            assert!(warning.starts_with("warning: "), "{warning}");
            assert!(
                warning.contains(&format!("`--exec-allow {name}`")),
                "{warning}"
            );
        }
        for name in INTERPRETERS {
            assert!(is_interpreter(name), "{name}");
        }
    }

    #[test]
    fn no_warning_for_programs_that_run_nothing_else() {
        let config = with(&[
            "--exec-allow",
            "ls",
            "--exec-allow",
            "cat",
            "--exec-allow",
            "grep",
            "--exec-allow",
            "shasum",
        ])
        .unwrap();
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        assert!(with(&[]).unwrap().warnings.is_empty());
        assert!(!is_interpreter("shell-check"));
        assert!(!is_interpreter("SH"));
    }

    #[test]
    fn reads_the_package_commands_by_name() {
        let config = with(&[
            "--package",
            r#"cargo-fetch=["cargo","fetch","--locked"]"#,
            "--package",
            r#"npm-install=["npm","install"]"#,
        ])
        .unwrap();
        let argv = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            config.limits.packages,
            BTreeMap::from([
                (
                    "cargo-fetch".to_owned(),
                    argv(&["cargo", "fetch", "--locked"])
                ),
                ("npm-install".to_owned(), argv(&["npm", "install"])),
            ])
        );
        assert!(config.warnings.is_empty());
        assert!(with(&[]).unwrap().limits.packages.is_empty());
        let config = with(&["--package", r#"script=["sh","-c","true"]"#]).unwrap();
        assert_eq!(
            config.warnings,
            ["warning: `--package script` runs sh, which runs other programs"]
        );
        let cases: [(&str, &str); 6] = [
            ("cargo-fetch", "not NAME="),
            ("x=cargo fetch", "not a JSON array"),
            (r#"x=["cargo",1]"#, "not a JSON array"),
            (r#"x=[]"#, "argv is empty"),
            (r#"x=["git","fetch"]"#, "git operations"),
            (r#"a b=["cargo"]"#, "the name"),
        ];
        for (value, expected) in cases {
            let error = with(&["--package", value]).unwrap_err();
            assert!(error.contains(expected), "{value}: {error}");
        }
        let packages = BTreeMap::from([
            ("empty".to_owned(), Vec::new()),
            ("script".to_owned(), argv(&["sh", "-c", "true"])),
            ("fetch".to_owned(), argv(&["cargo", "fetch"])),
        ]);
        assert_eq!(
            package_warnings(&packages),
            ["warning: `--package script` runs sh, which runs other programs"]
        );
        let error = with(&["--package", r#"x=["a"]"#, "--package", r#"x=["b"]"#]).unwrap_err();
        assert!(error.contains("given twice"), "{error}");
    }

    #[test]
    fn refuses_loader_env_names_by_name() {
        for name in [
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "LD_AUDIT",
            "DYLD_INSERT_LIBRARIES",
            "DYLD_LIBRARY_PATH",
        ] {
            let error = with(&["--exec-env", "LANG", "--exec-env", name]).unwrap_err();
            assert!(error.contains(&format!("`--exec-env {name}`")), "{error}");
            assert!(error.contains("refused"), "{error}");
        }
        // Case-sensitive prefixes: these are not the loader's names.
        let config = with(&["--exec-env", "ld_preload", "--exec-env", "OLD_PATH"]).unwrap();
        assert_eq!(config.limits.exec_env, ["ld_preload", "OLD_PATH"]);
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
