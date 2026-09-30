//! The topics of the workers' `worker_question` asks (ADR-t947-2 decision
//! 4): per primary topic how many asks, their share of the runs claimed in
//! the window, how long a person took to answer them (at night and by
//! day), and what became of the run after the answer; per topic of any
//! position how many asks carry it; and the `reason_category` each primary
//! topic came with. An ask opened before topics were kept is
//! [`UNLABELED_TOPIC`], and nothing recorded is rewritten.
use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::Value;

use super::{Summary, payload_status, summary, timestamp_millis};
use crate::domain::{EventId, RunEvent, TaskId, UNLABELED_TOPIC};

/// The hours of the host's local day, from the first up to (not
/// including) the second, when an ask opened counts as opened at night:
/// 22:00 to 07:00, as the analysis of task 950 split them.
pub const NIGHT_HOURS: (i64, i64) = (22, 7);

/// The worker_question asks opened in the window.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct WorkerQuestionTopics {
    /// The worker_question asks opened in the window.
    pub asks: i64,
    /// The runs claimed in the window: the whole the rates are shares of.
    pub runs: i64,
    /// Per primary topic.
    pub by_topic: BTreeMap<String, TopicAsks>,
    /// Per topic of any position: how many asks carry it.
    pub codes: BTreeMap<String, i64>,
}

/// The asks of one primary topic.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TopicAsks {
    pub asks: i64,
    /// The distinct runs they were asked on.
    pub runs: i64,
    /// `asks` over the window's `runs`.
    pub rate: Option<f64>,
    /// Per `reason_category` the asks came with.
    pub by_reason_category: BTreeMap<String, i64>,
    /// From `ask_opened` to the first `ask_answered` a person gave (the
    /// runtime's own closes are left out), of every ask and split by when
    /// it was opened: at night ([`NIGHT_HOURS`], the host's local time) or
    /// by day.
    pub to_answer: Summary,
    pub night: Summary,
    pub day: Summary,
    /// The seconds `night` and `day` sum, for `kpi`'s spreads.
    #[serde(skip)]
    pub night_values: Vec<i64>,
    #[serde(skip)]
    pub day_values: Vec<i64>,
    /// The asks with no answer yet.
    pub open: i64,
    /// What happened to the ask's run after the answer: it landed, it
    /// ended failed (its latest status is `failed` and it moved there
    /// after the answer), or its review raised a concern.
    pub outcomes: Outcomes,
}

/// What followed the answers on their runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Outcomes {
    pub landed: i64,
    pub failed: i64,
    pub concern: i64,
}

/// One worker_question as `ask_opened` recorded it.
struct Question {
    run: Option<String>,
    primary: String,
    topics: Vec<String>,
    reason: String,
    opened_ms: Option<i64>,
    /// The first `ask_answered`: its event and time, and whether the
    /// runtime wrote it itself.
    answered: Option<(EventId, Option<i64>, bool)>,
}

/// The topics an `ask_opened` payload recorded, primary first, or
/// [`UNLABELED_TOPIC`] alone when it has none.
pub fn topics_of(payload: &Value) -> Vec<String> {
    let topics: Vec<String> = payload["topics"]
        .as_array()
        .map(|topics| {
            topics
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if topics.is_empty() {
        vec![UNLABELED_TOPIC.to_owned()]
    } else {
        topics
    }
}

/// Whether `ms` falls in [`NIGHT_HOURS`] of the day `offset_ms` east of UTC.
pub fn at_night(ms: i64, offset_ms: i64) -> bool {
    let hour = (ms + offset_ms).rem_euclid(24 * 3_600_000) / 3_600_000;
    hour >= NIGHT_HOURS.0 || hour < NIGHT_HOURS.1
}

fn id_of(payload: &Value) -> Option<String> {
    let id = payload.get("ask_id")?;
    Some(id.as_str().map_or_else(|| id.to_string(), str::to_owned))
}

/// Count the worker_question asks opened with `after < id <= upto` whose
/// task `counts` accepts. The answers and what followed them are read from
/// every event, the ones after the window too; night and day are the
/// host's, `utc_offset_secs` east of UTC.
pub fn worker_question_topics(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    utc_offset_secs: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> WorkerQuestionTopics {
    let inside = |event: &RunEvent| event.id > after && event.id <= upto && counts(event.task_id);
    let mut table = WorkerQuestionTopics::default();
    let mut questions: Vec<Question> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    // Per run, the events after which something happened to it.
    let mut landed: HashMap<String, Vec<EventId>> = HashMap::new();
    // Per run, its latest status and the event it moved to it at, like
    // the `status` of `stats`' runs: a run that ended failed is one whose
    // latest status is `failed`.
    let mut statuses: HashMap<String, (String, EventId)> = HashMap::new();
    let mut concerns: HashMap<String, Vec<EventId>> = HashMap::new();
    for event in events {
        let run = event.run_id.as_ref().map(|run| run.as_str().to_owned());
        match event.kind.as_str() {
            "run_claimed" if inside(event) => table.runs += 1,
            "ask_opened"
                if inside(event) && event.payload["kind"].as_str() == Some("worker_question") =>
            {
                let topics = topics_of(&event.payload);
                if let Some(id) = id_of(&event.payload) {
                    by_id.insert(id, questions.len());
                }
                questions.push(Question {
                    run,
                    primary: topics[0].clone(),
                    topics,
                    reason: event.payload["reason_category"]
                        .as_str()
                        .unwrap_or(super::asks::UNKNOWN)
                        .to_owned(),
                    opened_ms: timestamp_millis(&event.created_at),
                    answered: None,
                });
                continue;
            }
            "ask_answered" => {
                if let Some(question) = id_of(&event.payload)
                    .and_then(|id| by_id.get(&id))
                    .and_then(|index| questions.get_mut(*index))
                    && question.answered.is_none()
                {
                    question.answered = Some((
                        event.id,
                        timestamp_millis(&event.created_at),
                        event.payload.get("runtime_closed") == Some(&Value::Bool(true)),
                    ));
                }
            }
            "run_integrated" => {
                if let Some(run) = &run {
                    landed.entry(run.clone()).or_default().push(event.id);
                }
            }
            "review_finished" if event.payload["verdict"].as_str() == Some("concern") => {
                if let Some(run) = &run {
                    concerns.entry(run.clone()).or_default().push(event.id);
                }
            }
            _ => {}
        }
        let status = if event.kind == "run_integrated" {
            Some("integrated")
        } else {
            payload_status(&event.payload)
        };
        if let (Some(status), Some(run)) = (status, run) {
            let latest = statuses
                .entry(run)
                .or_insert_with(|| (status.to_owned(), event.id));
            if latest.0 != status {
                *latest = (status.to_owned(), event.id);
            }
        }
    }
    let failed: HashMap<String, Vec<EventId>> = statuses
        .into_iter()
        .filter(|(_, (status, _))| status == "failed")
        .map(|(run, (_, since))| (run, vec![since]))
        .collect();
    let offset_ms = utc_offset_secs * 1000;
    let mut secs: BTreeMap<String, (Vec<i64>, Vec<i64>)> = BTreeMap::new();
    let mut runs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for question in &questions {
        table.asks += 1;
        for topic in &question.topics {
            *table.codes.entry(topic.clone()).or_default() += 1;
        }
        let entry = table.by_topic.entry(question.primary.clone()).or_default();
        entry.asks += 1;
        *entry
            .by_reason_category
            .entry(question.reason.clone())
            .or_default() += 1;
        if let Some(run) = &question.run {
            runs.entry(question.primary.clone())
                .or_default()
                .push(run.clone());
        }
        let Some((answered, at, runtime_closed)) = question.answered else {
            entry.open += 1;
            continue;
        };
        if !runtime_closed && let (Some(from), Some(to)) = (question.opened_ms, at) {
            let (night, day) = secs.entry(question.primary.clone()).or_default();
            if at_night(from, offset_ms) {
                night.push((to - from) / 1000);
            } else {
                day.push((to - from) / 1000);
            }
        }
        let after_answer = |of: &HashMap<String, Vec<EventId>>| {
            question.run.as_ref().is_some_and(|run| {
                of.get(run)
                    .is_some_and(|ids| ids.iter().any(|id| *id > answered))
            })
        };
        entry.outcomes.landed += i64::from(after_answer(&landed));
        entry.outcomes.failed += i64::from(after_answer(&failed));
        entry.outcomes.concern += i64::from(after_answer(&concerns));
    }
    for (topic, entry) in &mut table.by_topic {
        entry.rate = rate(entry.asks, table.runs);
        if let Some(mut of) = runs.remove(topic) {
            of.sort_unstable();
            of.dedup();
            entry.runs = i64::try_from(of.len()).unwrap_or(i64::MAX);
        }
        if let Some((night, day)) = secs.remove(topic) {
            entry.to_answer = summary(night.iter().chain(&day).copied().map(Some));
            entry.night = summary(night.iter().copied().map(Some));
            entry.day = summary(day.iter().copied().map(Some));
            entry.night_values = night;
            entry.day_values = day;
        }
    }
    table
}

fn rate(part: i64, whole: i64) -> Option<f64> {
    #[allow(clippy::cast_precision_loss)]
    (whole > 0).then(|| ((part as f64 / whole as f64) * 1000.0).round() / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RunId;
    use serde_json::json;

    /// An event at `hour`:`minute` UTC of 2026-09-28.
    fn event(id: i64, task: i64, run: &str, kind: &str, payload: Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task)),
            goal_id: None,
            run_id: Some(RunId::new(run).unwrap()),
            kind: kind.into(),
            payload,
            created_at: format!("2026-09-28T{at}:00.000Z"),
            actor: None,
        }
    }

    fn question(id: i64, ask: i64, task: i64, run: &str, topics: Value, at: &str) -> RunEvent {
        let mut payload = json!({"ask_id": ask, "kind": "worker_question", "asked_by": "worker",
            "reason_category": "scope"});
        if !topics.is_null() {
            payload["topics"] = topics;
        }
        event(id, task, run, "ask_opened", payload, at)
    }

    fn answer(id: i64, ask: i64, task: i64, run: &str, at: &str) -> RunEvent {
        event(
            id,
            task,
            run,
            "ask_answered",
            json!({"ask_id": ask, "kind": "worker_question", "answered_by": "inbox"}),
            at,
        )
    }

    /// Per primary topic the asks, their rate, the answer times at night
    /// and by day, and what followed; the secondary topics count in
    /// `codes`, an ask without topics is `unlabeled`, and only the window
    /// and the goal's tasks count.
    #[test]
    fn counts_the_worker_questions_by_their_primary_topic() {
        let events = vec![
            event(1, 1, "r1", "run_claimed", json!({}), "10:00"),
            event(2, 2, "r2", "run_claimed", json!({}), "10:00"),
            event(3, 3, "r3", "run_claimed", json!({}), "10:00"),
            event(4, 9, "r9", "run_claimed", json!({}), "10:00"),
            // At 23:00 JST (14:00 UTC), answered 10 minutes later; the run lands.
            question(
                5,
                1,
                1,
                "r1",
                json!(["adr_conflict", "task_overlap"]),
                "14:00",
            ),
            answer(6, 1, 1, "r1", "14:10"),
            event(7, 1, "r1", "run_integrated", json!({}), "14:30"),
            // At 12:00 JST (03:00 UTC), answered in 2 minutes; its review raises a concern.
            question(8, 2, 2, "r2", json!(["adr_conflict"]), "03:00"),
            answer(9, 2, 2, "r2", "03:02"),
            event(
                10,
                2,
                "r2",
                "review_finished",
                json!({"verdict": "concern"}),
                "03:20",
            ),
            // No topics (before they were kept), still open.
            question(11, 3, 3, "r3", Value::Null, "04:00"),
            // Another kind, and another goal's task, count nowhere.
            event(
                12,
                3,
                "r3",
                "ask_opened",
                json!({"ask_id": 4, "kind": "decide", "topics": ["x"]}),
                "04:00",
            ),
            question(13, 5, 9, "r9", json!(["other"]), "04:00"),
            // A failure after an answer the runtime wrote itself.
            question(14, 6, 3, "r3", json!(["precondition_missing"]), "05:00"),
            event(
                15,
                3,
                "r3",
                "ask_answered",
                json!({"ask_id": 6, "runtime_closed": true}),
                "05:01",
            ),
            event(
                16,
                3,
                "r3",
                "run_status_changed",
                json!({"status": "failed"}),
                "05:02",
            ),
        ];
        let jst = 9 * 3600;
        let table =
            worker_question_topics(&events, EventId::new(0), EventId::new(16), jst, |task| {
                task != Some(TaskId::new(9))
            });
        assert_eq!(table.asks, 4);
        assert_eq!(table.runs, 3);
        assert_eq!(
            table.codes,
            BTreeMap::from([
                ("adr_conflict".to_owned(), 2),
                ("precondition_missing".to_owned(), 1),
                ("task_overlap".to_owned(), 1),
                ("unlabeled".to_owned(), 1),
            ])
        );
        let adr = &table.by_topic["adr_conflict"];
        assert_eq!((adr.asks, adr.runs, adr.rate), (2, 2, Some(0.667)));
        assert_eq!(adr.by_reason_category["scope"], 2);
        assert_eq!((adr.to_answer.count, adr.to_answer.total), (2, 720));
        assert_eq!((adr.night.count, adr.night.total), (1, 600));
        assert_eq!((adr.day.count, adr.day.total), (1, 120));
        assert_eq!(
            adr.outcomes,
            Outcomes {
                landed: 1,
                failed: 0,
                concern: 1
            }
        );
        let unlabeled = &table.by_topic["unlabeled"];
        assert_eq!((unlabeled.asks, unlabeled.open), (1, 1));
        let closed = &table.by_topic["precondition_missing"];
        assert_eq!(closed.to_answer.count, 0, "the runtime's close is no wait");
        assert_eq!(closed.outcomes.failed, 1);
        assert!(!table.by_topic.contains_key("other"));

        // A window that starts after the questions counts none of them.
        let later =
            worker_question_topics(&events, EventId::new(16), EventId::new(16), jst, |_| true);
        assert_eq!((later.asks, later.runs), (0, 0));
        assert!(later.by_topic.is_empty());
    }

    /// A run that fails after the answer, is resumed and lands counts as
    /// landed only; one that ends failed after the answer counts as failed
    /// once, however many events repeat its status; a failure before the
    /// answer counts for none.
    #[test]
    fn counts_failed_only_when_the_run_ends_failed_after_the_answer() {
        let status = |id, task, run, kind, status: &str| {
            event(id, task, run, kind, json!({"status": status}), "10:30")
        };
        let events = vec![
            question(1, 1, 1, "r1", json!(["design_choice"]), "10:00"),
            answer(2, 1, 1, "r1", "10:05"),
            status(3, 1, "r1", "session_finished", "failed"),
            status(4, 1, "r1", "triage_finished", "failed"),
            status(5, 1, "r1", "resume_started", "running"),
            status(6, 1, "r1", "run_integrated", "integrated"),
            question(7, 2, 2, "r2", json!(["design_choice"]), "10:00"),
            answer(8, 2, 2, "r2", "10:05"),
            status(9, 2, "r2", "session_finished", "failed"),
            status(10, 2, "r2", "triage_finished", "failed"),
            status(11, 3, "r3", "session_finished", "failed"),
            question(12, 3, 3, "r3", json!(["other"]), "10:40"),
            event(
                13,
                3,
                "r3",
                "ask_answered",
                json!({"ask_id": 3, "status": "failed"}),
                "10:45",
            ),
        ];
        let table = worker_question_topics(&events, EventId::new(0), EventId::new(13), 0, |_| true);
        assert_eq!(
            table.by_topic["design_choice"].outcomes,
            Outcomes {
                landed: 1,
                failed: 1,
                concern: 0
            }
        );
        assert_eq!(table.by_topic["other"].outcomes, Outcomes::default());
    }

    #[test]
    fn night_is_from_22_to_7_of_the_hosts_day() {
        let hour = 3_600_000;
        let jst = 9 * hour;
        // 13:00 UTC is 22:00 JST, 22:00 UTC is 07:00 JST.
        assert!(at_night(13 * hour, jst));
        assert!(at_night(21 * hour + 59 * 60_000, jst));
        assert!(!at_night(22 * hour, jst));
        assert!(!at_night(12 * hour + 59 * 60_000, jst));
        assert!(at_night(-hour, 0), "23:00 of the day before");
        assert_eq!(topics_of(&json!({"topics": []})), ["unlabeled"]);
        assert_eq!(topics_of(&json!({"topics": ["a", 3, "b"]})), ["a", "b"]);
    }
}
