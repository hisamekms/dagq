//! Host-only advisory attestation. Read the controlling terminal itself,
//! never piped stdin; no environment variable can opt into confirmation.
use crate::domain::direct_human::Evidence;
use anyhow::Result;
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    time::Instant,
};

/// No terminal or any DAGQ_ROLE leaves the request unattested so that the
/// service, rather than the client, records and returns the refusal. The
/// prompt names `use_case`, so each step of a command that asks more than
/// once (`ready --inherit`'s read and its record) is told apart.
pub fn confirm(use_case: &str, params: &Value) -> Result<Option<Evidence>> {
    if std::env::var_os(crate::domain::actor::ROLE_ENV).is_some() {
        return Ok(None);
    }
    let Some(mut tty) = inherited_terminal().or_else(|| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()
    }) else {
        return Ok(None);
    };
    let mut name = [0; 1024];
    // SAFETY: tty is open and the writable buffer has the supplied length.
    if unsafe { libc::ttyname_r(tty.as_raw_fd(), name.as_mut_ptr(), name.len()) } != 0 {
        return Ok(None);
    }
    // SAFETY: successful ttyname_r wrote a NUL-terminated string.
    let terminal = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let target = crate::domain::direct_human::target(params);
    let mut parent_chain = Vec::new();
    let mut pid = std::process::id();
    while pid != 0 && parent_chain.len() < 64 && !parent_chain.contains(&pid) {
        parent_chain.push(pid);
        let Some(process) = super::process_read::read_process(pid) else {
            break;
        };
        pid = process.ppid;
    }
    // The full request is already visible on the caller's command line;
    // display only its digest and identity, never arbitrary params/secrets.
    let identity = params
        .get("id")
        .map(ToString::to_string)
        .or_else(|| params.pointer("/call/args/0").map(ToString::to_string))
        .unwrap_or_else(|| "operation".into());
    writeln!(
        tty,
        "Direct human confirmation of {use_case} {identity}, request SHA-256 {target}"
    )?;
    write!(tty, "Type {} to confirm: ", &target[..12])?;
    tty.flush()?;
    let start = Instant::now();
    let mut confirmation = String::new();
    BufReader::new(tty).take(256).read_line(&mut confirmation)?;
    Ok(Some(Evidence {
        target,
        confirmation: confirmation.trim().into(),
        terminal,
        role_env_absent: true,
        parent_chain,
        elapsed_ms: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        socket_peer: None,
    }))
}

/// A terminal already handed to the command may be usable where reopening
/// /dev/tty is restricted. Its session must be this process's controlling
/// session, and the descriptor must support both confirmation input and prompts.
fn inherited_terminal() -> Option<File> {
    // SAFETY: getsid(0) reads this process's session, without changing it.
    let session = unsafe { libc::getsid(0) };
    if session < 0 {
        return None;
    }
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: these calls inspect standard descriptors; invalid or closed
        // descriptors fail. A pipe cannot satisfy isatty/session checks.
        let usable = unsafe {
            libc::isatty(fd) == 1
                && libc::tcgetsid(fd) == session
                && libc::fcntl(fd, libc::F_GETFL) & libc::O_ACCMODE == libc::O_RDWR
        };
        if usable {
            // SAFETY: dup produces a separately owned descriptor on success.
            let duplicated = unsafe { libc::dup(fd) };
            if duplicated >= 0 {
                // SAFETY: only this File owns the successful dup result.
                return Some(unsafe { File::from_raw_fd(duplicated) });
            }
        }
    }
    None
}
