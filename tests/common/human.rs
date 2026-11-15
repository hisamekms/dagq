//! Existing CLI fixtures that intentionally exercise a successful weakening
//! operation use a controlling terminal and answer its digest challenge. This
//! helper never removes a role and never supplies a service attestation itself.
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::{Command, Output},
    time::{Duration, Instant},
};

pub fn weakening(args: &[&str]) -> bool {
    matches!(args.first(), Some(&"ready"))
        && args
            .iter()
            .any(|arg| matches!(*arg, "--bypass-review" | "--inherit"))
        || args.first() == Some(&"down") && args.contains(&"--force")
}

/// The ordinary stdout/stderr are captured separately from the terminal's
/// prompts. Polling and the child both have the shared test deadline; the
/// guard kills this child on panic or timeout, without leaving a reader thread.
pub fn output(command: &mut Command) -> std::io::Result<Output> {
    output_within(command, super::STEP_LIMIT)
}

/// [`output`] of a child given `limit` to finish.
fn output_within(command: &mut Command, limit: Duration) -> std::io::Result<Output> {
    let _waiting = super::within(limit, "the human-confirmed CLI fixture");
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: both output pointers are writable, optional termios/name are null.
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openpty returned two owned descriptors.
    let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    // Neither end goes on to another process the test spawns: the child
    // gets the terminal as its standard input alone, and a killed parent
    // leaves it no master to keep its terminal open.
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        // SAFETY: the descriptor is open; only its close-on-exec flag is set.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    // SAFETY: the descriptor is open; nonblocking reads allow bounded polling.
    if unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let out = tempfile::tempfile()?;
    let err = tempfile::tempfile()?;
    command
        .stdin(slave.try_clone()?)
        .stdout(out.try_clone()?)
        .stderr(err.try_clone()?);
    let terminal = slave.as_raw_fd();
    // SAFETY: only async-signal-safe syscalls run after fork. The slave stays
    // open in the parent until spawn finishes, and is only this child's TTY.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 || libc::ioctl(terminal, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = super::KillOnDrop::new(command.spawn()?, "the human-confirmed CLI child");
    drop(slave);
    let deadline = Instant::now() + limit;
    let mut prompts = String::new();
    let status = loop {
        let mut buffer = [0_u8; 4096];
        match master.read(&mut buffer) {
            Ok(n) => prompts.push_str(&String::from_utf8_lossy(&buffer[..n])),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            // A closed slave is EIO on Linux and EOF on macOS.
            Err(e) if e.raw_os_error() == Some(libc::EIO) => {}
            Err(e) => return Err(e),
        }
        while let Some(start) = prompts.find("Type ") {
            let tail = &prompts[start + 5..];
            let Some(end) = tail.find(" to confirm: ") else {
                break;
            };
            let digest = &tail[..end];
            assert!(
                digest.len() == 12 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                "unexpected challenge: {prompts}"
            );
            writeln!(master, "{digest}")?;
            prompts.drain(..start + 5 + end + " to confirm: ".len());
        }
        if let Some(status) = child.child().try_wait()? {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "confirmation did not finish: {prompts}"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    let read = |mut file: File| -> std::io::Result<Vec<u8>> {
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    Ok(Output {
        status,
        stdout: read(out)?,
        stderr: read(err)?,
    })
}

/// A use-case fixture below authentication, with the recent human-login
/// snapshot a team entrance supplies. It exercises the real AnswerPolicy;
/// it does not invent host terminal evidence or bypass a policy decision.
pub fn authenticated(now: i64) -> dagq::domain::ActorContext {
    let mut principal =
        dagq::domain::queue_service::Principal::human(dagq::domain::oidc::OidcIdentity {
            issuer: "https://identity.example.test".into(),
            subject: "fixture-person".into(),
        });
    principal.auth_time = Some(now);
    principal.actor()
}

/// Run an existing composition test in its own controlling-terminal process.
/// This keeps the parent test runner's role/session untouched, while both
/// socket and in-process lifecycle fixtures collect actual TTY confirmation.
/// Return true in the parent, whose caller must return after checking the child.
pub fn rerun_on_terminal() -> bool {
    use super::WithoutActor;
    const CHILD: &str = "DAGQ_TEST_CONFIRMATION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return false;
    }
    let thread = std::thread::current();
    let name = thread.name().expect("the test thread has a name");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .without_actor_env()
        .env(CHILD, "1")
        .args(["--exact", name, "--nocapture"]);
    let result = output_within(&mut command, super::TEST_LIMIT).unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    // A filter that matches nothing also exits 0: the child must have run
    // this one test, not none.
    assert!(
        result.status.success() && stdout.contains(" 1 passed;"),
        "terminal fixture {name}:\n{stdout}\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    true
}

/// The audit records of the queue at `db` whose direct-human evidence the
/// queue service stamped with the socket's owner peer: only the service
/// stamps it, so a record of one says the confirmed request went through
/// the service, not to the DB.
pub fn attested_through_the_service(db: &std::path::Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(db).unwrap();
    let mut statement = connection
        .prepare("SELECT record FROM audit_records ORDER BY seq")
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .filter(|record| record.contains("human_attested") && record.contains("owner; pid="))
        .collect()
}
