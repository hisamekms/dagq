//! The decisions of a person's inbox opened for a team's control side
//! (`DAGQ_CONTROL_URL`, ADR-t3985-1 decisions 6 and 8): whether `dagq
//! inbox` opens it, what its agent's environment holds, what the relay
//! sends, and what it does after a refusal of the inbox's token.
//!
//! The agent holds no credential and reaches no TCP: the `dagq inbox` that
//! started it stays as its parent, outside the agent's sandbox, and relays
//! each request its `dagq` writes on the session's unix socket
//! ([`crate::domain::queue_service::INBOX_RELAY_ENV`]) to the control side
//! with the inbox's token as the `Bearer`. Only that parent reads the
//! person's token, and only to ask for the inbox's (at the open and after a
//! refusal) and to revoke it at the session's end, each through
//! `infrastructure::queue_service::person_call_with`, which renews the
//! person's token once in all, expired or refused; an agent's request
//! never carries it. The relay is
//! advisory against the same OS user (ADR-t3102-1 decision 5): the control
//! side authorizes every request for the inbox token's principal.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{Value, json};

use crate::domain::actor::ROLE_ENV;
use crate::domain::queue_service::{
    CONTROL_URL_ENV, CREDENTIAL_FILE_ENV, INBOX_RELAY_ENV, SOCKET_ENV, ServiceErrorCode,
    ServiceRequest, ServiceResponse, URL_ENV, UseCase, understands,
};

/// Why `dagq inbox` for a control side opens nothing for the role `role`
/// its environment names, before any store is read: inside an inbox
/// ([`super::lifecycle::INBOX_INSIDE_REFUSED`], ADR-t2159-1 decision 2) or
/// any other role but the user's (`authorization_denied`, as the gate
/// refuses `dagq inbox` to it), an unknown value too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenRefusal {
    Inside,
    Denied(String),
}

/// [`OpenRefusal`] of `role`; `None` for no role or the user's.
pub fn open_refusal(role: Option<&str>) -> Option<OpenRefusal> {
    match role.map(str::trim) {
        None | Some("" | "user") => None,
        Some("inbox") => Some(OpenRefusal::Inside),
        Some(other) => Some(OpenRefusal::Denied(other.to_owned())),
    }
}

/// The params of the person's `issue_token` for their own inbox: the
/// principal asked for (the control side takes its parent from the
/// token's principal, never from this) and, at the open alone, the
/// launch's `inbox_opened` (its provider, model and guardrail), which the
/// control side records for the person (ADR-t2159-1 decision 2,
/// ADR-t1181-1 decision 5); a renewal's carries none and records nothing.
pub fn issue_params(principal: &Value, opened: Option<&Value>) -> Value {
    let mut params = json!({"principal": principal});
    if let Some(opened) = opened {
        params["opened"] = opened.clone();
    }
    params
}

/// The variables the agent of an inbox for a control side does not
/// inherit from the person's terminal: the routes and the token's file of
/// client mode (`DAGQ_CONTROL_URL` would have its `dagq` refused as
/// `client_misconfigured` beside the relay), the queue's paths (a
/// `DAGQ_DB` the hooks and the plugin's launcher would pass as `--db`,
/// which the relay's route refuses as `queue_named`), and the
/// person's login and store (each `DAGQ_LOGIN_*` of `inherited`), which
/// its `dagq` never opens.
pub fn agent_env_removed(inherited: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut removed: Vec<String> = [
        CONTROL_URL_ENV,
        SOCKET_ENV,
        URL_ENV,
        CREDENTIAL_FILE_ENV,
        super::lifecycle::QUEUE_ENV,
        "DAGQ_DB",
        "DAGQ_CREDENTIAL_STORE",
        "DAGQ_LOGIN_ISSUER",
        "DAGQ_LOGIN_CLIENT_ID",
        "DAGQ_LOGIN_SCOPE",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for name in inherited {
        if name.starts_with("DAGQ_LOGIN_") && !removed.contains(&name) {
            removed.push(name);
        }
    }
    removed
}

/// The variables the agent of an inbox for a control side is given:
/// the inbox's role, actor and session kind and its `launch` (as the
/// host's inbox, [`super::actor_executor::actor_env`] with no queue), and
/// [`INBOX_RELAY_ENV`] naming the session's `relay` socket.
pub fn agent_env(
    launch: &crate::domain::actor_model::ActorLaunch,
    relay: &Path,
) -> Result<Vec<(String, String)>> {
    let actor = super::lifecycle::session_actor(crate::domain::SessionRole::Inbox);
    let mut env: Vec<(String, String)> =
        super::actor_executor::actor_env(Path::new("/"), &actor, None, Some(launch))?
            .into_iter()
            .filter(|(name, _)| name != super::lifecycle::QUEUE_ENV)
            .collect();
    debug_assert!(env.iter().any(|(name, _)| name == ROLE_ENV));
    env.push((INBOX_RELAY_ENV.to_owned(), super::path_text(relay)?));
    Ok(env)
}

/// The request the relay sends for the agent's `request`: its use case
/// and params alone. Its token, role, queue and binding are dropped: the
/// inbox's token goes as the `Bearer`, and the URL says which service is
/// meant.
pub fn relayed(request: &ServiceRequest) -> ServiceRequest {
    ServiceRequest {
        caller: None,
        api_version: request.api_version,
        token: None,
        env_role: None,
        queue: None,
        binding: None,
        use_case: request.use_case,
        params: request.params.clone(),
    }
}

/// Where the relay is in its sends of one request ([`relay_next`]): the
/// first with the token the store held, one again with the token the
/// store held when it was read again, one again with a new token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayStep {
    Sent,
    Reread,
    Reissued,
}

/// What the relay does after a send ([`relay_next`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayNext {
    /// Take the answer as it is.
    Answer,
    /// Send once more with this token, the one the store holds now:
    /// another `dagq inbox` of the same person got it, which revoked the
    /// one sent (the issue of a new token revokes the last).
    Resend(String),
    /// Ask for a new token with the person's, keep it
    /// and send once more.
    Reissue,
    /// Stop with `dagq login`'s guide: nothing is sent again.
    Login,
}

/// [`RelayNext`] after a send at `step` that the control side `refused` as
/// `unauthenticated` (or that had no token to go with), `sent` the token
/// it went with and `reread` the one the store holds when read after it
/// (read only after the first). Each step is taken once, so at most three
/// sends go for one request.
pub fn relay_next(
    step: RelayStep,
    refused: bool,
    sent: Option<&str>,
    reread: Option<&str>,
) -> RelayNext {
    if !refused {
        return RelayNext::Answer;
    }
    match step {
        RelayStep::Sent => match reread {
            Some(token) if Some(token) != sent => RelayNext::Resend(token.to_owned()),
            _ => RelayNext::Reissue,
        },
        RelayStep::Reread => RelayNext::Reissue,
        RelayStep::Reissued => RelayNext::Login,
    }
}

/// Whether `response` refuses the token it went with: `unauthenticated` in
/// an answer of versions this client takes. Any other answer, a
/// `store_unavailable` or no answer among them, is not sent again.
pub fn refuses_the_token(response: &ServiceResponse) -> bool {
    understands(response)
        && response
            .error
            .as_ref()
            .is_some_and(|error| error.code == ServiceErrorCode::Unauthenticated)
}

/// What the relay's parent reads and asks for the inbox's token: the store
/// it keeps it in, read on each request, and the person's `issue_token`.
pub trait InboxTokens: Send + Sync {
    /// The inbox's token the person's store holds for the relay's control
    /// side, `None` when none or it expired; an error (no set for it after
    /// a `logout`, another control side's) stops with `dagq login`'s guide.
    fn read(&self) -> Result<Option<String>>;
    /// The token to send in place of `last`, the one refused
    /// ([`renewed_kept`]): under the lock of every `dagq inbox`'s issues,
    /// the store read again and its token when it is not `last`, else a new
    /// one got with the person's token (no `inbox_opened`) and kept in the
    /// store in place of the last.
    fn renew(&self, last: Option<&str>) -> Result<String>;
}

/// One HTTP request of the relay to the control side, with `token` as its
/// `Bearer` (none for `hello`): the answer as it is, or a failure in the
/// shape of one (no answer is `unreachable`), never sent again by it.
pub trait RelaySend: Send + Sync {
    fn send(&self, token: Option<&str>, request: &ServiceRequest) -> ServiceResponse;
}

/// The answer of a request the relay cannot send again: `dagq login`'s
/// guide, as `unauthenticated`, with why.
fn login_refusal(why: &str) -> ServiceResponse {
    ServiceResponse::failure(
        ServiceErrorCode::Unauthenticated,
        format!(
            "{why}: {}; then open the inbox again with dagq inbox",
            super::login::LOGIN_AGAIN
        ),
    )
}

/// Whether the session of a relay ended, behind the lock its requests ask
/// for a new token under: [`SessionGate::close`] waits for a reissue under
/// way, and none starts after it, so the session's last revocation and the
/// store's clearing come after every token the relay got.
#[derive(Debug, Default)]
pub struct SessionGate(Mutex<bool>);

impl SessionGate {
    /// End the session: wait for a reissue under way, then let none start.
    pub fn close(&self) {
        *self.lock() = true;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, bool> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn closed(&self) -> bool {
        *self.lock()
    }
}

/// What the relay answers once its session ended: nothing is sent, and
/// no token is got.
pub fn ended() -> ServiceResponse {
    ServiceResponse::failure(
        ServiceErrorCode::Unreachable,
        "the dagq inbox that started this inbox has ended: run dagq inbox again",
    )
}

/// Relay the agent's `request` ([`relayed`]) with the inbox's token from
/// `tokens` through `send`, and answer as the control side did. `hello`
/// goes with no token. After a refusal of the token ([`refuses_the_token`],
/// or no token to send), the next step is [`relay_next`]'s; a new token is
/// asked for ([`InboxTokens::renew`]) under `gate`'s lock, so never once
/// the session ended ([`ended`]), and under the lock of every `dagq
/// inbox`'s issues with the store read once more first, so two requests
/// refused at once, of this relay or of another `dagq inbox` of the same
/// person, ask once.
pub fn relay(
    request: &ServiceRequest,
    tokens: &dyn InboxTokens,
    send: &dyn RelaySend,
    gate: &SessionGate,
) -> ServiceResponse {
    if gate.closed() {
        return ended();
    }
    let request = relayed(request);
    if request.use_case == UseCase::Hello {
        return send.send(None, &request);
    }
    let sent = match tokens.read() {
        Ok(sent) => sent,
        Err(error) => return login_refusal(&format!("{error:#}")),
    };
    let attempt = |token: Option<&str>| match token {
        Some(token) => {
            let response = send.send(Some(token), &request);
            (refuses_the_token(&response), Some(response))
        }
        None => (true, None),
    };
    let (refused, mut answer) = attempt(sent.as_deref());
    let reread = if refused {
        match tokens.read() {
            Ok(reread) => reread,
            Err(error) => return login_refusal(&format!("{error:#}")),
        }
    } else {
        None
    };
    let mut step = RelayStep::Sent;
    // The token the last send went with: a new one is asked for only when
    // the store still holds it.
    let mut last = sent.clone();
    let mut next = relay_next(step, refused, sent.as_deref(), reread.as_deref());
    loop {
        match next {
            RelayNext::Answer => {
                return answer.unwrap_or_else(|| login_refusal("no inbox token"));
            }
            RelayNext::Login => {
                return login_refusal("the control side refused the inbox's new token too");
            }
            RelayNext::Resend(token) => {
                step = RelayStep::Reread;
                let (refused, response) = attempt(Some(&token));
                last = Some(token);
                answer = response;
                next = relay_next(step, refused, None, None);
            }
            RelayNext::Reissue => {
                step = RelayStep::Reissued;
                let token = {
                    let closed = gate.lock();
                    if *closed {
                        return ended();
                    }
                    tokens.renew(last.as_deref())
                };
                let token = match token {
                    Ok(token) => token,
                    Err(error) => {
                        return login_refusal(&format!(
                            "the inbox's token could not be got again: {error:#}"
                        ));
                    }
                };
                let (refused, response) = attempt(Some(&token));
                answer = response;
                next = relay_next(step, refused, None, None);
            }
        }
    }
}

/// The issue of the inbox's token with the person's and its keeping in
/// the store ([`issue_kept`]).
pub trait InboxIssue {
    /// Hold, until what it gives is dropped, the issue lock of the store the
    /// token is kept in ([`super::login::StoreLock::Issue`]), which every
    /// `dagq inbox` keeping its token in that store takes from an issue to
    /// its keeping.
    fn hold(&self) -> Result<Box<dyn std::any::Any>>;
    /// The person's `issue_token`, carrying `opened` at the open alone.
    fn issue(&self, opened: Option<&Value>) -> Result<super::login::InboxToken>;
    /// Keep `inbox` in the store in place of the last.
    fn keep(&self, inbox: &super::login::InboxToken) -> Result<()>;
}

/// A new inbox token issued and kept under [`InboxIssue::hold`]: each issue
/// revokes the one before it at the control side, so with two `dagq inbox`
/// of the same person issuing at once, the one kept last is the one issued
/// last, never an earlier one whose answer came late.
pub fn issue_kept(issuer: &dyn InboxIssue, opened: Option<&Value>) -> Result<String> {
    let _held = issuer.hold()?;
    issued_and_kept(issuer, opened)
}

/// The token to send in place of `last` after a refusal, under
/// [`InboxIssue::hold`] from the store's read to the keeping: the one the
/// store holds when it is not `last` (another request of this relay, or
/// another `dagq inbox` of the same person, got it, and its issue revoked
/// `last`), else a new one issued and kept ([`issue_kept`]). With the read
/// inside the lock, the second of two inboxes refused at once takes the
/// first's new token and never issues one that revokes it.
pub fn renewed_kept(
    issuer: &dyn InboxIssue,
    read: &dyn Fn() -> Result<Option<String>>,
    last: Option<&str>,
) -> Result<String> {
    let _held = issuer.hold()?;
    match read()? {
        Some(now) if Some(now.as_str()) != last => Ok(now),
        _ => issued_and_kept(issuer, None),
    }
}

/// [`InboxIssue::issue`] and [`InboxIssue::keep`], under the lock its
/// caller holds.
fn issued_and_kept(issuer: &dyn InboxIssue, opened: Option<&Value>) -> Result<String> {
    let inbox = issuer.issue(opened)?;
    issuer.keep(&inbox).map_err(|error| IssuedNotKept {
        why: format!("{error:#}"),
    })?;
    Ok(inbox.token)
}

/// An issue that got a token the store did not take (its save failed, or
/// left the store in a state not known): the token is valid at the control
/// side and may be in the store, so it is revoked and taken out
/// ([`run_session`]).
#[derive(Debug)]
pub struct IssuedNotKept {
    pub why: String,
}

impl std::fmt::Display for IssuedNotKept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the inbox's token was issued but not kept: {}", self.why)
    }
}

impl std::error::Error for IssuedNotKept {}

/// What the relay answers a request with.
pub type Answer = Arc<dyn Fn(&ServiceRequest) -> ServiceResponse + Send + Sync>;

/// A relay's socket opened ([`InboxSession::open_relay`]).
pub trait OpenRelay {
    /// Stop taking requests and remove the socket.
    fn close(self: Box<Self>);
}

/// The ports of the session of a person's inbox for a control side
/// ([`run_session`]): the person's token is read for [`Self::issue`] and
/// [`Self::revoke`] alone, each renewed once when expired or refused.
pub trait InboxSession {
    /// Take the signals that end the session (`SIGHUP`, `SIGTERM`) from
    /// now on: one that comes before the agent starts ends the session
    /// through [`end_session`] too, and the agent is not started.
    fn catch_ending(&self);
    /// Hold, until what it gives is dropped, the lock every `dagq inbox`
    /// of this user takes from an issue to its keeping
    /// ([`InboxIssue::hold`]): the end's revocation and clearing hold it,
    /// so another session's issue and keeping come wholly before or after.
    fn hold(&self) -> Result<Box<dyn std::any::Any>>;
    /// The inbox's token got with the person's (`issue_token` carrying
    /// `opened`, which the control side records as `inbox_opened`; it
    /// revokes the token of an earlier open) and kept in the store.
    fn issue(&self, opened: &Value) -> Result<()>;
    /// Open the session's relay, answering each request with `answer`.
    fn open_relay(&self, answer: Answer) -> Result<Box<dyn OpenRelay>>;
    /// Run the agent as the foreground child and wait for it, or for a
    /// `SIGHUP` or `SIGTERM` that ends the session: the exit code (the
    /// agent's, else the shell's `128 + signal`).
    fn run_agent(&self) -> Result<i32>;
    /// The person's `revoke_token` of their own inbox.
    fn revoke(&self) -> Result<()>;
    /// Take the inbox's token out of the store.
    fn forget(&self) -> Result<()>;
    /// Tell the person on the terminal.
    fn warn(&self, message: &str);
}

/// The session of a person's inbox for a control side (ADR-t3985-1
/// decision 6, ADR-t3102-1 decision 2): the inbox's token got with
/// `opened`, the relay opened with `tokens` and `send`, the agent run, and
/// at its end ([`end_session`]) the token revoked and taken out of the
/// store. The signals that end the session are taken before anything is
/// got. A token that cannot be got starts nothing and says to log in; one
/// got but not kept ([`IssuedNotKept`]) is revoked and taken out before
/// that; a relay that cannot open ends the session before any agent. The
/// exit code is the agent's.
pub fn run_session(
    session: &dyn InboxSession,
    opened: &Value,
    tokens: Arc<dyn InboxTokens>,
    send: Arc<dyn RelaySend>,
) -> Result<i32> {
    session.catch_ending();
    let gate = Arc::new(SessionGate::default());
    if let Err(error) = session.issue(opened) {
        if error.is::<IssuedNotKept>() {
            end_session(session, &gate, None);
        }
        return Err(anyhow::anyhow!(
            "get the inbox's token: {error:#}; the inbox was not opened ({})",
            super::login::LOGIN_AGAIN
        ));
    }
    let answer: Answer = {
        let gate = Arc::clone(&gate);
        Arc::new(move |request| relay(request, &*tokens, &*send, &gate))
    };
    let relay = match session.open_relay(answer) {
        Ok(relay) => relay,
        Err(error) => {
            end_session(session, &gate, None);
            return Err(error.context("open the inbox's relay; the inbox was not opened"));
        }
    };
    let code = session.run_agent();
    end_session(session, &gate, Some(relay));
    code
}

/// The end of a session: the gate closed (a reissue under way finishes
/// first, and none starts after), the relay closed, then, under the lock
/// of every `dagq inbox`'s issues ([`InboxSession::hold`]), the person's
/// inbox revoked and its token taken out of the store whatever the
/// revocation did, each failure only warned of: another session's issue
/// and keeping never come between the two, so the token it keeps is never
/// taken out unrevoked. Whether the revocation reached.
pub fn end_session(
    session: &dyn InboxSession,
    gate: &SessionGate,
    relay: Option<Box<dyn OpenRelay>>,
) -> bool {
    gate.close();
    if let Some(relay) = relay {
        relay.close();
    }
    let _held = match session.hold() {
        Ok(held) => Some(held),
        Err(error) => {
            session.warn(&format!(
                "the lock of the inbox's issues could not be taken ({error:#}); the token is \
                 revoked and taken out without it"
            ));
            None
        }
    };
    let reached = match session.revoke() {
        Ok(()) => true,
        Err(error) => {
            session.warn(&format!(
                "the inbox's token could not be revoked at the control side ({error:#}); it \
                 ends with the person's token or the next dagq inbox"
            ));
            false
        }
    };
    if let Err(error) = session.forget() {
        session.warn(&format!(
            "the inbox's token could not be taken out of the store: {error:#}"
        ));
    }
    reached
}

#[cfg(test)]
mod tests;
