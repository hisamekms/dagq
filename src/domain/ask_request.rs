//! A Codex worker's `dagq ask` as a request in its run directory
//! (ADR-t813-3 decision 3). The Codex worker's sandbox does not let it
//! write the queue's directory, so its `dagq ask` writes the ask it would
//! open to [`ASK_REQUESTS_DIR`] under the run directory instead, and the
//! supervisor, the trusted control plane, holds it to the same policy and
//! checks as the command before it opens it on the queue. The worker's
//! turn is told the directory in [`ASK_REQUESTS_ENV`].

use serde::{Deserialize, Serialize};

use super::{ActorContext, AskKind, AskReason, FindingId, NewAsk, RunId, TaskId, check_ask_kind};

/// The directory a Codex worker's turn writes its ask requests to, as its
/// environment names it; the `dagq ask` of a worker with it set writes
/// there instead of opening the queue.
pub const ASK_REQUESTS_ENV: &str = "DAGQ_ASK_REQUESTS";

/// The directory of the ask requests under a run directory.
pub const ASK_REQUESTS_DIR: &str = "ask-requests";

/// The suffix of a request waiting to be taken: `<id>.json`.
pub const PENDING_SUFFIX: &str = ".json";

/// The suffix of a request the supervisor took (opened or refused, as its
/// `ask_request_taken` records): `<id>.taken`.
pub const TAKEN_SUFFIX: &str = ".taken";

/// The longest request id.
const MAX_ID_CHARS: usize = 64;

/// One `dagq ask` of a worker, as it wrote it: what the command's options
/// said, with the request's own id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskRequest {
    /// Unique per request; the file is `<id>.json`.
    pub id: String,
    pub kind: String,
    pub because: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    // Written only when given, so that a supervisor of an older build
    // still reads the request (ADR-t451-1 decision 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<i64>,
    #[serde(default)]
    pub finding_id: Option<i64>,
}

/// Whether `id` may name a request: letters, digits, `-` and `_`, not
/// empty and not too long, so that it is a file name of its own.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_CHARS
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The file name of the pending request `id`.
pub fn pending_name(id: &str) -> String {
    format!("{id}{PENDING_SUFFIX}")
}

/// The file name of the taken request `id`.
pub fn taken_name(id: &str) -> String {
    format!("{id}{TAKEN_SUFFIX}")
}

/// The id of a pending request's file name; `None` for anything else (a
/// temporary file, a taken request).
pub fn pending_id(name: &str) -> Option<&str> {
    name.strip_suffix(PENDING_SUFFIX)
        .filter(|id| !id.starts_with('.'))
}

impl AskRequest {
    /// The ask a worker's request opens, as `worker` asks it, for the task
    /// `task` (the run's): none that names a finding (a worker's question is
    /// about its run), and the checks [`Self::to_new_ask`] and
    /// [`check_ask_kind`] make of the command's options. Whether the worker
    /// may open it on the run or task it names is the policy's, apart. The
    /// command checks this before it writes a request, and the supervisor
    /// again before it opens one, since the file is the worker's.
    pub fn worker_ask(
        &self,
        worker: &ActorContext,
        task: Option<TaskId>,
    ) -> Result<NewAsk, String> {
        if self.finding_id.is_some() {
            return Err("a worker's ask names no finding".to_owned());
        }
        let ask = self.to_new_ask(worker.written_by())?;
        check_ask_kind(
            &ask.kind,
            task.or(ask.task_id),
            ask.run_id.as_ref(),
            ask.reason_category,
        )
        .map_err(|error| error.to_string())?;
        Ok(ask)
    }

    /// The ask the request opens, asked by `asked_by`: its kind, reason and
    /// targets parsed as the command parses its options. A value the
    /// command would refuse is the error, as the text the refusal records.
    pub fn to_new_ask(&self, asked_by: &str) -> Result<NewAsk, String> {
        let kind: AskKind = self
            .kind
            .parse()
            .map_err(|error| format!("kind {:?}: {error}", self.kind))?;
        if let AskKind::Other(kind) = &kind {
            return Err(format!("kind {kind:?} is no kind of ask"));
        }
        let reason: AskReason = self
            .because
            .parse()
            .map_err(|error| format!("because {:?}: {error}", self.because))?;
        let run_id = self
            .run_id
            .as_deref()
            .map(RunId::new)
            .transpose()
            .map_err(|error| format!("run: {error}"))?;
        let confidence = self
            .confidence
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(|error| format!("confidence: {error}"))?;
        let ask = NewAsk {
            recommendation: self.recommend.clone(),
            confidence,
            kind,
            task_id: self.task_id.map(TaskId::new),
            run_id,
            question: self.question.clone(),
            options: self.options.clone(),
            asked_by: asked_by.to_owned(),
            reason_category: reason,
            topics: self.topics.clone(),
            finding_id: self.finding_id.map(FindingId::new),
        };
        ask.validate().map_err(|error| error.to_string())?;
        Ok(ask)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> AskRequest {
        AskRequest {
            id: "r-1".to_owned(),
            kind: "worker_question".to_owned(),
            because: "scope".to_owned(),
            question: "Which?".to_owned(),
            options: vec!["a".to_owned()],
            topics: vec!["design_choice".to_owned()],
            recommend: Some("a".to_owned()),
            confidence: Some("high".to_owned()),
            run_id: Some("run-1".to_owned()),
            task_id: None,
            finding_id: None,
        }
    }

    #[test]
    fn a_request_opens_the_ask_its_options_name() {
        let ask = request().to_new_ask("worker").unwrap();
        assert_eq!(ask.kind, AskKind::WorkerQuestion);
        assert_eq!(ask.reason_category, AskReason::Scope);
        assert_eq!(ask.run_id.unwrap().as_str(), "run-1");
        assert_eq!(ask.asked_by, "worker");
        assert_eq!(ask.recommendation.as_deref(), Some("a"));
        assert_eq!(ask.confidence, Some(crate::domain::AskConfidence::High));
        // Without them the request is written as an older build reads it.
        let plain = serde_json::to_string(&AskRequest {
            recommend: None,
            confidence: None,
            ..request()
        })
        .unwrap();
        assert!(
            !plain.contains("recommend") && !plain.contains("confidence"),
            "{plain}"
        );
    }

    #[test]
    fn a_request_the_command_would_refuse_is_refused() {
        let broken = [
            AskRequest {
                kind: "whatever".to_owned(),
                ..request()
            },
            AskRequest {
                because: "why".to_owned(),
                ..request()
            },
            AskRequest {
                question: " ".to_owned(),
                ..request()
            },
            AskRequest {
                topics: vec![],
                ..request()
            },
            AskRequest {
                recommend: Some("b".to_owned()),
                ..request()
            },
            AskRequest {
                confidence: Some("medium".to_owned()),
                ..request()
            },
        ];
        for request in broken {
            assert!(request.to_new_ask("worker").is_err(), "{request:?}");
        }
    }

    #[test]
    fn a_workers_ask_names_no_finding_and_no_reason_of_the_queue() {
        let worker = ActorContext::worker(&RunId::new("run-1").unwrap(), TaskId::new(3));
        let ask = request().worker_ask(&worker, Some(TaskId::new(3))).unwrap();
        assert_eq!(ask.asked_by, "worker");
        let refused = [
            AskRequest {
                finding_id: Some(1),
                ..request()
            },
            AskRequest {
                because: "cost".to_owned(),
                ..request()
            },
            AskRequest {
                topics: vec![],
                ..request()
            },
        ];
        for request in refused {
            assert!(
                request.worker_ask(&worker, Some(TaskId::new(3))).is_err(),
                "{request:?}"
            );
        }
    }

    #[test]
    fn ids_and_names() {
        assert!(valid_id("0f3a-B_9"));
        for id in ["", "a/b", "../x", ".x", "a.b", &"x".repeat(65)] {
            assert!(!valid_id(id), "{id}");
        }
        assert_eq!(pending_id(&pending_name("r1")), Some("r1"));
        assert_eq!(pending_id(&taken_name("r1")), None);
        assert_eq!(pending_id(".r1.json"), None);
        assert_eq!(pending_id("r1.json.tmp"), None);
    }
}
