//! A client-mode `dagq` to a control side (`DAGQ_CONTROL_URL`): the route
//! the environment names, and a call's sends, token reads and refusals over
//! a fake HTTP and fake token sources. No test opens a credential store of
//! the OS or reaches a network.

use std::cell::RefCell;
use std::sync::Mutex;

use super::*;
use crate::domain::queue_service::{CONTROL_URL_ENV, CREDENTIAL_FILE_ENV, SOCKET_ENV, URL_ENV};

const TOKEN: &str = "dagq-secret-token";
const RENEWED: &str = "dagq-renewed-token";

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    move |name| {
        pairs
            .iter()
            .find(|(named, _)| named == name)
            .map(|(_, value)| value.clone())
    }
}

/// What the fake HTTP was given: the URL, the bearer and the body.
type Sent = (String, Option<String>, Value);
/// What the fake HTTP answers one post: the status and the body.
type Posted = Result<(u16, Vec<u8>)>;

/// An HTTP that answers each post with the next of `answers`, an `Err`
/// for an answer that did not come.
struct FakeHttp {
    answers: RefCell<Vec<Posted>>,
    sent: RefCell<Vec<Sent>>,
}

impl FakeHttp {
    fn answering(answers: Vec<Posted>) -> Self {
        Self {
            answers: RefCell::new(answers),
            sent: RefCell::new(Vec::new()),
        }
    }
    fn sent(&self) -> Vec<Sent> {
        self.sent.borrow().clone()
    }
}

impl ControlHttp for FakeHttp {
    fn post(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: &[u8],
        _: Duration,
    ) -> Result<(u16, Vec<u8>)> {
        self.sent.borrow_mut().push((
            url.to_owned(),
            bearer.map(str::to_owned),
            serde_json::from_slice(body).unwrap(),
        ));
        let mut answers = self.answers.borrow_mut();
        assert!(!answers.is_empty(), "a request past the answers was sent");
        answers.remove(0)
    }
}

fn answer(response: &ServiceResponse) -> Result<(u16, Vec<u8>)> {
    Ok((200, serde_json::to_vec(response).unwrap()))
}

fn unauthenticated() -> Result<(u16, Vec<u8>)> {
    let refused = ServiceResponse::failure(ServiceErrorCode::Unauthenticated, "no such token");
    Ok((401, serde_json::to_vec(&refused).unwrap()))
}

/// A token source that gives [`TOKEN`], then [`RENEWED`] when renewing,
/// or fails as `fails` says, and records what it was asked.
struct FakeBearer {
    asked: Mutex<Vec<(String, bool)>>,
    fails: Option<bool>,
    /// The token it holds has expired: its first read renews it, gives
    /// [`RENEWED`] and says so.
    expired: bool,
}

impl FakeBearer {
    fn giving() -> Self {
        Self {
            asked: Mutex::new(Vec::new()),
            fails: None,
            expired: false,
        }
    }
    /// Failing when asked to renew (`true`) or always (`false`).
    fn failing(on_renewal_only: bool) -> Self {
        Self {
            asked: Mutex::new(Vec::new()),
            fails: Some(on_renewal_only),
            expired: false,
        }
    }
    fn expired() -> Self {
        Self {
            expired: true,
            ..Self::giving()
        }
    }
    fn asked(&self) -> Vec<(String, bool)> {
        self.asked.lock().unwrap().clone()
    }
}

impl BearerSource for FakeBearer {
    fn token(&self, url: &str, renew: bool) -> Result<String> {
        self.asked.lock().unwrap().push((url.to_owned(), renew));
        match self.fails {
            Some(on_renewal_only) if renew || !on_renewal_only => {
                bail!(crate::application::login::LOGIN_AGAIN)
            }
            _ => Ok(if renew || self.expired {
                RENEWED
            } else {
                TOKEN
            }
            .to_owned()),
        }
    }
    fn token_read(&self, url: &str, renew: bool) -> Result<(String, bool)> {
        self.token(url, renew)
            .map(|token| (token, renew || self.expired))
    }
}

fn show() -> ServiceRequest {
    control_request(UseCase::Show, json!({"id": 7}))
}

fn call(http: &FakeHttp, bearer: Option<&dyn BearerSource>) -> Result<ServiceResponse> {
    call_control(
        http,
        "https://control.example/",
        bearer,
        &show(),
        CLIENT_TIMEOUT,
    )
    .map_err(|error| control_failure(error, UseCase::Show, NEXT_CONTROL_URL))
}

fn code_of(error: &anyhow::Error) -> String {
    error
        .downcast_ref::<ClientFailure>()
        .map(|failure| failure.code.clone())
        .unwrap_or_else(|| format!("not a client failure: {error:#}"))
}

/// The environment's control side is the route alone: an HTTPS URL, or
/// HTTP on a numeric loopback address, with no token's file and no other
/// route beside it, and no bearer until the command line attaches one.
#[test]
fn a_control_url_is_the_route_alone_and_only_https_or_loopback_http() {
    for url in [
        "https://control.example",
        "https://control.example:8443/team/",
        "http://127.0.0.1:8080",
        "http://[::1]:8080/",
    ] {
        let client = Client::from_env(env_of(&[(CONTROL_URL_ENV, url), ("DAGQ_ROLE", "worker")]))
            .unwrap()
            .unwrap();
        assert_eq!(
            client.route,
            ClientRoute::Control {
                url: url.to_owned(),
                bearer: None
            }
        );
        assert!(client.credential.is_none() && client.queue.is_none());
        assert!(client.binding.is_none());
        assert_eq!(client.env_role.as_deref(), Some("worker"));
    }
    // HTTP off loopback, a host that only starts as one, a userinfo that
    // names one, a name the resolver decides, a fragment: none is a route.
    for url in [
        "http://control.example",
        "http://127.0.0.1@evil.example",
        "http://localhost.evil.example",
        "http://127.0.0.1.evil.example",
        "http://localhost:8080",
        "https://person@control.example",
        "https://control.example/#x",
        "https://control.example/?x=1",
        "https://control.example/team?x=1",
        "http://127.0.0.1:8080?x",
        "ftp://control.example",
        "not a url",
    ] {
        let refused = Client::from_env(env_of(&[(CONTROL_URL_ENV, url)])).unwrap_err();
        assert_eq!(refused.code, "client_misconfigured", "{url}");
        assert!(refused.message.contains(CONTROL_URL_ENV), "{url}");
    }
}

/// Beside a socket, a broker or a token's file, a control side refuses
/// every command: `dagq` chooses neither the route nor the token, whatever
/// the URL is.
#[test]
fn a_control_url_beside_another_route_or_a_token_file_is_misconfigured() {
    for other in [SOCKET_ENV, URL_ENV, CREDENTIAL_FILE_ENV] {
        for url in ["https://control.example", "http://control.example"] {
            let value = if other == URL_ENV {
                "http://127.0.0.1:9"
            } else {
                "/x"
            };
            let refused =
                Client::from_env(env_of(&[(CONTROL_URL_ENV, url), (other, value)])).unwrap_err();
            assert_eq!(refused.code, "client_misconfigured");
            assert!(refused.message.contains(other), "{}", refused.message);
        }
    }
    // A blank one is none, and with no control side named the routes are
    // as they were: a stored login alone sends nothing anywhere.
    assert!(matches!(
        Client::from_env(env_of(&[(CONTROL_URL_ENV, "https://c"), (SOCKET_ENV, " ")]))
            .unwrap()
            .unwrap()
            .route,
        ClientRoute::Control { .. }
    ));
    assert_eq!(
        Client::from_env(env_of(&[(CONTROL_URL_ENV, " ")])),
        Ok(None)
    );
    assert_eq!(
        Client::from_env(env_of(&[(CREDENTIAL_FILE_ENV, "/c")])),
        Ok(None)
    );
}

/// A call carries the use case alone to its path, with the token as the
/// bearer and nothing of it in the body, and takes the answer as the
/// socket's.
#[test]
fn a_call_goes_to_the_use_case_path_with_the_bearer_alone() {
    let http = FakeHttp::answering(vec![answer(&ServiceResponse::success(json!({"id": 7})))]);
    let bearer = FakeBearer::giving();
    let answered = call(&http, Some(&bearer)).unwrap();
    assert_eq!(answer_of(answered).unwrap(), json!({"id": 7}));
    let sent = http.sent();
    assert_eq!(sent.len(), 1);
    let (url, token, body) = &sent[0];
    assert_eq!(url, "https://control.example/api/use-case/show");
    assert_eq!(token.as_deref(), Some(TOKEN));
    assert_eq!(body, &serde_json::to_value(show()).unwrap());
    for field in ["token", "env_role", "queue", "binding", "caller"] {
        assert!(body.get(field).is_none(), "{field}");
    }
    assert_eq!(body["api_version"], API_VERSION);
    assert_eq!(
        bearer.asked(),
        vec![("https://control.example/".to_owned(), false)]
    );
}

/// An `unauthenticated` gets one renewal and one send again; a second is
/// the login's guide, with nothing sent a third time.
#[test]
fn an_unauthenticated_answer_renews_once_and_sends_once_more() {
    let http = FakeHttp::answering(vec![
        unauthenticated(),
        answer(&ServiceResponse::success(json!({"ok": 1}))),
    ]);
    let bearer = FakeBearer::giving();
    assert!(call(&http, Some(&bearer)).unwrap().ok);
    let tokens: Vec<_> = http.sent().into_iter().map(|(_, token, _)| token).collect();
    assert_eq!(tokens, vec![Some(TOKEN.into()), Some(RENEWED.into())]);
    assert_eq!(
        bearer
            .asked()
            .into_iter()
            .map(|(_, renew)| renew)
            .collect::<Vec<_>>(),
        vec![false, true]
    );

    let http = FakeHttp::answering(vec![unauthenticated(), unauthenticated()]);
    let bearer = FakeBearer::giving();
    let refused = call(&http, Some(&bearer)).unwrap_err();
    assert_eq!(code_of(&refused), "unauthenticated");
    assert!(format!("{refused:#}").contains("dagq login"));
    assert_eq!(http.sent().len(), 2);
    assert_eq!(bearer.asked().len(), 2);

    // A renewal that cannot be had sends nothing more.
    let http = FakeHttp::answering(vec![unauthenticated()]);
    let refused = call(&http, Some(&FakeBearer::failing(true))).unwrap_err();
    assert_eq!(code_of(&refused), "no_credential");
    assert_eq!(http.sent().len(), 1);

    // With no token sent, the refusal is the answer.
    let http = FakeHttp::answering(vec![unauthenticated()]);
    let unsent = call(&http, None).unwrap();
    assert_eq!(
        answer_of(unsent)
            .unwrap_err()
            .downcast_ref::<ClientFailure>()
            .unwrap()
            .code,
        "unauthenticated"
    );
    assert_eq!(http.sent()[0].1, None);
}

/// The relay's parent sends the person's token for the inbox's
/// `issue_token` (at the open and after a refusal) and its `revoke_token`
/// (at the end) alone: in each, a refused or expired person's token is
/// renewed once in all (an expiry's renewal at the first read is that one)
/// and the request sent once more; a renewal that cannot be had, or a
/// refusal of the renewed token, is the failure the open stops with (the
/// login's guide) and the end only warns of, nothing sent again.
#[test]
fn the_persons_token_is_renewed_once_for_the_inbox_s_issue_and_revocation() {
    for use_case in [UseCase::IssueToken, UseCase::RevokeToken] {
        let call = |http: &FakeHttp, bearer: &FakeBearer| {
            person_call_with(
                http,
                bearer,
                "https://control.example/",
                use_case,
                json!({"principal": {}}),
            )
        };
        let http = FakeHttp::answering(vec![
            unauthenticated(),
            answer(&ServiceResponse::success(json!({"ok": 1}))),
        ]);
        let bearer = FakeBearer::giving();
        assert_eq!(
            call(&http, &bearer).unwrap(),
            json!({"ok": 1}),
            "{use_case:?}"
        );
        let sent: Vec<_> = http.sent().into_iter().map(|(_, token, _)| token).collect();
        assert_eq!(sent, vec![Some(TOKEN.into()), Some(RENEWED.into())]);
        let renewals: Vec<_> = bearer.asked().into_iter().map(|(_, renew)| renew).collect();
        assert_eq!(renewals, vec![false, true]);

        let http = FakeHttp::answering(vec![unauthenticated(), unauthenticated()]);
        let refused = call(&http, &FakeBearer::giving()).unwrap_err();
        assert!(format!("{refused:#}").contains("dagq login"), "{refused:#}");
        assert_eq!(http.sent().len(), 2);

        let http = FakeHttp::answering(vec![unauthenticated()]);
        let unrenewed = call(&http, &FakeBearer::failing(true)).unwrap_err();
        assert_eq!(code_of(&unrenewed), "no_credential");
        assert_eq!(http.sent().len(), 1);

        // An expired token renewed at the first read is the one renewal:
        // its refusal asks for the login (the open stops with its guide,
        // the end warns of it), with no second renewal and nothing sent
        // again.
        let http = FakeHttp::answering(vec![unauthenticated()]);
        let bearer = FakeBearer::expired();
        let refused = call(&http, &bearer).unwrap_err();
        assert_eq!(code_of(&refused), "unauthenticated", "{use_case:?}");
        assert!(format!("{refused:#}").contains("dagq login"), "{refused:#}");
        let sent: Vec<_> = http.sent().into_iter().map(|(_, token, _)| token).collect();
        assert_eq!(sent, vec![Some(RENEWED.into())]);
        let renewals: Vec<_> = bearer.asked().into_iter().map(|(_, renew)| renew).collect();
        assert_eq!(renewals, vec![false]);
        // A person's token that does not read (another person's set, or a
        // renewal of another person's) sends nothing.
        let http = FakeHttp::answering(vec![]);
        let unread = call(&http, &FakeBearer::failing(false)).unwrap_err();
        assert_eq!(code_of(&unread), "no_credential");
        assert!(http.sent().is_empty(), "{use_case:?}");
        // And it goes through when the control side takes it.
        let http = FakeHttp::answering(vec![answer(&ServiceResponse::success(json!({"ok": 1})))]);
        assert_eq!(
            call(&http, &FakeBearer::expired()).unwrap(),
            json!({"ok": 1})
        );
    }
}

/// [`control_next`] renews only an `unauthenticated` of a token read, once.
#[test]
fn only_an_unauthenticated_of_a_token_read_is_renewed_and_only_once() {
    let refused = ServiceResponse::failure(ServiceErrorCode::Unauthenticated, "x");
    assert_eq!(control_next(&refused, true, false), ControlNext::Renew);
    assert_eq!(control_next(&refused, true, true), ControlNext::Login);
    assert_eq!(control_next(&refused, false, false), ControlNext::Answer);
    let mut newer = refused.clone();
    newer.min_api_version = API_VERSION + 1;
    newer.api_version = API_VERSION + 2;
    assert_eq!(control_next(&newer, true, false), ControlNext::Answer);
    for code in [
        ServiceErrorCode::AuthorizationDenied,
        ServiceErrorCode::Failed,
        ServiceErrorCode::NotLeader,
        ServiceErrorCode::Busy,
        ServiceErrorCode::Unreachable,
    ] {
        let other = ServiceResponse::failure(code, "x");
        assert_eq!(control_next(&other, true, false), ControlNext::Answer);
    }
    let done = ServiceResponse::success(json!({}));
    assert_eq!(control_next(&done, true, false), ControlNext::Answer);
}

/// A token that does not read sends nothing.
#[test]
fn a_token_that_does_not_read_sends_nothing() {
    let http = FakeHttp::answering(vec![]);
    let refused = call(&http, Some(&FakeBearer::failing(false))).unwrap_err();
    assert_eq!(code_of(&refused), "no_credential");
    assert!(format!("{refused:#}").contains("dagq login"));
    assert!(http.sent().is_empty());
}

/// No answer, an answer that is no use case's (a TLS end's 502, a
/// redirect) and the service's own refusals are not sent again: each is a
/// code, and none holds the token.
#[test]
fn what_is_no_answer_or_no_use_case_answer_is_not_sent_again() {
    let html = |status: u16| Ok((status, b"<html>Bad Gateway</html>".to_vec()));
    let cases: Vec<(Posted, &str)> = vec![
        (Err(anyhow!("connection reset")), "unreachable"),
        (html(502), "unreachable"),
        (Ok((302, Vec::new())), "unreachable"),
        (
            Ok((
                503,
                br#"{"error":{"code":"store_unavailable","message":"the store is down"}}"#.to_vec(),
            )),
            "store_unavailable",
        ),
        (
            answer(&ServiceResponse::failure(
                ServiceErrorCode::AuthorizationDenied,
                "worker may not",
            )),
            "authorization_denied",
        ),
        (
            answer(&ServiceResponse::failure(ServiceErrorCode::NotLeader, "x")),
            "not_leader",
        ),
        (
            Ok((200, vec![b' '; MAX_ANSWER_BYTES + 1])),
            "response_too_large",
        ),
    ];
    for (given, code) in cases {
        let http = FakeHttp::answering(vec![given]);
        let bearer = FakeBearer::giving();
        let failed = call(&http, Some(&bearer)).and_then(answer_of).unwrap_err();
        assert_eq!(code_of(&failed), code);
        assert!(!format!("{failed:#}").contains(TOKEN));
        assert_eq!(http.sent().len(), 1, "{code}");
        assert_eq!(bearer.asked().len(), 1, "{code}");
    }
    // A request past the limit is not sent, nor its token read.
    let http = FakeHttp::answering(vec![]);
    let bearer = FakeBearer::giving();
    let big = control_request(
        UseCase::Note,
        json!({"text": "x".repeat(MAX_REQUEST_BYTES)}),
    );
    let refused = call_control(&http, "https://c", Some(&bearer), &big, CLIENT_TIMEOUT)
        .map_err(|error| control_failure(error, UseCase::Note, NEXT_CONTROL_URL))
        .unwrap_err();
    assert_eq!(code_of(&refused), "request_too_large");
    assert!(http.sent().is_empty());
}

/// An answer of versions this client does not take stops with the range
/// and the install's guide, sending nothing again, whatever it holds.
#[test]
fn an_api_version_out_of_range_stops_with_the_range_and_the_install_guide() {
    for error in [None, Some(ServiceErrorCode::Unauthenticated)] {
        let mut newer = ServiceResponse::success(json!({}));
        if let Some(code) = error {
            newer = ServiceResponse::failure(code, "x");
        }
        newer.min_api_version = API_VERSION + 1;
        newer.api_version = API_VERSION + 3;
        let http = FakeHttp::answering(vec![answer(&newer)]);
        let failed = call(&http, Some(&FakeBearer::giving())).unwrap_err();
        let mismatch = failed
            .downcast_ref::<crate::application::login::ApiVersionMismatch>()
            .unwrap();
        assert_eq!(
            (mismatch.minimum, mismatch.maximum),
            (u64::from(API_VERSION + 1), u64::from(API_VERSION + 3))
        );
        assert!(failed.to_string().contains("install a compatible dagq"));
        assert_eq!(http.sent().len(), 1);
    }
    // The entrance's own range refusal, in range of nothing this sends.
    let refusal = ServiceResponse::version_mismatch(u64::from(API_VERSION));
    let http = FakeHttp::answering(vec![answer(&refusal)]);
    let refused = call(&http, Some(&FakeBearer::giving()))
        .and_then(answer_of)
        .unwrap_err();
    assert_eq!(code_of(&refused), "api_version_mismatch");
}

/// The person's token is the one their login kept for this control side:
/// a set kept for another URL (but its trailing `/`) is the login's guide,
/// and nothing goes there.
#[test]
fn a_token_kept_for_another_control_side_is_not_sent() {
    use crate::application::login::{
        self, CredentialStore, Credentials, Destination, Identity, LoginHttp, Params, Rejected,
    };
    struct Kept(Credentials);
    impl CredentialStore for Kept {
        fn load(&self) -> Result<Option<Credentials>> {
            Ok(Some(self.0.clone()))
        }
        fn save(&self, _: &Credentials) -> Result<()> {
            panic!("nothing is saved")
        }
        fn delete(&self) -> Result<()> {
            panic!("nothing is deleted")
        }
        fn hold(
            &self,
            _: crate::application::login::StoreLock,
        ) -> Result<crate::application::login::StoreHold> {
            Ok(Box::new(()))
        }
    }
    struct NoIssuer;
    impl LoginHttp for NoIssuer {
        fn get(&self, _: &str) -> Result<Value> {
            panic!("no issuer is asked")
        }
        fn form(&self, _: &str, _: &Params) -> Result<Value> {
            panic!("no issuer is asked")
        }
        fn exchange(&self, _: &str, _: &str) -> Result<Value> {
            panic!("no exchange is asked")
        }
    }
    struct Stored(Kept);
    impl BearerSource for Stored {
        fn token(&self, url: &str, renew: bool) -> Result<String> {
            let rejected = if renew { Rejected::First } else { Rejected::No };
            Ok(login::read_token(url, rejected, 100, &NoIssuer, &self.0)?.token)
        }
    }
    let kept = |control: &str| {
        Stored(Kept(Credentials {
            destination: Destination {
                control_url: control.into(),
                issuer: "https://issuer.example".into(),
                client_id: "dagq".into(),
            },
            token: TOKEN.into(),
            refresh_token: None,
            identity: Identity {
                issuer: "https://issuer.example".into(),
                subject: "s".into(),
                role: "user".into(),
                expires_at: 1_000,
                display_name: None,
            },
            inbox: None,
        }))
    };
    let http = FakeHttp::answering(vec![]);
    let refused = call(&http, Some(&kept("https://other.example"))).unwrap_err();
    assert_eq!(code_of(&refused), "no_credential");
    assert!(format!("{refused:#}").contains("dagq login"));
    assert!(http.sent().is_empty());

    // The same URL but its trailing `/` is the same control side.
    let http = FakeHttp::answering(vec![answer(&ServiceResponse::success(json!({})))]);
    call(&http, Some(&kept("https://control.example"))).unwrap();
    assert_eq!(http.sent()[0].1.as_deref(), Some(TOKEN));

    // With no refresh credential, an `unauthenticated` is the login's guide.
    let http = FakeHttp::answering(vec![unauthenticated()]);
    let refused = call(&http, Some(&kept("https://control.example"))).unwrap_err();
    assert_eq!(code_of(&refused), "no_credential");
    assert_eq!(http.sent().len(), 1);
}

/// A route's token source prints nothing of itself.
#[test]
fn a_bearer_prints_nothing_of_its_token() {
    let bearer = Bearer(Arc::new(FakeBearer::giving()));
    assert_eq!(format!("{bearer:?}"), "Bearer(..)");
    assert_eq!(bearer, bearer.clone());
    assert_ne!(bearer, Bearer(Arc::new(FakeBearer::giving())));
}

/// `locate`'s look at a control side sends `hello` with no token: the
/// entrance's `unauthenticated` is reached, and no answer is not, which
/// `locate` still answers.
#[test]
fn locate_looks_with_no_token_and_answers_whatever_it_finds() {
    let http = FakeHttp::answering(vec![unauthenticated()]);
    let reached = reach_control(&http, "https://control.example");
    assert_eq!(reached["reachable"], true);
    assert_eq!(reached["api_compatible"], true);
    let sent = http.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "https://control.example/api/use-case/hello");
    assert_eq!(sent[0].1, None);
    for given in [
        Err(anyhow!("connection refused")),
        Ok((502, b"<html>".to_vec())),
    ] {
        let reached = reach_control(&FakeHttp::answering(vec![given]), "https://c");
        assert_eq!(reached["reachable"], false);
    }
}

/// A URL with a query given straight to a call (a runner's or a CI's) is
/// refused before its token is read and before anything is sent, and a
/// path of its own is kept before the use case's.
#[test]
fn a_call_to_a_url_with_a_query_sends_nothing_and_a_path_is_kept() {
    let http = FakeHttp::answering(vec![]);
    let bearer = FakeBearer::giving();
    let refused = call_control(
        &http,
        "https://control.example/?x=1",
        Some(&bearer),
        &show(),
        CLIENT_TIMEOUT,
    )
    .unwrap_err();
    assert_eq!(code_of(&refused), "client_misconfigured");
    assert!(http.sent().is_empty() && bearer.asked().is_empty());
    let looked = reach_control(&http, "https://control.example/?x=1");
    assert_eq!(looked["reachable"], false);
    assert!(http.sent().is_empty());

    let http = FakeHttp::answering(vec![answer(&ServiceResponse::success(json!({})))]);
    call_control(
        &http,
        "https://control.example/team/",
        Some(&bearer),
        &show(),
        CLIENT_TIMEOUT,
    )
    .unwrap();
    assert_eq!(
        http.sent()[0].0,
        "https://control.example/team/api/use-case/show"
    );
}

/// An inbox's relay is the route alone: beside a socket, a broker, a
/// control side or a token's file it is `client_misconfigured`, and alone
/// its client holds no token's file, so it reads none.
#[test]
fn an_inbox_relay_is_the_route_alone_and_reads_no_token() {
    use crate::domain::queue_service::INBOX_RELAY_ENV;
    let client = Client::from_env(env_of(&[
        (INBOX_RELAY_ENV, "/tmp/dagq-1/inbox-2.sock"),
        ("DAGQ_ROLE", "inbox"),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(
        client.route,
        ClientRoute::Relay(PathBuf::from("/tmp/dagq-1/inbox-2.sock"))
    );
    assert_eq!(client.credential, None);
    assert_eq!(client.queue, None);
    assert_eq!(client.binding, None);
    for other in [SOCKET_ENV, URL_ENV, CONTROL_URL_ENV, CREDENTIAL_FILE_ENV] {
        let value = if other == URL_ENV || other == CONTROL_URL_ENV {
            "https://c.example"
        } else {
            "/x"
        };
        let refused = Client::from_env(env_of(&[(INBOX_RELAY_ENV, "/tmp/r.sock"), (other, value)]))
            .unwrap_err();
        assert_eq!(refused.code, "client_misconfigured", "{other}");
        assert!(refused.message.contains(other), "{}", refused.message);
    }
    // An empty value names none.
    assert!(
        Client::from_env(env_of(&[(INBOX_RELAY_ENV, " ")]))
            .unwrap()
            .is_none()
    );
}

/// The relay's one request: the control side's answer as it is, with the
/// inbox's token as the `Bearer` and nothing of the agent's request but
/// its use case and params; whatever stops it is an answer in the shape of
/// one, sent once (no answer and a page that is no use case's answer are
/// `unreachable`, a `store_unavailable` keeps its code, which the agent's
/// client reports as the HTTPS route does).
#[test]
fn the_relays_request_is_one_http_request_answered_as_it_is() {
    let agent = ServiceRequest {
        caller: None,
        api_version: API_VERSION,
        token: Some("agent".into()),
        env_role: Some("inbox".into()),
        queue: Some("/q.db".into()),
        binding: Some("/r".into()),
        use_case: UseCase::Show,
        params: json!({"id": 1}),
    };
    let relayed = crate::application::inbox_relay::relayed(&agent);
    let taken = ServiceResponse::success(json!({"id": 1}));
    let http = FakeHttp::answering(vec![answer(&taken)]);
    assert_eq!(
        relay_send(&http, "https://c.example", Some(TOKEN), &relayed),
        taken
    );
    let sent = http.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "https://c.example/api/use-case/show");
    assert_eq!(sent[0].1.as_deref(), Some(TOKEN));
    for name in ["token", "env_role", "queue", "binding"] {
        assert!(sent[0].2.get(name).is_none(), "{name}: {:?}", sent[0].2);
    }
    let html = |status: u16| Ok((status, b"<html>Bad Gateway</html>".to_vec()));
    for (given, code, named) in [
        (
            Err(anyhow!("connection reset")),
            ServiceErrorCode::Unreachable,
            None,
        ),
        (html(502), ServiceErrorCode::Unreachable, None),
        (Ok((302, Vec::new())), ServiceErrorCode::Unreachable, None),
        (
            Ok((
                503,
                br#"{"error":{"code":"store_unavailable","message":"down"}}"#.to_vec(),
            )),
            ServiceErrorCode::Failed,
            Some("store_unavailable"),
        ),
        (
            Ok((200, vec![b' '; MAX_ANSWER_BYTES + 1])),
            ServiceErrorCode::ResponseTooLarge,
            None,
        ),
    ] {
        let http = FakeHttp::answering(vec![given]);
        let response = relay_send(&http, "https://c.example", Some(TOKEN), &relayed);
        let error = response.error.clone().unwrap();
        assert_eq!(error.code, code);
        assert_eq!(error.relayed_code().map(|(code, _)| code), named);
        assert!(!error.message.contains(TOKEN));
        assert_eq!(http.sent().len(), 1);
        // The agent's `dagq` reports the code the control side named.
        let reported = answer_of(response).unwrap_err();
        let failure = reported.downcast_ref::<ClientFailure>().unwrap();
        assert_eq!(failure.code, named.unwrap_or(code.as_str()));
    }
}

/// The agent's client takes its relay's answer as the person's client
/// takes the control side's: versions it does not take stop with the
/// range and the install guide.
#[test]
fn a_relays_answer_of_other_versions_stops_with_the_install_guide() {
    let mut newer = ServiceResponse::success(json!({}));
    newer.min_api_version = API_VERSION + 1;
    newer.api_version = API_VERSION + 2;
    let failed = relay_answer(newer).unwrap_err();
    let mismatch = failed
        .downcast_ref::<crate::application::login::ApiVersionMismatch>()
        .unwrap();
    assert_eq!(mismatch.minimum, u64::from(API_VERSION + 1));
    assert!(failed.to_string().contains("install a compatible dagq"));
    let taken = ServiceResponse::success(json!({}));
    assert_eq!(relay_answer(taken.clone()).unwrap(), taken);
}
