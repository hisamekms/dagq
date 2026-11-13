//! The queue service of a wiring test: the service's real use cases on the
//! test's memdb queue ([`SqliteServiceBackend`]), with the tokens kept in
//! memory ([`MemoryTokens`]) and the processes asked of the test's fake
//! ([`super::host::FakeProcesses`]) instead of the host.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};

use anyhow::{Context, Result, bail};

use super::host::FakeProcesses;
use crate::{
    application::{
        Clock, ProcessControl, Spawned,
        queue_service::{Issued, ServiceAccess, ServiceBackend, ServiceQueue, TelemetryTokens},
    },
    domain::{
        ActorContext, ActorRole, LeaseToken, TaskId,
        queue_service::{HeldToken, Principal, SessionProcess, TokenIssuer},
        session_step::StepTarget,
        telemetry::{HeldTelemetry, KeptStepsFile, TargetFacts},
    },
    infrastructure::queue_service::{SqliteServiceBackend, credential_path, sha256_hex},
};

/// What a token's record keeps beside its value, as the service's files do.
#[derive(Clone)]
struct Record {
    principal: Principal,
    issued_at: i64,
    issued_under: Option<LeaseToken>,
    issued_by: Option<TokenIssuer>,
}

impl Record {
    fn held(&self) -> HeldToken {
        HeldToken {
            principal: self.principal.clone(),
            issued_by: self.issued_by.clone(),
            issued_at: self.issued_at,
        }
    }
}

#[derive(Default)]
struct Tokens {
    /// The value each token's file holds, by the file the service hands.
    files: HashMap<PathBuf, String>,
    /// The record of each token that holds, by its value.
    records: HashMap<String, Record>,
}

/// The tokens of a test's queue service in memory: each "file" is a path
/// under a directory that is never made (named apart from the memdb's
/// URI), which the caller reads back here ([`Self::read`]). Issues and
/// revocations keep the service's rules: an issue revokes the token its
/// key's file held, a revocation removes the record first, and a lookup
/// finds only a token whose record is there.
pub(super) struct MemoryTokens {
    dir: PathBuf,
    /// What each token's issue records as its time.
    clock: Arc<dyn Clock>,
    tokens: Mutex<Tokens>,
    revocations: Mutex<Vec<(StepTarget, Option<LeaseToken>)>>,
    /// Whether an issue holds the lock of the service's issues
    /// ([`ServiceBackend::hold_issues`]), and its waits.
    issuing: Arc<(Mutex<bool>, Condvar)>,
}

impl MemoryTokens {
    /// The tokens whose files are named under `dir`.
    pub(super) fn new(dir: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self {
            dir,
            clock,
            tokens: Mutex::default(),
            revocations: Mutex::default(),
            issuing: Arc::default(),
        }
    }

    /// The directory the tokens' files are named under, made nowhere.
    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Attempts made through the telemetry port, including an already
    /// absent token: a saved revocation must still reach this port once.
    pub(super) fn revocations(&self) -> Vec<(StepTarget, Option<LeaseToken>)> {
        self.revocations.lock().unwrap().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Tokens> {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The value of the token the file `file` holds, as a caller reads the
    /// file it was handed.
    pub(super) fn read(&self, file: &Path) -> Result<String> {
        self.lock()
            .files
            .get(file)
            .cloned()
            .with_context(|| format!("read the token file {}", file.display()))
    }

    fn file(&self, key: &str) -> PathBuf {
        credential_path(&self.dir, key)
    }

    /// Issue `principal`'s token, issued under `under` at `by`'s request,
    /// revoking the one its key's file held: its file.
    fn issue_token(
        &self,
        principal: &Principal,
        under: Option<&LeaseToken>,
        by: Option<&TokenIssuer>,
    ) -> Result<PathBuf> {
        principal.check().map_err(anyhow::Error::msg)?;
        let file = self.file(&principal.credential_key());
        let value = sha256_hex(uuid::Uuid::new_v4().as_bytes());
        let issued_at = self.clock.now();
        let mut tokens = self.lock();
        // One bound to a session revokes every earlier session's of its
        // actor, as the service's issue does.
        if principal.session.is_some() {
            let Tokens { files, records } = &mut *tokens;
            records.retain(|value, record| {
                let earlier = record.principal.actor_id == principal.actor_id
                    && record.principal.session.is_some();
                if earlier {
                    files.retain(|_, held| held != value);
                }
                !earlier
            });
        }
        if let Some(old) = tokens.files.remove(&file) {
            tokens.records.remove(&old);
        }
        tokens.records.insert(
            value.clone(),
            Record {
                principal: principal.clone(),
                issued_at,
                issued_under: under.cloned(),
                issued_by: by.cloned(),
            },
        );
        tokens.files.insert(file.clone(), value);
        Ok(file)
    }

    /// Revoke the token the file of `key` holds: whether one was.
    fn revoke_key(&self, key: &str) -> bool {
        let file = self.file(key);
        let mut tokens = self.lock();
        let Some(value) = tokens.files.remove(&file) else {
            return false;
        };
        tokens.records.remove(&value).is_some()
    }

    /// Revoke the tokens of `actor_id` issued under `under` (all of them
    /// when `None`), and its file once its value holds no record.
    fn revoke_under(&self, actor_id: &str, under: Option<&LeaseToken>) -> bool {
        let file = self.file(actor_id);
        let mut tokens = self.lock();
        let before = tokens.records.len();
        tokens.records.retain(|_, record| {
            record.principal.actor_id != actor_id
                || under.is_some_and(|under| record.issued_under.as_ref() != Some(under))
        });
        let revoked = tokens.records.len() != before;
        if let Some(value) = tokens.files.get(&file).cloned()
            && !tokens.records.contains_key(&value)
        {
            tokens.files.remove(&file);
        }
        revoked
    }

    fn lookup(&self, token: &str) -> Option<Principal> {
        self.lock()
            .records
            .get(token)
            .map(|record| record.principal.clone())
    }

    fn records(&self) -> Vec<(String, Record)> {
        self.lock()
            .records
            .iter()
            .map(|(value, record)| (value.clone(), record.clone()))
            .collect()
    }
}

impl ServiceAccess for MemoryTokens {
    fn socket(&self, _: &Path) -> PathBuf {
        self.dir.join("service.sock")
    }

    fn issue(&self, _: &Path, principal: &Principal) -> Result<Issued> {
        Ok(Issued::new(self.issue_token(principal, None, None)?))
    }

    fn credential(&self, _: &Path, actor_id: &str) -> Option<PathBuf> {
        let file = self.file(actor_id);
        self.lock().files.contains_key(&file).then_some(file)
    }

    fn revoke(&self, _: &Path, actor_id: &str) -> Result<bool> {
        Ok(self.revoke_key(actor_id))
    }

    /// Revoke each token whose principal `ended` says has ended: its record,
    /// and its value when its key's file still holds that token. A
    /// supervisor's sign-in is the service's to judge, and a job's token is
    /// kept (no job runs in a wiring test, so none asks for one).
    fn sweep(&self, _: &Path, ended: &mut dyn FnMut(&Principal) -> bool) -> Result<Vec<String>> {
        let mut revoked = Vec::new();
        for (value, record) in self.records() {
            let principal = record.principal;
            if principal.role == ActorRole::Supervisor
                || crate::domain::queue_service::is_job(principal.role)
                || !ended(&principal)
            {
                continue;
            }
            let file = self.file(&principal.credential_key());
            let mut tokens = self.lock();
            tokens.records.remove(&value);
            if tokens.files.get(&file) == Some(&value) {
                tokens.files.remove(&file);
            }
            drop(tokens);
            if !revoked.contains(&principal.actor_id) {
                revoked.push(principal.actor_id);
            }
        }
        Ok(revoked)
    }

    /// The child as it is: a job's token is the test's to drop with the
    /// queue.
    fn revoke_on_exit(&self, _: &Path, _: &str, child: Box<dyn Spawned>) -> Box<dyn Spawned> {
        child
    }
}

impl TelemetryTokens for MemoryTokens {
    fn issue_telemetry(
        &self,
        _: &Path,
        target: &StepTarget,
        task: Option<TaskId>,
        issued_under: Option<&LeaseToken>,
    ) -> Result<PathBuf> {
        let principal = Principal::telemetry(target, task);
        self.revoke_under(&principal.actor_id, None);
        self.issue_token(&principal, issued_under, None)
    }

    fn revoke_telemetry(
        &self,
        _: &Path,
        target: &StepTarget,
        issued_under: Option<&LeaseToken>,
    ) -> Result<bool> {
        self.revocations
            .lock()
            .unwrap()
            .push((target.clone(), issued_under.cloned()));
        Ok(self.revoke_under(&crate::domain::telemetry::actor_id(target), issued_under))
    }

    fn telemetry_credential(&self, db: &Path, target: &StepTarget) -> Option<PathBuf> {
        let file = self.credential(db, &crate::domain::telemetry::actor_id(target))?;
        let value = self.read(&file).ok()?;
        self.lookup(&value).map(|_| file)
    }

    /// None: a test's wrapper keeps no steps.
    fn kept_steps(&self, _: &Path) -> Result<Vec<KeptStepsFile>> {
        Ok(Vec::new())
    }

    fn forget_steps(&self, _: &Path, _: &Path) -> Result<()> {
        Ok(())
    }

    fn held_telemetry(&self, _: &Path) -> Result<Vec<HeldTelemetry>> {
        Ok(self
            .records()
            .into_iter()
            .filter(|(_, record)| record.principal.role == ActorRole::ExecutorTelemetry)
            .filter_map(|(_, record)| {
                Some(HeldTelemetry {
                    target: record.principal.target?,
                    issued_under: record.issued_under,
                })
            })
            .collect())
    }

    /// Refused, as the host's access refuses it: only the service's backend
    /// reads the queue.
    fn target_facts(&self, _: &Path, target: &StepTarget) -> Result<TargetFacts> {
        bail!(
            "{} {} is read only by the queue service",
            target.kind.as_str(),
            target.id
        )
    }

    fn register_telemetry_session(&self, _: &Path, target: &StepTarget, _: &str) -> Result<()> {
        bail!(
            "a session of {} {} is registered only by the queue service",
            target.kind.as_str(),
            target.id
        )
    }
}

/// The lock of the service's issues, held until it is dropped.
struct IssuesHeld(Arc<(Mutex<bool>, Condvar)>);

impl Drop for IssuesHeld {
    fn drop(&mut self) {
        let (held, released) = &*self.0;
        *held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
        released.notify_all();
    }
}

/// The backend of a test's queue service: the queue's use cases on the
/// memdb queue through [`SqliteServiceBackend`] (never its tokens' files
/// nor the host's processes), the tokens in [`MemoryTokens`] and the
/// processes of [`FakeProcesses`].
pub(super) struct MemoryBackend {
    pub(super) sqlite: SqliteServiceBackend,
    pub(super) tokens: Arc<MemoryTokens>,
    pub(super) processes: Arc<FakeProcesses>,
    /// Run once by the next [`ServiceBackend::held_records`], after it
    /// read the records: a test's look into a sweep's transaction.
    pub(super) on_held_records: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Run once by the next [`ServiceBackend::bind_update_job`], before it
    /// binds: a test's look at the transaction the bind runs in, whose
    /// error is the binding's, as a write that failed (nothing bound).
    pub(super) on_bind: std::sync::Mutex<Option<BindHook>>,
}

/// A test's look at the next update job's binding ([`MemoryBackend::on_bind`]).
pub(super) type BindHook = Box<dyn FnOnce() -> Result<()> + Send>;

impl ServiceBackend for MemoryBackend {
    fn answer_authorizer(&self) -> Box<dyn crate::domain::Authorizer + '_> {
        self.sqlite.answer_authorizer()
    }

    fn principal(&self, token: &str) -> Result<Option<Principal>> {
        Ok(self.tokens.lookup(token))
    }

    fn open(&self, actor: &ActorContext) -> Result<Box<dyn ServiceQueue>> {
        self.sqlite.open(actor)
    }

    fn open_read(&self, actor: &ActorContext) -> Result<Box<dyn ServiceQueue>> {
        self.sqlite.open_read(actor)
    }

    fn open_watch(&self, actor: &ActorContext) -> Result<Box<dyn ServiceQueue>> {
        self.sqlite.open_watch(actor)
    }

    fn record_unauthenticated(&self, payload: serde_json::Value) -> Result<()> {
        self.sqlite.record_unauthenticated(payload)
    }

    fn now_ms(&self) -> i64 {
        self.sqlite.now_ms()
    }

    fn db(&self) -> &Path {
        &self.sqlite.db
    }

    fn access(&self) -> &dyn ServiceAccess {
        &*self.tokens
    }

    fn telemetry(&self) -> &dyn TelemetryTokens {
        &*self.tokens
    }

    fn principals(&self) -> Result<Vec<Principal>> {
        Ok(self
            .tokens
            .records()
            .into_iter()
            .map(|(_, record)| record.principal)
            .collect())
    }

    /// The memdb's URI, compared as it is: no file names it.
    fn serves(&self, db: &Path) -> bool {
        db == self.sqlite.db
    }

    fn binding(&self) -> Result<Option<String>> {
        self.sqlite.binding()
    }

    fn session_alive(&self, session: &SessionProcess) -> bool {
        let alive = self.processes.alive(session.pid);
        session.runs(
            alive,
            alive
                .then(|| self.processes.session_start(session.pid))
                .flatten()
                .as_deref(),
        )
    }

    fn session_ended(&self, session: &SessionProcess) -> bool {
        let alive = self.processes.alive(session.pid);
        session.ended(
            alive,
            alive
                .then(|| self.processes.session_start(session.pid))
                .flatten()
                .as_deref(),
        )
    }

    fn process_of(&self, pid: u32) -> Option<SessionProcess> {
        self.processes
            .session_start(pid)
            .map(|start| SessionProcess { pid, start })
    }

    fn started_by_ended(&self, pid: u32, _: i64) -> bool {
        !self.processes.alive(pid)
    }

    fn issue_as(&self, principal: &Principal, issuer: &TokenIssuer) -> Result<PathBuf> {
        self.tokens.issue_token(principal, None, Some(issuer))
    }

    fn held(&self, key: &str) -> Result<Option<HeldToken>> {
        let file = self.tokens.file(key);
        let tokens = self.tokens.lock();
        Ok(tokens
            .files
            .get(&file)
            .and_then(|value| tokens.records.get(value))
            .map(Record::held))
    }

    fn held_records(&self, principal: &Principal) -> Result<Vec<(String, HeldToken)>> {
        let held = self
            .tokens
            .records()
            .into_iter()
            .filter(|(_, record)| record.principal == *principal)
            .map(|(value, record)| (sha256_hex(value.as_bytes()), record.held()))
            .collect();
        let hook = self
            .on_held_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(hook) = hook {
            hook();
        }
        Ok(held)
    }

    fn revoke_record(&self, principal: &Principal, name: &str) -> Result<bool> {
        let file = self.tokens.file(&principal.credential_key());
        let mut tokens = self.tokens.lock();
        let Some(value) = tokens
            .records
            .iter()
            .find(|(value, record)| {
                record.principal == *principal && sha256_hex(value.as_bytes()) == name
            })
            .map(|(value, _)| value.clone())
        else {
            return Ok(false);
        };
        tokens.records.remove(&value);
        if tokens.files.get(&file) == Some(&value) {
            tokens.files.remove(&file);
        }
        Ok(true)
    }

    fn written_record(&self, key: &str) -> Result<Option<String>> {
        let file = self.tokens.file(key);
        Ok(self
            .tokens
            .lock()
            .files
            .get(&file)
            .map(|value| sha256_hex(value.as_bytes())))
    }

    fn bind_update_job(&self, token: &str, job: SessionProcess) -> Result<Option<SessionProcess>> {
        let hook = self
            .on_bind
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(hook) = hook {
            hook()?;
        }
        let mut tokens = self.tokens.lock();
        Ok(tokens
            .records
            .get_mut(token)
            .filter(|record| record.principal.role == ActorRole::Updater)
            .and_then(|record| record.issued_by.as_mut())
            .and_then(|issuer| {
                crate::domain::queue_service::bind_update_job(issuer, job);
                issuer.job.clone()
            }))
    }

    fn return_update_job(&self, token: &str, job: Option<SessionProcess>) -> Result<bool> {
        let mut tokens = self.tokens.lock();
        let Some(record) = tokens
            .records
            .get(token)
            .filter(|record| record.principal.role == ActorRole::Updater)
        else {
            return Ok(false);
        };
        let bound = record
            .issued_by
            .as_ref()
            .and_then(|issuer| issuer.job.as_ref());
        if crate::domain::queue_service::update_job_return_refusal(bound, job.as_ref()).is_some() {
            return Ok(false);
        }
        let file = self.tokens.file(&record.principal.credential_key());
        tokens.records.remove(token);
        if tokens.files.get(&file).map(String::as_str) == Some(token) {
            tokens.files.remove(&file);
        }
        Ok(true)
    }

    fn hold_issues(&self) -> Result<Box<dyn std::any::Any>> {
        let (held, released) = &*self.tokens.issuing;
        let mut held = held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *held {
            held = released
                .wait(held)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *held = true;
        Ok(Box::new(IssuesHeld(self.tokens.issuing.clone())))
    }

    fn issue_telemetry_as(
        &self,
        target: &StepTarget,
        task: Option<TaskId>,
        issued_under: Option<&LeaseToken>,
        issuer: &TokenIssuer,
    ) -> Result<PathBuf> {
        let principal = Principal::telemetry(target, task);
        self.tokens.revoke_under(&principal.actor_id, None);
        self.tokens
            .issue_token(&principal, issued_under, Some(issuer))
    }

    fn alive(&self, pid: u32) -> bool {
        self.processes.alive(pid)
    }
}
