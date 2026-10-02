//! The host's KPI push (ADR-0051 decisions 22 and 23): the `[push]` table
//! of `host.toml` (the queue's `<queue dir>/host.toml` replaces the
//! host-wide `$XDG_CONFIG_HOME/dagq/host.toml`'s as a whole; neither is in
//! the repository), and the command run without a shell, its message on
//! stdin, in a process group of its own that a timeout stops as a whole.
//!
//! ```toml
//! [push]
//! command = ["/Users/me/.local/bin/dagq-push-ntfy"]
//! timeout_secs = 30
//! daily = true
//! breach = true
//! max_breach_per_day = 3
//! ```
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::process::{CommandExt, ExitStatusExt},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};

use super::adapters::output_file;
use super::kpi_config::HOST_FILE_NAME;
use super::run_env::{parse_positive, parse_string, strip_comment};
use crate::application::push::{PushOutcome, PushRequest};
use crate::domain::kpi::push::PushConfig;

/// The bytes of stderr kept, from the end.
const STDERR_KEPT: usize = 64 * 1024;

/// A one-line array of strings: `["a", 'b']`.
fn parse_strings(text: &str) -> Result<Vec<String>> {
    let mut rest = text
        .trim()
        .strip_prefix('[')
        .context("expected an array of strings")?
        .trim_start();
    let mut values = Vec::new();
    loop {
        if let Some(after) = rest.strip_prefix(']') {
            let after = after.trim();
            ensure!(
                after.is_empty() || after.starts_with('#'),
                "unexpected text after the array: {after}"
            );
            return Ok(values);
        }
        let quote = rest.chars().next().context("unterminated array")?;
        ensure!(
            quote == '"' || quote == '\'',
            "expected a quoted string in the array"
        );
        // The string ends at its closing quote not escaped.
        let mut end = None;
        let mut escaped = false;
        for (index, c) in rest.char_indices().skip(1) {
            if quote == '"' && c == '\\' && !escaped {
                escaped = true;
                continue;
            }
            if c == quote && !escaped {
                end = Some(index);
                break;
            }
            escaped = false;
        }
        let end = end.context("unterminated string")?;
        values.push(parse_string(&rest[..=end])?);
        rest = rest[end + 1..].trim_start();
        ensure!(!rest.is_empty(), "unterminated array");
        if let Some(after) = rest.strip_prefix(',') {
            rest = after.trim_start();
        } else {
            ensure!(rest.starts_with(']'), "expected , or ] in the array");
        }
    }
}

fn parse_bool(text: &str) -> Result<bool> {
    match strip_comment(text) {
        "true" => Ok(true),
        "false" => Ok(false),
        other => bail!("expected true or false, not {other}"),
    }
}

/// The `[push]` table of a `host.toml`'s text, `label` naming the file in
/// errors; `None` when it has none. A table with an empty `command` is
/// kept (it turns the push off for the queue).
pub fn parse_host_push(text: &str, label: &str) -> Result<Option<PushConfig>> {
    let mut config: Option<PushConfig> = None;
    let mut command_set = false;
    let mut seen: Vec<String> = Vec::new();
    let mut in_push = false;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = strip_comment(header)
                .strip_suffix(']')
                .with_context(|| format!("{label}:{number}: unclosed table header"))?
                .trim();
            in_push = name == "push";
            if in_push {
                ensure!(
                    config.is_none(),
                    "{label}:{number}: [push] is defined twice"
                );
                config = Some(PushConfig::new(Vec::new()));
            }
            continue;
        }
        if !in_push {
            continue;
        }
        let Some(push) = config.as_mut() else {
            continue;
        };
        let (key, rest) = line
            .split_once('=')
            .with_context(|| format!("{label}:{number}: expected KEY = value"))?;
        let key = key.trim();
        let rest = rest.trim();
        ensure!(
            !seen.iter().any(|seen| seen == key),
            "{label}:{number}: {key} is defined twice"
        );
        seen.push(key.to_owned());
        let at = || format!("{label}:{number}: value of {key}");
        match key {
            "command" => {
                push.command = parse_strings(rest).with_context(at)?;
                command_set = true;
            }
            "timeout_secs" => {
                push.timeout_secs =
                    u64::try_from(parse_positive(rest, "number of seconds").with_context(at)?)?;
            }
            "daily" => push.daily = parse_bool(rest).with_context(at)?,
            "breach" => push.breach = parse_bool(rest).with_context(at)?,
            "max_breach_per_day" => {
                let value = strip_comment(rest)
                    .parse::<usize>()
                    .map_err(|_| anyhow::anyhow!("expected a whole number"))
                    .with_context(at)?;
                push.max_breach_per_day = value;
            }
            _ => bail!(
                "{label}:{number}: unknown key {key} in [push]; the keys are command, timeout_secs, daily, breach, max_breach_per_day"
            ),
        }
    }
    if let Some(push) = &config {
        ensure!(
            command_set,
            "{label}: [push] needs command (an array; empty turns the push off)"
        );
        ensure!(
            push.command
                .first()
                .is_none_or(|program| !program.is_empty()),
            "{label}: the program of [push] command is empty"
        );
    }
    Ok(config)
}

/// The host's push: the queue's `[push]` (`<queue_dir>/host.toml`), else
/// the host-wide file's; `None` without one or with an empty `command`.
pub fn load_host_push(queue_dir: &Path, host_wide: Option<&Path>) -> Result<Option<PushConfig>> {
    for path in [Some(queue_dir.join(HOST_FILE_NAME).as_path()), host_wide]
        .into_iter()
        .flatten()
    {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        if let Some(config) = parse_host_push(&text, &path.display().to_string())? {
            return Ok(Some(config).filter(|config| !config.command.is_empty()));
        }
    }
    Ok(None)
}

/// Run `request`'s command once: its message on stdin, its stdout
/// dropped, the tail of its stderr kept; past the timeout its process
/// group is killed. Its stderr goes to an unlinked file, not a pipe: a
/// child it left behind, or a process the supervisor spawned meanwhile
/// that inherited the pipe, would hold a read to the pipe's end (task
/// 1273), and the file is complete once the command exits.
pub fn run_push(request: &PushRequest) -> PushOutcome {
    let Some((program, args)) = request.command.split_first() else {
        return PushOutcome::error("the push command is empty");
    };
    let stderr = match output_file().and_then(|file| Ok((file.try_clone()?, file))) {
        Ok(files) => files,
        Err(error) => {
            return PushOutcome::error(&format!(
                "could not take the push command's stderr: {error}"
            ));
        }
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(request.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(stderr.0)
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        // The program's path is not recorded: it is the host's.
        Err(error) => {
            return PushOutcome::error(&format!("could not start the push command: {error}"));
        }
    };
    // The message is written by a thread: a command that does not read it
    // must not hold this one past its timeout.
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = request.stdin.clone();
        thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let deadline = Instant::now() + request.timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                if let Ok(pgid) = i32::try_from(child.id()) {
                    // SAFETY: kill(2) on the group this command leads.
                    unsafe {
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                }
                break child.wait().ok();
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    let stderr = String::from_utf8_lossy(&stderr_tail(stderr.1)).into_owned();
    PushOutcome {
        success: !timed_out && status.is_some_and(|status| status.success()),
        exit_code: status.and_then(|status| status.code()),
        signal: status.and_then(|status| status.signal()),
        timed_out,
        stderr,
        error: status
            .is_none()
            .then(|| "the push command could not be waited for".to_owned()),
    }
}

/// The last [`STDERR_KEPT`] bytes of `file` (empty when it cannot be read).
fn stderr_tail(mut file: File) -> Vec<u8> {
    let mut kept = Vec::new();
    let from = file
        .metadata()
        .map(|metadata| metadata.len().saturating_sub(STDERR_KEPT as u64))
        .unwrap_or(0);
    if file.seek(SeekFrom::Start(from)).is_ok() {
        let _ = file.take(STDERR_KEPT as u64).read_to_end(&mut kept);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_push_and_skips_other_tables() {
        let text = "\u{feff}[report]\nkeep_daily_days = 3\n[push] # mine\ncommand = [\"/bin/push\", 'a b', \"c\\\"d\"] # argv\ntimeout_secs = 5\ndaily = false\nbreach = true # yes\nmax_breach_per_day = 0\n[kpi]\ncommand = oops\n";
        let config = parse_host_push(text, "h").unwrap().unwrap();
        assert_eq!(config.command, ["/bin/push", "a b", "c\"d"]);
        assert_eq!(config.timeout_secs, 5);
        assert!(!config.daily);
        assert!(config.breach);
        assert_eq!(config.max_breach_per_day, 0);
        assert_eq!(parse_host_push("[report]\n", "h").unwrap(), None);
        let defaults = parse_host_push("[push]\ncommand = [\"x\"]\n", "h")
            .unwrap()
            .unwrap();
        assert_eq!(defaults, PushConfig::new(vec!["x".into()]));
        assert_eq!(
            parse_host_push("[push]\ncommand = []\n", "h")
                .unwrap()
                .unwrap()
                .command,
            Vec::<String>::new()
        );
    }

    #[test]
    fn refuses_what_the_table_does_not_support() {
        let error = |text: &str| format!("{:#}", parse_host_push(text, "h").unwrap_err());
        assert!(error("[push]\ncommand = [\"x\"]\nurl = 1\n").contains("unknown key url"));
        assert!(error("[push]\ntimeout_secs = 3\n").contains("needs command"));
        assert!(error("[push]\ncommand = \"x\"\n").contains("array of strings"));
        assert!(error("[push]\ncommand = [\"x\"\n").contains("unterminated array"));
        assert!(error("[push]\ncommand = [\"x\n").contains("unterminated string"));
        assert!(error("[push]\ncommand = [x]\n").contains("quoted string"));
        assert!(error("[push]\ncommand = [\"x\" \"y\"]\n").contains("expected , or ]"));
        assert!(error("[push]\ncommand = [\"x\"] y\n").contains("after the array"));
        assert!(error("[push]\ncommand = [\"\"]\n").contains("program"));
        assert!(error("[push]\ncommand = [\"x\"]\ndaily = yes\n").contains("true or false"));
        assert!(error("[push]\ncommand = [\"x\"]\ntimeout_secs = 0\n").contains("positive"));
        assert!(error("[push]\ncommand = [\"x\"]\nmax_breach_per_day = -1\n").contains("whole"));
        assert!(error("[push]\ncommand = [\"x\"]\ncommand = [\"y\"]\n").contains("twice"));
        assert!(error("[push]\n[push]\n").contains("defined twice"));
        assert!(error("[push]\ncommand\n").contains("KEY = value"));
        assert!(error("[push\n").contains("unclosed"));
    }

    #[test]
    fn the_queue_table_replaces_the_host_wide_one() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_host_push(dir.path(), None).unwrap(), None);
        let wide = dir.path().join("wide.toml");
        fs::write(&wide, "[push]\ncommand = [\"/wide\"]\ntimeout_secs = 9\n").unwrap();
        assert_eq!(
            load_host_push(dir.path(), Some(&wide))
                .unwrap()
                .unwrap()
                .command,
            ["/wide"]
        );
        // A queue file without [push] leaves the host-wide one.
        fs::write(dir.path().join(HOST_FILE_NAME), "[report]\n").unwrap();
        assert_eq!(
            load_host_push(dir.path(), Some(&wide))
                .unwrap()
                .unwrap()
                .timeout_secs,
            9
        );
        fs::write(
            dir.path().join(HOST_FILE_NAME),
            "[push]\ncommand = [\"/q\"]\n",
        )
        .unwrap();
        let config = load_host_push(dir.path(), Some(&wide)).unwrap().unwrap();
        assert_eq!(
            (config.command[0].as_str(), config.timeout_secs),
            ("/q", 30)
        );
        // An empty command turns it off for the queue.
        fs::write(dir.path().join(HOST_FILE_NAME), "[push]\ncommand = []\n").unwrap();
        assert_eq!(load_host_push(dir.path(), Some(&wide)).unwrap(), None);
        fs::write(dir.path().join(HOST_FILE_NAME), "[push]\ncommand = 1\n").unwrap();
        assert!(load_host_push(dir.path(), None).is_err());
    }

    fn request(script: &str, timeout: Duration) -> PushRequest {
        PushRequest {
            command: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env: vec![("DAGQ_PUSH_KIND".into(), "daily".into())],
            stdin: b"{\"title\":\"t\"}\n".to_vec(),
            timeout,
        }
    }

    #[test]
    fn a_command_reads_its_message_and_its_failure_is_told() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let script = format!(
            "cat > '{}'; printf '%s' \"$DAGQ_PUSH_KIND\" >> '{}'",
            out.display(),
            out.display()
        );
        let outcome = run_push(&request(&script, Duration::from_secs(10)));
        assert!(outcome.success, "{outcome:?}");
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "{\"title\":\"t\"}\ndaily"
        );

        let outcome = run_push(&request("echo broke >&2; exit 4", Duration::from_secs(10)));
        assert!(!outcome.success);
        assert_eq!(outcome.exit_code, Some(4));
        assert_eq!(outcome.stderr, "broke\n");
        assert!(!outcome.timed_out);

        let missing = run_push(&PushRequest {
            command: vec![dir.path().join("none").display().to_string()],
            ..request("", Duration::from_secs(1))
        });
        assert!(!missing.success);
        assert!(missing.error.unwrap().contains("could not start"));
        let empty = run_push(&PushRequest {
            command: Vec::new(),
            ..request("", Duration::from_secs(1))
        });
        assert!(!empty.success);
    }

    /// Only the end of a long stderr is kept, and a child the command left
    /// behind holding stderr does not hold the call.
    #[test]
    fn the_tail_of_stderr_is_kept_without_waiting_for_a_holder() {
        let script = format!(
            "(sleep 30) & head -c {} /dev/zero | tr '\\0' a >&2; echo end >&2; exit 1",
            STDERR_KEPT
        );
        let started = Instant::now();
        let outcome = run_push(&request(&script, Duration::from_secs(20)));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(outcome.exit_code, Some(1));
        assert_eq!(outcome.stderr.len(), STDERR_KEPT);
        assert!(
            outcome.stderr.ends_with("aend\n"),
            "{:?}",
            &outcome.stderr[STDERR_KEPT - 8..]
        );
    }

    #[test]
    fn a_command_past_its_timeout_is_stopped_with_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        // A child of the command that would outlive it holds stderr open.
        let script = format!("(sleep 30; touch '{}') & sleep 30", child.display());
        let started = Instant::now();
        let outcome = run_push(&request(&script, Duration::from_millis(300)));
        assert!(outcome.timed_out);
        assert!(!outcome.success);
        assert_eq!(outcome.signal, Some(libc::SIGKILL));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
