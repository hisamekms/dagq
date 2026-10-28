//! A caller of the queue service that loses the answers of the calls it
//! names after the service did them, which the supervisor's tests through
//! the service share.

use std::path::Path;
use std::sync::Arc;

use dagq::application::supervisor_service::{ServiceCaller, ServiceUnreachable};
use dagq::domain::LeaseToken;

/// The service's caller that loses answers after the service did the
/// calls, as a service that stops or is replaced between its commit and
/// its answer: the answer of the first call of the first set of operations
/// it was given, then of the next set's first call after that, and so on.
pub struct LosingAnswers {
    pub inner: Arc<dyn ServiceCaller>,
    pub lose: std::sync::Mutex<Vec<&'static [&'static str]>>,
    pub lost: std::sync::Mutex<Vec<String>>,
}

impl ServiceCaller for LosingAnswers {
    fn call(
        &self,
        use_case: dagq::domain::queue_service::UseCase,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let op = params["op"].as_str().unwrap_or_default().to_owned();
        let answer = self.inner.call(use_case, params)?;
        let mut lose = self.lose.lock().unwrap();
        if lose.first().is_some_and(|ops| ops.contains(&op.as_str())) {
            lose.remove(0);
            self.lost.lock().unwrap().push(op.clone());
            return Err(
                ServiceUnreachable::maybe_done(format!("the answer to {op} was lost")).into(),
            );
        }
        Ok(answer)
    }
    fn with_token(&self, file: &Path) -> anyhow::Result<Arc<dyn ServiceCaller>> {
        self.inner.with_token(file)
    }
    fn sign_in(&self, registration: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.sign_in(registration, pid)
    }
    fn take_back(&self, registration: &LeaseToken, pid: u32) -> anyhow::Result<()> {
        self.inner.take_back(registration, pid)
    }
    fn hand_over(&self) {
        self.inner.hand_over();
    }
    fn sign_out(&self) {
        self.inner.sign_out();
    }
    fn credential(&self) -> Option<std::path::PathBuf> {
        self.inner.credential()
    }
}
