//! The reason codes of the review and plan review verdicts that sent the
//! work back (ADR-t947-1 decision 5): per primary code how many verdicts,
//! runs or proposals and at what rate, the time they cost (the revise's
//! fix, the concern's wait for a person, the work after a `send_back`) and
//! what a person's answer made of them; per code of any item how many
//! verdicts carry it; and per change of task the rate. A verdict recorded
//! before the codes is `unlabeled`, and nothing recorded is rewritten.
use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;
use serde_json::Value;

use super::{Summary, summary, timestamp_millis};
use crate::domain::{
    EventId, RunEvent, TaskChange, TaskId, event_kind,
    review_reason::{DEVIATION_REJECTED, OUTCOMES, UNLABELED},
};

/// The run reviews' and the plan reviews' tables.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ReviewReasons {
    pub review: ReasonTable,
    pub plan_review: ReasonTable,
}

/// The verdicts of one kind of review in the window.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ReasonTable {
    /// Every verdict, the passes too.
    pub verdicts: i64,
    /// The runs (review) or proposals (plan review) with a verdict.
    pub reviewed: i64,
    /// Those with a `revise` or a `concern` among them.
    pub sent_back: i64,
    /// `sent_back` over `reviewed`.
    pub rate: Option<f64>,
    /// Per primary code of the verdicts that sent the work back.
    pub by_code: BTreeMap<String, CodeStats>,
    /// Per code of any item of those verdicts: how many verdicts carry it.
    pub codes: BTreeMap<String, i64>,
    /// Per change of the task (ADR-t980-1; of a proposal: the task its
    /// events are on), set by [`super::with_changes`]; tasks without a
    /// change last.
    pub by_change: Vec<ChangeReasons>,
    /// Each reviewed run or proposal: its task and the primary codes of
    /// its verdicts that sent it back.
    #[serde(skip)]
    subjects: Vec<(Option<TaskId>, BTreeSet<String>)>,
}

/// The verdicts of one primary code.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CodeStats {
    /// Its verdicts, and of them the revises and the concerns (plan
    /// review: by the decision applied).
    pub verdicts: i64,
    pub revise: i64,
    pub concern: i64,
    /// The runs or proposals with one of its verdicts, and their share of
    /// `reviewed`.
    pub subjects: i64,
    pub rate: Option<f64>,
    /// A revise's fix: review, from the request to `revise_finished`;
    /// plan review, from the verdict to the proposal's next review.
    pub revise_secs: Summary,
    /// A concern's ask from its opening to its answer (else from the
    /// verdict to the outcome).
    pub concern_wait_secs: Summary,
    /// After a `send_back`: review, the resume that followed; plan review,
    /// up to the proposal's next review.
    pub send_back_resume_secs: Summary,
    /// What the answers made of its concerns (`review_outcome` /
    /// `plan_review_outcome`).
    pub outcomes: BTreeMap<&'static str, i64>,
}

/// The rate of one change of task (ADR-t980-1).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChangeReasons {
    pub change: Option<TaskChange>,
    pub reviewed: i64,
    pub sent_back: i64,
    pub rate: Option<f64>,
    /// The runs or proposals sent back per primary code.
    pub by_code: BTreeMap<String, i64>,
}

/// One verdict of a run that sent it back, for `stats`' `runs[]`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunReviewReason {
    pub attempt: Option<i64>,
    pub verdict: String,
    pub primary_code: String,
    pub reason_codes: Vec<Vec<String>>,
    /// What the answer to its ask made of it; null without one.
    pub outcome: Option<String>,
}

fn rate(part: i64, whole: i64) -> Option<f64> {
    #[allow(clippy::cast_precision_loss)]
    (whole > 0).then(|| ((part as f64 / whole as f64) * 1000.0).round() / 1000.0)
}

fn ms(event: &RunEvent) -> Option<i64> {
    timestamp_millis(&event.created_at)
}

fn secs(from: Option<i64>, to: Option<i64>) -> Option<i64> {
    Some((to? - from?) / 1000)
}

/// The codes a verdict's event recorded, each item's; a verdict before the
/// codes has each of its reasons (or, with none, itself) unlabeled.
fn codes_of(payload: &Value) -> (String, Vec<Vec<String>>) {
    let codes: Vec<Vec<String>> = serde_json::from_value(payload["reason_codes"].clone())
        .ok()
        .filter(|codes: &Vec<Vec<String>>| !codes.is_empty())
        .unwrap_or_else(|| {
            let reasons = payload["reasons"].as_array().map_or(0, Vec::len);
            vec![vec![UNLABELED.to_owned()]; reasons.max(1)]
        });
    let primary = payload["primary_code"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| codes.first()?.first().cloned())
        .unwrap_or_else(|| UNLABELED.to_owned());
    (primary, codes)
}

fn text_id(value: &Value) -> Option<String> {
    (!value.is_null()).then(|| {
        value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned)
    })
}

/// When each ask was opened and answered, by its id.
fn ask_times(events: &[RunEvent]) -> HashMap<String, (Option<i64>, Option<i64>)> {
    let mut times: HashMap<String, (Option<i64>, Option<i64>)> = HashMap::new();
    for event in events {
        let id = || {
            text_id(
                event
                    .payload
                    .get("ask_id")
                    .or_else(|| event.payload.get("id"))?,
            )
        };
        match event.kind.as_str() {
            event_kind::ASK_OPENED => {
                if let Some(id) = id() {
                    times.entry(id).or_default().0 = ms(event);
                }
            }
            event_kind::ASK_ANSWERED => {
                if let Some(id) = id() {
                    let entry = times.entry(id).or_default();
                    entry.1 = entry.1.or(ms(event));
                }
            }
            _ => {}
        }
    }
    times
}

/// One verdict that sent the work back, and what came of it.
struct SentBack<'a> {
    decision: &'a str,
    primary: String,
    codes: Vec<Vec<String>>,
    revise_secs: Option<i64>,
    concern_wait_secs: Option<i64>,
    send_back_resume_secs: Option<i64>,
    outcome: Option<&'static str>,
}

/// The verdicts of every run review in `events` that sent a run back, with
/// what came of them, per run.
fn run_verdicts<'a>(
    events: &'a [RunEvent],
    asks: &HashMap<String, (Option<i64>, Option<i64>)>,
) -> Vec<(&'a RunEvent, Option<SentBack<'a>>)> {
    let mut by_run: HashMap<&str, Vec<&RunEvent>> = HashMap::new();
    for event in events {
        if let Some(run_id) = &event.run_id
            && matches!(
                event.kind.as_str(),
                event_kind::REVIEW_FINISHED
                    | event_kind::REVISE_REQUESTED
                    | event_kind::REVISE_FINISHED
                    | event_kind::REVIEW_OUTCOME
                    | event_kind::RESUME_STARTED
                    | event_kind::RESUME_FINISHED
            )
        {
            by_run.entry(run_id.as_str()).or_default().push(event);
        }
    }
    let mut verdicts = Vec::new();
    for run_events in by_run.values() {
        for (index, event) in run_events.iter().enumerate() {
            if event.kind != event_kind::REVIEW_FINISHED {
                continue;
            }
            let decision = event.payload["verdict"].as_str().unwrap_or_default();
            if !matches!(decision, "revise" | "concern") {
                verdicts.push((*event, None));
                continue;
            }
            // What followed up to the run's next review.
            let after: Vec<&RunEvent> = run_events[index + 1..]
                .iter()
                .copied()
                .take_while(|e| e.kind != event_kind::REVIEW_FINISHED)
                .collect();
            let find = |kind: &str, from: usize| {
                after
                    .iter()
                    .enumerate()
                    .skip(from)
                    .find(|(_, e)| e.kind == kind)
            };
            let revise_secs = find(event_kind::REVISE_REQUESTED, 0).and_then(|(at, requested)| {
                let attempt = &requested.payload["attempt"];
                let (_, finished) = after.iter().enumerate().skip(at).find(|(_, e)| {
                    e.kind == event_kind::REVISE_FINISHED && e.payload["attempt"] == *attempt
                })?;
                secs(ms(requested), ms(finished))
            });
            let outcome = find(event_kind::REVIEW_OUTCOME, 0);
            let concern_wait_secs = outcome.and_then(|(_, outcome)| {
                let ask = text_id(&outcome.payload["ask_id"]).and_then(|id| asks.get(&id));
                match ask {
                    Some(&(Some(opened), Some(answered))) => secs(Some(opened), Some(answered)),
                    _ => secs(ms(event), ms(outcome)),
                }
            });
            let outcome_text = outcome
                .and_then(|(_, outcome)| outcome.payload["outcome"].as_str())
                .and_then(|text| OUTCOMES.iter().copied().find(|known| *known == text));
            let send_back_resume_secs = outcome
                .filter(|_| outcome_text == Some(DEVIATION_REJECTED))
                .and_then(|(at, _)| {
                    let (at, started) = find(event_kind::RESUME_STARTED, at)?;
                    let (_, finished) = find(event_kind::RESUME_FINISHED, at)?;
                    secs(ms(started), ms(finished))
                });
            let (primary, codes) = codes_of(&event.payload);
            verdicts.push((
                *event,
                Some(SentBack {
                    decision,
                    primary,
                    codes,
                    revise_secs,
                    concern_wait_secs,
                    send_back_resume_secs,
                    outcome: outcome_text,
                }),
            ));
        }
    }
    verdicts.sort_by_key(|(event, _)| event.id);
    verdicts
}

/// The verdicts of every plan review in `events`, with what came of those
/// that sent a proposal back.
fn plan_verdicts<'a>(
    events: &'a [RunEvent],
    asks: &HashMap<String, (Option<i64>, Option<i64>)>,
) -> Vec<(&'a RunEvent, Option<SentBack<'a>>)> {
    let mut by_proposal: HashMap<i64, Vec<&RunEvent>> = HashMap::new();
    for event in events.iter().filter(|e| {
        matches!(
            e.kind.as_str(),
            event_kind::PLAN_REVIEW_STARTED
                | event_kind::PLAN_REVIEW_FINISHED
                | event_kind::PLAN_REVIEW_OUTCOME
        )
    }) {
        if let Some(proposal) = event.payload["proposal_id"].as_i64() {
            by_proposal.entry(proposal).or_default().push(event);
        }
    }
    let mut verdicts = Vec::new();
    for proposal_events in by_proposal.values() {
        for (index, event) in proposal_events.iter().copied().enumerate() {
            if event.kind != event_kind::PLAN_REVIEW_FINISHED {
                continue;
            }
            let decision = event.payload["decision"]
                .as_str()
                .or_else(|| event.payload["verdict"].as_str())
                .unwrap_or_default();
            if !matches!(decision, "revise" | "concern") {
                verdicts.push((event, None));
                continue;
            }
            let later = || proposal_events[index + 1..].iter().copied();
            let next_review = |from: i64| {
                later()
                    .filter(|e| e.kind == event_kind::PLAN_REVIEW_STARTED)
                    .filter_map(ms)
                    .find(|&at| at >= from)
            };
            let revise_secs = (decision == "revise")
                .then(|| ms(event).and_then(|at| secs(Some(at), next_review(at))))
                .flatten();
            let plan_review_id = &event.payload["plan_review_id"];
            let outcome = later().find(|e| {
                e.kind == event_kind::PLAN_REVIEW_OUTCOME
                    && e.payload["plan_review_id"] == *plan_review_id
            });
            let concern_wait_secs = outcome.and_then(|outcome| {
                match text_id(&event.payload["ask_id"]).and_then(|id| asks.get(&id)) {
                    Some(&(Some(opened), Some(answered))) => secs(Some(opened), Some(answered)),
                    _ => secs(ms(event), ms(outcome)),
                }
            });
            let outcome_text = outcome
                .and_then(|outcome| outcome.payload["outcome"].as_str())
                .and_then(|text| OUTCOMES.iter().copied().find(|known| *known == text));
            let send_back_resume_secs = outcome
                .filter(|_| outcome_text == Some(DEVIATION_REJECTED))
                .and_then(ms)
                .and_then(|at| secs(Some(at), next_review(at)));
            let (primary, codes) = codes_of(&event.payload);
            verdicts.push((
                event,
                Some(SentBack {
                    decision,
                    primary,
                    codes,
                    revise_secs,
                    concern_wait_secs,
                    send_back_resume_secs,
                    outcome: outcome_text,
                }),
            ));
        }
    }
    verdicts.sort_by_key(|(event, _)| event.id);
    verdicts
}

/// The table of `verdicts` whose event is in the window after `after` up
/// to `upto` and whose task `counts` accepts; `subject` names the run or
/// proposal a verdict is of.
fn table(
    verdicts: &[(&RunEvent, Option<SentBack<'_>>)],
    after: EventId,
    upto: EventId,
    counts: &impl Fn(Option<TaskId>) -> bool,
    subject: impl Fn(&RunEvent) -> Option<String>,
) -> ReasonTable {
    let mut table = ReasonTable::default();
    let mut subjects: BTreeMap<String, (Option<TaskId>, BTreeSet<String>)> = BTreeMap::new();
    let mut per_code: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut times: BTreeMap<String, [Vec<Option<i64>>; 3]> = BTreeMap::new();
    for (event, sent_back) in verdicts
        .iter()
        .filter(|(e, _)| e.id > after && e.id <= upto && counts(e.task_id))
    {
        table.verdicts += 1;
        let Some(name) = subject(event) else {
            continue;
        };
        let entry = subjects
            .entry(name.clone())
            .or_insert_with(|| (event.task_id, BTreeSet::new()));
        let Some(sent_back) = sent_back else {
            continue;
        };
        entry.1.insert(sent_back.primary.clone());
        per_code
            .entry(sent_back.primary.clone())
            .or_default()
            .insert(name);
        let code = table.by_code.entry(sent_back.primary.clone()).or_default();
        code.verdicts += 1;
        match sent_back.decision {
            "revise" => code.revise += 1,
            _ => code.concern += 1,
        }
        if let Some(outcome) = sent_back.outcome {
            *code.outcomes.entry(outcome).or_default() += 1;
        }
        let spans = times.entry(sent_back.primary.clone()).or_default();
        spans[0].push(sent_back.revise_secs);
        spans[1].push(sent_back.concern_wait_secs);
        spans[2].push(sent_back.send_back_resume_secs);
        let carried: BTreeSet<&String> = sent_back.codes.iter().flatten().collect();
        for code in carried {
            *table.codes.entry(code.clone()).or_default() += 1;
        }
    }
    table.reviewed = subjects.len() as i64;
    table.sent_back = subjects
        .values()
        .filter(|(_, codes)| !codes.is_empty())
        .count() as i64;
    table.rate = rate(table.sent_back, table.reviewed);
    for (name, code) in &mut table.by_code {
        code.subjects = per_code.get(name).map_or(0, BTreeSet::len) as i64;
        code.rate = rate(code.subjects, table.reviewed);
        for outcome in OUTCOMES {
            code.outcomes.entry(outcome).or_default();
        }
        if let Some([revise, concern, resume]) = times.remove(name) {
            code.revise_secs = summary(revise.into_iter());
            code.concern_wait_secs = summary(concern.into_iter());
            code.send_back_resume_secs = summary(resume.into_iter());
        }
    }
    table.subjects = subjects.into_values().collect();
    table
}

/// The tables of the verdicts recorded in the window after `after` up to
/// `upto`, of the tasks `counts` accepts (the time after a verdict is read
/// past the window's end).
pub fn review_reasons(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> ReviewReasons {
    let asks = ask_times(events);
    ReviewReasons {
        review: table(&run_verdicts(events, &asks), after, upto, &counts, |e| {
            e.run_id.as_ref().map(|run| run.as_str().to_owned())
        }),
        plan_review: table(&plan_verdicts(events, &asks), after, upto, &counts, |e| {
            e.payload["proposal_id"].as_i64().map(|id| id.to_string())
        }),
    }
}

/// The verdicts that sent `run_events` (one run's, ascending) back, for
/// `stats`' `runs[]`.
pub fn per_run(run_events: &[RunEvent]) -> Vec<RunReviewReason> {
    let asks = HashMap::new();
    run_verdicts(run_events, &asks)
        .into_iter()
        .filter_map(|(event, sent_back)| {
            let sent_back = sent_back?;
            Some(RunReviewReason {
                attempt: event.payload["attempt"].as_i64(),
                verdict: sent_back.decision.to_owned(),
                primary_code: sent_back.primary,
                reason_codes: sent_back.codes,
                outcome: sent_back.outcome.map(str::to_owned),
            })
        })
        .collect()
}

/// One group's tally of the reviewed runs or proposals.
#[derive(Default)]
struct Tally {
    reviewed: i64,
    sent_back: i64,
    by_code: BTreeMap<String, i64>,
}

impl ReasonTable {
    /// The reviewed runs or proposals grouped by `key` of their task
    /// (`None` last), in the order of the key's text.
    fn grouped<K: Clone>(
        &self,
        key: impl Fn(Option<TaskId>) -> Option<K>,
        text: impl Fn(&K) -> &str,
    ) -> Vec<(Option<K>, Tally, Option<f64>)> {
        let mut groups: BTreeMap<(bool, Option<String>), (Option<K>, Tally)> = BTreeMap::new();
        for (task_id, codes) in &self.subjects {
            let value = key(*task_id);
            let (_, group) = groups
                .entry((value.is_none(), value.as_ref().map(|v| text(v).to_owned())))
                .or_insert_with(|| (value.clone(), Tally::default()));
            group.reviewed += 1;
            if !codes.is_empty() {
                group.sent_back += 1;
            }
            for code in codes {
                *group.by_code.entry(code.clone()).or_default() += 1;
            }
        }
        groups
            .into_values()
            .map(|(value, tally)| {
                let rate = rate(tally.sent_back, tally.reviewed);
                (value, tally, rate)
            })
            .collect()
    }

    /// Group the reviewed runs or proposals by the change of their task
    /// (ADR-t980-1).
    pub(super) fn with_changes(&mut self, changes: &HashMap<TaskId, Option<TaskChange>>) {
        self.by_change = self
            .grouped(
                |task| task.and_then(|task| changes.get(&task).cloned().flatten()),
                TaskChange::as_str,
            )
            .into_iter()
            .map(|(change, tally, rate)| ChangeReasons {
                change,
                reviewed: tally.reviewed,
                sent_back: tally.sent_back,
                rate,
                by_code: tally.by_code,
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::RunId;

    fn event(
        id: i64,
        task: i64,
        run: Option<&str>,
        kind: &str,
        secs: i64,
        payload: Value,
    ) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task)),
            run_id: run.map(|run| RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            goal_id: None,
            payload,
            created_at: format!("2026-09-28T00:{:02}:{:02}Z", secs / 60, secs % 60),
            actor: None,
        }
    }

    #[test]
    fn a_run_reviews_codes_times_and_outcomes_are_tallied() {
        let events = vec![
            // Run a: a revise (fixed in 60 s), then a concern sent back.
            event(
                1,
                1,
                Some("a"),
                "review_finished",
                0,
                json!({"verdict": "revise", "attempt": 1, "reasons": ["t"], "reason_codes": [["test_gap", "docs_drift"]], "primary_code": "test_gap"}),
            ),
            event(
                2,
                1,
                Some("a"),
                "revise_requested",
                10,
                json!({"attempt": 1}),
            ),
            event(
                3,
                1,
                Some("a"),
                "revise_finished",
                70,
                json!({"attempt": 1}),
            ),
            event(
                4,
                1,
                Some("a"),
                "review_finished",
                100,
                json!({"verdict": "concern", "attempt": 2, "reasons": ["x", "y"], "reason_codes": [["adr_conflict"], ["unlabeled"]], "primary_code": "adr_conflict"}),
            ),
            event(5, 1, Some("a"), "ask_opened", 110, json!({"ask_id": 7})),
            event(6, 1, Some("a"), "ask_answered", 410, json!({"ask_id": 7})),
            event(
                7,
                1,
                Some("a"),
                "review_outcome",
                420,
                json!({"attempt": 2, "ask_id": 7, "outcome": "deviation_rejected", "reason_codes": [["adr_conflict"]], "primary_code": "adr_conflict"}),
            ),
            event(8, 1, Some("a"), "resume_started", 500, json!({})),
            event(9, 1, Some("a"), "resume_finished", 800, json!({})),
            // Run b: a concern from before the codes, landed by a person.
            event(
                10,
                2,
                Some("b"),
                "review_finished",
                900,
                json!({"verdict": "concern", "attempt": 1, "reasons": ["old"]}),
            ),
            event(
                11,
                2,
                Some("b"),
                "review_outcome",
                1000,
                json!({"attempt": 1, "ask_id": 8, "outcome": "deviation_accepted"}),
            ),
            // Run c: a pass.
            event(
                12,
                3,
                Some("c"),
                "review_finished",
                1100,
                json!({"verdict": "pass", "reasons": []}),
            ),
        ];
        let reasons = review_reasons(&events, EventId::new(0), EventId::new(12), |_| true);
        let review = &reasons.review;
        assert_eq!(
            (review.verdicts, review.reviewed, review.sent_back),
            (4, 3, 2)
        );
        assert_eq!(review.rate, Some(0.667));
        let gap = &review.by_code["test_gap"];
        assert_eq!(
            (gap.verdicts, gap.revise, gap.concern, gap.subjects),
            (1, 1, 0, 1)
        );
        assert_eq!(
            gap.revise_secs,
            Summary {
                count: 1,
                total: 60,
                median: Some(60)
            }
        );
        let adr = &review.by_code["adr_conflict"];
        assert_eq!(adr.concern_wait_secs.total, 300);
        assert_eq!(adr.send_back_resume_secs.total, 300);
        assert_eq!(adr.outcomes[DEVIATION_REJECTED], 1);
        assert_eq!(adr.outcomes["canceled"], 0);
        let old = &review.by_code[UNLABELED];
        assert_eq!(old.outcomes["deviation_accepted"], 1);
        // Measured from the verdict when no ask times were recorded.
        assert_eq!(old.concern_wait_secs.total, 100);
        assert_eq!(review.codes["docs_drift"], 1);
        assert_eq!(review.codes[UNLABELED], 2);
        assert_eq!(reasons.plan_review, ReasonTable::default());

        let mut review = review.clone();
        let changes = HashMap::from([
            (TaskId::new(1), Some("fix".parse::<TaskChange>().unwrap())),
            (TaskId::new(2), Some("docs".parse::<TaskChange>().unwrap())),
            (TaskId::new(3), None),
        ]);
        review.with_changes(&changes);
        let names: Vec<Option<&str>> = review
            .by_change
            .iter()
            .map(|group| group.change.as_ref().map(TaskChange::as_str))
            .collect();
        assert_eq!(names, [Some("docs"), Some("fix"), None]);
        let fix = &review.by_change[1];
        assert_eq!((fix.reviewed, fix.sent_back, fix.rate), (1, 1, Some(1.0)));
        assert_eq!(fix.by_code["adr_conflict"], 1);
        assert_eq!(fix.by_code["test_gap"], 1);
        assert_eq!(review.by_change[0].by_code[UNLABELED], 1);
        let unknown = &review.by_change[2];
        assert_eq!(
            (unknown.reviewed, unknown.sent_back, unknown.rate),
            (1, 0, Some(0.0))
        );

        let run: Vec<RunEvent> = events
            .iter()
            .filter(|e| e.task_id == Some(TaskId::new(1)))
            .cloned()
            .collect();
        let per = per_run(&run);
        assert_eq!(per.len(), 2);
        assert_eq!(per[0].primary_code, "test_gap");
        assert_eq!(per[1].outcome.as_deref(), Some(DEVIATION_REJECTED));
        // Outside the window nothing counts.
        let none = review_reasons(&events, EventId::new(12), EventId::new(12), |_| true);
        assert_eq!(none.review.verdicts, 0);
    }

    #[test]
    fn a_plan_reviews_codes_times_and_outcomes_are_tallied() {
        let events = vec![
            event(
                1,
                5,
                None,
                "plan_review_started",
                0,
                json!({"proposal_id": 3, "plan_review_id": 1}),
            ),
            event(
                2,
                5,
                None,
                "plan_review_finished",
                60,
                json!({"proposal_id": 3, "plan_review_id": 1, "verdict": "revise", "decision": "revise", "reasons": ["dep"], "reason_codes": [["missing_dependency"]], "primary_code": "missing_dependency"}),
            ),
            event(
                3,
                5,
                None,
                "plan_review_started",
                660,
                json!({"proposal_id": 3, "plan_review_id": 2}),
            ),
            event(
                4,
                5,
                None,
                "plan_review_finished",
                700,
                json!({"proposal_id": 3, "plan_review_id": 2, "verdict": "revise", "decision": "concern", "reasons": ["adr"], "reason_codes": [["adr_conflict"]], "primary_code": "adr_conflict", "ask_id": 9}),
            ),
            event(
                5,
                5,
                None,
                "plan_review_outcome",
                1000,
                json!({"proposal_id": 3, "plan_review_id": 2, "ask_id": 9, "outcome": "deviation_rejected"}),
            ),
            event(
                6,
                5,
                None,
                "plan_review_started",
                1300,
                json!({"proposal_id": 3, "plan_review_id": 3}),
            ),
            event(
                7,
                5,
                None,
                "plan_review_finished",
                1400,
                json!({"proposal_id": 3, "plan_review_id": 3, "verdict": "pass", "decision": "pass", "reasons": []}),
            ),
            event(
                8,
                6,
                None,
                "plan_review_finished",
                1500,
                json!({"proposal_id": 4, "plan_review_id": 4, "verdict": "pass", "reasons": []}),
            ),
        ];
        let plan = review_reasons(&events, EventId::new(0), EventId::new(8), |_| true).plan_review;
        assert_eq!((plan.verdicts, plan.reviewed, plan.sent_back), (4, 2, 1));
        assert_eq!(plan.rate, Some(0.5));
        let dependency = &plan.by_code["missing_dependency"];
        assert_eq!(dependency.revise_secs.total, 600);
        let adr = &plan.by_code["adr_conflict"];
        assert_eq!((adr.revise, adr.concern), (0, 1));
        assert_eq!(adr.concern_wait_secs.total, 300);
        assert_eq!(adr.send_back_resume_secs.total, 300);
        assert_eq!(adr.outcomes[DEVIATION_REJECTED], 1);
        // Only the tasks counted.
        let other = review_reasons(&events, EventId::new(0), EventId::new(8), |task| {
            task == Some(TaskId::new(6))
        });
        assert_eq!(
            (other.plan_review.reviewed, other.plan_review.sent_back),
            (1, 0)
        );
    }
}
