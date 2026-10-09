//! What decides a round of an eval (ADR-t1728-1 decisions 7 to 9): the
//! limits of `[eval]`, the estimate before a round starts and the check
//! before each run, what a run cost and where that came from, the once
//! rule of a hold-out, the order the waiting rounds go in, and whether the
//! review's provider can take a run now. Everything here is without side
//! effects: the supervisor reads the events and the definition and acts.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::Split;
use crate::domain::tokens::TokenUsage;
use crate::domain::{DomainError, Provider};

/// `[eval] max_runs` when the configuration names none: the runs of a
/// round (each case × its `k`) at most. The Spike's dev round of
/// adr-rules took about 100 runs.
pub const DEFAULT_MAX_RUNS: u32 = 120;
/// `[eval] max_cost_usd` when the configuration names none: the dollars a
/// round may spend at most. A dev round of the Spike cost $20 to $24.
pub const DEFAULT_MAX_COST_USD: f64 = 30.0;
/// `[eval] concurrency` when the configuration names none: the provider's
/// processes of one round at once, as the Spike ran 4.
pub const DEFAULT_CONCURRENCY: usize = 4;
/// `[eval] recent_runs` when the configuration names none: how many of the
/// latest runs of the same agent on the same provider the estimate of one
/// run takes its largest cost from.
pub const DEFAULT_RECENT_RUNS: usize = 20;
/// `[eval.providers.claude] default_run_usd` when the configuration names
/// none: one run's estimate without a run measured yet, twice the Spike's
/// measured $0.20 a run. Codex has no default: it returns no dollars.
pub const DEFAULT_CLAUDE_RUN_USD: f64 = 0.40;

/// `[eval.providers.<provider>]`: what one run of `provider` is estimated
/// at, and its prices per token for a provider that returns no dollars.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProviderCost {
    /// `default_run_usd`: one run's estimate when no run of the agent on
    /// the provider was measured yet.
    pub default_run_usd: Option<f64>,
    /// `input_usd_per_mtok`: dollars a million input tokens (without the
    /// cache).
    pub input_usd_per_mtok: Option<f64>,
    /// `cached_input_usd_per_mtok`: dollars a million cached input tokens;
    /// without it they cost as input.
    pub cached_input_usd_per_mtok: Option<f64>,
    /// `output_usd_per_mtok`: dollars a million output tokens.
    pub output_usd_per_mtok: Option<f64>,
}

impl ProviderCost {
    /// The keys of `[eval.providers.<provider>]`.
    pub const KEYS: [&'static str; 4] = [
        "default_run_usd",
        "input_usd_per_mtok",
        "cached_input_usd_per_mtok",
        "output_usd_per_mtok",
    ];

    /// Set the key `key` (one of [`Self::KEYS`]) to `value`.
    pub fn set(&mut self, key: &str, value: f64) {
        match key {
            "default_run_usd" => self.default_run_usd = Some(value),
            "input_usd_per_mtok" => self.input_usd_per_mtok = Some(value),
            "cached_input_usd_per_mtok" => self.cached_input_usd_per_mtok = Some(value),
            _ => self.output_usd_per_mtok = Some(value),
        }
    }

    /// Whether tokens can be turned into dollars: the input and output
    /// prices are both set.
    pub fn converts(&self) -> bool {
        self.input_usd_per_mtok.is_some() && self.output_usd_per_mtok.is_some()
    }

    /// The dollars of `tokens` at these prices; `None` without them.
    pub fn convert(&self, tokens: &TokenUsage) -> Option<f64> {
        let input = self.input_usd_per_mtok?;
        let output = self.output_usd_per_mtok?;
        let cached = self.cached_input_usd_per_mtok.unwrap_or(input);
        let mtok = |count: i64, price: f64| count.max(0) as f64 / 1_000_000.0 * price;
        Some(
            mtok(tokens.input, input)
                + mtok(tokens.cache_read + tokens.cache_creation, cached)
                + mtok(tokens.output, output),
        )
    }
}

/// `[eval]` and `[eval.providers.<provider>]` of `dagq.toml`, each key's
/// default when it is not set ([`EvalConfig::default`]).
#[derive(Debug, Clone, PartialEq)]
pub struct EvalConfig {
    /// `max_runs`, a whole number above 0 ([`DEFAULT_MAX_RUNS`]).
    pub max_runs: u32,
    /// `max_cost_usd`, dollars above 0 ([`DEFAULT_MAX_COST_USD`]).
    pub max_cost_usd: f64,
    /// `concurrency`, a whole number above 0 ([`DEFAULT_CONCURRENCY`]).
    pub concurrency: usize,
    /// `threshold`, from 0 to 1, which the judgment's and the rule codes'
    /// recall and precision must each reach
    /// ([`super::DEFAULT_THRESHOLD`]).
    pub threshold: f64,
    /// `recent_runs`, a whole number above 0 ([`DEFAULT_RECENT_RUNS`]).
    pub recent_runs: usize,
    /// `[eval.providers.<provider>]` by provider; a provider without its
    /// table has Claude's [`DEFAULT_CLAUDE_RUN_USD`] for Claude and nothing
    /// for Codex.
    pub providers: BTreeMap<&'static str, ProviderCost>,
}

impl EvalConfig {
    /// The keys of `[eval]`.
    pub const KEYS: [&'static str; 5] = [
        "max_runs",
        "max_cost_usd",
        "concurrency",
        "threshold",
        "recent_runs",
    ];

    /// `provider`'s table.
    pub fn provider(&self, provider: Provider) -> ProviderCost {
        self.providers
            .get(provider.as_str())
            .copied()
            .unwrap_or_default()
    }

    /// `provider`'s table to set.
    pub fn provider_mut(&mut self, provider: Provider) -> &mut ProviderCost {
        self.providers.entry(provider.as_str()).or_default()
    }
}

impl Default for EvalConfig {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            Provider::Claude.as_str(),
            ProviderCost {
                default_run_usd: Some(DEFAULT_CLAUDE_RUN_USD),
                ..ProviderCost::default()
            },
        );
        Self {
            max_runs: DEFAULT_MAX_RUNS,
            max_cost_usd: DEFAULT_MAX_COST_USD,
            concurrency: DEFAULT_CONCURRENCY,
            threshold: super::DEFAULT_THRESHOLD,
            recent_runs: DEFAULT_RECENT_RUNS,
            providers,
        }
    }
}

/// Whether `provider`'s run reports the dollars it cost: Claude Code's
/// result has `total_cost_usd`; Codex gives tokens only (the Spike's
/// REPORT 4.), which its `[eval.providers.codex]` prices turn into
/// dollars.
pub const fn reports_dollars(provider: Provider) -> bool {
    matches!(provider, Provider::Claude)
}

// Where one run's estimate came from: the largest of the latest runs of
// the same agent on the same provider (`recent_max`), else the provider's
// `default_run_usd` (`provider_default`).
string_enum!(EstimateSource {
    RecentMax => "recent_max",
    ProviderDefault => "provider_default",
});

// Where a run's dollars came from: the provider's own (`actual`), its
// tokens at the configured prices (`converted`), or its estimate for a run
// that returned neither (`estimated`, a conservative charge).
string_enum!(CostSource {
    Actual => "actual",
    Converted => "converted",
    Estimated => "estimated",
});

/// What a round is estimated to cost before it starts, recorded as its
/// reservation on `agent_eval_started`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    /// The runs the round plans: each case's `k` summed.
    pub planned_runs: u32,
    /// One run's estimate.
    pub per_run_usd: f64,
    pub per_run_source: EstimateSource,
    /// `planned_runs` × `per_run_usd`.
    pub total_usd: f64,
}

impl Estimate {
    /// The estimate as its events record it.
    pub fn record(&self) -> Value {
        json!({
            "planned_runs": self.planned_runs,
            "per_run_usd": self.per_run_usd,
            "per_run_source": self.per_run_source,
            "total_usd": self.total_usd,
        })
    }
}

// Why a round was not started (`agent_eval_refused`'s `reason`): its runs
// over `max_runs`, its estimate over `max_cost_usd`, dollars that could
// not be known (`cost_unknown`), a hold-out already run with the same key
// and no rerun asked, a definition past the agent job's limit
// (ADR-t1869-1), no definition in the landing branch's commit, one whose
// declared tools are a mistake (ADR-t1728-2), or case lists that do not
// read or name no case to run.
string_enum!(RefusalReason {
    OverRunLimit => "over_run_limit",
    OverCostLimit => "over_cost_limit",
    CostUnknown => "cost_unknown",
    HoldoutUsed => "holdout_used",
    DefinitionOverLimit => "definition_over_limit",
    DefinitionMissing => "definition_missing",
    DefinitionInvalid => "definition_invalid",
    CasesInvalid => "cases_invalid",
});

/// A round not started and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub reason: RefusalReason,
    /// What the reason is about, said for a person.
    pub detail: String,
    /// The estimate, when one was made.
    pub estimate: Option<Estimate>,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason.as_str(), self.detail)
    }
}

fn refuse(reason: RefusalReason, detail: String, estimate: Option<Estimate>) -> Refusal {
    Refusal {
        reason,
        detail,
        estimate,
    }
}

/// Estimate a round of `planned_runs` runs of an agent on `provider` before
/// it starts (ADR-t1728-1 decision 8): one run is the largest of `recent`
/// (the costs of the latest runs of the same agent on the same provider),
/// else the provider's `default_run_usd`. Refused when the runs are over
/// `max_runs` (with the estimate when one run can be estimated); when its
/// dollars could not be known after each run (a provider that reports
/// none and has no token prices, whatever its default), or there is no
/// estimate of one run; and when the estimate is over `max_cost_usd`.
pub fn estimate(
    planned_runs: u32,
    provider: Provider,
    recent: &[f64],
    config: &EvalConfig,
) -> Result<Estimate, Refusal> {
    let prices = config.provider(provider);
    let measured = recent.iter().copied().filter(|cost| cost.is_finite());
    let per_run = match measured.reduce(f64::max) {
        Some(max) => Some((max, EstimateSource::RecentMax)),
        None => prices
            .default_run_usd
            .map(|default| (default, EstimateSource::ProviderDefault)),
    };
    let estimate = per_run.map(|(per_run_usd, per_run_source)| Estimate {
        planned_runs,
        per_run_usd,
        per_run_source,
        total_usd: f64::from(planned_runs) * per_run_usd,
    });
    if planned_runs > config.max_runs {
        return Err(refuse(
            RefusalReason::OverRunLimit,
            format!(
                "the round plans {planned_runs} runs, over [eval] max_runs {}",
                config.max_runs
            ),
            estimate,
        ));
    }
    if !reports_dollars(provider) && !prices.converts() {
        return Err(refuse(
            RefusalReason::CostUnknown,
            format!(
                "{} reports no dollars and [eval.providers.{}] has no input_usd_per_mtok and output_usd_per_mtok to convert its tokens with",
                provider.as_str(),
                provider.as_str()
            ),
            None,
        ));
    }
    let Some(estimate) = estimate else {
        return Err(refuse(
            RefusalReason::CostUnknown,
            format!(
                "no run of the agent on {} was measured and [eval.providers.{}] has no default_run_usd",
                provider.as_str(),
                provider.as_str()
            ),
            None,
        ));
    };
    if estimate.total_usd > config.max_cost_usd {
        return Err(refuse(
            RefusalReason::OverCostLimit,
            format!(
                "the round is estimated at ${:.2} ({planned_runs} runs × ${:.2}), over [eval] max_cost_usd ${:.2}",
                estimate.total_usd, estimate.per_run_usd, config.max_cost_usd
            ),
            Some(estimate),
        ));
    }
    Ok(estimate)
}

/// Whether one more run may start (ADR-t1728-1 decision 8): what the round
/// spent, what its `running` runs are estimated at and the next run's
/// estimate stay within `max_cost_usd`.
pub fn may_start_next(spent_usd: f64, running: usize, per_run_usd: f64, max_cost_usd: f64) -> bool {
    spent_usd + (running as f64 + 1.0) * per_run_usd <= max_cost_usd
}

/// What a run cost and where that came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunCost {
    pub usd: f64,
    pub source: CostSource,
}

impl RunCost {
    pub fn record(&self) -> Value {
        json!({"usd": self.usd, "source": self.source})
    }

    /// A run's cost as its `agent_eval_run_finished` recorded it.
    pub fn read(value: &Value) -> Option<Self> {
        Some(Self {
            usd: value.get("usd")?.as_f64()?,
            source: value.get("source")?.as_str()?.parse().ok()?,
        })
    }
}

/// What a run cost, from what its job's output gave (`tokens`, `None` when
/// it gave nothing): the dollars it reports, else its tokens at `prices`,
/// else (a run that ended without either, such as one stopped) its
/// estimate `per_run_usd`.
pub fn run_cost(tokens: Option<&TokenUsage>, prices: &ProviderCost, per_run_usd: f64) -> RunCost {
    if let Some(usd) = tokens.and_then(|tokens| tokens.cost_usd) {
        return RunCost {
            usd,
            source: CostSource::Actual,
        };
    }
    if let Some(usd) = tokens.and_then(|tokens| prices.convert(tokens)) {
        return RunCost {
            usd,
            source: CostSource::Converted,
        };
    }
    RunCost {
        usd: per_run_usd,
        source: CostSource::Estimated,
    }
}

/// The SHA-256 of a definition's text, as the review's snapshot digests
/// it.
pub fn definition_digest(definition: &str) -> String {
    format!("{:x}", Sha256::digest(definition.as_bytes()))
}

/// The digest of the cases a round runs (ADR-t1728-1 decision 7): each
/// case's id and the SHA-256 of its patch's content, one line each in the
/// ids' order, hashed. The same for the repository's `holdout.json` and
/// for a production set kept in the queue.
pub fn case_set_digest<'a>(cases: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> String {
    let mut lines: Vec<String> = cases
        .into_iter()
        .map(|(id, patch)| format!("{id}\t{:x}\n", Sha256::digest(patch)))
        .collect();
    lines.sort();
    format!("{:x}", Sha256::digest(lines.concat().as_bytes()))
}

/// What a hold-out is run once for (ADR-t1728-1 decision 7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundKey {
    pub agent: String,
    pub definition_digest: String,
    pub case_set_digest: String,
}

/// Whether a split is held out: run once per [`RoundKey`]. Dev is run as
/// often as asked; hold-out and production are both held out (a production
/// set is run as a hold-out once).
pub const fn held_out(split: Split) -> bool {
    !matches!(split, Split::Dev)
}

/// Refuse a round of `split` with `key` when a held-out round with the same
/// key started before (`started`, the keys of every held-out round's
/// `agent_eval_started`) and no rerun was asked. A key with another
/// definition or another set of cases is a first run.
pub fn holdout_refusal(
    split: Split,
    key: &RoundKey,
    started: &[RoundKey],
    rerun: bool,
) -> Option<Refusal> {
    if !held_out(split) || rerun || !started.contains(key) {
        return None;
    }
    Some(refuse(
        RefusalReason::HoldoutUsed,
        format!(
            "the {} cases (digest {}) were run on {}'s definition (digest {}) already; only a person's --rerun runs them again",
            split.as_str(),
            short(&key.case_set_digest),
            key.agent,
            short(&key.definition_digest)
        ),
        None,
    ))
}

fn short(digest: &str) -> &str {
    &digest[..digest.len().min(12)]
}

/// A round waiting to start: its id (the event id of its request) and
/// whether it is the dev a run waits for before it lands (ADR-t1728-1
/// decision 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waiting {
    pub id: i64,
    pub before_landing: bool,
}

/// The round to start next (ADR-t1728-1 decision 9): none while one runs
/// (one round per queue at once), else a run's dev before it lands first,
/// then the oldest request.
pub fn next_round(waiting: &[Waiting], running: bool) -> Option<i64> {
    if running {
        return None;
    }
    waiting
        .iter()
        .min_by_key(|round| (!round.before_landing, round.id))
        .map(|round| round.id)
}

/// How many more runs of a round may start now: up to `concurrency`
/// processes at once, and no more than are left to start.
pub fn launchable(running: usize, concurrency: usize, left: usize) -> usize {
    concurrency.saturating_sub(running).min(left)
}

// Why the review's provider takes no new run now: it is held (a login, a
// usage limit, a provider hold), the supervisor runs with `--no-claude`
// and it is Claude, or the supervisor has no agent of it. A round does
// not move to another provider (its scores, estimates and prices are the
// provider's): its new runs wait, and those running go on.
string_enum!(ProviderWait {
    Held => "provider_held",
    NoClaude => "no_claude",
    NoAgent => "no_agent",
});

/// Whether `provider` takes a new run of a round: `held` it is held,
/// `no_claude` the supervisor runs without Claude, `has_agent` the
/// supervisor has an agent that runs it.
pub fn provider_wait(
    provider: Provider,
    held: bool,
    no_claude: bool,
    has_agent: bool,
) -> Option<ProviderWait> {
    if no_claude && provider == Provider::Claude {
        Some(ProviderWait::NoClaude)
    } else if held {
        Some(ProviderWait::Held)
    } else if !has_agent {
        Some(ProviderWait::NoAgent)
    } else {
        None
    }
}

/// Whether `token`, a supervisor with no round in hand, may take up a
/// running round whose owner is `owner` (the supervisor of its latest start
/// or take-up; `None` for one recorded without). A round has one owner at a
/// time (ADR-t1728-1 decision 9): it is taken up only from an owner that
/// is gone (not registered, or its heartbeat older than
/// [`crate::domain::HEARTBEAT_TIMEOUT_SECS`] while its process is gone too:
/// a host just woken from sleep has every heartbeat old), or from `token`
/// itself (a process that exec'd and lost the round it ran).
/// `registration` is the owner's heartbeat (unix seconds) and whether its
/// process lives, `None` when it is not registered; `now` is unix seconds.
pub fn owner_gone(
    owner: Option<&str>,
    token: &str,
    registration: Option<(i64, bool)>,
    now: i64,
) -> bool {
    let Some(owner) = owner else {
        return true;
    };
    if owner == token {
        return true;
    }
    match registration {
        None => true,
        Some((heartbeat, alive)) => {
            now - heartbeat > crate::domain::HEARTBEAT_TIMEOUT_SECS && !alive
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EvalConfig {
        EvalConfig::default()
    }

    fn tokens(input: i64, output: i64, cost_usd: Option<f64>) -> TokenUsage {
        TokenUsage {
            input,
            output,
            cost_usd,
            ..TokenUsage::default()
        }
    }

    #[test]
    fn a_round_over_its_runs_or_its_estimate_is_refused_with_the_estimate() {
        let runs = estimate(121, Provider::Claude, &[], &config()).unwrap_err();
        assert_eq!(runs.reason, RefusalReason::OverRunLimit);
        // The estimate is recorded with the refusal: 121 runs at the
        // default $0.40.
        let over = runs.estimate.unwrap();
        assert_eq!(over.planned_runs, 121);
        assert_eq!(over.per_run_source, EstimateSource::ProviderDefault);
        assert!((over.total_usd - 48.4).abs() < 1e-9);
        // Without an estimate of one run, the refusal carries none.
        let mut unpriced = config();
        unpriced.provider_mut(Provider::Claude).default_run_usd = None;
        let bare = estimate(121, Provider::Claude, &[], &unpriced).unwrap_err();
        assert_eq!(
            (bare.reason, bare.estimate),
            (RefusalReason::OverRunLimit, None)
        );
        // 100 runs at the recent largest $0.31 is $31 over $30.
        let cost = estimate(100, Provider::Claude, &[0.2, 0.31, 0.1], &config()).unwrap_err();
        assert_eq!(cost.reason, RefusalReason::OverCostLimit);
        let estimated = cost.estimate.unwrap();
        assert_eq!(estimated.per_run_source, EstimateSource::RecentMax);
        assert!((estimated.total_usd - 31.0).abs() < 1e-9);
        assert!(cost.detail.contains("$31.00"), "{cost}");
    }

    #[test]
    fn a_round_within_its_limits_is_estimated_from_recent_runs_else_the_default() {
        let recent = estimate(10, Provider::Claude, &[0.25, 0.5], &config()).unwrap();
        assert_eq!(
            (recent.per_run_usd, recent.per_run_source, recent.total_usd),
            (0.5, EstimateSource::RecentMax, 5.0)
        );
        let default = estimate(10, Provider::Claude, &[], &config()).unwrap();
        assert_eq!(
            (default.per_run_usd, default.per_run_source),
            (DEFAULT_CLAUDE_RUN_USD, EstimateSource::ProviderDefault)
        );
        assert_eq!(
            default.record()["per_run_source"],
            json!("provider_default")
        );
    }

    #[test]
    fn a_provider_without_dollars_or_prices_is_cost_unknown_even_with_a_default() {
        let mut config = config();
        config.provider_mut(Provider::Codex).default_run_usd = Some(0.3);
        let unknown = estimate(4, Provider::Codex, &[0.2], &config).unwrap_err();
        assert_eq!(unknown.reason, RefusalReason::CostUnknown);
        // With prices it converts, and so it is estimated.
        let prices = config.provider_mut(Provider::Codex);
        prices.input_usd_per_mtok = Some(1.0);
        prices.output_usd_per_mtok = Some(10.0);
        assert!(estimate(4, Provider::Codex, &[], &config).is_ok());
    }

    #[test]
    fn a_round_without_a_measured_run_or_a_default_is_cost_unknown() {
        let mut config = config();
        config.provider_mut(Provider::Claude).default_run_usd = None;
        let unknown = estimate(4, Provider::Claude, &[], &config).unwrap_err();
        assert_eq!(unknown.reason, RefusalReason::CostUnknown);
        assert!(unknown.detail.contains("default_run_usd"), "{unknown}");
    }

    #[test]
    fn the_next_run_starts_only_while_spent_running_and_next_stay_within_the_limit() {
        assert!(may_start_next(28.0, 1, 1.0, 30.0));
        assert!(!may_start_next(28.5, 1, 1.0, 30.0));
        assert!(!may_start_next(29.5, 0, 1.0, 30.0));
    }

    #[test]
    fn a_runs_cost_is_actual_converted_or_estimated() {
        let prices = ProviderCost {
            input_usd_per_mtok: Some(2.0),
            cached_input_usd_per_mtok: Some(0.5),
            output_usd_per_mtok: Some(8.0),
            ..ProviderCost::default()
        };
        let actual = run_cost(Some(&tokens(10, 10, Some(0.21))), &prices, 0.4);
        assert_eq!((actual.usd, actual.source), (0.21, CostSource::Actual));
        let mut used = tokens(1_000_000, 100_000, None);
        used.cache_read = 2_000_000;
        let converted = run_cost(Some(&used), &prices, 0.4);
        assert_eq!(converted.source, CostSource::Converted);
        assert!((converted.usd - (2.0 + 1.0 + 0.8)).abs() < 1e-9);
        // No prices: the tokens cannot be turned into dollars.
        let estimated = run_cost(Some(&used), &ProviderCost::default(), 0.4);
        assert_eq!(
            (estimated.usd, estimated.source),
            (0.4, CostSource::Estimated)
        );
        let nothing = run_cost(None, &prices, 0.4);
        assert_eq!((nothing.usd, nothing.source), (0.4, CostSource::Estimated));
        assert_eq!(RunCost::read(&nothing.record()), Some(nothing));
    }

    fn key(definition: &str, cases: &str) -> RoundKey {
        RoundKey {
            agent: "adr-rules".into(),
            definition_digest: definition.into(),
            case_set_digest: cases.into(),
        }
    }

    #[test]
    fn a_holdout_runs_once_per_definition_and_case_set_unless_rerun() {
        let started = [key("d1", "c1")];
        assert!(holdout_refusal(Split::Holdout, &key("d1", "c1"), &[], false).is_none());
        let again = holdout_refusal(Split::Holdout, &key("d1", "c1"), &started, false).unwrap();
        assert_eq!(again.reason, RefusalReason::HoldoutUsed);
        assert!(again.detail.contains("--rerun"), "{again}");
        assert!(holdout_refusal(Split::Holdout, &key("d1", "c1"), &started, true).is_none());
        // Another set of cases, or another definition, is a first run.
        assert!(holdout_refusal(Split::Holdout, &key("d1", "c2"), &started, false).is_none());
        assert!(holdout_refusal(Split::Holdout, &key("d2", "c1"), &started, false).is_none());
        assert!(holdout_refusal(Split::Production, &key("d1", "c1"), &started, false).is_some());
        assert!(holdout_refusal(Split::Dev, &key("d1", "c1"), &started, false).is_none());
    }

    #[test]
    fn the_case_sets_digest_follows_the_ids_and_the_patches_content() {
        let a = case_set_digest([("a", b"x".as_slice()), ("b", b"y".as_slice())]);
        let reordered = case_set_digest([("b", b"y".as_slice()), ("a", b"x".as_slice())]);
        assert_eq!(a, reordered);
        assert_ne!(
            a,
            case_set_digest([("a", b"x".as_slice()), ("b", b"z".as_slice())])
        );
        assert_ne!(a, case_set_digest([("a", b"x".as_slice())]));
        assert_eq!(definition_digest("x").len(), 64);
    }

    #[test]
    fn rounds_wait_while_one_runs_then_a_landings_dev_goes_before_the_oldest() {
        let waiting = [
            Waiting {
                id: 3,
                before_landing: false,
            },
            Waiting {
                id: 9,
                before_landing: true,
            },
            Waiting {
                id: 5,
                before_landing: false,
            },
        ];
        assert_eq!(next_round(&waiting, true), None);
        assert_eq!(next_round(&waiting, false), Some(9));
        assert_eq!(next_round(&waiting[..1], false), Some(3));
        assert_eq!(next_round(&[waiting[2], waiting[0]], false), Some(3));
        assert_eq!(next_round(&[], false), None);
    }

    #[test]
    fn a_running_round_is_taken_up_only_from_a_gone_owner() {
        let now = 1_000;
        // A live owner keeps it, a fresh heartbeat or a live process.
        assert!(!owner_gone(Some("a"), "b", Some((now - 5, true)), now));
        assert!(!owner_gone(Some("a"), "b", Some((now - 5, false)), now));
        assert!(!owner_gone(Some("a"), "b", Some((now - 600, true)), now));
        // A stale heartbeat whose process is gone, no registration, no
        // owner recorded, or this supervisor's own token: taken up.
        assert!(owner_gone(Some("a"), "b", Some((now - 600, false)), now));
        assert!(owner_gone(Some("a"), "b", None, now));
        assert!(owner_gone(None, "b", None, now));
        assert!(owner_gone(Some("b"), "b", Some((now, true)), now));
    }

    #[test]
    fn a_round_starts_no_more_processes_than_its_concurrency() {
        assert_eq!(launchable(0, 4, 10), 4);
        assert_eq!(launchable(3, 4, 10), 1);
        assert_eq!(launchable(4, 4, 10), 0);
        assert_eq!(launchable(5, 4, 10), 0);
        assert_eq!(launchable(0, 4, 2), 2);
    }

    #[test]
    fn a_provider_that_cannot_be_used_makes_the_runs_wait_without_a_switch() {
        assert_eq!(
            provider_wait(Provider::Claude, false, true, true),
            Some(ProviderWait::NoClaude)
        );
        // --no-claude leaves Codex alone.
        assert_eq!(provider_wait(Provider::Codex, false, true, true), None);
        assert_eq!(
            provider_wait(Provider::Codex, true, false, true),
            Some(ProviderWait::Held)
        );
        assert_eq!(
            provider_wait(Provider::Codex, false, false, false),
            Some(ProviderWait::NoAgent)
        );
        assert_eq!(provider_wait(Provider::Claude, false, false, true), None);
    }
}
