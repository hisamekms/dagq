//! The diagnosis CLI: one broker operation per invocation, the answer as
//! JSON on stdout. The endpoint and the token file come from `--url` and
//! `--token-file`, or `DAGQ_BROKER_URL` and `DAGQ_BROKER_TOKEN_FILE`; the
//! token's value is never an argument and never printed.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;

use dagq_broker_protocol::{ErrorBody, fs as fs_ops, git, process};
use serde::Serialize;

use crate::client::{BrokerClient, ClientError, Endpoint, TOKEN_FILE_ENV, URL_ENV};
use crate::{BUILD, NAME};

/// The exit status when the command ran and the broker answered.
pub const EXIT_OK: i32 = 0;
/// The broker refused or failed the operation; its error body is on stderr.
pub const EXIT_REFUSED: i32 = 1;
/// The command line is wrong.
pub const EXIT_USAGE: i32 = 2;
/// The client could not ask: configuration, transport or protocol.
pub const EXIT_CLIENT: i32 = 3;

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub url: Option<String>,
    pub token_file: Option<PathBuf>,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Version,
    Help,
    Health {
        json: bool,
    },
    TokenInspect,
    FsRead(fs_ops::ReadRequest),
    FsList(fs_ops::ListRequest),
    /// `content` `None` reads stdin.
    FsWrite {
        path: String,
        content: Option<String>,
        create_dirs: bool,
    },
    FsEdit(fs_ops::EditRequest),
    Exec(process::ExecRequest),
    GitStatus,
    GitDiff(git::DiffRequest),
    GitLog(git::LogRequest),
    GitShow(git::ShowRequest),
    GitAdd(git::AddRequest),
    GitCommit(git::CommitRequest),
    GitRestore(git::RestoreRequest),
    /// The worker's MCP server on stdio ([`crate::mcp`]).
    Mcp,
}

/// Run the command line `args` (without the program name) with `env`,
/// `stdin`, `out` and `err`, and return the exit status.
pub fn run(
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let invocation = match parse(args) {
        Ok(invocation) => invocation,
        Err(message) => {
            let _ = writeln!(err, "{NAME}: {message}\n{}", usage());
            return EXIT_USAGE;
        }
    };
    if invocation.command == Command::Mcp {
        return mcp(invocation, env, stdin, out, err);
    }
    match execute(invocation, env, stdin) {
        Ok(text) => match writeln!(out, "{text}").and_then(|()| out.flush()) {
            Ok(()) => EXIT_OK,
            Err(error) => {
                let _ = writeln!(err, "{NAME}: write the output: {error}");
                EXIT_CLIENT
            }
        },
        Err(ClientError::Broker { error, .. }) => {
            let body =
                serde_json::to_string(&ErrorBody { error }).expect("an error body serializes");
            let _ = writeln!(err, "{body}");
            EXIT_REFUSED
        }
        Err(error) => {
            let _ = writeln!(err, "{NAME}: {error}");
            EXIT_CLIENT
        }
    }
}

/// Serve MCP until stdin ends. Without a usable URL the server does not
/// start (exit 3); a missing token file fails each call instead.
fn mcp(
    invocation: Invocation,
    env: &dyn Fn(&str) -> Option<String>,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let client = match client(&invocation, env) {
        Ok(client) => client,
        Err(error) => {
            let _ = writeln!(err, "{NAME}: {error}");
            return EXIT_CLIENT;
        }
    };
    // `required` names the run's receipt for `write_receipt` (ADR-t838-1).
    let receipt = env(crate::mcp::RECEIPT_FILE_ENV)
        .filter(|file| !file.is_empty())
        .map(PathBuf::from);
    match crate::mcp::serve_with(
        &client,
        receipt.as_deref(),
        &mut std::io::BufReader::new(stdin),
        out,
    ) {
        Ok(()) => EXIT_OK,
        Err(error) => {
            let _ = writeln!(err, "{NAME}: mcp: {error}");
            EXIT_CLIENT
        }
    }
}

/// The token file of `--token-file` or `DAGQ_BROKER_TOKEN_FILE`.
fn token_file(invocation: &Invocation, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    invocation.token_file.clone().or_else(|| {
        env(TOKEN_FILE_ENV)
            .filter(|file| !file.is_empty())
            .map(PathBuf::from)
    })
}

/// The client of `--url` or `DAGQ_BROKER_URL` with the token file.
fn client(
    invocation: &Invocation,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<BrokerClient, ClientError> {
    let url = invocation
        .url
        .clone()
        .or_else(|| env(URL_ENV).filter(|url| !url.is_empty()))
        .ok_or_else(|| ClientError::Config(format!("no --url and {URL_ENV} is not set")))?;
    Ok(BrokerClient::new(
        Endpoint::parse(&url)?,
        token_file(invocation, env),
    ))
}

fn execute(
    invocation: Invocation,
    env: &dyn Fn(&str) -> Option<String>,
    stdin: &mut dyn Read,
) -> Result<String, ClientError> {
    let token_file = token_file(&invocation, env);
    let client = client(&invocation, env);
    let client = || client.clone();
    match invocation.command {
        Command::Version => Ok(format!("{NAME} {BUILD}")),
        Command::Help => Ok(usage()),
        Command::Health { json } => {
            let health = client()?.health()?;
            Ok(if json {
                to_json(&health)
            } else {
                format!(
                    "{} build {} protocol {}",
                    health.status, health.build, health.protocol
                )
            })
        }
        Command::TokenInspect => {
            let file = token_file.ok_or_else(|| {
                ClientError::Config(format!("no --token-file and {TOKEN_FILE_ENV} is not set"))
            })?;
            let claims = crate::client::read_token_file(&file)?
                .unverified_claims()
                .map_err(|error| {
                    ClientError::Config(format!(
                        "the token file {} holds no token: {error}",
                        file.display()
                    ))
                })?;
            Ok(to_json(&claims))
        }
        Command::FsRead(request) => Ok(to_json(&client()?.fs_read(&request)?)),
        Command::FsList(request) => Ok(to_json(&client()?.fs_list(&request)?)),
        Command::FsWrite {
            path,
            content,
            create_dirs,
        } => {
            let client = client()?;
            let content = match content {
                Some(content) => content,
                None => {
                    let mut content = String::new();
                    stdin.read_to_string(&mut content).map_err(|error| {
                        ClientError::Config(format!("read the content from stdin: {error}"))
                    })?;
                    content
                }
            };
            let request = fs_ops::WriteRequest {
                path,
                content,
                create_dirs,
            };
            Ok(to_json(&client.fs_write(&request)?))
        }
        Command::FsEdit(request) => Ok(to_json(&client()?.fs_edit(&request)?)),
        Command::Exec(request) => Ok(to_json(&client()?.exec(&request)?)),
        Command::GitStatus => Ok(to_json(&client()?.git_status()?)),
        Command::GitDiff(request) => Ok(to_json(&client()?.git_diff(&request)?)),
        Command::GitLog(request) => Ok(to_json(&client()?.git_log(&request)?)),
        Command::GitShow(request) => Ok(to_json(&client()?.git_show(&request)?)),
        Command::GitAdd(request) => Ok(to_json(&client()?.git_add(&request)?)),
        Command::GitCommit(request) => Ok(to_json(&client()?.git_commit(&request)?)),
        Command::GitRestore(request) => Ok(to_json(&client()?.git_restore(&request)?)),
        Command::Mcp => unreachable!("mcp is served by `run`"),
    }
}

fn to_json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("a protocol value serializes")
}

/// The words of one command after its name: flags (with their values) and
/// positional words. `--` ends the flags.
struct Words<'a> {
    rest: std::slice::Iter<'a, String>,
    positional: Vec<String>,
    after_dashes: bool,
}

impl<'a> Words<'a> {
    fn new(rest: &'a [String]) -> Self {
        Self {
            rest: rest.iter(),
            positional: Vec::new(),
            after_dashes: false,
        }
    }

    /// The next flag, collecting positional words on the way.
    fn next_flag(&mut self) -> Option<&'a str> {
        for word in self.rest.by_ref() {
            if self.after_dashes {
                self.positional.push(word.clone());
            } else if word == "--" {
                self.after_dashes = true;
            } else if word.starts_with('-') && word.len() > 1 {
                return Some(word);
            } else {
                self.positional.push(word.clone());
            }
        }
        None
    }

    fn value(&mut self, flag: &str) -> Result<String, String> {
        self.rest
            .next()
            .cloned()
            .ok_or_else(|| format!("`{flag}` needs a value"))
    }

    fn number<T: std::str::FromStr>(&mut self, flag: &str) -> Result<T, String> {
        let value = self.value(flag)?;
        value
            .parse()
            .map_err(|_| format!("`{flag} {value}` is not a number"))
    }
}

fn unknown(flag: &str) -> String {
    format!("unknown flag `{flag}`")
}

/// Exactly one positional word, the path.
fn one_path(positional: Vec<String>) -> Result<String, String> {
    match <[String; 1]>::try_from(positional) {
        Ok([path]) => Ok(path),
        Err(words) => Err(format!("expected one path, got {words:?}")),
    }
}

fn no_positional(positional: Vec<String>) -> Result<(), String> {
    if positional.is_empty() {
        Ok(())
    } else {
        Err(format!("unexpected arguments {positional:?}"))
    }
}

/// Read the command line.
pub fn parse(args: &[String]) -> Result<Invocation, String> {
    let mut url = None;
    let mut token_file = None;
    let mut index = 0;
    while let Some(word) = args.get(index) {
        match word.as_str() {
            "--url" | "--token-file" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("`{word}` needs a value"))?
                    .clone();
                if word == "--url" {
                    url = Some(value);
                } else {
                    token_file = Some(PathBuf::from(value));
                }
                index += 2;
            }
            _ => break,
        }
    }
    let command = parse_command(&args[index..])?;
    Ok(Invocation {
        url,
        token_file,
        command,
    })
}

fn parse_command(args: &[String]) -> Result<Command, String> {
    let words: Vec<&str> = args.iter().take(2).map(String::as_str).collect();
    let (name, rest): (&str, &[String]) = match words.as_slice() {
        [] => return Err("no command".to_owned()),
        ["fs" | "git" | "token", sub, ..] => {
            let name = match (words[0], *sub) {
                ("fs", "read") => "fs read",
                ("fs", "list") => "fs list",
                ("fs", "write") => "fs write",
                ("fs", "edit") => "fs edit",
                ("git", "status") => "git status",
                ("git", "diff") => "git diff",
                ("git", "log") => "git log",
                ("git", "show") => "git show",
                ("git", "add") => "git add",
                ("git", "commit") => "git commit",
                ("git", "restore") => "git restore",
                ("token", "inspect") => "token inspect",
                (group, sub) => return Err(format!("unknown command `{group} {sub}`")),
            };
            (name, &args[2..])
        }
        [group @ ("fs" | "git" | "token")] => {
            return Err(format!("`{group}` needs a subcommand"));
        }
        [name, ..] => (*name, &args[1..]),
    };
    let mut words = Words::new(rest);
    let command = match name {
        "--version" | "-V" => Command::Version,
        "--help" | "-h" | "help" => Command::Help,
        "health" => {
            let mut json = false;
            while let Some(flag) = words.next_flag() {
                match flag {
                    "--json" => json = true,
                    _ => return Err(unknown(flag)),
                }
            }
            no_positional(words.positional)?;
            Command::Health { json }
        }
        "token inspect" | "git status" | "mcp" => {
            if let Some(flag) = words.next_flag() {
                return Err(unknown(flag));
            }
            no_positional(words.positional)?;
            if name == "git status" {
                Command::GitStatus
            } else if name == "mcp" {
                Command::Mcp
            } else {
                Command::TokenInspect
            }
        }
        "fs read" => {
            let (mut offset, mut limit) = (None, None);
            while let Some(flag) = words.next_flag() {
                match flag {
                    "--offset" => offset = Some(words.number(flag)?),
                    "--limit" => limit = Some(words.number(flag)?),
                    _ => return Err(unknown(flag)),
                }
            }
            Command::FsRead(fs_ops::ReadRequest {
                path: one_path(words.positional)?,
                offset,
                limit,
            })
        }
        "fs list" => {
            if let Some(flag) = words.next_flag() {
                return Err(unknown(flag));
            }
            Command::FsList(fs_ops::ListRequest {
                path: one_path(words.positional)?,
            })
        }
        "fs write" => {
            let (mut content, mut create_dirs) = (None, false);
            while let Some(flag) = words.next_flag() {
                match flag {
                    "--content" => content = Some(words.value(flag)?),
                    "--create-dirs" => create_dirs = true,
                    _ => return Err(unknown(flag)),
                }
            }
            Command::FsWrite {
                path: one_path(words.positional)?,
                content,
                create_dirs,
            }
        }
        "fs edit" => {
            let (mut old_string, mut new_string, mut replace_all) = (None, None, false);
            while let Some(flag) = words.next_flag() {
                match flag {
                    "--old" => old_string = Some(words.value(flag)?),
                    "--new" => new_string = Some(words.value(flag)?),
                    "--replace-all" => replace_all = true,
                    _ => return Err(unknown(flag)),
                }
            }
            Command::FsEdit(fs_ops::EditRequest {
                path: one_path(words.positional)?,
                old_string: old_string.ok_or("`fs edit` needs --old")?,
                new_string: new_string.ok_or("`fs edit` needs --new")?,
                replace_all,
            })
        }
        "exec" => {
            let (mut env, mut stdin, mut timeout_secs) = (BTreeMap::new(), None, None);
            while let Some(flag) = words.next_flag() {
                match flag {
                    "--env" => {
                        let pair = words.value(flag)?;
                        let (name, value) = pair
                            .split_once('=')
                            .ok_or_else(|| "`--env` takes NAME=VALUE".to_owned())?;
                        env.insert(name.to_owned(), value.to_owned());
                    }
                    "--stdin" => stdin = Some(words.value(flag)?),
                    "--timeout-secs" => timeout_secs = Some(words.number(flag)?),
                    _ => return Err(unknown(flag)),
                }
            }
            if words.positional.is_empty() {
                return Err("`exec` needs the program and its arguments after --".to_owned());
            }
            Command::Exec(process::ExecRequest {
                argv: words.positional,
                env,
                stdin,
                timeout_secs,
            })
        }
        "git diff" | "git restore" => {
            let mut staged = false;
            while let Some(flag) = words.next_flag() {
                match flag {
                    "--staged" => staged = true,
                    _ => return Err(unknown(flag)),
                }
            }
            let paths = words.positional;
            if name == "git restore" && paths.is_empty() {
                return Err("`git restore` needs at least one path".to_owned());
            }
            if name == "git diff" {
                Command::GitDiff(git::DiffRequest { staged, paths })
            } else {
                Command::GitRestore(git::RestoreRequest { staged, paths })
            }
        }
        "git log" => {
            let mut limit = None;
            while let Some(flag) = words.next_flag() {
                match flag {
                    "--limit" => limit = Some(words.number(flag)?),
                    _ => return Err(unknown(flag)),
                }
            }
            no_positional(words.positional)?;
            Command::GitLog(git::LogRequest { limit })
        }
        "git show" => {
            let mut commit = None;
            while let Some(flag) = words.next_flag() {
                match flag {
                    "--commit" => commit = Some(words.value(flag)?),
                    _ => return Err(unknown(flag)),
                }
            }
            Command::GitShow(git::ShowRequest {
                commit,
                paths: words.positional,
            })
        }
        "git add" => {
            if let Some(flag) = words.next_flag() {
                return Err(unknown(flag));
            }
            if words.positional.is_empty() {
                return Err("`git add` needs at least one path".to_owned());
            }
            Command::GitAdd(git::AddRequest {
                paths: words.positional,
            })
        }
        "git commit" => {
            let mut message = None;
            while let Some(flag) = words.next_flag() {
                match flag {
                    "--message" | "-m" => message = Some(words.value(flag)?),
                    _ => return Err(unknown(flag)),
                }
            }
            no_positional(words.positional)?;
            Command::GitCommit(git::CommitRequest {
                message: message.ok_or("`git commit` needs --message")?,
            })
        }
        other => return Err(format!("unknown command `{other}`")),
    };
    if matches!(command, Command::Version | Command::Help) && !rest.is_empty() {
        return Err(format!("unexpected arguments {rest:?}"));
    }
    Ok(command)
}

/// The usage text.
pub fn usage() -> String {
    format!(
        "Usage: {NAME} [--url URL] [--token-file FILE] COMMAND
The broker is --url or ${URL_ENV} (http://127.0.0.1:PORT), and the run's
token file --token-file or ${TOKEN_FILE_ENV}; the token itself is never an
argument and never printed. Answers are JSON on stdout. A refusal prints the
broker's error body on stderr and exits 1; a bad command line exits 2; a
client-side failure (configuration, connection, protocol) exits 3.

Commands:
  --version | --help
  health [--json]                     the broker's health (no token)
  token inspect                       the token file's claims (not the token)
  fs read PATH [--offset N] [--limit N]
  fs list PATH
  fs write PATH [--content TEXT] [--create-dirs]   (without --content, stdin)
  fs edit PATH --old TEXT --new TEXT [--replace-all]
  exec [--env NAME=VALUE]... [--stdin TEXT] [--timeout-secs N] -- PROGRAM [ARG]...
  git status
  git diff [--staged] [PATH]...
  git log [--limit N]
  git show [--commit REV] [PATH]...
  git add PATH...
  git commit --message TEXT
  git restore [--staged] PATH...
  mcp                                 the worker's MCP server on stdio"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    fn command(words: &[&str]) -> Result<Command, String> {
        parse(&args(words)).map(|invocation| invocation.command)
    }

    #[test]
    fn parses_the_global_options_and_each_command() {
        let invocation = parse(&args(&[
            "--url",
            "http://127.0.0.1:1",
            "--token-file",
            "/t",
            "git",
            "status",
        ]))
        .unwrap();
        assert_eq!(invocation.url.as_deref(), Some("http://127.0.0.1:1"));
        assert_eq!(invocation.token_file, Some(PathBuf::from("/t")));
        assert_eq!(invocation.command, Command::GitStatus);

        assert_eq!(command(&["-V"]), Ok(Command::Version));
        assert_eq!(command(&["help"]), Ok(Command::Help));
        assert_eq!(command(&["health"]), Ok(Command::Health { json: false }));
        assert_eq!(
            command(&["health", "--json"]),
            Ok(Command::Health { json: true })
        );
        assert_eq!(command(&["token", "inspect"]), Ok(Command::TokenInspect));
        assert_eq!(
            command(&["fs", "read", "a", "--offset", "2", "--limit", "3"]),
            Ok(Command::FsRead(fs_ops::ReadRequest {
                path: "a".into(),
                offset: Some(2),
                limit: Some(3)
            }))
        );
        assert_eq!(
            command(&["fs", "list", "."]),
            Ok(Command::FsList(fs_ops::ListRequest { path: ".".into() }))
        );
        assert_eq!(
            command(&["fs", "write", "a", "--create-dirs", "--content", "-x"]),
            Ok(Command::FsWrite {
                path: "a".into(),
                content: Some("-x".into()),
                create_dirs: true
            })
        );
        assert_eq!(
            command(&[
                "fs",
                "edit",
                "a",
                "--old",
                "x",
                "--new",
                "y",
                "--replace-all"
            ]),
            Ok(Command::FsEdit(fs_ops::EditRequest {
                path: "a".into(),
                old_string: "x".into(),
                new_string: "y".into(),
                replace_all: true
            }))
        );
        assert_eq!(
            command(&[
                "exec",
                "--env",
                "A=1=2",
                "--stdin",
                "in",
                "--timeout-secs",
                "5",
                "--",
                "sh",
                "-c",
                "echo"
            ]),
            Ok(Command::Exec(process::ExecRequest {
                argv: args(&["sh", "-c", "echo"]),
                env: [("A".to_owned(), "1=2".to_owned())].into_iter().collect(),
                stdin: Some("in".into()),
                timeout_secs: Some(5)
            }))
        );
        assert_eq!(
            command(&["git", "diff", "--staged", "a", "b"]),
            Ok(Command::GitDiff(git::DiffRequest {
                staged: true,
                paths: args(&["a", "b"])
            }))
        );
        assert_eq!(
            command(&["git", "log", "--limit", "4"]),
            Ok(Command::GitLog(git::LogRequest { limit: Some(4) }))
        );
        assert_eq!(
            command(&["git", "show", "--commit", "HEAD~1", "a"]),
            Ok(Command::GitShow(git::ShowRequest {
                commit: Some("HEAD~1".into()),
                paths: args(&["a"])
            }))
        );
        assert_eq!(
            command(&["git", "add", "a", "--", "-b"]),
            Ok(Command::GitAdd(git::AddRequest {
                paths: args(&["a", "-b"])
            }))
        );
        assert_eq!(
            command(&["git", "commit", "-m", "msg"]),
            Ok(Command::GitCommit(git::CommitRequest {
                message: "msg".into()
            }))
        );
        assert_eq!(
            command(&["git", "restore", "--staged", "a"]),
            Ok(Command::GitRestore(git::RestoreRequest {
                staged: true,
                paths: args(&["a"])
            }))
        );
    }

    #[test]
    fn refuses_a_wrong_command_line() {
        for (words, why) in [
            (&[][..], "no command"),
            (&["--url"][..], "needs a value"),
            (&["frobnicate"][..], "unknown command `frobnicate`"),
            (&["mcp", "--x"][..], "unknown flag `--x`"),
            (&["mcp", "a"][..], "unexpected arguments"),
            (&["fs"][..], "needs a subcommand"),
            (&["git", "push"][..], "unknown command `git push`"),
            (&["--version", "x"][..], "unexpected arguments"),
            (&["health", "--x"][..], "unknown flag `--x`"),
            (&["token", "inspect", "--x"][..], "unknown flag"),
            (&["git", "status", "a"][..], "unexpected arguments"),
            (&["health", "a"][..], "unexpected arguments"),
            (&["fs", "read"][..], "expected one path"),
            (&["fs", "read", "a", "b"][..], "expected one path"),
            (&["fs", "read", "a", "--limit", "x"][..], "is not a number"),
            (&["fs", "read", "a", "--x"][..], "unknown flag"),
            (&["fs", "list", "a", "--x"][..], "unknown flag"),
            (&["fs", "write", "a", "--x"][..], "unknown flag"),
            (&["fs", "write", "a", "--content"][..], "needs a value"),
            (&["fs", "edit", "a", "--new", "y"][..], "needs --old"),
            (&["fs", "edit", "a", "--old", "y"][..], "needs --new"),
            (&["fs", "edit", "a", "--x"][..], "unknown flag"),
            (&["exec"][..], "needs the program"),
            (&["exec", "--env", "A", "--", "sh"][..], "NAME=VALUE"),
            (&["exec", "--x"][..], "unknown flag"),
            (&["git", "diff", "--x"][..], "unknown flag"),
            (&["git", "log", "a"][..], "unexpected arguments"),
            (&["git", "log", "--x"][..], "unknown flag"),
            (&["git", "show", "--x"][..], "unknown flag"),
            (&["git", "add", "--x"][..], "unknown flag"),
            (&["git", "add"][..], "needs at least one path"),
            (
                &["git", "restore", "--staged"][..],
                "needs at least one path",
            ),
            (&["git", "commit"][..], "needs --message"),
            (
                &["git", "commit", "-m", "m", "a"][..],
                "unexpected arguments",
            ),
            (&["git", "commit", "--x"][..], "unknown flag"),
        ] {
            let error = command(words).unwrap_err();
            assert!(error.contains(why), "{words:?}: {error}");
        }
    }

    fn run_with(words: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        let lookup = move |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&args(words), &lookup, &mut &b""[..], &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn exits_by_what_failed() {
        let (code, out, err) = run_with(&["--version"], &[]);
        assert_eq!(
            (code, out, err),
            (EXIT_OK, format!("{NAME} {BUILD}\n"), String::new())
        );
        let (code, out, _) = run_with(&["--help"], &[]);
        assert_eq!(code, EXIT_OK);
        assert!(out.starts_with("Usage: dagq-broker-client"));
        assert_eq!(command(&["mcp"]), Ok(Command::Mcp));
        // The MCP server does not start without a broker to ask.
        let (code, out, err) = run_with(&["mcp"], &[]);
        assert_eq!(code, EXIT_CLIENT);
        assert!(out.is_empty());
        assert!(err.contains("no --url"), "{err}");
        // With one, it serves until stdin ends (here at once).
        let (code, out, err) = run_with(&["--url", "http://127.0.0.1:9", "mcp"], &[]);
        assert_eq!((code, out.as_str(), err.as_str()), (EXIT_OK, "", ""));
        let (code, _, err) = run_with(&["health"], &[]);
        assert_eq!(code, EXIT_CLIENT);
        assert!(
            err.contains("no --url and DAGQ_BROKER_URL is not set"),
            "{err}"
        );
        let (code, _, err) = run_with(&["health"], &[(URL_ENV, "http://10.0.0.1:1")]);
        assert_eq!(code, EXIT_CLIENT);
        assert!(err.contains("not a loopback address"), "{err}");
        let (code, _, err) = run_with(&["token", "inspect"], &[]);
        assert_eq!(code, EXIT_CLIENT);
        assert!(err.contains("no --token-file"), "{err}");
    }

    /// `mcp` serves `write_receipt` only when `DAGQ_RECEIPT_FILE` names the
    /// receipt (`required`, ADR-t838-1), and writes it there with no broker
    /// answering.
    #[test]
    fn mcp_serves_write_receipt_with_the_receipt_file_of_its_env() {
        let dir = tempfile::tempdir().unwrap();
        let receipt = dir.path().join("receipt.json");
        let serve = |env: Vec<(&str, String)>| {
            let lookup = move |name: &str| {
                env.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone())
            };
            let input = concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"write_receipt","arguments":{"receipt":{"result":"failed"}}}}"#,
                "\n"
            );
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = run(
                &args(&["--url", "http://127.0.0.1:9", "mcp"]),
                &lookup,
                &mut input.as_bytes(),
                &mut out,
                &mut err,
            );
            assert_eq!(code, EXIT_OK, "{}", String::from_utf8_lossy(&err));
            String::from_utf8(out)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .collect::<Vec<_>>()
        };
        let with = serve(vec![(
            crate::mcp::RECEIPT_FILE_ENV,
            receipt.to_str().unwrap().to_owned(),
        )]);
        assert!(with[0].to_string().contains("write_receipt"), "{}", with[0]);
        assert_eq!(with[1]["result"]["isError"], false, "{}", with[1]);
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&receipt).unwrap()).unwrap();
        assert_eq!(written, serde_json::json!({"result": "failed"}));

        std::fs::remove_file(&receipt).unwrap();
        for env in [vec![], vec![(crate::mcp::RECEIPT_FILE_ENV, String::new())]] {
            let without = serve(env);
            assert!(!without[0].to_string().contains("write_receipt"));
            assert!(without[1].get("error").is_some(), "{}", without[1]);
            assert!(!receipt.exists());
        }
    }

    #[test]
    fn token_inspect_prints_the_claims_and_not_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        std::fs::write(&file, "not-a-token\n").unwrap();
        let path = file.to_str().unwrap();
        let (code, _, err) = run_with(&["token", "inspect"], &[(TOKEN_FILE_ENV, path)]);
        assert_eq!(code, EXIT_CLIENT);
        assert!(err.contains("holds no token"), "{err}");
        assert!(!err.contains("not-a-token"), "{err}");
    }
}
