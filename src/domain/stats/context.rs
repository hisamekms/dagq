//! How large the context of the Executions grew in `stats` (ADR-t1486-1,
//! request 39): `runs[].context` and `sessions.by_kind.<kind>.context`,
//! from the `peak_context`, `context_window`, `compactions` and
//! `context_reason` a worker's turn (`turn_finished`) and a headless job's
//! end (`review_*`, `recovery_finished`, `plan_review_*`, `goal_review_*`,
//! `observe_finished`, `throughput_review_finished`) record beside their
//! tokens ([`crate::domain::tokens::ExecutionContext`]).
//!
//! The counting takes only the values of the records ([`ContextRecord`]),
//! so that the queue's events ([`records`]) and any other reading of the
//! same records give the same counts. An Execution with no context
//! recorded (one recorded before it was, or by a reader that does not
//! record it), a value not measured (`null` with its reason) and a window
//! not known are never counted as 0: each is left out of the values it
//! lacks and counted beside them. An interactive session's cuts record no
//! context and are not read.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{
    executions::Execution,
    landing::p90,
    median,
    sessions::{INTERACTIVE, Ratio},
};
use crate::domain::RunId;

/// What one Execution's record says of its context, as recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordedContext {
    /// `peak_context`; `None`: not measured.
    pub peak: Option<i64>,
    /// `context_window`; `None`: not known.
    pub window: Option<i64>,
    /// `compactions`; `None`: not measured.
    pub compactions: Option<i64>,
    /// `context_reason`: why `peak` or `compactions` is not measured.
    pub reason: Option<String>,
}

impl RecordedContext {
    /// The context of the payload of the event that ended an Execution;
    /// `None` when it records none (no `peak_context` key).
    pub fn of(payload: &Value) -> Option<Self> {
        payload.get("peak_context")?;
        Some(Self {
            peak: payload["peak_context"].as_i64(),
            window: payload["context_window"].as_i64(),
            compactions: payload["compactions"].as_i64(),
            reason: payload["context_reason"].as_str().map(str::to_owned),
        })
    }

    /// `peak` over `window`, in thousandths; `None` without both or with
    /// a window not positive.
    fn ratio(&self) -> Option<Ratio> {
        Ratio::of(self.peak?, self.window?)
    }
}

/// One Execution as the counting reads it: who ran it on what, when it
/// ended and its context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextRecord {
    /// The kind of session the supervisor started it as.
    pub actor: String,
    /// `claude` / `codex`, or `unknown`.
    pub provider: String,
    /// The run it was of; `None` for a job of no run.
    pub run_id: Option<RunId>,
    /// When it ended, unix milliseconds.
    pub at: i64,
    /// `None`: it records no context.
    pub context: Option<RecordedContext>,
}

/// The records of `executions` (from [`super::executions::executions`]),
/// an interactive session's cuts left out.
pub fn records<'a>(executions: impl IntoIterator<Item = &'a Execution>) -> Vec<ContextRecord> {
    executions
        .into_iter()
        .filter(|execution| execution.route != Some(INTERACTIVE))
        .map(|execution| ContextRecord {
            actor: execution.actor.to_owned(),
            provider: execution.provider.clone(),
            run_id: execution.run_id.clone(),
            at: execution.at,
            context: execution.context.clone(),
        })
        .collect()
}

/// Count, median, nearest-rank 90th percentile and maximum of the
/// Executions that have a value, and how many of those that record their
/// context do not, per reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextSpread<T> {
    pub count: usize,
    pub median: Option<T>,
    pub p90: Option<T>,
    pub max: Option<T>,
    /// The Executions without a value: not measured, or (`window_ratio`)
    /// without a peak or a window.
    pub missing: usize,
    /// The same per `context_reason`; `window_unknown` for a ratio whose
    /// peak was measured but whose window is not known, `unknown` for a
    /// value not measured without a reason.
    pub missing_by_reason: BTreeMap<String, usize>,
}

impl<T> Default for ContextSpread<T> {
    fn default() -> Self {
        Self {
            count: 0,
            median: None,
            p90: None,
            max: None,
            missing: 0,
            missing_by_reason: BTreeMap::new(),
        }
    }
}

/// The reason of a ratio whose window is not known.
pub const WINDOW_UNKNOWN: &str = "window_unknown";
/// The reason of a value not measured whose record names none.
pub const REASON_UNKNOWN: &str = "unknown";

/// The values of one group, before they are summarized.
#[derive(Default)]
struct Values {
    executions: usize,
    not_recorded: usize,
    peak: Vec<i64>,
    peak_missing: BTreeMap<String, usize>,
    ratio: Vec<i64>,
    ratio_missing: BTreeMap<String, usize>,
    compactions: Vec<i64>,
    compactions_missing: BTreeMap<String, usize>,
}

impl Values {
    fn add(&mut self, record: &ContextRecord) {
        self.executions += 1;
        let Some(context) = &record.context else {
            self.not_recorded += 1;
            return;
        };
        let reason = || {
            context
                .reason
                .clone()
                .unwrap_or_else(|| REASON_UNKNOWN.to_owned())
        };
        match context.peak {
            Some(peak) => self.peak.push(peak),
            None => *self.peak_missing.entry(reason()).or_default() += 1,
        }
        match (context.peak, context.ratio()) {
            (_, Some(ratio)) => self.ratio.push(ratio.thousandths()),
            (Some(_), None) => {
                *self
                    .ratio_missing
                    .entry(WINDOW_UNKNOWN.to_owned())
                    .or_default() += 1
            }
            (None, None) => *self.ratio_missing.entry(reason()).or_default() += 1,
        }
        match context.compactions {
            Some(compactions) => self.compactions.push(compactions),
            None => *self.compactions_missing.entry(reason()).or_default() += 1,
        }
    }

    fn spread(mut values: Vec<i64>, missing: BTreeMap<String, usize>) -> ContextSpread<i64> {
        ContextSpread {
            count: values.len(),
            median: median(&mut values),
            p90: p90(&mut values),
            max: values.iter().copied().max(),
            missing: missing.values().sum(),
            missing_by_reason: missing,
        }
    }

    fn finish(self) -> ContextStats {
        let ratio = Self::spread(self.ratio, self.ratio_missing);
        ContextStats {
            executions: self.executions,
            not_recorded: self.not_recorded,
            peak_context: Self::spread(self.peak, self.peak_missing),
            window_ratio: ContextSpread {
                count: ratio.count,
                median: ratio.median.map(Ratio::thousandths_of),
                p90: ratio.p90.map(Ratio::thousandths_of),
                max: ratio.max.map(Ratio::thousandths_of),
                missing: ratio.missing,
                missing_by_reason: ratio.missing_by_reason,
            },
            compactions: Self::spread(self.compactions, self.compactions_missing),
        }
    }
}

/// The context of one kind's Executions on one provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ContextStats {
    /// Its Executions, with their context recorded or not.
    pub executions: usize,
    /// Of them, those that record no context: in none of the spreads.
    pub not_recorded: usize,
    /// The largest input of one call of the model (`peak_context`), in
    /// tokens.
    pub peak_context: ContextSpread<i64>,
    /// `peak_context` over the model's window (`context_window`); an
    /// Execution whose window is not known is not in the count.
    pub window_ratio: ContextSpread<Ratio>,
    /// How many times its context was compacted (`compactions`).
    pub compactions: ContextSpread<i64>,
}

/// The context of the Executions of `records` that ended in
/// `(from, until]` (unix milliseconds; no `from`: from the first), per
/// kind and provider.
pub fn by_kind(
    records: &[ContextRecord],
    from: Option<i64>,
    until: i64,
) -> BTreeMap<String, BTreeMap<String, ContextStats>> {
    let mut values: BTreeMap<String, BTreeMap<String, Values>> = BTreeMap::new();
    for record in records
        .iter()
        .filter(|record| from.is_none_or(|from| record.at > from) && record.at <= until)
    {
        values
            .entry(record.actor.clone())
            .or_default()
            .entry(record.provider.clone())
            .or_default()
            .add(record);
    }
    values
        .into_iter()
        .map(|(kind, providers)| {
            let providers = providers
                .into_iter()
                .map(|(provider, values)| (provider, values.finish()))
                .collect();
            (kind, providers)
        })
        .collect()
}

/// The context of one run's Executions (its own sessions' turns and its
/// jobs), all of them whenever they ended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RunContext {
    /// Its Executions, with their context recorded or not.
    pub executions: usize,
    /// Of them, those that record no context: in none of the values.
    pub not_recorded: usize,
    /// The largest `peak_context` of its Executions; null when none was
    /// measured.
    pub peak_context: Option<i64>,
    /// Of those that record their context, those whose peak was not
    /// measured.
    pub peak_context_missing: usize,
    /// The largest `peak_context` over `context_window` of its Executions;
    /// null when none has both.
    pub window_ratio: Option<Ratio>,
    /// Of those that record their context, those without the ratio (no
    /// peak, or the window not known).
    pub window_ratio_missing: usize,
    /// The sum of the `compactions` its Executions measured; null when
    /// none was.
    pub compactions: Option<i64>,
    /// Of those that record their context, those whose compactions were
    /// not measured.
    pub compactions_missing: usize,
}

/// The context per run of the Executions of `records` that are of a run.
pub fn per_run(records: &[ContextRecord]) -> BTreeMap<RunId, RunContext> {
    let mut runs: BTreeMap<RunId, RunContext> = BTreeMap::new();
    for record in records {
        let Some(run_id) = &record.run_id else {
            continue;
        };
        let run = runs.entry(run_id.clone()).or_default();
        run.executions += 1;
        let Some(context) = &record.context else {
            run.not_recorded += 1;
            continue;
        };
        match context.peak {
            Some(peak) => run.peak_context = run.peak_context.max(Some(peak)),
            None => run.peak_context_missing += 1,
        }
        match context.ratio() {
            Some(ratio) => run.window_ratio = run.window_ratio.max(Some(ratio)),
            None => run.window_ratio_missing += 1,
        }
        match context.compactions {
            Some(compactions) => {
                run.compactions = Some(run.compactions.unwrap_or(0) + compactions);
            }
            None => run.compactions_missing += 1,
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, RunEvent, TaskId, tokens::ROLLOUT_MISSING};
    use serde_json::json;

    fn record(
        actor: &str,
        provider: &str,
        run: Option<&str>,
        at: i64,
        context: Option<RecordedContext>,
    ) -> ContextRecord {
        ContextRecord {
            actor: actor.to_owned(),
            provider: provider.to_owned(),
            run_id: run.map(|run| RunId::new(run).unwrap()),
            at,
            context,
        }
    }

    fn measured(peak: i64, window: Option<i64>, compactions: i64) -> Option<RecordedContext> {
        Some(RecordedContext {
            peak: Some(peak),
            window,
            compactions: Some(compactions),
            reason: None,
        })
    }

    fn unmeasured(reason: &str) -> Option<RecordedContext> {
        Some(RecordedContext {
            reason: Some(reason.to_owned()),
            ..RecordedContext::default()
        })
    }

    /// Each kind and provider has the median, p90 and max of its values;
    /// a Codex Execution whose rollout was missing (`null` with its
    /// reason) is counted as missing, not as 0, beside those that measured
    /// no compaction; one with no record and one whose window is not known
    /// are counted apart too. Only what ended in the window counts.
    #[test]
    fn a_kind_on_a_provider_has_its_spreads_and_counts_what_is_missing_apart() {
        let records = vec![
            record(
                "review",
                "codex",
                Some("r1"),
                10,
                measured(100, Some(1000), 0),
            ),
            record(
                "review",
                "codex",
                Some("r1"),
                20,
                measured(300, Some(1000), 0),
            ),
            record(
                "review",
                "codex",
                Some("r2"),
                30,
                measured(200, Some(1000), 2),
            ),
            record("review", "codex", None, 40, unmeasured(ROLLOUT_MISSING)),
            record("review", "codex", None, 50, None),
            record("review", "claude", None, 60, measured(500, None, 1)),
            record("worker", "claude", Some("r1"), 70, unmeasured("no_stream")),
            // Ended before the window.
            record("review", "codex", None, 5, measured(9_000, Some(10_000), 9)),
        ];
        let kinds = by_kind(&records, Some(5), 70);
        let codex = &kinds["review"]["codex"];
        assert_eq!((codex.executions, codex.not_recorded), (5, 1));
        let peak = &codex.peak_context;
        assert_eq!(
            (peak.count, peak.median, peak.p90, peak.max, peak.missing),
            (3, Some(200), Some(300), Some(300), 1)
        );
        assert_eq!(peak.missing_by_reason[ROLLOUT_MISSING], 1);
        let compactions = &codex.compactions;
        assert_eq!(
            (compactions.count, compactions.median, compactions.max),
            (3, Some(0), Some(2))
        );
        assert_eq!(compactions.missing_by_reason[ROLLOUT_MISSING], 1);
        let ratio = &codex.window_ratio;
        assert_eq!(
            (ratio.count, ratio.median, ratio.max, ratio.missing),
            (3, Ratio::of(200, 1000), Ratio::of(300, 1000), 1)
        );
        // A peak without a window is out of the ratio's count.
        let claude = &kinds["review"]["claude"];
        assert_eq!(claude.peak_context.max, Some(500));
        assert_eq!(claude.window_ratio.count, 0);
        assert_eq!(claude.window_ratio.missing_by_reason[WINDOW_UNKNOWN], 1);
        let worker = &kinds["worker"]["claude"];
        assert_eq!(worker.peak_context.missing_by_reason["no_stream"], 1);
        assert_eq!(worker.compactions.count, 0);
        let json = serde_json::to_value(codex).unwrap();
        assert_eq!(json["window_ratio"]["max"], json!(0.3));
        assert_eq!(json["compactions"]["missing"], 1);
        assert_eq!(json["not_recorded"], 1);
        assert!(by_kind(&records, Some(70), 80).is_empty());
    }

    /// A run has the largest peak and ratio of its Executions and their
    /// compactions together; what was not recorded or not measured is
    /// counted, not taken as 0, and a job of no run is in no run.
    #[test]
    fn a_run_has_its_largest_peak_and_ratio_and_its_compactions() {
        let records = vec![
            record(
                "worker",
                "claude",
                Some("r1"),
                10,
                measured(100, Some(1000), 1),
            ),
            record("revise", "claude", Some("r1"), 20, measured(400, None, 2)),
            record(
                "review",
                "codex",
                Some("r1"),
                30,
                measured(300, Some(500), 0),
            ),
            record(
                "review",
                "codex",
                Some("r1"),
                40,
                unmeasured(ROLLOUT_MISSING),
            ),
            record("worker", "claude", Some("r2"), 50, None),
            record("plan_review", "codex", None, 60, measured(1, Some(2), 3)),
        ];
        let runs = per_run(&records);
        assert_eq!(runs.len(), 2);
        let r1 = &runs[&RunId::new("r1").unwrap()];
        assert_eq!(
            r1,
            &RunContext {
                executions: 4,
                not_recorded: 0,
                peak_context: Some(400),
                peak_context_missing: 1,
                window_ratio: Ratio::of(300, 500),
                window_ratio_missing: 2,
                compactions: Some(3),
                compactions_missing: 1,
            }
        );
        let r2 = &runs[&RunId::new("r2").unwrap()];
        assert_eq!((r2.executions, r2.not_recorded), (1, 1));
        assert_eq!((r2.peak_context, r2.compactions), (None, None));
        assert_eq!(
            serde_json::to_value(r1).unwrap()["window_ratio"],
            json!(0.6)
        );
    }

    /// The events give the records: the context an Execution's end
    /// recorded, `null`s as not measured, none when it predates the
    /// context; an interactive session's cuts are not read.
    #[test]
    fn the_events_give_the_records_of_the_executions() {
        let event = |id: i64, kind: &str, payload: Value| RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new("r1").unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: "2026-10-01T09:00:00Z".to_owned(),
            actor: None,
        };
        let tokens = json!({"input": 1, "output": 1, "cache_read": 0, "cache_creation": 0});
        let events = vec![
            event(
                1,
                "turn_finished",
                json!({"provider": "claude", "tokens": tokens, "tokens_source": "model_usage",
                       "tokens_by_model": [], "peak_context": 120, "context_window": 200,
                       "compactions": 1, "context_reason": null}),
            ),
            event(
                2,
                "review_finished",
                json!({"tokens": null, "tokens_source": null, "tokens_reason": "rollout_missing",
                       "provider": "codex", "tokens_by_model": [], "peak_context": null,
                       "context_window": null, "compactions": null,
                       "context_reason": "rollout_missing"}),
            ),
            // Recorded before the context was.
            event(
                3,
                "review_finished",
                json!({"tokens": tokens, "tokens_source": "token_usage_record",
                       "tokens_by_model": []}),
            ),
            event(
                4,
                "session_tokens",
                json!({"kind": "inbox", "at": "2026-10-01T09:00:00Z", "tokens": tokens,
                       "tokens_source": "transcript", "tokens_by_model": []}),
            ),
        ];
        let records = records(&super::super::executions::executions(&events));
        let contexts: Vec<(&str, &str, Option<RecordedContext>)> = records
            .iter()
            .map(|r| (r.actor.as_str(), r.provider.as_str(), r.context.clone()))
            .collect();
        assert_eq!(
            contexts,
            [
                ("worker", "claude", measured(120, Some(200), 1)),
                ("review", "codex", unmeasured(ROLLOUT_MISSING)),
                ("review", "codex", None),
            ]
        );
    }
}
