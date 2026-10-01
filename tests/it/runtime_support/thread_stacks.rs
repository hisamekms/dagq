//! Best-effort timeout diagnostics, independent of the supervisor's locks.
use std::{
    io::{self, Read, Seek, Write},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(super) fn print() {
    // Linux Yama normally denies a child debugger attaching to its parent.
    // Opt this already-failing test process in only while collecting stacks.
    #[cfg(target_os = "linux")]
    let _ptrace = AllowPtrace::new();
    let pid = std::process::id().to_string();
    let mut commands = Vec::new();
    if cfg!(target_os = "macos") {
        let mut sample = Command::new("sample");
        sample.args([&pid, "1", "-file", "/dev/stdout"]);
        commands.push(sample);
    } else if cfg!(target_os = "linux") {
        // ptrace stops every thread of this process, including the ones
        // that enforce our limits, so only an outside process can bound a
        // debugger that hangs while attached. TERM first lets gdb detach.
        let debugger = |args: &[&str]| {
            let mut command = Command::new("timeout");
            command.args(["--kill-after=2", "8"]).args(args);
            command
        };
        commands.push(debugger(&["eu-stack", "-p", &pid]));
        commands.push(debugger(&[
            "gdb",
            "-nx",
            "--batch",
            "-p",
            &pid,
            "-ex",
            "set pagination off",
            "-ex",
            "thread apply all bt",
            "-ex",
            "detach",
        ]));
    }
    // Do not hold stderr's lock while the sampler inspects this process.
    let report = collect(&mut commands, Duration::from_secs(10));
    let _ = io::stderr()
        .lock()
        .write_all(format!("thread stacks of test process {pid}:\n{report}").as_bytes());
}

#[cfg(target_os = "linux")]
struct AllowPtrace;

#[cfg(target_os = "linux")]
impl AllowPtrace {
    fn new() -> Self {
        // SAFETY: PR_SET_PTRACER takes an integer pid/ANY, not pointers.
        if unsafe { libc::prctl(libc::PR_SET_PTRACER, libc::PR_SET_PTRACER_ANY, 0, 0, 0) } != 0 {
            let _ = writeln!(
                io::stderr(),
                "could not allow stack debugger: {}",
                io::Error::last_os_error()
            );
        }
        Self
    }
}

#[cfg(target_os = "linux")]
impl Drop for AllowPtrace {
    fn drop(&mut self) {
        // SAFETY: zero clears this test process's temporary ptrace exception.
        unsafe { libc::prctl(libc::PR_SET_PTRACER, 0, 0, 0, 0) };
    }
}

fn collect(commands: &mut [Command], limit: Duration) -> String {
    let started = Instant::now();
    let mut report = String::new();
    let mut found = false;
    for command in commands {
        let result = capture(command, limit.saturating_sub(started.elapsed()));
        if result
            .as_ref()
            .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
        {
            report += &format!("{:?}: tool not found\n", command.get_program());
            continue;
        }
        found = true;
        match result {
            Ok((success, output)) => {
                report += &output;
                if success {
                    return report;
                }
            }
            Err(error) => report += &format!("{command:?}: {error}\n"),
        }
    }
    if !found {
        report += "no thread stack tool found; continuing timeout cleanup\n";
    }
    report
}

fn capture(command: &mut Command, limit: Duration) -> io::Result<(bool, String)> {
    // A file avoids pipe backpressure and retains partial output if the
    // debugger hangs. Do not use Bounded: the timeout monitor is our caller.
    let mut output = tempfile::tempfile()?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output.try_clone()?)
        .spawn()?;
    let pid = child.id();
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < limit => thread::sleep(Duration::from_millis(20)),
            other => {
                let _ = child.kill();
                let _ = child.wait();
                break match other {
                    Err(error) => Err(error),
                    _ => Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "stack tool timed out",
                    )),
                };
            }
        }
    };
    // `timeout` exits 127 when the tool it wraps does not exist.
    if status.as_ref().is_ok_and(|s| s.code() == Some(127)) {
        return Err(io::ErrorKind::NotFound.into());
    }
    output.rewind()?;
    let mut bytes = Vec::new();
    output.read_to_end(&mut bytes)?;
    Ok((
        status.as_ref().is_ok_and(|s| s.success()),
        format!(
            "pid={pid} {command:?}: {status:?}\n{}\n",
            String::from_utf8_lossy(&bytes)
        ),
    ))
}

#[test]
fn missing_tools_do_not_stop_cleanup() {
    let mut commands = [Command::new("/nonexistent-dagq-stack-tool")];
    let report = collect(&mut commands, Duration::from_secs(1));
    assert!(report.contains("tool not found"), "{report}");
    assert!(report.contains("continuing timeout cleanup"), "{report}");
}

#[test]
fn a_wrapped_tool_that_is_missing_counts_as_not_found() {
    let mut missing = Command::new("/bin/sh");
    missing.args(["-c", "exit 127"]);
    let report = collect(&mut [missing], Duration::from_secs(1));
    assert!(report.contains("tool not found"), "{report}");
    assert!(report.contains("continuing timeout cleanup"), "{report}");
}

#[test]
fn failed_tool_reports_output_and_tries_the_fallback() {
    let mut failed = Command::new("/bin/sh");
    failed.args(["-c", "echo attach-denied >&2; exit 1"]);
    let mut fallback = Command::new("/bin/sh");
    fallback.args(["-c", "echo thread-backtrace"]);
    let report = collect(&mut [failed, fallback], Duration::from_secs(1));
    assert!(report.contains("attach-denied\n"), "{report}");
    assert!(report.contains("thread-backtrace\n"), "{report}");
}

#[test]
fn a_stuck_tool_is_killed_and_reaped() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "echo $$; exec sleep 60"]);
    let (success, report) = capture(&mut command, Duration::from_millis(200)).unwrap();
    assert!(!success);
    assert!(report.contains("stack tool timed out"), "{report}");
    let pid: libc::pid_t = report
        .split_whitespace()
        .next()
        .unwrap()
        .strip_prefix("pid=")
        .unwrap()
        .parse()
        .unwrap();
    // Probe with signal 0 rather than waitpid, which could reap another
    // test's child that reused the pid.
    // SAFETY: kill with signal 0 only checks that the pid exists.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[test]
fn supervise_timeout_prints_stacks_before_cleanup() {
    const CHILD: &str = "DAGQ_TEST_STACK_TIMEOUT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _diagnostics = super::supervise_diagnostics(std::path::Path::new("/nonexistent-queue"));
        let _waiting = crate::common::within(Duration::ZERO, "synthetic supervise timeout");
        loop {
            thread::park();
        }
    }
    use crate::common::Bounded;
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime_support::thread_stacks::supervise_timeout_prints_stacks_before_cleanup",
        ])
        .env(CHILD, "1")
        .bounded_output()
        .unwrap();
    assert_eq!(output.status.code(), Some(crate::common::TIMED_OUT));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("thread stacks of test process"), "{stderr}");
    assert!(
        stderr.contains("the events of the queue /nonexistent-queue could not be read"),
        "{stderr}"
    );
    assert!(
        stderr.find("thread stacks of test process").unwrap()
            < stderr
                .find("the events of the queue /nonexistent-queue could not be read")
                .unwrap(),
        "{stderr}"
    );
    // Emit the real sampler's output for manual verification of this path.
    eprintln!("{stderr}");
}
