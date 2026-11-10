//! Runner host credentials at ~/.config/dagq/runner/credential.json. Ignore
//! XDG_CONFIG_HOME: these are runner host credentials, separate from login.
//! Reject symlinks and repository/run paths, including configured HOME aliases.
use crate::application::runner_enroll::{
    self as app, Credential, RunnerCommand, RunnerCredentialStore, RunnerExchange,
};
use crate::domain::queue_service::{API_VERSION, ServiceRequest, ServiceResponse, UseCase};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub struct FileCredential {
    pub directory: PathBuf,
}
/// Directory must deny every group/other permission; file must be exactly 0600.
pub fn private_mode(mode: u32, directory: bool) -> bool {
    if directory {
        mode & 0o7777 == 0o700
    } else {
        mode & 0o7777 == 0o600
    }
}
/// Refuse a checkout or any run-directory ancestor, even via a HOME alias.
pub fn private_location(path: &Path, in_worktree: bool) -> bool {
    !in_worktree && !path.components().any(|part| part.as_os_str() == "runs")
}
impl FileCredential {
    pub fn from_home(home: &Path) -> Result<Self> {
        ensure!(home.is_absolute(), "runner HOME must be absolute");
        Ok(Self {
            directory: home.join(".config/dagq/runner"),
        })
    }
    fn check_location(&self) -> Result<()> {
        ensure!(
            self.directory.is_absolute(),
            "runner credential location must be absolute"
        );
        // Canonicalize the closest existing ancestor, so a symlink above the
        // credential directory cannot redirect secrets into a checkout or run.
        let ancestor = self
            .directory
            .ancestors()
            .find(|p| p.exists())
            .ok_or_else(|| anyhow::anyhow!("runner home unavailable"))?
            .canonicalize()?;
        let in_worktree = ancestor.ancestors().any(|path| path.join(".git").exists());
        ensure!(
            private_location(&ancestor, in_worktree)
                && private_location(&self.directory, in_worktree),
            "runner credentials must be outside worktrees and run directories"
        );
        Ok(())
    }
    fn check_dir(&self) -> Result<()> {
        self.check_location()?;
        check_private_dir(&self.directory)
    }
}
fn check_private_dir(directory: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(directory)?;
    ensure!(
        meta.is_dir() && private_mode(meta.permissions().mode(), true),
        "runner credential directory must be private (0700)"
    );
    Ok(())
}
fn read_private(directory: &Path) -> Result<Credential> {
    check_private_dir(directory)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("credential.json"))?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && private_mode(meta.permissions().mode(), false),
        "runner credential file must have mode 0600"
    );
    let mut bytes = Vec::new();
    (&mut file).take(16385).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16384, "runner credential file too large");
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid runner credential; {}", app::ENROLL_AGAIN))
}
/// The file primitive is separate so atomic replacement can be checked in a
/// disposable directory; only FileCredential chooses a production location.
fn replace_private(directory: &Path, value: &Credential) -> Result<()> {
    replace_private_with(directory, value, |from, to| fs::rename(from, to))
}
fn replace_private_with(
    directory: &Path,
    value: &Credential,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    check_private_dir(directory)?;
    let temp = directory.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&serde_json::to_vec(value)?)?;
        file.sync_all()?;
        rename(&temp, &directory.join("credential.json"))?;
        fs::File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result.map_err(|_| anyhow::anyhow!("cannot save runner credential"))
}
impl RunnerCredentialStore for FileCredential {
    fn load(&self) -> Result<Credential> {
        self.check_dir()?;
        read_private(&self.directory)
    }
    fn save(&self, value: &Credential) -> Result<()> {
        self.check_location()?;
        replace_private(&self.directory, value)
    }
}
/// HTTPS only, with the numeric loopback exception shared with human login.
/// No bearer is used for the single-use enroll/refresh exchange. No redirects,
/// transport error bodies or secrets reach the CLI's error/log path.
pub struct HttpExchange;
impl RunnerExchange for HttpExchange {
    fn exchange(&self, url: &str, use_case: UseCase, params: Value) -> Result<ServiceResponse> {
        self.exchange_within(url, use_case, params, Duration::from_secs(30), &|| false)
    }
    fn exchange_within(
        &self,
        url: &str,
        use_case: UseCase,
        params: Value,
        within: Duration,
        give_up: &dyn Fn() -> bool,
    ) -> Result<ServiceResponse> {
        let url = url.to_owned();
        wait_or_give_up(
            move || {
                exchange_with(
                    &super::oidc_keys::HttpsDocumentSource::for_control(),
                    &url,
                    use_case,
                    params,
                    within,
                )
            },
            within,
            give_up,
        )
    }
}
const UNAVAILABLE: &str = "runner exchange unavailable; no credentials saved";

/// A waiting call's answer and its clock: the call's thread and the
/// monotonic clock in production ([`ThreadAnswer`]), values in tests.
pub(crate) trait AnswerSource<T> {
    /// How long since the call was sent.
    fn elapsed(&self) -> Duration;
    /// The answer if it comes within `at_most`, `None` if it has not yet.
    fn wait(&mut self, at_most: Duration) -> Option<Result<T>>;
}

/// Wait on `source` step by step as
/// [`crate::domain::runner_process::answer_wait`] decides: no wait longer
/// than the time left or [`crate::domain::runner_process::GIVE_UP_POLL`], and
/// an answer taken only if, once it came, the budget has not passed and
/// `give_up` does not hold.
pub(crate) fn await_answer<T>(
    source: &mut dyn AnswerSource<T>,
    within: Duration,
    give_up: &dyn Fn() -> bool,
) -> Result<T> {
    use crate::domain::runner_process::{AnswerWait, answer_wait};
    let mut answer = None;
    loop {
        match answer_wait(source.elapsed(), within, give_up(), answer.is_some()) {
            AnswerWait::Take => {
                return answer.unwrap_or_else(|| Err(anyhow::anyhow!(UNAVAILABLE)));
            }
            AnswerWait::GiveUp => anyhow::bail!(UNAVAILABLE),
            AnswerWait::Wait(at_most) => answer = source.wait(at_most),
        }
    }
}

/// The answer of a call on its own thread, on the monotonic clock.
struct ThreadAnswer<T> {
    started: std::time::Instant,
    answer: std::sync::mpsc::Receiver<Result<T>>,
}
impl<T> AnswerSource<T> for ThreadAnswer<T> {
    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
    fn wait(&mut self, at_most: Duration) -> Option<Result<T>> {
        use std::sync::mpsc::RecvTimeoutError;
        match self.answer.recv_timeout(at_most) {
            Ok(answer) => Some(answer),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Err(anyhow::anyhow!(UNAVAILABLE))),
        }
    }
}

/// Run `call` on its own thread and wait for its result as [`await_answer`]
/// decides. A call given up on is left to end on its thread (its own
/// timeout bounds it) and its result is dropped: for a refresh, a rotation
/// it may still commit is never saved, which the caller counts as an
/// unknown outcome.
pub(crate) fn wait_or_give_up<T: Send + 'static>(
    call: impl FnOnce() -> Result<T> + Send + 'static,
    within: Duration,
    give_up: &dyn Fn() -> bool,
) -> Result<T> {
    let (sender, answer) = std::sync::mpsc::channel();
    let started = std::time::Instant::now();
    std::thread::spawn(move || {
        let _ = sender.send(call());
    });
    await_answer(&mut ThreadAnswer { started, answer }, within, give_up)
}
fn exchange_with(
    http: &dyn super::queue_service::ControlHttp,
    url: &str,
    use_case: UseCase,
    params: Value,
    within: Duration,
) -> Result<ServiceResponse> {
    super::queue_service::check_control_url(url)?;
    let request = ServiceRequest {
        caller: None,
        api_version: API_VERSION,
        token: None,
        env_role: None,
        queue: None,
        binding: None,
        use_case,
        params,
    };
    super::queue_service::send_control(http, url, None, &request, within)
        .map_err(|_| anyhow::anyhow!("runner exchange unavailable; no credentials saved"))
}
/// Refuse before reading stdin/files or opening any local queue. The saved human
/// login, rather than a host deployment variable, supplies the issuer binding.
pub fn run(
    command: &RunnerCommand,
    db_named: bool,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Value> {
    let url = env("DAGQ_CONTROL_URL");
    let role = env("DAGQ_ROLE");
    let conflict = [
        "DAGQ_SERVICE_SOCKET",
        "DAGQ_SERVICE_URL",
        "DAGQ_SERVICE_CREDENTIAL_FILE",
    ]
    .iter()
    .any(|name| env(name).is_some_and(|v| !v.trim().is_empty()));
    let url =
        app::destination(url.as_deref(), conflict, db_named, role.as_deref()).map_err(|error| {
            super::queue_service::ClientFailure::new(
                if role.as_deref().is_some_and(|r| r != "user") {
                    "authorization_denied"
                } else {
                    "runner_configuration"
                },
                error.to_string(),
            )
        })?;
    super::queue_service::check_control_url(url)?;
    if let RunnerCommand::Enroll { file } = command {
        let home = env("HOME").ok_or_else(|| anyhow::anyhow!("runner HOME is required"))?;
        let store = FileCredential::from_home(Path::new(&home))?;
        store.check_location()?;
        let mut input: Box<dyn Read> = match app::source(file.as_deref()) {
            app::CodeSource::File(path) => Box::new(
                fs::File::open(path)
                    .map_err(|_| anyhow::anyhow!("cannot read registration code file"))?,
            ),
            app::CodeSource::Stdin => Box::new(std::io::stdin()),
        };
        let mut bytes = Vec::new();
        (&mut input)
            .take(4098)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow::anyhow!("cannot read registration code"))?;
        app::enroll(&HttpExchange, &store, url, &bytes)?;
        return Ok(json!({"enrolled":true}));
    }
    use crate::application::login::{CredentialStore as _, StoreChoice, Stores};
    let file = super::credentials::FileCredentials::from_env(&env)?;
    let stores = Stores {
        choice: StoreChoice::parse(env("DAGQ_CREDENTIAL_STORE").as_deref())?,
        os: &super::credentials::OsCredentials,
        file: &file,
    };
    let saved = stores
        .load()?
        .ok_or_else(|| anyhow::anyhow!("runner commands require an OIDC login; run dagq login"))?;
    app::human_issuer(Some(&saved.destination.issuer))?;
    let client = human_client(env, Arc::new(super::credentials::TerminalBearer))?;
    let (use_case, params) = command.request();
    client.call(use_case, params)
}

/// Human management uses the same terminal token source as the general CLI;
/// from_env only chooses a route, and deliberately attaches no credentials.
fn human_client(
    env: impl Fn(&str) -> Option<String>,
    bearer: Arc<dyn super::queue_service::BearerSource>,
) -> Result<super::queue_service::Client> {
    Ok(super::queue_service::Client::from_env(env)?
        .ok_or_else(|| anyhow::anyhow!("runner commands require DAGQ_CONTROL_URL"))?
        .with_bearer(bearer))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_human_runner_command_attaches_its_terminal_bearer() {
        use super::super::queue_service::{BearerSource, ClientRoute, ControlHttp, call_control};
        struct Token;
        impl BearerSource for Token {
            fn token(&self, url: &str, renew: bool) -> Result<String> {
                assert_eq!(url, "https://control");
                assert!(!renew);
                Ok("terminal-secret".into())
            }
        }
        struct Http(UseCase);
        impl ControlHttp for Http {
            fn post(
                &self,
                url: &str,
                bearer: Option<&str>,
                body: &[u8],
                _: Duration,
            ) -> Result<(u16, Vec<u8>)> {
                assert_eq!(
                    url,
                    format!("https://control/api/use-case/{}", self.0.as_str())
                );
                assert_eq!(bearer, Some("terminal-secret"));
                let request: ServiceRequest = serde_json::from_slice(body).unwrap();
                assert_eq!(request.use_case, self.0);
                assert!(request.token.is_none());
                assert!(
                    !std::str::from_utf8(body)
                        .unwrap()
                        .contains("terminal-secret")
                );
                Ok((
                    200,
                    serde_json::to_vec(&ServiceResponse::success(json!({"ok":true})))?,
                ))
            }
        }
        for command in [
            RunnerCommand::RegisterCode,
            RunnerCommand::List,
            RunnerCommand::Revoke {
                runner_id: "r".into(),
            },
            RunnerCommand::Share {
                runner_id: "r".into(),
                remove: false,
            },
            RunnerCommand::Share {
                runner_id: "r".into(),
                remove: true,
            },
        ] {
            // Use the production client factory, then the same control call
            // Client::call dispatches to, with a fake transport instead of TCP.
            let client = human_client(
                |name| (name == "DAGQ_CONTROL_URL").then(|| "https://control".into()),
                Arc::new(Token),
            )
            .unwrap();
            let ClientRoute::Control {
                url,
                bearer: Some(bearer),
            } = client.route
            else {
                panic!("human client must carry a bearer")
            };
            let (use_case, params) = command.request();
            let request = ServiceRequest {
                caller: None,
                api_version: API_VERSION,
                token: None,
                env_role: None,
                queue: None,
                binding: None,
                use_case,
                params,
            };
            assert!(
                call_control(
                    &Http(use_case),
                    &url,
                    Some(bearer.0.as_ref()),
                    &request,
                    Duration::from_secs(30)
                )
                .unwrap()
                .ok
            );
        }
    }
    /// An answer that comes `at` on a clock that moves only by the waits.
    struct FakeAnswer {
        now: std::rc::Rc<std::cell::Cell<Duration>>,
        at: Duration,
        answer: Option<Result<u8>>,
        /// Each wait: when it began and how long it could last.
        waits: Vec<(Duration, Duration)>,
    }
    impl AnswerSource<u8> for FakeAnswer {
        fn elapsed(&self) -> Duration {
            self.now.get()
        }
        fn wait(&mut self, at_most: Duration) -> Option<Result<u8>> {
            let now = self.now.get();
            self.waits.push((now, at_most));
            if self.at <= now + at_most {
                self.now.set(self.at.max(now));
                self.answer.take()
            } else {
                self.now.set(now + at_most);
                None
            }
        }
    }
    fn awaited(
        at_ms: u64,
        answer: Result<u8>,
        within_ms: u64,
        give_up_from_ms: Option<u64>,
    ) -> (Result<u8>, Duration, Vec<(Duration, Duration)>) {
        let ms = Duration::from_millis;
        let now = std::rc::Rc::new(std::cell::Cell::new(Duration::ZERO));
        let mut source = FakeAnswer {
            now: now.clone(),
            at: ms(at_ms),
            answer: Some(answer),
            waits: Vec::new(),
        };
        let clock = now.clone();
        let give_up = move || give_up_from_ms.is_some_and(|from| clock.get() >= ms(from));
        let result = await_answer(&mut source, ms(within_ms), &give_up);
        (result, now.get(), source.waits)
    }

    #[test]
    fn a_waiting_call_takes_only_an_answer_before_its_budget_and_its_give_up() {
        let ms = Duration::from_millis;
        let (answer, at, waits) = awaited(50, Ok(7), 1_000, None);
        assert_eq!(answer.unwrap(), 7);
        assert_eq!((at, waits), (ms(50), vec![(ms(0), ms(100))]));
        assert_eq!(awaited(999, Ok(7), 1_000, None).0.unwrap(), 7);
        for late in [1_000, 1_050] {
            let (answer, at, _) = awaited(late, Ok(7), 1_000, None);
            assert_eq!(
                answer.err().unwrap().to_string(),
                UNAVAILABLE,
                "at {late} ms"
            );
            assert_eq!(at, ms(late.min(1_000)));
        }
        // No wait outlasts the time left: a 1 ms budget waits 1 ms.
        let (answer, at, waits) = awaited(50, Ok(7), 1, None);
        assert!(answer.is_err());
        assert_eq!((at, waits), (ms(1), vec![(ms(0), ms(1))]));
        let (_, _, waits) = awaited(5_000, Ok(7), 1_050, None);
        assert!(waits.iter().all(|(from, most)| *from + *most <= ms(1_050)));
        assert_eq!(waits.last(), Some(&(ms(1_000), ms(50))));
        // Given up when asked, an answer that comes then included.
        let (answer, at, _) = awaited(500, Ok(7), 30_000, Some(300));
        assert!(answer.is_err());
        assert_eq!(at, ms(300));
        assert!(awaited(300, Ok(7), 30_000, Some(300)).0.is_err());
        // A failure that came in time is the call's own, scrubbed message.
        let failed = awaited(10, Err(anyhow::anyhow!("scrubbed")), 1_000, None).0;
        assert_eq!(failed.err().unwrap().to_string(), "scrubbed");
    }

    #[test]
    fn exchange_uses_https_without_bearer_and_scrubs_transport_errors() {
        use std::cell::Cell;
        struct Http(Cell<usize>);
        impl super::super::queue_service::ControlHttp for Http {
            fn post(
                &self,
                url: &str,
                bearer: Option<&str>,
                body: &[u8],
                timeout: Duration,
            ) -> Result<(u16, Vec<u8>)> {
                self.0.set(self.0.get() + 1);
                assert_eq!(timeout, Duration::from_secs(7), "the caller's budget");
                assert_eq!(url, "https://control/api/use-case/runner_enroll");
                assert!(bearer.is_none());
                let request: ServiceRequest = serde_json::from_slice(body).unwrap();
                assert!(
                    request.token.is_none()
                        && request.env_role.is_none()
                        && request.queue.is_none()
                );
                assert_eq!(request.params["code"], "sensitive-code");
                anyhow::bail!("sensitive-code")
            }
        }
        let http = Http(Cell::new(0));
        let within = Duration::from_secs(7);
        assert!(
            exchange_with(
                &http,
                "http://evil",
                UseCase::RunnerEnroll,
                json!({}),
                within
            )
            .is_err()
        );
        assert_eq!(http.0.get(), 0);
        let error = exchange_with(
            &http,
            "https://control",
            UseCase::RunnerEnroll,
            json!({"code":"sensitive-code"}),
            within,
        )
        .err()
        .unwrap();
        assert_eq!(http.0.get(), 1);
        assert!(!format!("{error:#}").contains("sensitive-code"));
    }
    #[test]
    fn mode_judgment_refuses_public_executable_and_special_files() {
        for mode in [0o644, 0o640, 0o660, 0o700, 0o4600] {
            assert!(!private_mode(mode, false));
        }
        assert!(private_mode(0o100600, false));
        assert!(private_mode(0o40700, true));
        for mode in [0o755, 0o750, 0o770, 0o1700] {
            assert!(!private_mode(mode, true));
        }
    }
    #[test]
    fn atomic_file_rotation_keeps_complete_private_refresh_without_short_token() {
        let temp = tempfile::tempdir().unwrap();
        let store = FileCredential::from_home(temp.path()).unwrap();
        let value = |refresh: &str| Credential {
            control_url: "https://control".into(),
            runner_id: "runner".into(),
            refresh: refresh.into(),
        };
        replace_private(&store.directory, &value("old")).unwrap();
        assert!(
            replace_private_with(&store.directory, &value("interrupted"), |from, to| {
                let pending: Credential = serde_json::from_slice(&fs::read(from)?).unwrap();
                let old: Credential = serde_json::from_slice(&fs::read(to)?).unwrap();
                assert_eq!(pending.refresh, "interrupted");
                assert_eq!(old.refresh, "old");
                Err(std::io::Error::other("interrupted before rename"))
            })
            .is_err()
        );
        assert_eq!(read_private(&store.directory).unwrap().refresh, "old");
        replace_private(&store.directory, &value("new")).unwrap();
        assert_eq!(read_private(&store.directory).unwrap().refresh, "new");
        let file = store.directory.join("credential.json");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            fs::metadata(&store.directory).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        assert_eq!(fs::read_dir(&store.directory).unwrap().count(), 1);
        fs::set_permissions(&store.directory, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(replace_private(&store.directory, &value("broken")).is_err());
        fs::set_permissions(&store.directory, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(read_private(&store.directory).unwrap().refresh, "new");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private(&store.directory).is_err());
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(temp.path().join("elsewhere"), &file).unwrap();
        assert!(read_private(&store.directory).is_err());
    }
    #[test]
    fn locations_exclude_worktrees_and_run_paths() {
        assert!(private_location(
            Path::new("/home/person/.config/dagq/runner"),
            false
        ));
        assert!(!private_location(
            Path::new("/repo/.config/dagq/runner"),
            true
        ));
        assert!(!private_location(
            Path::new("/data/dagq/queue/runs/id/tmp"),
            false
        ));
    }
    #[test]
    fn no_destination_or_nonhuman_reads_no_input_or_credentials_and_sends_nothing() {
        for command in [
            RunnerCommand::RegisterCode,
            RunnerCommand::List,
            RunnerCommand::Revoke {
                runner_id: "r".into(),
            },
            RunnerCommand::Share {
                runner_id: "r".into(),
                remove: false,
            },
            RunnerCommand::Enroll {
                file: Some("nonexistent".into()),
            },
        ] {
            assert!(
                run(&command, false, |_| None)
                    .unwrap_err()
                    .to_string()
                    .contains("DAGQ_CONTROL_URL")
            );
            assert!(
                run(&command, false, |n| match n {
                    "DAGQ_CONTROL_URL" => Some("https://control".into()),
                    "DAGQ_ROLE" => Some("worker".into()),
                    _ => None,
                })
                .unwrap_err()
                .to_string()
                .contains("authorization_denied")
            );
        }
    }
}
