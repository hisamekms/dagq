use super::*;
use std::sync::Mutex as StdMutex;

fn request(use_case: UseCase) -> ServiceRequest {
    ServiceRequest {
        caller: Some("someone".into()),
        api_version: crate::domain::queue_service::API_VERSION,
        token: Some("agent-token".into()),
        env_role: Some("inbox".into()),
        queue: Some("/q.db".into()),
        binding: Some("/repo/.git".into()),
        use_case,
        params: json!({"id": 1}),
    }
}

/// A control side's `store_unavailable` as the relay's send gives it: a
/// `failed` that keeps the code it named.
fn store_unavailable() -> ServiceResponse {
    let mut response = ServiceResponse::failure(
        ServiceErrorCode::Failed,
        "store_unavailable: the store is down",
    );
    response.error.as_mut().unwrap().relayed = true;
    response
}

fn refused() -> ServiceResponse {
    ServiceResponse::failure(ServiceErrorCode::Unauthenticated, "unknown_token")
}

/// The store as the relay reads it: what it holds each time it is read,
/// and the token a reissue gets.
struct Tokens {
    reads: StdMutex<Vec<Result<Option<String>, String>>>,
    reissued: StdMutex<Vec<Result<String, String>>>,
    asked: StdMutex<usize>,
}

impl Tokens {
    fn new(reads: &[Option<&str>], reissued: &[Result<&str, &str>]) -> Self {
        Self {
            reads: StdMutex::new(
                reads
                    .iter()
                    .rev()
                    .map(|read| Ok(read.map(str::to_owned)))
                    .collect(),
            ),
            reissued: StdMutex::new(
                reissued
                    .iter()
                    .rev()
                    .map(|got| got.map(str::to_owned).map_err(str::to_owned))
                    .collect(),
            ),
            asked: StdMutex::new(0),
        }
    }
}

impl InboxTokens for Tokens {
    fn read(&self) -> Result<Option<String>> {
        let mut reads = self.reads.lock().unwrap();
        // The last read stands for every later one.
        let read = if reads.len() > 1 {
            reads.pop().unwrap()
        } else {
            reads.last().cloned().unwrap_or(Ok(None))
        };
        read.map_err(anyhow::Error::msg)
    }
    /// The store read once more, as [`renewed_kept`] reads it under the
    /// lock, then a reissue when it still holds `last`.
    fn renew(&self, last: Option<&str>) -> Result<String> {
        match self.read()? {
            Some(now) if Some(now.as_str()) != last => Ok(now),
            _ => {
                *self.asked.lock().unwrap() += 1;
                self.reissued
                    .lock()
                    .unwrap()
                    .pop()
                    .expect("an unexpected reissue")
                    .map_err(anyhow::Error::msg)
            }
        }
    }
}

/// The control side: the token each request came with, and what it
/// answers to which token.
struct Control {
    sent: StdMutex<Vec<(Option<String>, ServiceRequest)>>,
    accepts: Vec<&'static str>,
}

impl RelaySend for Control {
    fn send(&self, token: Option<&str>, request: &ServiceRequest) -> ServiceResponse {
        self.sent
            .lock()
            .unwrap()
            .push((token.map(str::to_owned), request.clone()));
        match token {
            None if request.use_case == UseCase::Hello => ServiceResponse::success(json!({})),
            Some(token) if self.accepts.contains(&token) => {
                ServiceResponse::success(json!({"by": token}))
            }
            _ => refused(),
        }
    }
}

fn control(accepts: &[&'static str]) -> Control {
    Control {
        sent: StdMutex::new(Vec::new()),
        accepts: accepts.to_vec(),
    }
}

fn tokens_sent(control: &Control) -> Vec<Option<String>> {
    control
        .sent
        .lock()
        .unwrap()
        .iter()
        .map(|(token, _)| token.clone())
        .collect()
}

/// The relay drops what the agent put in the request (its token, role,
/// queue and binding) and sends the use case and params alone, with the
/// inbox's token as the `Bearer`; `hello` goes with no token.
#[test]
fn the_relay_drops_the_agents_token_role_queue_and_binding() {
    let relayed = relayed(&request(UseCase::Show));
    assert_eq!(relayed.token, None);
    assert_eq!(relayed.env_role, None);
    assert_eq!(relayed.queue, None);
    assert_eq!(relayed.binding, None);
    assert_eq!(relayed.caller, None);
    assert_eq!(relayed.use_case, UseCase::Show);
    assert_eq!(relayed.params, json!({"id": 1}));
    let side = control(&["inbox-1"]);
    let tokens = Tokens::new(&[Some("inbox-1")], &[]);
    let answer = relay(
        &request(UseCase::Show),
        &tokens,
        &side,
        &SessionGate::default(),
    );
    assert!(answer.ok);
    assert_eq!(answer.result, Some(json!({"by": "inbox-1"})));
    let sent = side.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0.as_deref(), Some("inbox-1"));
    assert_eq!(sent[0].1, relayed);
    drop(sent);
    let side = control(&[]);
    let hello = relay(
        &request(UseCase::Hello),
        &Tokens::new(&[Some("inbox-1")], &[]),
        &side,
        &SessionGate::default(),
    );
    assert!(hello.ok);
    assert_eq!(tokens_sent(&side), vec![None]);
}

/// The steps after a refusal of the token: the token the store holds now
/// when it differs from the one sent, else a new one, else `dagq login`'s
/// guide; an answer that is not a refusal is taken at any step.
#[test]
fn a_refused_token_is_read_again_then_got_again_then_the_login_is_asked_for() {
    use RelayNext::*;
    use RelayStep::*;
    for step in [Sent, Reread, Reissued] {
        assert_eq!(relay_next(step, false, Some("a"), Some("b")), Answer);
    }
    assert_eq!(
        relay_next(Sent, true, Some("a"), Some("b")),
        Resend("b".into())
    );
    assert_eq!(relay_next(Sent, true, None, Some("b")), Resend("b".into()));
    assert_eq!(relay_next(Sent, true, Some("a"), Some("a")), Reissue);
    assert_eq!(relay_next(Sent, true, Some("a"), None), Reissue);
    assert_eq!(relay_next(Sent, true, None, None), Reissue);
    assert_eq!(relay_next(Reread, true, None, None), Reissue);
    assert_eq!(relay_next(Reissued, true, None, None), Login);
    // Only an `unauthenticated` this client understands is a refusal of
    // the token: no answer and `store_unavailable` are not sent again.
    assert!(refuses_the_token(&refused()));
    for other in [
        ServiceResponse::failure(ServiceErrorCode::Unreachable, "no answer"),
        store_unavailable(),
        ServiceResponse::failure(ServiceErrorCode::AuthorizationDenied, "no"),
        ServiceResponse::success(json!({})),
    ] {
        assert!(!refuses_the_token(&other), "{other:?}");
    }
    let mut old = refused();
    old.api_version = 0;
    old.min_api_version = 0;
    assert!(!refuses_the_token(&old));
}

/// The relay's sends for one request, by what the store holds and what
/// the control side takes: each step once at most.
#[test]
fn the_relay_sends_again_once_per_step_and_stops_with_the_login_guide() {
    struct Case {
        reads: &'static [Option<&'static str>],
        reissued: &'static [Result<&'static str, &'static str>],
        accepts: &'static [&'static str],
        sent: &'static [Option<&'static str>],
        ok: bool,
        reissues: usize,
    }
    for (name, case) in [
        (
            "taken",
            Case {
                reads: &[Some("a")],
                reissued: &[],
                accepts: &["a"],
                sent: &[Some("a")],
                ok: true,
                reissues: 0,
            },
        ),
        (
            "another inbox of the person got a new one",
            Case {
                reads: &[Some("a"), Some("b")],
                reissued: &[],
                accepts: &["b"],
                sent: &[Some("a"), Some("b")],
                ok: true,
                reissues: 0,
            },
        ),
        (
            "the same one is still there",
            Case {
                reads: &[Some("a"), Some("a")],
                reissued: &[Ok("c")],
                accepts: &["c"],
                sent: &[Some("a"), Some("c")],
                ok: true,
                reissues: 1,
            },
        ),
        (
            "none, or it expired",
            Case {
                reads: &[None],
                reissued: &[Ok("c")],
                accepts: &["c"],
                sent: &[Some("c")],
                ok: true,
                reissues: 1,
            },
        ),
        (
            "the one read again is refused too",
            Case {
                reads: &[Some("a"), Some("b")],
                reissued: &[Ok("c")],
                accepts: &["c"],
                sent: &[Some("a"), Some("b"), Some("c")],
                ok: true,
                reissues: 1,
            },
        ),
        (
            "the new one is refused too",
            Case {
                reads: &[Some("a"), Some("a")],
                reissued: &[Ok("c")],
                accepts: &[],
                sent: &[Some("a"), Some("c")],
                ok: false,
                reissues: 1,
            },
        ),
        (
            "no new one",
            Case {
                reads: &[Some("a"), Some("a")],
                reissued: &[Err("the person's token did not renew")],
                accepts: &[],
                sent: &[Some("a")],
                ok: false,
                reissues: 1,
            },
        ),
    ] {
        let side = control(case.accepts);
        let tokens = Tokens::new(case.reads, case.reissued);
        let answer = relay(
            &request(UseCase::Show),
            &tokens,
            &side,
            &SessionGate::default(),
        );
        assert_eq!(answer.ok, case.ok, "{name}: {answer:?}");
        let expected: Vec<Option<String>> =
            case.sent.iter().map(|t| t.map(str::to_owned)).collect();
        assert_eq!(tokens_sent(&side), expected, "{name}");
        assert_eq!(*tokens.asked.lock().unwrap(), case.reissues, "{name}");
        if !case.ok {
            let error = answer.error.unwrap();
            assert_eq!(error.code, ServiceErrorCode::Unauthenticated, "{name}");
            assert!(error.message.contains("dagq login"), "{name}: {error:?}");
            assert!(!error.message.contains("inbox-secret"));
        }
    }
}

/// No set for the control side (after `logout`, or another's): the login's
/// guide with nothing sent; no answer and `store_unavailable` come back as
/// they are, sent once.
#[test]
fn a_missing_set_sends_nothing_and_no_answer_is_not_sent_again() {
    struct NoSet;
    impl InboxTokens for NoSet {
        fn read(&self) -> Result<Option<String>> {
            anyhow::bail!(crate::application::login::LOGIN_AGAIN)
        }
        fn renew(&self, _: Option<&str>) -> Result<String> {
            panic!("no reissue without a set")
        }
    }
    let side = control(&[]);
    let answer = relay(
        &request(UseCase::Show),
        &NoSet,
        &side,
        &SessionGate::default(),
    );
    assert_eq!(
        answer.error.unwrap().code,
        ServiceErrorCode::Unauthenticated
    );
    assert!(tokens_sent(&side).is_empty());
    struct Fails(ServiceResponse, StdMutex<usize>);
    impl RelaySend for Fails {
        fn send(&self, _: Option<&str>, _: &ServiceRequest) -> ServiceResponse {
            *self.1.lock().unwrap() += 1;
            self.0.clone()
        }
    }
    for failure in [
        ServiceResponse::failure(
            ServiceErrorCode::Unreachable,
            "the control side did not answer",
        ),
        store_unavailable(),
    ] {
        let side = Fails(failure.clone(), StdMutex::new(0));
        let answer = relay(
            &request(UseCase::Show),
            &Tokens::new(&[Some("a")], &[]),
            &side,
            &SessionGate::default(),
        );
        assert_eq!(answer, failure);
        assert_eq!(*side.1.lock().unwrap(), 1);
    }
}

/// Two requests refused at once ask for one new token: the second, which
/// read the old token before the first got a new one, finds the new one in
/// the store under the lock and sends with it.
#[test]
fn requests_refused_at_once_get_one_new_token() {
    struct Shared {
        held: StdMutex<String>,
        reads: StdMutex<usize>,
        asked: StdMutex<usize>,
    }
    impl InboxTokens for Shared {
        fn read(&self) -> Result<Option<String>> {
            let mut reads = self.reads.lock().unwrap();
            *reads += 1;
            // The second request's first two reads came before the
            // first's reissue.
            if (4..=5).contains(&*reads) {
                return Ok(Some("old".into()));
            }
            Ok(Some(self.held.lock().unwrap().clone()))
        }
        fn renew(&self, last: Option<&str>) -> Result<String> {
            match self.read()? {
                Some(now) if Some(now.as_str()) != last => Ok(now),
                _ => {
                    *self.asked.lock().unwrap() += 1;
                    *self.held.lock().unwrap() = "new".into();
                    Ok("new".into())
                }
            }
        }
    }
    let tokens = Shared {
        held: StdMutex::new("old".into()),
        reads: StdMutex::new(0),
        asked: StdMutex::new(0),
    };
    let side = control(&["new"]);
    let lock = SessionGate::default();
    for _ in 0..2 {
        assert!(relay(&request(UseCase::Show), &tokens, &side, &lock).ok);
    }
    assert_eq!(*tokens.asked.lock().unwrap(), 1);
    assert_eq!(
        tokens_sent(&side),
        vec![
            Some("old".into()),
            Some("new".into()),
            Some("old".into()),
            Some("new".into())
        ]
    );
}

/// The open's `issue_token` carries the launch and a renewal's none.
#[test]
fn the_opens_issue_alone_carries_the_launch() {
    let principal = json!({"role": "inbox"});
    let opened = json!({"provider": "codex", "model": null, "guardrail": true});
    assert_eq!(
        issue_params(&principal, Some(&opened)),
        json!({"principal": principal, "opened": opened})
    );
    assert_eq!(
        issue_params(&principal, None),
        json!({"principal": principal})
    );
}

/// `dagq inbox` for a control side opens nothing inside an inbox, and
/// nothing for another role's environment, whatever its name.
#[test]
fn only_the_users_environment_opens_an_inbox_for_a_control_side() {
    for role in [None, Some(""), Some(" "), Some("user")] {
        assert_eq!(open_refusal(role), None, "{role:?}");
    }
    assert_eq!(open_refusal(Some("inbox")), Some(OpenRefusal::Inside));
    for role in crate::domain::actor::ActorRole::ALL {
        let refused = open_refusal(Some(role.as_str()));
        match role {
            crate::domain::actor::ActorRole::User => assert_eq!(refused, None),
            crate::domain::actor::ActorRole::Inbox => {
                assert_eq!(refused, Some(OpenRefusal::Inside))
            }
            other => assert_eq!(
                refused,
                Some(OpenRefusal::Denied(other.as_str().to_owned()))
            ),
        }
    }
    assert_eq!(
        open_refusal(Some("someone")),
        Some(OpenRefusal::Denied("someone".into()))
    );
}

/// The agent's environment: the inbox's role and the relay's socket, and
/// none of the person's routes, queue, login or store, whatever the
/// terminal held.
#[test]
fn the_agents_environment_names_the_relay_and_nothing_of_the_persons() {
    let launch = crate::domain::actor_model::inbox_launch(
        &crate::domain::actor_model::RoleModels::default(),
        Some(crate::domain::Provider::Codex),
        None,
    )
    .unwrap();
    let env = agent_env(&launch, Path::new("/tmp/dagq-1/inbox-2.sock")).unwrap();
    let value = |name: &str| {
        env.iter()
            .find(|(named, _)| named == name)
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(value(ROLE_ENV), Some("inbox"));
    assert_eq!(value(INBOX_RELAY_ENV), Some("/tmp/dagq-1/inbox-2.sock"));
    assert!(value(crate::domain::sessions::LAUNCH_ENV).is_some());
    let removed = agent_env_removed([
        "DAGQ_LOGIN_SOMETHING_NEW".to_owned(),
        "DAGQ_LOGIN_ISSUER".to_owned(),
        "PATH".to_owned(),
    ]);
    for name in [
        CONTROL_URL_ENV,
        crate::application::lifecycle::QUEUE_ENV,
        CREDENTIAL_FILE_ENV,
        SOCKET_ENV,
        URL_ENV,
        "DAGQ_LOGIN_ISSUER",
        "DAGQ_LOGIN_CLIENT_ID",
        "DAGQ_LOGIN_SCOPE",
        "DAGQ_LOGIN_SOMETHING_NEW",
        "DAGQ_CREDENTIAL_STORE",
        "DAGQ_DB",
    ] {
        assert!(removed.iter().any(|named| named == name), "{name}");
        assert_eq!(value(name), None, "{name}");
    }
    assert!(!removed.iter().any(|named| named == "PATH"));
    assert_eq!(
        removed
            .iter()
            .filter(|named| *named == "DAGQ_LOGIN_ISSUER")
            .count(),
        1
    );
}

/// The session's ports as a log of what was done, each failing as asked.
#[derive(Default)]
struct Session {
    steps: Arc<StdMutex<Vec<&'static str>>>,
    fails: Vec<&'static str>,
    agent: i32,
    warned: StdMutex<Vec<String>>,
}

struct Closing(Arc<StdMutex<Vec<&'static str>>>);
impl OpenRelay for Closing {
    fn close(self: Box<Self>) {
        self.0.lock().unwrap().push("close");
    }
}

impl Session {
    fn failing(fails: &[&'static str], agent: i32) -> Self {
        Self {
            fails: fails.to_vec(),
            agent,
            ..Self::default()
        }
    }
    fn step(&self, name: &'static str) -> Result<()> {
        self.steps.lock().unwrap().push(name);
        if self.fails.contains(&name) {
            anyhow::bail!("{name} failed");
        }
        Ok(())
    }
    fn steps(&self) -> Vec<&'static str> {
        self.steps.lock().unwrap().clone()
    }
}

impl InboxSession for Session {
    fn catch_ending(&self) {
        self.steps.lock().unwrap().push("signals");
    }
    fn hold(&self) -> Result<Box<dyn std::any::Any>> {
        self.step("hold")?;
        Ok(Box::new(()))
    }
    fn issue(&self, _: &Value) -> Result<()> {
        self.step("issue")?;
        if self.fails.contains(&"kept") {
            return Err(IssuedNotKept {
                why: "the store did not save".into(),
            }
            .into());
        }
        Ok(())
    }
    fn open_relay(&self, _: Answer) -> Result<Box<dyn OpenRelay>> {
        self.step("open")?;
        Ok(Box::new(Closing(Arc::clone(&self.steps))))
    }
    fn run_agent(&self) -> Result<i32> {
        self.step("agent")?;
        Ok(self.agent)
    }
    fn revoke(&self) -> Result<()> {
        self.step("revoke")
    }
    fn forget(&self) -> Result<()> {
        self.step("forget")
    }
    fn warn(&self, message: &str) {
        self.warned.lock().unwrap().push(message.to_owned());
    }
}

fn run(session: &Session) -> Result<i32> {
    run_session(
        session,
        &json!({"provider": "claude"}),
        Arc::new(Tokens::new(&[Some("t")], &[])),
        Arc::new(control(&["t"])),
    )
}

/// The session, scene by scene: a token that cannot be got (no login, the
/// person's token not renewed) starts no relay and no agent and says to
/// log in; the agent's end, whatever its code (the shell's `128 + signal`
/// for a `SIGHUP` or `SIGTERM` that ended it), closes the relay, then
/// revokes and takes the token out of the store, and the code is the
/// agent's; a revocation or a clearing that fails is warned of, the rest
/// still done and the code kept; a relay that cannot open ends the session
/// before any agent.
#[test]
fn the_session_ends_by_revoking_and_forgetting_whatever_fails() {
    const END: [&str; 3] = ["hold", "revoke", "forget"];
    let all =
        |head: &[&'static str]| -> Vec<&'static str> { head.iter().copied().chain(END).collect() };
    let unopened = Session::failing(&["issue"], 0);
    let error = run(&unopened).unwrap_err();
    assert!(
        format!("{error:#}").contains(crate::application::login::LOGIN_AGAIN),
        "{error:#}"
    );
    assert_eq!(unopened.steps(), ["signals", "issue"]);

    // A token issued that the store did not take is revoked and taken out
    // before the open stops.
    let unkept = Session::failing(&["kept"], 0);
    assert!(run(&unkept).is_err());
    assert_eq!(unkept.steps(), all(&["signals", "issue"]));

    for code in [0, 3, 128 + 1, 128 + 15] {
        let session = Session::failing(&[], code);
        assert_eq!(run(&session).unwrap(), code);
        assert_eq!(
            session.steps(),
            all(&["signals", "issue", "open", "agent", "close"])
        );
        assert!(session.warned.lock().unwrap().is_empty());
    }

    for fails in [
        &["revoke"][..],
        &["forget"],
        &["revoke", "forget"],
        &["hold"],
    ] {
        let session = Session::failing(fails, 7);
        assert_eq!(run(&session).unwrap(), 7, "{fails:?}");
        assert_eq!(
            session.steps(),
            all(&["signals", "issue", "open", "agent", "close"])
        );
        assert_eq!(session.warned.lock().unwrap().len(), fails.len());
    }

    let unrelayed = Session::failing(&["open"], 0);
    assert!(run(&unrelayed).is_err());
    assert_eq!(unrelayed.steps(), all(&["signals", "issue", "open"]));

    let unstarted = Session::failing(&["agent"], 0);
    assert!(run(&unstarted).is_err());
    assert_eq!(
        unstarted.steps(),
        all(&["signals", "issue", "open", "agent", "close"])
    );
}

/// Once the session ended, a request is refused with nothing sent, and a
/// request under way that the control side refuses gets no new token.
#[test]
fn an_ended_session_gets_no_new_token() {
    let gate = SessionGate::default();
    gate.close();
    let tokens = Tokens::new(&[Some("t")], &[]);
    let side = control(&["t"]);
    let answer = relay(&request(UseCase::Show), &tokens, &side, &gate);
    assert_eq!(answer.error.unwrap().code, ServiceErrorCode::Unreachable);
    assert!(tokens_sent(&side).is_empty());

    /// Ends the session while the request is on its way, then refuses it.
    struct EndsMidway(Arc<SessionGate>);
    impl RelaySend for EndsMidway {
        fn send(&self, _: Option<&str>, _: &ServiceRequest) -> ServiceResponse {
            self.0.close();
            refused()
        }
    }
    let gate = Arc::new(SessionGate::default());
    let tokens = Tokens::new(&[Some("t")], &[]);
    let answer = relay(
        &request(UseCase::Show),
        &tokens,
        &EndsMidway(Arc::clone(&gate)),
        &gate,
    );
    assert_eq!(answer.error.unwrap().code, ServiceErrorCode::Unreachable);
    assert_eq!(*tokens.asked.lock().unwrap(), 0);
}

/// How long a test that starts threads may take, its joins too
/// (testing.md「待ちの上限」).
const TEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);
/// How long one step of such a test may wait for another thread.
const STEP_LIMIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Runs the test `test`'s `body` on a thread of its own and joins it; past
/// [`TEST_LIMIT`] the test fails naming itself and `what` it waited for.
/// A panic in `body` fails the test as it is.
fn within(test: &str, what: &str, body: impl FnOnce() + Send + 'static) {
    let (done, finished) = std::sync::mpsc::channel();
    let runner = std::thread::spawn(move || {
        body();
        let _ = done.send(());
    });
    if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = finished.recv_timeout(TEST_LIMIT) {
        panic!("{test}: {what} did not end within {TEST_LIMIT:?}");
    }
    if let Err(panic) = runner.join() {
        std::panic::resume_unwind(panic);
    }
}

/// A lock's guard that owns its mutex, as a file's lock does: a thread
/// holds `lock` until the guard drops, which joins that thread.
struct Held {
    release: Option<std::sync::mpsc::Sender<()>>,
    holder: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Held {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(holder) = self.holder.take() {
            let _ = holder.join();
        }
    }
}

/// Takes `lock` as [`Held`], waiting for it at most [`STEP_LIMIT`]; when it
/// is taken already, says so through `waits` first.
fn held(
    test: &str,
    lock: &Arc<StdMutex<()>>,
    waits: Option<&StdMutex<std::sync::mpsc::Sender<()>>>,
) -> Box<dyn std::any::Any> {
    use std::sync::mpsc;
    if let Some(waits) = waits
        && matches!(lock.try_lock(), Err(std::sync::TryLockError::WouldBlock))
    {
        waits.lock().unwrap().send(()).unwrap();
    }
    let lock = Arc::clone(lock);
    let (release, released) = mpsc::channel::<()>();
    let (taken, took) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _guard = lock.lock().unwrap();
        taken.send(()).unwrap();
        let _ = released.recv();
    });
    took.recv_timeout(STEP_LIMIT)
        .unwrap_or_else(|_| panic!("{test}: the lock within {STEP_LIMIT:?}"));
    Box::new(Held {
        release: Some(release),
        holder: Some(holder),
    })
}

/// The session's end waits for a reissue under way: the last revocation
/// comes after the token it got.
#[test]
fn the_end_waits_for_a_reissue_under_way() {
    const TEST: &str = "the_end_waits_for_a_reissue_under_way";
    within(TEST, "the relay, the end and their joins", || {
        use std::sync::mpsc;
        struct Blocking {
            entered: StdMutex<mpsc::Sender<()>>,
            go: StdMutex<mpsc::Receiver<()>>,
        }
        impl InboxTokens for Blocking {
            fn read(&self) -> Result<Option<String>> {
                Ok(Some("old".into()))
            }
            fn renew(&self, _: Option<&str>) -> Result<String> {
                self.entered.lock().unwrap().send(()).unwrap();
                self.go
                    .lock()
                    .unwrap()
                    .recv_timeout(STEP_LIMIT)
                    .unwrap_or_else(|_| panic!("{TEST}: the go within {STEP_LIMIT:?}"));
                Ok("new".into())
            }
        }
        let (entered, entering) = mpsc::channel();
        let (go, going) = mpsc::channel();
        let tokens = Arc::new(Blocking {
            entered: StdMutex::new(entered),
            go: StdMutex::new(going),
        });
        let gate = Arc::new(SessionGate::default());
        let side = Arc::new(control(&["new"]));
        let request_thread = {
            let (tokens, gate, side) = (Arc::clone(&tokens), Arc::clone(&gate), Arc::clone(&side));
            std::thread::spawn(move || relay(&request(UseCase::Show), &*tokens, &*side, &gate))
        };
        entering
            .recv_timeout(STEP_LIMIT)
            .unwrap_or_else(|_| panic!("{TEST}: the reissue within {STEP_LIMIT:?}"));
        // The reissue under way holds the session's lock, which the end
        // takes: the end cannot come before the token it gets.
        assert!(
            matches!(gate.0.try_lock(), Err(std::sync::TryLockError::WouldBlock)),
            "{TEST}: the reissue holds the session's lock"
        );
        let (closed, closing) = mpsc::channel();
        let close_thread = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                gate.close();
                closed.send(()).unwrap();
            })
        };
        assert!(
            closing.try_recv().is_err(),
            "{TEST}: the end did not wait for the reissue"
        );
        go.send(()).unwrap();
        closing
            .recv_timeout(STEP_LIMIT)
            .unwrap_or_else(|_| panic!("{TEST}: the end within {STEP_LIMIT:?}"));
        close_thread.join().unwrap();
        assert!(request_thread.join().unwrap().ok);
        assert_eq!(
            tokens_sent(&side),
            [Some("old".to_owned()), Some("new".to_owned())]
        );
    });
}

/// Two `dagq inbox` of the same person issuing at once: the issue and the
/// keeping go under one lock, so the token kept last is the one issued
/// last, even when the first issue's answer comes late.
#[test]
fn the_token_issued_last_is_the_one_kept() {
    const TEST: &str = "the_token_issued_last_is_the_one_kept";
    within(TEST, "the two issues and their joins", || {
        use std::sync::mpsc;
        struct Shared {
            lock: Arc<StdMutex<()>>,
            issued: StdMutex<usize>,
            kept: StdMutex<Vec<String>>,
        }
        struct One<'a> {
            shared: &'a Shared,
            late: Option<StdMutex<mpsc::Receiver<()>>>,
            entered: StdMutex<mpsc::Sender<()>>,
            waits: Option<StdMutex<mpsc::Sender<()>>>,
        }
        impl InboxIssue for One<'_> {
            fn hold(&self) -> Result<Box<dyn std::any::Any>> {
                Ok(held(TEST, &self.shared.lock, self.waits.as_ref()))
            }
            fn issue(&self, _: Option<&Value>) -> Result<crate::application::login::InboxToken> {
                let n = {
                    let mut issued = self.shared.issued.lock().unwrap();
                    *issued += 1;
                    *issued
                };
                self.entered.lock().unwrap().send(()).unwrap();
                if let Some(late) = &self.late {
                    late.lock()
                        .unwrap()
                        .recv_timeout(STEP_LIMIT)
                        .unwrap_or_else(|_| {
                            panic!("{TEST}: the late answer within {STEP_LIMIT:?}")
                        });
                }
                Ok(crate::application::login::InboxToken {
                    token: format!("t{n}"),
                    expires_at: 1,
                })
            }
            fn keep(&self, inbox: &crate::application::login::InboxToken) -> Result<()> {
                self.shared.kept.lock().unwrap().push(inbox.token.clone());
                Ok(())
            }
        }
        let shared = Arc::new(Shared {
            lock: Arc::new(StdMutex::new(())),
            issued: StdMutex::new(0),
            kept: StdMutex::new(Vec::new()),
        });
        let (go, late) = mpsc::channel();
        let (entered_a, entering_a) = mpsc::channel();
        let (entered_b, entering_b) = mpsc::channel();
        let (waits_b, waiting_b) = mpsc::channel();
        let first = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let one = One {
                    shared: &shared,
                    late: Some(StdMutex::new(late)),
                    entered: StdMutex::new(entered_a),
                    waits: None,
                };
                issue_kept(&one, None).unwrap()
            })
        };
        entering_a
            .recv_timeout(STEP_LIMIT)
            .unwrap_or_else(|_| panic!("{TEST}: the first issue within {STEP_LIMIT:?}"));
        let second = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let one = One {
                    shared: &shared,
                    late: None,
                    entered: StdMutex::new(entered_b),
                    waits: Some(StdMutex::new(waits_b)),
                };
                issue_kept(&one, None).unwrap()
            })
        };
        // The second finds the first's lock taken and waits for its
        // keeping: it issues nothing yet.
        waiting_b.recv_timeout(STEP_LIMIT).unwrap_or_else(|_| {
            panic!("{TEST}: the second issue finds the lock taken within {STEP_LIMIT:?}")
        });
        assert!(entering_b.try_recv().is_err());
        assert_eq!(*shared.issued.lock().unwrap(), 1);
        go.send(()).unwrap();
        assert_eq!(first.join().unwrap(), "t1");
        assert_eq!(second.join().unwrap(), "t2");
        assert_eq!(*shared.kept.lock().unwrap(), ["t1", "t2"]);
    });
}

/// A session's end holds the lock of the issues across its revocation and
/// its clearing: another `dagq inbox`'s issue and keeping wait for both,
/// so the token it keeps is neither revoked nor taken out by this end.
#[test]
fn the_end_revokes_and_forgets_under_the_issues_lock() {
    const TEST: &str = "the_end_revokes_and_forgets_under_the_issues_lock";
    within(TEST, "the end, the other issue and their joins", || {
        use std::sync::mpsc;
        /// The person's store and the control side, shared by two
        /// sessions: the valid token and the store's, and the lock of the
        /// issues.
        #[derive(Default)]
        struct Shared {
            lock: Arc<StdMutex<()>>,
            valid: StdMutex<Option<String>>,
            kept: StdMutex<Option<String>>,
            issued: StdMutex<usize>,
        }
        struct Ending<'a> {
            shared: &'a Shared,
            revoked: StdMutex<mpsc::Sender<()>>,
            go: StdMutex<mpsc::Receiver<()>>,
        }
        impl InboxSession for Ending<'_> {
            fn catch_ending(&self) {}
            fn hold(&self) -> Result<Box<dyn std::any::Any>> {
                Ok(held(TEST, &self.shared.lock, None))
            }
            fn issue(&self, _: &Value) -> Result<()> {
                unreachable!("this session only ends")
            }
            fn open_relay(&self, _: Answer) -> Result<Box<dyn OpenRelay>> {
                unreachable!("this session only ends")
            }
            fn run_agent(&self) -> Result<i32> {
                unreachable!("this session only ends")
            }
            fn revoke(&self) -> Result<()> {
                *self.shared.valid.lock().unwrap() = None;
                self.revoked.lock().unwrap().send(()).unwrap();
                self.go
                    .lock()
                    .unwrap()
                    .recv_timeout(STEP_LIMIT)
                    .unwrap_or_else(|_| panic!("{TEST}: the go within {STEP_LIMIT:?}"));
                Ok(())
            }
            fn forget(&self) -> Result<()> {
                *self.shared.kept.lock().unwrap() = None;
                Ok(())
            }
            fn warn(&self, message: &str) {
                panic!("{message}");
            }
        }
        /// The other session's issue: it says when it finds the lock
        /// taken, before it waits for it.
        struct Issuing<'a>(&'a Shared, StdMutex<mpsc::Sender<()>>);
        impl InboxIssue for Issuing<'_> {
            fn hold(&self) -> Result<Box<dyn std::any::Any>> {
                Ok(held(TEST, &self.0.lock, Some(&self.1)))
            }
            fn issue(&self, _: Option<&Value>) -> Result<crate::application::login::InboxToken> {
                *self.0.issued.lock().unwrap() += 1;
                *self.0.valid.lock().unwrap() = Some("b".into());
                Ok(crate::application::login::InboxToken {
                    token: "b".into(),
                    expires_at: 1,
                })
            }
            fn keep(&self, inbox: &crate::application::login::InboxToken) -> Result<()> {
                *self.0.kept.lock().unwrap() = Some(inbox.token.clone());
                Ok(())
            }
        }
        let shared = Arc::new(Shared::default());
        let (revoked, revoking) = mpsc::channel();
        let (go, going) = mpsc::channel();
        let ending = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let session = Ending {
                    shared: &shared,
                    revoked: StdMutex::new(revoked),
                    go: StdMutex::new(going),
                };
                end_session(&session, &SessionGate::default(), None)
            })
        };
        revoking
            .recv_timeout(STEP_LIMIT)
            .unwrap_or_else(|_| panic!("{TEST}: the revocation within {STEP_LIMIT:?}"));
        let (waits, waiting) = mpsc::channel();
        let issuing = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                issue_kept(&Issuing(&shared, StdMutex::new(waits)), None).unwrap()
            })
        };
        // Between the revocation and the clearing, the other issue waits.
        waiting.recv_timeout(STEP_LIMIT).unwrap_or_else(|_| {
            panic!("{TEST}: the other issue finds the lock taken within {STEP_LIMIT:?}")
        });
        assert_eq!(*shared.issued.lock().unwrap(), 0);
        go.send(()).unwrap();
        assert!(ending.join().unwrap());
        assert_eq!(issuing.join().unwrap(), "b");
        // The other session's token is valid and kept.
        assert_eq!(shared.valid.lock().unwrap().as_deref(), Some("b"));
        assert_eq!(shared.kept.lock().unwrap().as_deref(), Some("b"));
    });
}

/// Two `dagq inbox` of the same person, each with its own session's gate
/// and its own `XDG_CONFIG_HOME` (a file store's lock of its own) but the
/// one OS store, whose requests are refused at once: the issue lock is the
/// OS store's, so the second reads the store again only once it holds it,
/// after the first kept its new token, and sends with that one. One token
/// is issued, and neither request ends at the login's guide (an issue of
/// its own would revoke the first's).
#[test]
fn two_inboxes_of_a_person_refused_at_once_get_one_new_token() {
    const TEST: &str = "two_inboxes_of_a_person_refused_at_once_get_one_new_token";
    within(TEST, "the two relays and their joins", || {
        use crate::application::login::{
            CredentialStore, Credentials, InboxToken, StoreChoice, StoreHold, StoreLock, Stores,
        };
        use std::sync::mpsc;
        /// One process's store of the default choice: its lock (the OS
        /// store's, shared, or its config directory's file, its own), saying
        /// when it finds it taken (`waits`).
        struct Place {
            lock: Arc<StdMutex<()>>,
            waits: Option<StdMutex<mpsc::Sender<()>>>,
        }
        impl CredentialStore for Place {
            fn load(&self) -> Result<Option<Credentials>> {
                unreachable!("the shared set stands for the stores")
            }
            fn save(&self, _: &Credentials) -> Result<()> {
                unreachable!("the shared set stands for the stores")
            }
            fn delete(&self) -> Result<()> {
                unreachable!("the shared set stands for the stores")
            }
            fn hold(&self, lock: StoreLock) -> Result<StoreHold> {
                assert_eq!(lock, StoreLock::Issue);
                Ok(held(TEST, &self.lock, self.waits.as_ref()))
            }
        }
        /// The person's set in the OS store and that store's lock, shared
        /// by the two inboxes, and the tokens the control side issued: it
        /// takes the last alone (an issue revokes the one before it).
        struct Person {
            lock: Arc<StdMutex<()>>,
            kept: StdMutex<String>,
            issued: StdMutex<Vec<String>>,
        }
        /// One inbox's reads and issues: the first's issue answers once
        /// told to (`late`), and the second says when it finds the OS
        /// store's lock taken.
        struct Inbox {
            person: Arc<Person>,
            os: Place,
            file: Place,
            entered: Option<StdMutex<mpsc::Sender<()>>>,
            late: Option<StdMutex<mpsc::Receiver<()>>>,
        }
        impl InboxIssue for Inbox {
            fn hold(&self) -> Result<Box<dyn std::any::Any>> {
                Stores {
                    choice: StoreChoice::Auto,
                    os: &self.os,
                    file: &self.file,
                }
                .hold(StoreLock::Issue)
            }
            fn issue(&self, _: Option<&Value>) -> Result<InboxToken> {
                let token = {
                    let mut issued = self.person.issued.lock().unwrap();
                    let token = format!("t{}", issued.len() + 1);
                    issued.push(token.clone());
                    token
                };
                if let Some(entered) = &self.entered {
                    entered.lock().unwrap().send(()).unwrap();
                }
                if let Some(late) = &self.late {
                    late.lock()
                        .unwrap()
                        .recv_timeout(STEP_LIMIT)
                        .unwrap_or_else(|_| panic!("{TEST}: the go within {STEP_LIMIT:?}"));
                }
                Ok(InboxToken {
                    token,
                    expires_at: 1,
                })
            }
            fn keep(&self, inbox: &InboxToken) -> Result<()> {
                *self.person.kept.lock().unwrap() = inbox.token.clone();
                Ok(())
            }
        }
        impl InboxTokens for Inbox {
            fn read(&self) -> Result<Option<String>> {
                Ok(Some(self.person.kept.lock().unwrap().clone()))
            }
            fn renew(&self, last: Option<&str>) -> Result<String> {
                renewed_kept(self, &|| self.read(), last)
            }
        }
        /// The control side as one inbox reaches it, with the tokens its
        /// requests came with.
        struct Side(Arc<Person>, StdMutex<Vec<Option<String>>>);
        impl RelaySend for Side {
            fn send(&self, token: Option<&str>, _: &ServiceRequest) -> ServiceResponse {
                self.1.lock().unwrap().push(token.map(str::to_owned));
                let valid = self.0.issued.lock().unwrap().last().cloned();
                match token {
                    Some(token) if Some(token) == valid.as_deref() => {
                        ServiceResponse::success(json!({"by": token}))
                    }
                    _ => refused(),
                }
            }
        }
        let person = Arc::new(Person {
            lock: Arc::new(StdMutex::new(())),
            kept: StdMutex::new("t0".into()),
            issued: StdMutex::new(Vec::new()),
        });
        let (entered, entering) = mpsc::channel();
        let (go, late) = mpsc::channel();
        let (waits, waiting) = mpsc::channel();
        let start = |inbox: Inbox| {
            let side = Arc::new(Side(Arc::clone(&inbox.person), StdMutex::new(Vec::new())));
            let thread = {
                let side = Arc::clone(&side);
                std::thread::spawn(move || {
                    relay(
                        &request(UseCase::Show),
                        &inbox,
                        &*side,
                        &SessionGate::default(),
                    )
                })
            };
            (side, thread)
        };
        let own_file = || Place {
            lock: Arc::new(StdMutex::new(())),
            waits: None,
        };
        let (side_a, first) = start(Inbox {
            person: Arc::clone(&person),
            os: Place {
                lock: Arc::clone(&person.lock),
                waits: None,
            },
            file: own_file(),
            entered: Some(StdMutex::new(entered)),
            late: Some(StdMutex::new(late)),
        });
        entering
            .recv_timeout(STEP_LIMIT)
            .unwrap_or_else(|_| panic!("{TEST}: the first issue within {STEP_LIMIT:?}"));
        let (side_b, second) = start(Inbox {
            person: Arc::clone(&person),
            os: Place {
                lock: Arc::clone(&person.lock),
                waits: Some(StdMutex::new(waits)),
            },
            file: own_file(),
            entered: None,
            late: None,
        });
        // The second read the old token twice and was refused, and it waits
        // for the lock the first's issue holds before it reads again.
        waiting.recv_timeout(STEP_LIMIT).unwrap_or_else(|_| {
            panic!("{TEST}: the second finds the lock taken within {STEP_LIMIT:?}")
        });
        assert_eq!(*side_b.1.lock().unwrap(), [Some("t0".to_owned())]);
        go.send(()).unwrap();
        let (first, second) = (first.join().unwrap(), second.join().unwrap());
        assert_eq!(first.result, Some(json!({"by": "t1"})), "{first:?}");
        assert_eq!(second.result, Some(json!({"by": "t1"})), "{second:?}");
        assert_eq!(*person.issued.lock().unwrap(), ["t1"]);
        assert_eq!(*person.kept.lock().unwrap(), "t1");
        for side in [side_a, side_b] {
            assert_eq!(
                *side.1.lock().unwrap(),
                [Some("t0".to_owned()), Some("t1".to_owned())]
            );
        }
    });
}
