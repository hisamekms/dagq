//! The tokens a Claude session used (task 199): the `message.usage` of the
//! assistant records of its transcript that fall in a span, summed once per
//! message. The cost is Claude Code's own `costUSD` when every message
//! counted has one; it is never computed from prices. Reading the file is
//! the infrastructure's (`infrastructure::transcripts`); this is pure.
//! The model and effort a span's messages were written with (task 579)
//! come from the same records ([`span_models`]).

use std::collections::{BTreeMap, HashSet};

use serde_json::{Value, json};

use super::transcript::TranscriptRecord;

/// The span's assistant records carry a `usage` this version of the reader
/// does not understand: no count is recorded.
pub const USAGE_UNSUPPORTED: &str = "usage_unsupported";

/// The tokens of a span, per kind of token, and the messages they came
/// from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenUsage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    /// Assistant messages counted (records of one message are one).
    pub messages: i64,
    /// The sum of Claude Code's `costUSD`, when every message had one.
    pub cost_usd: Option<f64>,
}

impl TokenUsage {
    /// The counts of a [`TokenUsage::payload`]; `None` when it is not an
    /// object with numeric `input` and `output`.
    pub fn from_payload(payload: &Value) -> Option<Self> {
        let count = |key: &str| payload.get(key).and_then(Value::as_i64);
        Some(Self {
            input: count("input")?,
            output: count("output")?,
            cache_read: count("cache_read").unwrap_or(0),
            cache_creation: count("cache_creation").unwrap_or(0),
            messages: count("messages").unwrap_or(0),
            cost_usd: payload.get("cost_usd").and_then(Value::as_f64),
        })
    }

    /// What was used after `earlier`, when both are running totals of one
    /// session (Codex's `turn.completed`): each count less the earlier one,
    /// never below 0, as one turn without a cost.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            input: (self.input - earlier.input).max(0),
            output: (self.output - earlier.output).max(0),
            cache_read: (self.cache_read - earlier.cache_read).max(0),
            cache_creation: (self.cache_creation - earlier.cache_creation).max(0),
            messages: 1,
            cost_usd: None,
        }
    }

    /// The `tokens` of a `session_closed`: the counts, and `cost_usd` only
    /// when there is one.
    pub fn payload(&self) -> Value {
        let mut payload = json!({
            "input": self.input,
            "output": self.output,
            "cache_read": self.cache_read,
            "cache_creation": self.cache_creation,
            "messages": self.messages,
        });
        if let Some(cost) = self.cost_usd {
            payload["cost_usd"] = json!((cost * 1e6).round() / 1e6);
        }
        payload
    }
}

/// Where the tokens of an Execution (one `claude -p` / `codex exec` call:
/// a headless turn or a headless job, ADR-t1486-1) were counted from: the
/// `tokens_source` of its record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// Claude's result's `modelUsage`: every model of the session, its
    /// subagents included, summed.
    ModelUsage,
    /// Claude's result's `usage`, which leaves the subagents out: the
    /// fallback for an output without a `modelUsage` (an older Claude
    /// Code).
    ResultUsage,
    /// Codex's `turn.completed`, the thread's running total.
    ThreadUsage,
}

impl TokenSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelUsage => "model_usage",
            Self::ResultUsage => "result_usage",
            Self::ThreadUsage => "thread_usage",
        }
    }
}

/// The `tokens_reason` of an Execution whose output ended without the
/// provider's result: nothing was counted.
pub const NO_RESULT: &str = "no_result";
/// The `tokens_reason` of an Execution whose result had no usage that
/// could be read: nothing was counted.
pub const NO_USAGE: &str = "no_usage";
/// The `tokens_reason` of a Claude Execution whose result had no
/// `modelUsage`: its tokens are its `usage`'s, without its subagents.
pub const MODEL_USAGE_MISSING: &str = "model_usage_missing";

/// One model's tokens in an Execution (an entry of Claude's `modelUsage`):
/// the same kinds as [`TokenUsage`], and the cost when the provider gave
/// one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelTokens {
    pub model: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    pub cost_usd: Option<f64>,
}

impl ModelTokens {
    fn counts(&self) -> [i64; 4] {
        [
            self.input,
            self.output,
            self.cache_read,
            self.cache_creation,
        ]
    }

    /// An entry of `tokens_by_model`: the counts, and `cost_usd` only when
    /// there is one.
    pub fn payload(&self) -> Value {
        let mut payload = json!({
            "model": self.model,
            "input": self.input,
            "output": self.output,
            "cache_read": self.cache_read,
            "cache_creation": self.cache_creation,
        });
        if let Some(cost) = self.cost_usd {
            payload["cost_usd"] = json!((cost * 1e6).round() / 1e6);
        }
        payload
    }

    /// The entries of a `tokens_by_model` array; `None` when it is not an
    /// array of entries with a model and numeric `input` and `output`.
    pub fn from_payloads(payload: &Value) -> Option<Vec<Self>> {
        payload
            .as_array()?
            .iter()
            .map(|entry| {
                let count = |key: &str| entry.get(key).and_then(Value::as_i64);
                Some(Self {
                    model: entry.get("model")?.as_str()?.to_owned(),
                    input: count("input")?,
                    output: count("output")?,
                    cache_read: count("cache_read").unwrap_or(0),
                    cache_creation: count("cache_creation").unwrap_or(0),
                    cost_usd: entry.get("cost_usd").and_then(Value::as_f64),
                })
            })
            .collect()
    }

    /// The tokens of the models together, as one Execution's (`messages`
    /// 1) with `cost_usd`.
    pub fn total(models: &[Self], cost_usd: Option<f64>) -> TokenUsage {
        let mut total = TokenUsage {
            messages: 1,
            cost_usd,
            ..TokenUsage::default()
        };
        for model in models {
            total.input += model.input;
            total.output += model.output;
            total.cache_read += model.cache_read;
            total.cache_creation += model.cache_creation;
        }
        total
    }

    /// What was used after `earlier`, when both are a session's running
    /// totals per model (Claude's `modelUsage`): each model's counts and
    /// cost less its earlier ones, the models that added nothing left out.
    /// `None` when a count fell (a model of `earlier` missing counts as
    /// 0): the totals are not of one session.
    pub fn since(totals: &[Self], earlier: &[Self]) -> Option<Vec<Self>> {
        let before = |model: &str| earlier.iter().find(|e| e.model == model);
        if earlier
            .iter()
            .any(|e| !totals.iter().any(|total| total.model == e.model))
        {
            return None;
        }
        let mut own = Vec::new();
        for total in totals {
            let Some(before) = before(&total.model) else {
                own.push(total.clone());
                continue;
            };
            let counts: Vec<i64> = total
                .counts()
                .iter()
                .zip(before.counts())
                .map(|(now, then)| now - then)
                .collect();
            if counts.iter().any(|count| *count < 0) {
                return None;
            }
            if counts.iter().all(|count| *count == 0) {
                continue;
            }
            own.push(Self {
                model: total.model.clone(),
                input: counts[0],
                output: counts[1],
                cache_read: counts[2],
                cache_creation: counts[3],
                cost_usd: match (total.cost_usd, before.cost_usd) {
                    (Some(now), Some(then)) => Some((((now - then) * 1e6).round() / 1e6).max(0.0)),
                    (now, None) => now,
                    (None, Some(_)) => None,
                },
            });
        }
        Some(own)
    }
}

/// The tokens of one Execution as its record carries them (ADR-t1486-1):
/// the form every provider's turn and job is recorded in. A count of 0 is
/// a measured 0; `tokens` `None` with a `reason` is not measured.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecutionTokens {
    /// Its tokens, every model and every subagent (child thread) of it
    /// together; `None` when they could not be counted.
    pub tokens: Option<TokenUsage>,
    /// The same per model, when the provider gives them per model (empty
    /// otherwise).
    pub by_model: Vec<ModelTokens>,
    /// What they were counted from; `None` when they were not.
    pub source: Option<TokenSource>,
    /// Why nothing was counted ([`NO_RESULT`], [`NO_USAGE`]), or why they
    /// were counted from a fallback that leaves some out
    /// ([`MODEL_USAGE_MISSING`]).
    pub reason: Option<&'static str>,
    /// The subagents (Claude's `subagent_stats.spawned`) or child threads
    /// the Execution started, when the provider says.
    pub children: Option<i64>,
}

impl ExecutionTokens {
    /// Nothing counted, for `reason`.
    pub fn unmeasured(reason: &'static str) -> Self {
        Self {
            reason: Some(reason),
            ..Self::default()
        }
    }

    /// Put them into the payload of the event that records the Execution
    /// (a `turn_finished`, the end of a headless job): `tokens` (`null`
    /// when not measured), `tokens_by_model`, `tokens_source`,
    /// `tokens_reason` and `children`.
    pub fn record(&self, payload: &mut Value) {
        payload["tokens"] = self
            .tokens
            .as_ref()
            .map_or(Value::Null, TokenUsage::payload);
        payload["tokens_by_model"] = self.by_model.iter().map(ModelTokens::payload).collect();
        payload["tokens_source"] = json!(self.source.map(TokenSource::as_str));
        payload["tokens_reason"] = json!(self.reason);
        payload["children"] = json!(self.children);
    }
}

/// One message's counts: the largest of its records' (the last block of a
/// message carries its final output count).
#[derive(Default)]
struct Message {
    counts: [i64; 4],
    cost: Option<f64>,
}

/// The tokens of the assistant records of `records` from `start` to `end`
/// (unix milliseconds, `[start, end)`), a subagent's included. `Err` with
/// [`USAGE_UNSUPPORTED`] when a record's usage has no numeric input and
/// output counts, or no assistant record has a usage at all.
pub fn span_usage(
    records: &[TranscriptRecord],
    start: i64,
    end: i64,
) -> Result<TokenUsage, &'static str> {
    #[cfg(test)]
    super::transcript::count_analysis();
    let mut messages: BTreeMap<String, Message> = BTreeMap::new();
    let mut assistants = 0;
    for (at, record) in records.iter().enumerate() {
        if !record.assistant || record.at < start || record.at >= end {
            continue;
        }
        assistants += 1;
        let Some(usage) = &record.usage else {
            continue;
        };
        let count = |key: &str| usage.get(key).map(Value::as_i64);
        let (Some(Some(input)), Some(Some(output))) =
            (count("input_tokens"), count("output_tokens"))
        else {
            return Err(USAGE_UNSUPPORTED);
        };
        let cache = |key: &str| count(key).flatten().unwrap_or(0);
        let counts = [
            input,
            output,
            cache("cache_read_input_tokens"),
            cache("cache_creation_input_tokens"),
        ];
        // A record without a message id is a message of its own.
        let key = record
            .message_id
            .clone()
            .unwrap_or_else(|| format!("#{at}"));
        let message = messages.entry(key).or_default();
        for (kept, count) in message.counts.iter_mut().zip(counts) {
            *kept = (*kept).max(count);
        }
        if record.cost_usd.is_some() {
            message.cost = record.cost_usd;
        }
    }
    if assistants > 0 && messages.is_empty() {
        return Err(USAGE_UNSUPPORTED);
    }
    let mut usage = TokenUsage {
        messages: messages.len() as i64,
        cost_usd: messages
            .values()
            .map(|message| message.cost)
            .sum::<Option<f64>>(),
        ..TokenUsage::default()
    };
    if messages.is_empty() {
        usage.cost_usd = None;
    }
    for message in messages.values() {
        usage.input += message.counts[0];
        usage.output += message.counts[1];
        usage.cache_read += message.counts[2];
        usage.cache_creation += message.counts[3];
    }
    Ok(usage)
}

/// The model Claude Code names for a message it made up itself (an API
/// error, an interruption): no model wrote it.
const SYNTHETIC_MODEL: &str = "<synthetic>";

/// The model and effort of the messages of a span, and how many messages
/// each pair wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelUse {
    pub model: String,
    /// Absent when the records do not say (older Claude Code).
    pub effort: Option<String>,
    pub messages: i64,
}

/// The models and efforts of the session's own (not a subagent's)
/// assistant messages from `start` to `end` (unix milliseconds, `[start,
/// end)`), the pair that wrote the most messages first (the later one on a
/// tie). A message is counted once, with its first record that names a
/// model. Empty when no message names one.
pub fn span_models(records: &[TranscriptRecord], start: i64, end: i64) -> Vec<ModelUse> {
    #[cfg(test)]
    super::transcript::count_analysis();
    let mut seen: HashSet<String> = HashSet::new();
    // (model, effort) → (messages, the last message's position).
    let mut pairs: BTreeMap<(String, Option<String>), (i64, usize)> = BTreeMap::new();
    for (at, record) in records.iter().enumerate() {
        if !record.assistant || record.sidechain || record.at < start || record.at >= end {
            continue;
        }
        let Some(model) = record.model.as_deref().filter(|m| *m != SYNTHETIC_MODEL) else {
            continue;
        };
        let key = record
            .message_id
            .clone()
            .unwrap_or_else(|| format!("#{at}"));
        if !seen.insert(key) {
            continue;
        }
        let pair = pairs
            .entry((model.to_owned(), record.effort.clone()))
            .or_default();
        pair.0 += 1;
        pair.1 = at;
    }
    let mut uses: Vec<(ModelUse, usize)> = pairs
        .into_iter()
        .map(|((model, effort), (messages, last))| {
            (
                ModelUse {
                    model,
                    effort,
                    messages,
                },
                last,
            )
        })
        .collect();
    uses.sort_by(|(a, a_last), (b, b_last)| b.messages.cmp(&a.messages).then(b_last.cmp(a_last)));
    uses.into_iter().map(|(model, _)| model).collect()
}

/// The [`span_usage`] and [`span_models`] of a span's records from `start`
/// to every cut up to `end` (task 1334), made before a write lock so that a
/// close only picks those to its cut ([`SpanTokens::at`]). They change only
/// where an assistant record is, so they are made once per such time, the
/// records taken in the order of their times, each message's counts, cost
/// and model kept as [`span_usage`] and [`span_models`] keep them.
#[derive(Debug, Clone, Default)]
pub struct SpanTokens {
    /// Each time of an assistant record, with the usage and models of the
    /// records to it (a cut just past it), in order.
    steps: Vec<(i64, Result<TokenUsage, &'static str>, Vec<ModelUse>)>,
}

/// A message's usage as [`span_usage`] keeps it.
#[derive(Default)]
struct Kept {
    counts: [i64; 4],
    /// The position and cost of its last record (in the transcript's
    /// order) with a cost.
    cost: Option<(usize, f64)>,
}

impl SpanTokens {
    pub fn new(records: &[TranscriptRecord], start: i64, end: i64) -> Self {
        #[cfg(test)]
        super::transcript::count_analysis();
        let mut order: Vec<usize> = (0..records.len())
            .filter(|&at| {
                let record = &records[at];
                record.assistant && record.at >= start && record.at < end
            })
            .collect();
        order.sort_by_key(|&at| (records[at].at, at));
        let mut steps = Vec::new();
        let mut unsupported = false;
        let mut messages: BTreeMap<String, Kept> = BTreeMap::new();
        let mut totals = [0i64; 4];
        let mut without_cost = 0usize;
        // The model's message → (its first position naming a model, its
        // pair); each pair → the positions of the messages it counts.
        let mut named: BTreeMap<String, (usize, (String, Option<String>))> = BTreeMap::new();
        let mut pairs: BTreeMap<(String, Option<String>), std::collections::BTreeSet<usize>> =
            BTreeMap::new();
        for (index, &at) in order.iter().enumerate() {
            let record = &records[at];
            let key = || {
                record
                    .message_id
                    .clone()
                    .unwrap_or_else(|| format!("#{at}"))
            };
            if let Some(usage) = &record.usage {
                let count = |key: &str| usage.get(key).map(Value::as_i64);
                match (count("input_tokens"), count("output_tokens")) {
                    (Some(Some(input)), Some(Some(output))) => {
                        let cache = |key: &str| count(key).flatten().unwrap_or(0);
                        let counts = [
                            input,
                            output,
                            cache("cache_read_input_tokens"),
                            cache("cache_creation_input_tokens"),
                        ];
                        let message = messages.entry(key()).or_insert_with(|| {
                            without_cost += 1;
                            Kept::default()
                        });
                        for ((kept, total), count) in
                            message.counts.iter_mut().zip(&mut totals).zip(counts)
                        {
                            if count > *kept {
                                *total += count - *kept;
                                *kept = count;
                            }
                        }
                        if let Some(cost) = record.cost_usd
                            && message.cost.is_none_or(|(last, _)| last < at)
                        {
                            if message.cost.is_none() {
                                without_cost -= 1;
                            }
                            message.cost = Some((at, cost));
                        }
                    }
                    _ => unsupported = true,
                }
            }
            if !record.sidechain
                && let Some(model) = record.model.as_deref().filter(|m| *m != SYNTHETIC_MODEL)
            {
                let pair = (model.to_owned(), record.effort.clone());
                let key = key();
                let earlier = match named.get(&key) {
                    Some((first, _)) if *first < at => None,
                    Some((first, old)) => Some(Some((*first, old.clone()))),
                    None => Some(None),
                };
                if let Some(replaced) = earlier {
                    if let Some((first, old)) = replaced
                        && let Some(positions) = pairs.get_mut(&old)
                    {
                        positions.remove(&first);
                        if positions.is_empty() {
                            pairs.remove(&old);
                        }
                    }
                    pairs.entry(pair.clone()).or_default().insert(at);
                    named.insert(key, (at, pair));
                }
            }
            if order
                .get(index + 1)
                .is_some_and(|&next| records[next].at == record.at)
            {
                continue;
            }
            let usage = if unsupported || messages.is_empty() {
                Err(USAGE_UNSUPPORTED)
            } else {
                Ok(TokenUsage {
                    input: totals[0],
                    output: totals[1],
                    cache_read: totals[2],
                    cache_creation: totals[3],
                    messages: messages.len() as i64,
                    // Summed as `span_usage` sums them, in its order.
                    cost_usd: (without_cost == 0).then(|| {
                        messages
                            .values()
                            .map(|message| message.cost.map_or(0.0, |(_, cost)| cost))
                            .sum()
                    }),
                })
            };
            let mut uses: Vec<(ModelUse, usize)> = pairs
                .iter()
                .map(|((model, effort), positions)| {
                    (
                        ModelUse {
                            model: model.clone(),
                            effort: effort.clone(),
                            messages: positions.len() as i64,
                        },
                        positions.last().copied().unwrap_or_default(),
                    )
                })
                .collect();
            uses.sort_by(|(a, a_last), (b, b_last)| {
                b.messages.cmp(&a.messages).then(b_last.cmp(a_last))
            });
            steps.push((
                record.at,
                usage,
                uses.into_iter().map(|(model, _)| model).collect(),
            ));
        }
        Self { steps }
    }

    /// The [`span_usage`] and [`span_models`] of its records to `cut` (one
    /// past the end it was made to is that end's): picked, not counted.
    pub fn at(&self, cut: i64) -> (Result<TokenUsage, &'static str>, Vec<ModelUse>) {
        match self.steps.partition_point(|(at, ..)| *at < cut) {
            0 => (Ok(TokenUsage::default()), Vec::new()),
            to => {
                let (_, usage, models) = &self.steps[to - 1];
                (usage.clone(), models.clone())
            }
        }
    }
}

/// What a `session_closed` records of `uses`: `model` and `effort` of the
/// pair that wrote the most, and `models` (each pair with its `messages`)
/// when there was more than one. Nothing when `uses` is empty.
pub fn models_payload(uses: &[ModelUse], payload: &mut Value) {
    let Some(main) = uses.first() else {
        return;
    };
    payload["model"] = json!(main.model);
    payload["effort"] = json!(main.effort);
    if uses.len() > 1 {
        payload["models"] = uses
            .iter()
            .map(|pair| json!({"model": pair.model, "effort": pair.effort, "messages": pair.messages}))
            .collect();
    }
}

#[cfg(test)]
mod tests {

    /// A payload reads back to its counts, and a running total less an
    /// earlier one is one turn's, never below 0 and without a cost.
    #[test]
    fn a_running_total_less_an_earlier_one_is_a_turn() {
        let total = TokenUsage {
            input: 30,
            output: 12,
            cache_read: 8,
            cache_creation: 1,
            messages: 1,
            cost_usd: Some(0.5),
        };
        assert_eq!(
            TokenUsage::from_payload(&total.payload()),
            Some(total.clone())
        );
        assert_eq!(TokenUsage::from_payload(&json!({"input": 1})), None);
        assert_eq!(TokenUsage::from_payload(&Value::Null), None);
        let earlier = TokenUsage {
            input: 10,
            output: 20,
            cache_read: 3,
            ..TokenUsage::default()
        };
        assert_eq!(
            total.since(&earlier),
            TokenUsage {
                input: 20,
                output: 0,
                cache_read: 5,
                cache_creation: 1,
                messages: 1,
                cost_usd: None,
            }
        );
    }
    use super::*;
    use crate::domain::transcript::{Transcript, millis_text};

    fn model(name: &str, counts: [i64; 4], cost: Option<f64>) -> ModelTokens {
        ModelTokens {
            model: name.to_owned(),
            input: counts[0],
            output: counts[1],
            cache_read: counts[2],
            cache_creation: counts[3],
            cost_usd: cost,
        }
    }

    /// A session's running totals per model less earlier ones are what was
    /// used since, the models that added nothing left out; a count that
    /// fell, or an earlier model gone, is no later total of the session.
    #[test]
    fn running_totals_per_model_less_earlier_ones_are_what_was_used_since() {
        let earlier = [
            model("opus", [10, 20, 30, 4], Some(1.0)),
            model("haiku", [1, 2, 0, 0], Some(0.1)),
        ];
        let totals = [
            model("haiku", [1, 2, 0, 0], Some(0.1)),
            model("opus", [15, 25, 60, 4], Some(1.5)),
            model("sonnet", [3, 3, 0, 0], None),
        ];
        assert_eq!(
            ModelTokens::since(&totals, &earlier),
            Some(vec![
                model("opus", [5, 5, 30, 0], Some(0.5)),
                model("sonnet", [3, 3, 0, 0], None),
            ])
        );
        let fell = [model("opus", [9, 25, 60, 4], Some(1.5)), totals[0].clone()];
        assert_eq!(ModelTokens::since(&fell, &earlier), None);
        assert_eq!(ModelTokens::since(&totals[1..], &earlier), None);
        assert_eq!(ModelTokens::since(&totals, &[]), Some(totals.to_vec()));
        let total = ModelTokens::total(&totals, Some(1.6));
        assert_eq!(
            (
                total.input,
                total.output,
                total.cache_read,
                total.cache_creation
            ),
            (19, 30, 60, 4)
        );
        assert_eq!((total.messages, total.cost_usd), (1, Some(1.6)));
        let payloads: Value = totals.iter().map(ModelTokens::payload).collect();
        assert_eq!(ModelTokens::from_payloads(&payloads), Some(totals.to_vec()));
        assert_eq!(ModelTokens::from_payloads(&json!([{"model": "x"}])), None);
        assert_eq!(ModelTokens::from_payloads(&Value::Null), None);
    }

    /// An Execution that used nothing records a 0 that was measured; one
    /// whose tokens could not be counted records `null` and why.
    #[test]
    fn a_measured_zero_is_told_from_tokens_not_measured() {
        let mut zero = json!({});
        ExecutionTokens {
            tokens: Some(ModelTokens::total(&[], None)),
            source: Some(TokenSource::ModelUsage),
            children: Some(0),
            ..ExecutionTokens::default()
        }
        .record(&mut zero);
        assert_eq!(
            zero,
            json!({
                "tokens": {"input": 0, "output": 0, "cache_read": 0, "cache_creation": 0, "messages": 1},
                "tokens_by_model": [],
                "tokens_source": "model_usage",
                "tokens_reason": null,
                "children": 0,
            })
        );
        let mut unmeasured = json!({});
        ExecutionTokens::unmeasured(NO_RESULT).record(&mut unmeasured);
        assert_eq!(
            unmeasured,
            json!({
                "tokens": null,
                "tokens_by_model": [],
                "tokens_source": null,
                "tokens_reason": "no_result",
                "children": null,
            })
        );
    }

    const SESSION: &str = "22222222-2222-4222-8222-222222222222";

    fn line(secs: i64, extra: Value) -> String {
        let mut value = json!({
            "type": "assistant",
            "timestamp": millis_text(secs * 1000),
            "sessionId": SESSION,
            "version": "2.1.283",
        });
        for (key, field) in extra.as_object().unwrap() {
            value[key] = field.clone();
        }
        value.to_string()
    }

    fn usage(id: &str, input: i64, output: i64, read: i64, created: i64) -> Value {
        json!({"message": {"id": id, "content": [], "usage": {
            "input_tokens": input,
            "output_tokens": output,
            "cache_read_input_tokens": read,
            "cache_creation_input_tokens": created,
        }}})
    }

    fn records(lines: &[String]) -> Vec<TranscriptRecord> {
        Transcript::parse(&lines.join("\n"), SESSION)
            .unwrap()
            .records
    }

    /// The records of one message count once, with its largest counts; a
    /// subagent's count; records outside the span do not.
    #[test]
    fn a_span_sums_its_messages_once_each() {
        let user = json!({"type": "user", "message": {"content": "hi"}});
        let records = records(&[
            line(5, usage("m0", 1000, 1000, 0, 0)),
            line(10, user),
            line(11, usage("m1", 3, 10, 100, 20)),
            line(12, usage("m1", 3, 40, 100, 20)),
            line(13, {
                let mut sub = usage("m2", 5, 7, 50, 0);
                sub["isSidechain"] = json!(true);
                sub
            }),
            line(
                14,
                json!({"message": {"content": [], "usage": {"input_tokens": 1, "output_tokens": 2}}}),
            ),
            line(20, usage("m3", 1000, 1000, 0, 0)),
        ]);
        let usage = span_usage(&records, 10_000, 20_000).unwrap();
        assert_eq!(
            usage,
            TokenUsage {
                input: 9,
                output: 49,
                cache_read: 150,
                cache_creation: 20,
                messages: 3,
                cost_usd: None,
            }
        );
        let payload = usage.payload();
        assert_eq!(payload["output"], 49);
        assert!(payload.get("cost_usd").is_none());
        // Nothing in the span: zero, not an error.
        assert_eq!(
            span_usage(&records, 30_000, 40_000),
            Ok(TokenUsage::default())
        );
    }

    /// The cost is Claude Code's, and only when every message has one.
    #[test]
    fn the_cost_is_recorded_only_when_every_message_has_one() {
        let costed = |secs, id: &str, cost: f64| {
            let mut value = usage(id, 1, 1, 0, 0);
            value["costUSD"] = json!(cost);
            line(secs, value)
        };
        let all = records(&[costed(1, "a", 0.25), costed(2, "b", 0.125)]);
        let usage = span_usage(&all, 0, 10_000).unwrap();
        assert_eq!(usage.cost_usd, Some(0.375));
        assert_eq!(usage.payload()["cost_usd"], 0.375);
        let some = records(&[costed(1, "a", 0.25), line(2, usage_of("b"))]);
        assert_eq!(span_usage(&some, 0, 10_000).unwrap().cost_usd, None);
    }

    fn usage_of(id: &str) -> Value {
        usage(id, 1, 1, 0, 0)
    }

    fn written(secs: i64, id: &str, model: &str, effort: Option<&str>) -> String {
        let mut value = json!({"message": {"id": id, "model": model, "content": []}});
        if let Some(effort) = effort {
            value["effort"] = json!(effort);
        }
        line(secs, value)
    }

    /// Each message counts once for its model and effort; subagents,
    /// synthetic messages and records outside the span do not count.
    #[test]
    fn a_span_records_the_models_and_efforts_its_messages_used() {
        let opus = "claude-opus-5-5";
        let records = records(&[
            written(5, "m0", "claude-sonnet-5", Some("low")),
            written(10, "m1", opus, Some("medium")),
            written(11, "m1", opus, Some("medium")),
            written(12, "m2", opus, Some("high")),
            written(13, "m3", opus, Some("medium")),
            {
                let mut sub = json!({"isSidechain": true,
                    "message": {"id": "s1", "model": "claude-haiku-4-5", "content": []}});
                sub["effort"] = json!("medium");
                line(14, sub)
            },
            written(15, "m4", "<synthetic>", None),
            written(20, "m5", "claude-sonnet-5", Some("low")),
        ]);
        let uses = span_models(&records, 10_000, 20_000);
        assert_eq!(
            uses,
            vec![
                ModelUse {
                    model: opus.into(),
                    effort: Some("medium".into()),
                    messages: 2,
                },
                ModelUse {
                    model: opus.into(),
                    effort: Some("high".into()),
                    messages: 1,
                },
            ]
        );
        let mut payload = json!({});
        models_payload(&uses, &mut payload);
        assert_eq!(
            payload,
            json!({"model": opus, "effort": "medium", "models": [
                {"model": opus, "effort": "medium", "messages": 2},
                {"model": opus, "effort": "high", "messages": 1},
            ]})
        );
        // One pair: no breakdown. A tie goes to the later pair; an effort
        // the records do not name is null.
        let tie = records_of(&[
            written(1, "a", opus, None),
            written(2, "b", "claude-sonnet-5", Some("medium")),
        ]);
        let uses = span_models(&tie, 0, 10_000);
        assert_eq!(uses[0].model, "claude-sonnet-5");
        let mut payload = json!({});
        models_payload(&uses[1..], &mut payload);
        assert_eq!(payload, json!({"model": opus, "effort": null}));
        // No message names a model: nothing.
        assert!(span_models(&records, 30_000, 40_000).is_empty());
        let mut payload = json!({});
        models_payload(&[], &mut payload);
        assert_eq!(payload, json!({}));
    }

    fn records_of(lines: &[String]) -> Vec<TranscriptRecord> {
        records(lines)
    }

    /// One assistant record at `ms` for [`span_tokens_pick_what_span_usage_and_span_models_count_to_any_cut`].
    #[allow(clippy::too_many_arguments)]
    fn record_at(
        ms: i64,
        id: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
        counts: Option<(i64, i64)>,
        cost: Option<f64>,
        sidechain: bool,
    ) -> String {
        let mut message = json!({"content": []});
        if let Some(id) = id {
            message["id"] = json!(id);
        }
        if let Some(model) = model {
            message["model"] = json!(model);
        }
        if let Some((input, output)) = counts {
            message["usage"] = json!({"input_tokens": input, "output_tokens": output,
                                      "cache_read_input_tokens": input * 3});
        }
        let mut value = json!({"type": "assistant", "timestamp": millis_text(ms),
                               "sessionId": SESSION, "message": message});
        if let Some(effort) = effort {
            value["effort"] = json!(effort);
        }
        if let Some(cost) = cost {
            value["costUSD"] = json!(cost);
        }
        if sidechain {
            value["isSidechain"] = json!(true);
        }
        value.to_string()
    }

    /// The usage and models [`SpanTokens`] picks to any cut are those
    /// [`span_usage`] and [`span_models`] count to it (task 1334), with
    /// records out of the order of their times, the records of one message
    /// at different times (counts, costs and models), records sharing a
    /// time, records without an id, a subagent's and synthetic records,
    /// costs on every message or on some, a usage this reader does not
    /// know, and records without a usage; before, in and after the span.
    #[test]
    fn span_tokens_pick_what_span_usage_and_span_models_count_to_any_cut() {
        let opus = Some("claude-opus-5-5");
        let sonnet = Some("claude-sonnet-5");
        let mixed = records(&[
            record_at(500, Some("early"), opus, None, Some((1, 1)), None, false),
            record_at(
                3_000,
                Some("a"),
                opus,
                Some("high"),
                Some((5, 2)),
                None,
                false,
            ),
            record_at(
                1_000,
                Some("a"),
                sonnet,
                Some("low"),
                Some((3, 9)),
                None,
                false,
            ),
            line(2, json!({"type": "user", "message": {"content": "hi"}})),
            record_at(2_000, None, sonnet, None, Some((7, 7)), None, false),
            record_at(
                2_000,
                Some("b"),
                opus,
                Some("high"),
                Some((2, 4)),
                None,
                false,
            ),
            record_at(
                2_500,
                Some("s"),
                Some("claude-haiku-4-5"),
                None,
                Some((9, 9)),
                None,
                true,
            ),
            record_at(
                4_000,
                Some("c"),
                Some("<synthetic>"),
                None,
                Some((1, 0)),
                None,
                false,
            ),
            record_at(
                4_000,
                Some("b"),
                sonnet,
                Some("low"),
                Some((2, 6)),
                None,
                false,
            ),
            record_at(1_500, Some("d"), opus, Some("high"), None, None, false),
            record_at(6_000, None, sonnet, Some("low"), Some((4, 4)), None, false),
            record_at(
                5_000,
                Some("e"),
                opus,
                Some("high"),
                Some((8, 1)),
                None,
                false,
            ),
            record_at(
                9_000,
                Some("late"),
                sonnet,
                None,
                Some((100, 100)),
                None,
                false,
            ),
        ]);
        let costed = records(&[
            record_at(1_000, Some("a"), opus, None, Some((1, 1)), Some(0.1), false),
            record_at(3_000, Some("a"), opus, None, Some((1, 2)), Some(0.2), false),
            record_at(2_000, Some("a"), opus, None, Some((1, 3)), Some(0.7), false),
            record_at(1_500, Some("b"), opus, None, Some((2, 2)), Some(0.3), false),
            record_at(2_500, None, opus, None, Some((2, 2)), Some(1e-7), false),
            record_at(2_500, Some("c"), opus, None, Some((2, 2)), None, false),
            record_at(
                3_500,
                Some("c"),
                opus,
                None,
                Some((2, 2)),
                Some(0.05),
                false,
            ),
        ]);
        let unknown = records(&[
            record_at(1_000, Some("a"), opus, None, Some((1, 1)), None, false),
            line(
                2,
                json!({"message": {"id": "x", "usage": {"input_tokens": "3"}}}),
            ),
            record_at(3_000, Some("b"), opus, None, Some((1, 1)), None, false),
        ]);
        let bare = records(&[
            record_at(1_000, Some("a"), opus, None, None, None, false),
            record_at(2_000, Some("b"), opus, None, Some((1, 1)), None, false),
        ]);
        for records in [&mixed, &costed, &unknown, &bare] {
            let mut cuts: Vec<i64> = records
                .iter()
                .flat_map(|r| [r.at - 1, r.at, r.at + 1])
                .chain([i64::MIN / 2, 0, 20_000])
                .collect();
            cuts.sort_unstable();
            cuts.dedup();
            for (start, end) in [(0, 20_000), (1_000, 4_000), (1_001, 9_000), (7_000, 8_000)] {
                let tokens = SpanTokens::new(records, start, end);
                for &cut in &cuts {
                    let to = cut.min(end);
                    assert_eq!(
                        tokens.at(cut),
                        (
                            span_usage(records, start, to),
                            span_models(records, start, to)
                        ),
                        "{start}..{end} to {cut}"
                    );
                }
            }
        }
    }

    /// A usage without numeric counts, or assistant records none of which
    /// has a usage, is a format this reader does not know.
    #[test]
    fn an_unknown_usage_is_unsupported() {
        let text = records(&[line(
            1,
            json!({"message": {"id": "a", "usage": {"input_tokens": "3"}}}),
        )]);
        assert_eq!(span_usage(&text, 0, 10_000), Err(USAGE_UNSUPPORTED));
        let none = records(&[line(1, json!({"message": {"id": "a", "content": []}}))]);
        assert_eq!(span_usage(&none, 0, 10_000), Err(USAGE_UNSUPPORTED));
    }
}
