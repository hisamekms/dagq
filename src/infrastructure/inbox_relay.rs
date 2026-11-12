//! The relay of a person's inbox for a control side on the person's
//! machine ([`crate::application::inbox_relay`]): the session's unix
//! socket the agent's `dagq` writes its requests on, the person's store the
//! inbox's token is kept in, and the agent run as a foreground child that
//! `dagq inbox` waits for, so its end revokes the token.
//!
//! The socket takes what the queue service's socket takes: one line of a
//! `ServiceRequest` of at most `MAX_REQUEST_BYTES`, answered with one
//! `ServiceResponse`, each connection on a thread of its own. It is in a
//! directory of this user's alone under `/tmp` (the queue service's short
//! socket directory, where a socket's path fits `sun_path`), mode 0600, and
//! removed when the relay closes. Against the same OS user it is advisory
//! (ADR-t3102-1 decision 5): the control side authorizes each request for
//! the principal of the inbox's token.

use std::{
    fs,
    io::{BufRead, BufReader, ErrorKind, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, Ordering::SeqCst},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::application::login::{self, CredentialStore, InboxToken};
use crate::domain::oidc::OidcIdentity;
use crate::domain::queue_service::{
    MAX_REQUEST_BYTES, Principal, ServiceErrorCode, ServiceRequest, ServiceResponse, UseCase,
};

/// How long a connection's request may take to arrive and its answer to
/// be taken.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the accepting thread waits for a connection before it looks
/// whether the relay closed.
const POLL: Duration = Duration::from_millis(200);

/// The socket of this process's relay: the queue service's directory for
/// sockets that do not fit under their queue, one per `dagq inbox`.
pub fn socket_path() -> PathBuf {
    super::queue_service::short_socket_dir().join(format!("inbox-{}.sock", std::process::id()))
}

/// [`socket_path`] with its directory made (this user's alone) and
/// resolved, before the agent's command names it: a Codex sandbox allows a
/// socket by its real path (`/tmp` is `/private/tmp` on macOS), which a
/// directory not made yet would not resolve to.
pub fn prepared_socket_path() -> Result<PathBuf> {
    let path = socket_path();
    let dir = path.parent().context("a socket has a directory")?;
    let name = path.file_name().context("a socket has a name")?;
    prepared_in(dir, name)
}

/// The socket `name` in `dir`, made if missing, by `dir`'s real path.
fn prepared_in(dir: &Path, name: &std::ffi::OsStr) -> Result<PathBuf> {
    super::queue_service::prepare_socket_dir(&dir.join(name))?;
    Ok(dir
        .canonicalize()
        .with_context(|| format!("resolve {}", dir.display()))?
        .join(name))
}

/// Where the guardrail's settings of an inbox for a control side are
/// written: `dagq/inbox` under the user's configuration (`XDG_CONFIG_HOME`,
/// else `~/.config`), as there is no queue's directory to put them in.
pub fn settings_dir() -> Result<PathBuf> {
    let credentials =
        super::credentials::FileCredentials::from_env(|name| std::env::var(name).ok())?;
    let config = credentials
        .directory
        .parent()
        .context("the user's configuration directory")?
        .join("inbox");
    fs::create_dir_all(&config).with_context(|| format!("create {}", config.display()))?;
    Ok(config)
}

pub use crate::application::inbox_relay::Answer;

/// The answer's line to the request's `line`, as the queue service's
/// socket reads one: one past [`MAX_REQUEST_BYTES`] or that does not read
/// is `bad_request`, and nothing is sent for it.
pub fn answer_line(line: &[u8], answer: &dyn Fn(&ServiceRequest) -> ServiceResponse) -> Vec<u8> {
    let response = if line.len() > MAX_REQUEST_BYTES {
        ServiceResponse::failure(ServiceErrorCode::BadRequest, "the request is too long")
    } else {
        match serde_json::from_slice::<ServiceRequest>(line) {
            Ok(request) => answer(&request),
            Err(error) => ServiceResponse::failure(
                ServiceErrorCode::BadRequest,
                format!("the request does not read: {error}"),
            ),
        }
    };
    let mut out = serde_json::to_vec(&response).unwrap_or_default();
    out.push(b'\n');
    out
}

/// A relay's socket, taking requests on a thread of its own until it is
/// closed (or dropped), when the socket is removed.
pub struct Relay {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    accepting: Option<thread::JoinHandle<()>>,
}

impl Relay {
    /// Open the relay at `path`, answering each request with `answer`.
    pub fn open(path: &Path, answer: Answer) -> Result<Self> {
        super::queue_service::prepare_socket_dir(path)?;
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
        }
        let listener =
            UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let accepting = thread::spawn(move || {
            while !stopped.load(SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let answer = Arc::clone(&answer);
                        thread::spawn(move || {
                            if let Err(error) = serve(stream, &*answer) {
                                tracing::warn!("the inbox's relay did not answer: {error:#}");
                            }
                        });
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        super::queue_service::wait_for_connection(&listener, POLL);
                    }
                    Err(_) => thread::sleep(POLL),
                }
            }
        });
        Ok(Self {
            path: path.to_path_buf(),
            stop,
            accepting: Some(accepting),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop taking requests and remove the socket.
    pub fn close(mut self) {
        self.shut();
    }

    fn shut(&mut self) {
        self.stop.store(true, SeqCst);
        if let Some(accepting) = self.accepting.take() {
            let _ = accepting.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.shut();
    }
}

impl crate::application::inbox_relay::OpenRelay for Relay {
    fn close(self: Box<Self>) {
        Relay::close(*self);
    }
}

/// Read one request from `stream`, answer it and close.
fn serve(
    stream: std::os::unix::net::UnixStream,
    answer: &(dyn Fn(&ServiceRequest) -> ServiceResponse + Send + Sync),
) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut line = Vec::new();
    BufReader::new(stream.try_clone()?)
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)?;
    (&stream).write_all(&answer_line(&line, answer))?;
    Ok(())
}

/// `then` with the person's store under its lock
/// ([`super::credentials::with_terminal_store`]): only the relay's parent
/// opens it.
fn with_store<T>(then: impl FnOnce(&dyn CredentialStore) -> Result<T>) -> Result<T> {
    super::credentials::with_terminal_store(then)
}

fn now() -> i64 {
    crate::application::unix_seconds(std::time::SystemTime::now())
}

/// The person of the store's set for the control side at `url`, read once
/// as the session opens: the session is bound to that person, and every
/// later read, issue, keeping and revocation checks the set is still
/// theirs ([`login::PERSON_CHANGED`]).
pub fn person(url: &str) -> Result<OidcIdentity> {
    with_store(|store| login::person_of(url, store))
}

/// The issue of the inbox's token and its keeping for `person` at `url`
/// ([`crate::application::inbox_relay::InboxIssue`]), under the lock every
/// `dagq inbox` of this user takes for it.
pub struct HostIssue<'a> {
    pub url: &'a str,
    pub person: &'a OidcIdentity,
}

impl crate::application::inbox_relay::InboxIssue for HostIssue<'_> {
    /// The person's store's [`StoreLock::Issue`](crate::application::login::StoreLock::Issue),
    /// where the store keeps its set: the same for every `dagq inbox` that
    /// keeps its token in that set, whatever its `XDG_CONFIG_HOME`.
    fn hold(&self) -> Result<Box<dyn std::any::Any>> {
        super::credentials::hold_terminal_store(crate::application::login::StoreLock::Issue)
    }

    /// The person's `issue_token` of their inbox with the person's token
    /// ([`super::credentials::PersonBearer`], renewed once), carrying
    /// `opened` at the open alone.
    fn issue(&self, opened: Option<&Value>) -> Result<InboxToken> {
        let principal = Principal::human_inbox(self.person.clone());
        let params = crate::application::inbox_relay::issue_params(&json!(principal), opened);
        let result = super::queue_service::person_call_with(
            &super::oidc_keys::HttpsDocumentSource::for_control(),
            &super::credentials::PersonBearer {
                person: self.person.clone(),
            },
            self.url,
            UseCase::IssueToken,
            params,
        )?;
        Ok(InboxToken {
            token: result["token"]
                .as_str()
                .filter(|token| !token.is_empty())
                .context("the control side's issue holds no token")?
                .to_owned(),
            expires_at: result["expires_at"]
                .as_i64()
                .context("the control side's issue holds no expiry")?,
        })
    }

    fn keep(&self, inbox: &InboxToken) -> Result<()> {
        with_store(|store| login::keep_inbox_token(self.url, self.person, inbox.clone(), store))
    }
}

/// A warning on the terminal, which may have hung up: a failed write is
/// left, never a panic that would skip the rest of the end.
fn warn(message: &str) {
    let _ = writeln!(std::io::stderr(), "warning: {message}");
}

/// The session's ports on this machine
/// ([`crate::application::inbox_relay::InboxSession`]): the control side at
/// `url` and the `person` the session was opened for, the relay's socket
/// at `relay` and the agent's `command`.
pub struct HostInboxSession<'a> {
    pub url: &'a str,
    pub person: &'a OidcIdentity,
    pub relay: &'a Path,
    pub command: &'a crate::application::CommandSpec,
}

impl crate::application::inbox_relay::InboxSession for HostInboxSession<'_> {
    fn catch_ending(&self) {
        catch_ending_signals();
    }
    fn hold(&self) -> Result<Box<dyn std::any::Any>> {
        crate::application::inbox_relay::InboxIssue::hold(&HostIssue {
            url: self.url,
            person: self.person,
        })
    }
    fn issue(&self, opened: &Value) -> Result<()> {
        let issuer = HostIssue {
            url: self.url,
            person: self.person,
        };
        crate::application::inbox_relay::issue_kept(&issuer, Some(opened)).map(drop)
    }
    fn open_relay(
        &self,
        answer: Answer,
    ) -> Result<Box<dyn crate::application::inbox_relay::OpenRelay>> {
        Ok(Box::new(Relay::open(self.relay, answer)?))
    }
    fn run_agent(&self) -> Result<i32> {
        run_in_foreground(self.command)
    }
    /// The person's `revoke_token` of their inbox with their own token
    /// (refused once the set is another person's): the control side
    /// revokes the inbox of the token's person, which holds one token at a
    /// time.
    fn revoke(&self) -> Result<()> {
        super::queue_service::person_call_with(
            &super::oidc_keys::HttpsDocumentSource::for_control(),
            &super::credentials::PersonBearer {
                person: self.person.clone(),
            },
            self.url,
            UseCase::RevokeToken,
            json!({"principal": Principal::human_inbox(self.person.clone())}),
        )
        .map(drop)
    }
    fn forget(&self) -> Result<()> {
        with_store(|store| login::forget_inbox_token(self.url, self.person, store))
    }
    fn warn(&self, message: &str) {
        warn(message);
    }
}

/// The inbox's token in the person's store for the relay's control side
/// ([`crate::application::inbox_relay::InboxTokens`]), read on each
/// request, for the person the session was opened for alone.
pub struct StoreTokens {
    pub url: String,
    pub person: OidcIdentity,
}

impl crate::application::inbox_relay::InboxTokens for StoreTokens {
    fn read(&self) -> Result<Option<String>> {
        with_store(|store| login::read_inbox_token(&self.url, &self.person, now(), store))
    }

    fn renew(&self, last: Option<&str>) -> Result<String> {
        let issuer = HostIssue {
            url: &self.url,
            person: &self.person,
        };
        crate::application::inbox_relay::renewed_kept(&issuer, &|| self.read(), last)
    }
}

/// The relay's sends to the control side at `url`
/// ([`super::queue_service::relay_send`]).
pub struct HttpRelaySend {
    pub url: String,
    pub http: super::oidc_keys::HttpsDocumentSource,
}

impl crate::application::inbox_relay::RelaySend for HttpRelaySend {
    fn send(&self, token: Option<&str>, request: &ServiceRequest) -> ServiceResponse {
        super::queue_service::relay_send(&self.http, &self.url, token, request)
    }
}

/// The signal that ends the session (`SIGHUP`, `SIGTERM`), 0 for none.
static ENDING: AtomicI32 = AtomicI32::new(0);

extern "C" fn note_ending(signal: libc::c_int) {
    ENDING.store(signal, SeqCst);
}

/// A `SIGINT` is the agent's: the foreground child takes it, and this
/// process stays. A handler, not `SIG_IGN`, so the child does not inherit
/// it ignored.
extern "C" fn stay(_: libc::c_int) {}

/// The exit code of a session whose agent exited with `code` or was ended
/// by `signal`, or that `ending` (a signal this process took) ended: the
/// agent's code, else the shell's `128 + signal`.
pub fn exit_code(code: Option<i32>, signal: Option<i32>, ending: i32) -> i32 {
    if ending != 0 {
        return 128 + ending;
    }
    code.unwrap_or_else(|| 128 + signal.unwrap_or(0))
}

/// Run `command` as the foreground child of this process, in its terminal
/// and with its streams (ADR-t2159-1 decision 2 keeps the inbox in the
/// person's terminal), and wait for it: a `SIGINT` goes to the child alone,
/// and a `SIGHUP` or `SIGTERM` is passed on to it and ends the wait, the
/// child killed when it has not ended within a few seconds. The exit code
/// ([`exit_code`]).
pub fn run_in_foreground(command: &crate::application::CommandSpec) -> Result<i32> {
    catch_ending_signals();
    // A signal taken before the agent starts ends the session: no agent.
    if let Some(code) = ended_before_start(ENDING.load(SeqCst)) {
        return Ok(code);
    }
    let mut child = super::process::command(command).spawn().with_context(|| {
        format!(
            "start the inbox's agent {}",
            command.get_program().to_string_lossy()
        )
    })?;
    use std::os::unix::process::ExitStatusExt;
    loop {
        // The signal is read after the child's state: a child that ended
        // on the signal this process took too still ends the session with
        // the signal's code.
        let exited = child
            .try_wait()?
            .map(|status| (status.code(), status.signal()));
        let ending = ENDING.load(SeqCst);
        match observe(exited, ending) {
            Observed::Exit(code) => return Ok(code),
            Observed::Wait => {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            Observed::Forward => {
                // SAFETY: kill(2) takes no pointer.
                unsafe { libc::kill(child.id() as libc::pid_t, ending) };
                for _ in 0..50 {
                    if child.try_wait()?.is_some() {
                        return Ok(exit_code(None, None, ending));
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
                return Ok(exit_code(None, None, ending));
            }
        }
    }
}

/// Take `SIGHUP` and `SIGTERM` as the end of the session ([`ENDING`]) and
/// leave `SIGINT` to the agent, from the session's start (before its token
/// is got), so a signal at any point ends it through its end's steps.
pub fn catch_ending_signals() {
    // SAFETY: the handlers only store to an atomic, which is
    // async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, stay as *const () as libc::sighandler_t);
        for signal in [libc::SIGHUP, libc::SIGTERM] {
            libc::signal(signal, note_ending as *const () as libc::sighandler_t);
        }
    }
}

/// The exit code of a session a signal (`ending`, 0 for none) ended before
/// its agent started, which then does not start.
pub fn ended_before_start(ending: i32) -> Option<i32> {
    (ending != 0).then(|| exit_code(None, None, ending))
}

/// What the wait of [`run_in_foreground`] does on one look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    /// Look again.
    Wait,
    /// End with this exit code ([`exit_code`]).
    Exit(i32),
    /// Pass the signal that ends the session on to the running child.
    Forward,
}

/// [`Observed`] after a look that found the child `exited` (its code and
/// signal) or running, with `ending` the signal this process took (0 for
/// none): an exit with a signal taken ends with the signal's code, never
/// the child's.
pub fn observe(exited: Option<(Option<i32>, Option<i32>)>, ending: i32) -> Observed {
    match (exited, ending) {
        (Some((code, signal)), ending) => Observed::Exit(exit_code(code, signal, ending)),
        (None, 0) => Observed::Wait,
        (None, _) => Observed::Forward,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request reads as the queue service's socket reads one: a line too
    /// long or that does not read is `bad_request` and reaches no answer;
    /// one that reads is answered, on one line.
    #[test]
    fn the_relay_takes_one_line_as_the_services_socket_does() {
        let asked = std::sync::Mutex::new(Vec::new());
        let answer = |request: &ServiceRequest| {
            asked.lock().unwrap().push(request.use_case);
            ServiceResponse::success(json!({"ok": 1}))
        };
        let line = serde_json::to_vec(&json!({
            "api_version": crate::domain::queue_service::API_VERSION,
            "use_case": "show",
            "params": {"id": 1},
        }))
        .unwrap();
        let out = answer_line(&line, &answer);
        assert!(out.ends_with(b"\n"));
        let response: ServiceResponse = serde_json::from_slice(&out).unwrap();
        assert!(response.ok);
        assert_eq!(*asked.lock().unwrap(), vec![UseCase::Show]);
        for bad in [b"not json".to_vec(), vec![b' '; MAX_REQUEST_BYTES + 1]] {
            let response: ServiceResponse =
                serde_json::from_slice(&answer_line(&bad, &answer)).unwrap();
            assert_eq!(response.error.unwrap().code, ServiceErrorCode::BadRequest);
        }
        assert_eq!(asked.lock().unwrap().len(), 1);
    }

    /// The session ends with the agent's code, the shell's `128 + signal`
    /// for an agent a signal ended, and that of the signal that ended this
    /// process's wait.
    #[test]
    fn the_session_ends_with_the_agents_code_or_the_signals() {
        assert_eq!(exit_code(Some(0), None, 0), 0);
        assert_eq!(exit_code(Some(3), None, 0), 3);
        assert_eq!(exit_code(None, Some(libc::SIGINT), 0), 130);
        assert_eq!(exit_code(Some(0), None, libc::SIGHUP), 129);
        assert_eq!(exit_code(None, None, libc::SIGTERM), 143);
        // A child that exited, seen with a signal taken: the signal's code.
        for (exited, ending, want) in [
            (Some((Some(0), None)), 0, Observed::Exit(0)),
            (Some((Some(0), None)), libc::SIGHUP, Observed::Exit(129)),
            (Some((Some(0), None)), libc::SIGTERM, Observed::Exit(143)),
            (
                Some((None, Some(libc::SIGTERM))),
                libc::SIGTERM,
                Observed::Exit(143),
            ),
            (None, 0, Observed::Wait),
            (None, libc::SIGTERM, Observed::Forward),
        ] {
            assert_eq!(observe(exited, ending), want, "{exited:?} {ending}");
        }
        // A signal taken before the agent starts: no agent, its code.
        assert_eq!(ended_before_start(0), None);
        assert_eq!(ended_before_start(libc::SIGHUP), Some(129));
        assert_eq!(ended_before_start(libc::SIGTERM), Some(143));
    }

    /// The relay's socket is named by its real path with its directory
    /// made, even when that directory was not there before.
    #[test]
    fn the_relays_socket_is_named_by_its_real_path_once_its_directory_is_made() {
        // Not in `$TMPDIR`, which may be too deep for a socket's path.
        let base = tempfile::tempdir_in("/tmp").unwrap();
        let dir = base.path().join("relay-dir");
        assert!(!dir.exists());
        let path = prepared_in(&dir, std::ffi::OsStr::new("inbox-1.sock")).unwrap();
        assert!(dir.is_dir());
        assert_eq!(
            path,
            base.path()
                .canonicalize()
                .unwrap()
                .join("relay-dir/inbox-1.sock")
        );
        assert_eq!(
            path.parent().unwrap().canonicalize().unwrap(),
            path.parent().unwrap()
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    /// The relay's socket fits a socket's path and is one per process.
    #[test]
    fn the_relays_socket_is_short_and_this_processs() {
        let path = socket_path();
        assert!(path.as_os_str().len() < 100, "{}", path.display());
        assert!(
            path.to_string_lossy()
                .ends_with(&format!("inbox-{}.sock", std::process::id()))
        );
    }
}
