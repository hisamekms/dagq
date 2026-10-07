//! The review's harness: a case's `review.input` and `review.expected`,
//! one run of a review agent read from its result, and the round's scores.
//!
//! The unit is a run (a case × one of its `k` runs). The verdict counts
//! `revise` and `concern` as a violation and `pass` as clean; the rule
//! codes are the codes of the result's `reasons[].codes`, counted against
//! the expected codes, and a code of `acceptable_codes` the agent names is
//! not a false positive. A run without a judgment is an error and counts in
//! neither. A disputed case is left out of the primary scores and counted
//! in [`ReviewScore::with_disputed`]. The formulas are the Spike's
//! `runner/eval.py` `score` (dagq-agent-eval commit cfb8a4c), without its
//! Wilson intervals, per-round minimum and maximum, near-miss false
//! positive rate, pass^k and flip rates and cost totals;
//! docs/design/agent-eval.md's "採点" states the contract.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::{AgentRole, Case, Fields, Harness, PerCode, ProblemKind, ratio};
use crate::domain::ReviewDecision;

/// What a review case expects: violation or clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Judgement {
    Violation,
    Clean,
}

impl Judgement {
    pub const fn as_str(self) -> &'static str {
        match self {
            Judgement::Violation => "violation",
            Judgement::Clean => "clean",
        }
    }

    /// `revise` and `concern` find a violation, `pass` none.
    pub const fn of(decision: ReviewDecision) -> Judgement {
        match decision {
            ReviewDecision::Pass => Judgement::Clean,
            ReviewDecision::Revise | ReviewDecision::Concern => Judgement::Violation,
        }
    }

    fn parse(name: &str) -> Option<Judgement> {
        [Judgement::Violation, Judgement::Clean]
            .into_iter()
            .find(|judgement| judgement.as_str() == name)
    }
}

/// A case's `review.expected`: `verdict` and `codes` are required,
/// `acceptable_codes`, `note` and `near_miss` may be left out (none, empty,
/// `false`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewExpected {
    pub verdict: Judgement,
    /// The codes the agent must name: some for a violation, none for a
    /// clean case, each in the list's `codes`.
    pub codes: BTreeSet<String>,
    /// Codes the agent may name without a false positive, none of them in
    /// `codes`.
    pub acceptable_codes: BTreeSet<String>,
    /// Why the case expects this, and why a code is acceptable.
    pub note: String,
    /// A clean case close to a violation.
    pub near_miss: bool,
}

/// A case's `review`: the review's `input` (empty for now: the diff is the
/// patch and the rules are the definition's) and `expected`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewCase {
    pub input: serde_json::Map<String, Value>,
    pub expected: ReviewExpected,
}

/// One run of the agent on a case: `outcome` is `None` when it gave no
/// judgment (it did not complete, or its result could not be read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRun {
    pub case: String,
    /// Which of the case's `k` runs, from 0.
    pub round: u32,
    pub outcome: Option<ReviewOutcome>,
}

/// The judgment of a completed run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOutcome {
    pub verdict: ReviewDecision,
    /// The codes the agent named; none for a `pass`.
    pub codes: BTreeSet<String>,
}

impl ReviewOutcome {
    /// Read an agent's result as the review prints it (`status`,
    /// `verdict`, `reasons`): a judgment when `status` is `completed` and
    /// `verdict` is `pass`, `revise` or `concern`. The codes are those of
    /// the `codes` of `reasons`' items (a string or a list), only for a
    /// violation; the text and the `summary` are not read, as the summary
    /// also names rules that did not apply.
    pub fn from_agent_result(result: &Value) -> Option<ReviewOutcome> {
        if result.get("status").and_then(Value::as_str) != Some("completed") {
            return None;
        }
        let verdict: ReviewDecision = result.get("verdict")?.as_str()?.parse().ok()?;
        let codes = if Judgement::of(verdict) == Judgement::Violation {
            result
                .get("reasons")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|reason| reason.get("codes"))
                .flat_map(|codes| match codes {
                    Value::String(code) => vec![code.as_str()],
                    Value::Array(codes) => codes.iter().filter_map(Value::as_str).collect(),
                    _ => Vec::new(),
                })
                .map(str::trim)
                .filter(|code| !code.is_empty())
                .map(str::to_owned)
                .collect()
        } else {
            BTreeSet::new()
        };
        Some(ReviewOutcome { verdict, codes })
    }

    pub fn judgement(&self) -> Judgement {
        Judgement::of(self.verdict)
    }
}

/// True positives, false positives and false negatives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub true_positives: usize,
    pub false_positives: usize,
    pub false_negatives: usize,
}

impl Counts {
    /// `None` when nothing was to be found.
    pub fn recall(&self) -> Option<f64> {
        ratio(
            self.true_positives,
            self.true_positives + self.false_negatives,
        )
    }

    /// `None` when nothing was found.
    pub fn precision(&self) -> Option<f64> {
        ratio(
            self.true_positives,
            self.true_positives + self.false_positives,
        )
    }

    fn add(&mut self, other: Counts) {
        self.true_positives += other.true_positives;
        self.false_positives += other.false_positives;
        self.false_negatives += other.false_negatives;
    }
}

/// The verdict's and the rule codes' (summed over the codes) counts of a
/// set of runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Metrics {
    pub verdict: Counts,
    /// Clean runs judged clean.
    pub true_negatives: usize,
    pub codes: Counts,
    pub runs: usize,
    /// Runs without a judgment.
    pub errors: usize,
}

/// One case's runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseResult {
    pub id: String,
    pub disputed: bool,
    pub runs: usize,
    /// Runs whose verdict was the expected one.
    pub correct: usize,
    pub errors: usize,
    /// Runs that named every expected code and nothing but expected or
    /// acceptable codes.
    pub codes_exact: usize,
}

impl CaseResult {
    /// A run judged wrongly, or one without a judgment.
    pub fn failed(&self) -> bool {
        self.correct < self.runs
    }
}

/// The scores of a round of the review harness.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewScore {
    /// Without the disputed cases.
    pub primary: Metrics,
    /// Each code's counts over the primary runs: every code the cases
    /// expect, and every one the agent named.
    pub per_code: PerCode<Counts>,
    /// With the disputed cases, `None` when no case is disputed.
    pub with_disputed: Option<Metrics>,
    /// Every case's, the disputed ones too, in the cases' order.
    pub cases: Vec<CaseResult>,
}

/// A score compared with the threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    VerdictRecall,
    VerdictPrecision,
    CodesRecall,
    CodesPrecision,
}

impl Metric {
    pub const ALL: [Metric; 4] = [
        Metric::VerdictRecall,
        Metric::VerdictPrecision,
        Metric::CodesRecall,
        Metric::CodesPrecision,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Metric::VerdictRecall => "verdict_recall",
            Metric::VerdictPrecision => "verdict_precision",
            Metric::CodesRecall => "codes_recall",
            Metric::CodesPrecision => "codes_precision",
        }
    }

    pub fn of(self, metrics: &Metrics) -> Option<f64> {
        match self {
            Metric::VerdictRecall => metrics.verdict.recall(),
            Metric::VerdictPrecision => metrics.verdict.precision(),
            Metric::CodesRecall => metrics.codes.recall(),
            Metric::CodesPrecision => metrics.codes.precision(),
        }
    }
}

/// The primary scores against a threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct ThresholdCheck {
    pub threshold: f64,
    /// Every [`Metric`] at or over the threshold and no run without a
    /// judgment.
    pub passed: bool,
    /// The metrics under the threshold, or without a value (nothing to
    /// count), with their values.
    pub below: Vec<(Metric, Option<f64>)>,
    pub errors: usize,
}

/// One code's values against a threshold (shown, not used to adopt).
#[derive(Debug, Clone, PartialEq)]
pub struct CodeCheck {
    pub code: String,
    pub counts: Counts,
    pub recall: Option<f64>,
    pub precision: Option<f64>,
    /// Both values at or over the threshold; a value without a count (the
    /// code was neither expected nor named wrongly) does not hold it back.
    pub meets: bool,
}

impl ReviewScore {
    /// Compare the primary scores with `threshold`. Whether a definition
    /// is adopted is decided by these values for the agent, not by
    /// [`ReviewScore::per_code_checks`].
    pub fn check(&self, threshold: f64) -> ThresholdCheck {
        let below: Vec<(Metric, Option<f64>)> = Metric::ALL
            .into_iter()
            .map(|metric| (metric, metric.of(&self.primary)))
            .filter(|(_, value)| !value.is_some_and(|value| value >= threshold))
            .collect();
        ThresholdCheck {
            threshold,
            passed: below.is_empty() && self.primary.errors == 0,
            below,
            errors: self.primary.errors,
        }
    }

    /// Each code's recall and precision against `threshold`.
    pub fn per_code_checks(&self, threshold: f64) -> Vec<CodeCheck> {
        self.per_code
            .iter()
            .map(|(code, counts)| {
                let (recall, precision) = (counts.recall(), counts.precision());
                CodeCheck {
                    code: code.clone(),
                    counts: *counts,
                    recall,
                    precision,
                    meets: [recall, precision]
                        .into_iter()
                        .all(|value| value.is_none_or(|value| value >= threshold)),
                }
            })
            .collect()
    }

    /// The cases with a run judged wrongly or without a judgment.
    pub fn failed_cases(&self) -> impl Iterator<Item = &CaseResult> {
        self.cases.iter().filter(|case| case.failed())
    }
}

/// The review's harness.
pub struct ReviewHarness;

impl Harness for ReviewHarness {
    const ROLE: AgentRole = AgentRole::Review;
    type Case = ReviewCase;
    type Run = ReviewRun;
    type Score = ReviewScore;

    fn read(fields: &Value, codes: &BTreeSet<String>) -> Result<ReviewCase, Vec<ProblemKind>> {
        read(fields, codes)
    }

    fn score(cases: &[Case], runs: &[ReviewRun]) -> ReviewScore {
        score(cases, runs)
    }
}

fn read(value: &Value, list_codes: &BTreeSet<String>) -> Result<ReviewCase, Vec<ProblemKind>> {
    let Some(object) = value.as_object() else {
        return Err(vec![ProblemKind::WrongType {
            field: "review".to_owned(),
            expected: "an object",
        }]);
    };
    let mut fields = Fields::new(object, "review.");
    let input = fields.object("input");
    let expected = fields.object("expected");
    let mut problems = std::mem::take(&mut fields.problems);
    let Some(expected) = expected else {
        return Err(problems);
    };
    let mut fields = Fields::new(expected, "review.expected.");
    let verdict = fields.string("verdict");
    let codes = fields.strings("codes");
    let acceptable = fields.optional_strings("acceptable_codes");
    let note = fields.optional_string("note");
    let near_miss = fields.optional_bool("near_miss");
    problems.extend(fields.problems);
    let verdict = verdict.and_then(|name| {
        let verdict = Judgement::parse(&name);
        if verdict.is_none() {
            problems.push(ProblemKind::UnknownVerdict(name));
        }
        verdict
    });
    let codes: Option<BTreeSet<String>> = codes.map(|codes| codes.into_iter().collect());
    let acceptable: Option<BTreeSet<String>> = acceptable.map(|codes| codes.into_iter().collect());
    if let (Some(verdict), Some(codes)) = (verdict, &codes) {
        match verdict {
            Judgement::Violation if codes.is_empty() => {
                problems.push(ProblemKind::ViolationWithoutCodes);
            }
            Judgement::Clean if !codes.is_empty() => problems.push(ProblemKind::CleanWithCodes),
            _ => {}
        }
    }
    if let (Some(codes), Some(acceptable)) = (&codes, &acceptable) {
        let unknown: Vec<String> = codes
            .union(acceptable)
            .filter(|code| !list_codes.contains(*code))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            problems.push(ProblemKind::UnknownCodes(unknown));
        }
        let both: Vec<String> = codes.intersection(acceptable).cloned().collect();
        if !both.is_empty() {
            problems.push(ProblemKind::ExpectedAndAcceptable(both));
        }
    }
    match (input, verdict, codes, acceptable, note, near_miss) {
        (
            Some(input),
            Some(verdict),
            Some(codes),
            Some(acceptable_codes),
            Some(note),
            Some(near_miss),
        ) if problems.is_empty() => Ok(ReviewCase {
            input: input.clone(),
            expected: ReviewExpected {
                verdict,
                codes,
                acceptable_codes,
                note,
                near_miss,
            },
        }),
        _ => Err(problems),
    }
}

/// Score `runs` on `cases`. A run of a case `cases` does not have, or of
/// a case of another role, is not counted.
pub fn score(cases: &[Case], runs: &[ReviewRun]) -> ReviewScore {
    let expected: BTreeMap<&str, (&ReviewExpected, bool)> = cases
        .iter()
        .filter_map(|case| {
            let review = case.review()?;
            Some((case.id.as_str(), (&review.expected, case.is_disputed())))
        })
        .collect();
    let counted: Vec<(&ReviewRun, &ReviewExpected, bool)> = runs
        .iter()
        .filter_map(|run| {
            let (expected, disputed) = expected.get(run.case.as_str())?;
            Some((run, *expected, *disputed))
        })
        .collect();
    let primary_runs: Vec<(&ReviewRun, &ReviewExpected)> = counted
        .iter()
        .filter(|(_, _, disputed)| !disputed)
        .map(|(run, expected, _)| (*run, *expected))
        .collect();
    let all_runs: Vec<(&ReviewRun, &ReviewExpected)> = counted
        .iter()
        .map(|(run, expected, _)| (*run, *expected))
        .collect();
    let primary_expected = expected
        .values()
        .filter(|(_, disputed)| !disputed)
        .map(|(expected, _)| *expected);
    let (primary, per_code) = metrics(primary_expected, &primary_runs);
    let any_disputed = expected.values().any(|(_, disputed)| *disputed);
    let with_disputed = any_disputed
        .then(|| metrics(expected.values().map(|(expected, _)| *expected), &all_runs).0);
    let cases = cases
        .iter()
        .filter_map(|case| {
            let review = case.review()?;
            Some(case_result(case, &review.expected, runs))
        })
        .collect();
    ReviewScore {
        primary,
        per_code,
        with_disputed,
        cases,
    }
}

fn metrics<'a>(
    cases: impl Iterator<Item = &'a ReviewExpected>,
    runs: &[(&ReviewRun, &ReviewExpected)],
) -> (Metrics, PerCode<Counts>) {
    let judged: Vec<(&ReviewOutcome, &ReviewExpected)> = runs
        .iter()
        .filter_map(|(run, expected)| Some((run.outcome.as_ref()?, *expected)))
        .collect();
    let mut verdict = Counts::default();
    let mut true_negatives = 0;
    for (outcome, expected) in &judged {
        match (expected.verdict, outcome.judgement()) {
            (Judgement::Violation, Judgement::Violation) => verdict.true_positives += 1,
            (Judgement::Violation, Judgement::Clean) => verdict.false_negatives += 1,
            (Judgement::Clean, Judgement::Violation) => verdict.false_positives += 1,
            (Judgement::Clean, Judgement::Clean) => true_negatives += 1,
        }
    }
    let mut per_code: PerCode<Counts> = cases
        .flat_map(|expected| expected.codes.iter())
        .chain(judged.iter().flat_map(|(outcome, _)| outcome.codes.iter()))
        .map(|code| (code.clone(), Counts::default()))
        .collect();
    for (code, counts) in &mut per_code {
        for (outcome, expected) in &judged {
            let named = outcome.codes.contains(code);
            if expected.codes.contains(code) {
                if named {
                    counts.true_positives += 1;
                } else {
                    counts.false_negatives += 1;
                }
            } else if named && !expected.acceptable_codes.contains(code) {
                counts.false_positives += 1;
            }
        }
    }
    let mut codes = Counts::default();
    for counts in per_code.values() {
        codes.add(*counts);
    }
    let metrics = Metrics {
        verdict,
        true_negatives,
        codes,
        runs: runs.len(),
        errors: runs.len() - judged.len(),
    };
    (metrics, per_code)
}

fn case_result(case: &Case, expected: &ReviewExpected, runs: &[ReviewRun]) -> CaseResult {
    let runs: Vec<&ReviewRun> = runs.iter().filter(|run| run.case == case.id).collect();
    let judged: Vec<&ReviewOutcome> = runs.iter().filter_map(|run| run.outcome.as_ref()).collect();
    CaseResult {
        id: case.id.clone(),
        disputed: case.is_disputed(),
        runs: runs.len(),
        correct: judged
            .iter()
            .filter(|outcome| outcome.judgement() == expected.verdict)
            .count(),
        errors: runs.len() - judged.len(),
        codes_exact: judged
            .iter()
            .filter(|outcome| {
                expected.codes.is_subset(&outcome.codes)
                    && outcome.codes.iter().all(|code| {
                        expected.codes.contains(code) || expected.acceptable_codes.contains(code)
                    })
            })
            .count(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::*;
    use crate::domain::agent_eval::{DEFAULT_THRESHOLD, RoleCase, Source, read_case_file};

    fn case(id: &str, verdict: Judgement, codes: &[&str], acceptable: &[&str]) -> Case {
        Case {
            id: id.to_owned(),
            source: Source::Generated,
            made_by: "test".to_owned(),
            base_commit: "0".repeat(40),
            patch: "a".repeat(64),
            adjudicated: None,
            disputed: None,
            k: None,
            role: RoleCase::Review(ReviewCase {
                input: Default::default(),
                expected: ReviewExpected {
                    verdict,
                    codes: codes.iter().map(|c| (*c).to_owned()).collect(),
                    acceptable_codes: acceptable.iter().map(|c| (*c).to_owned()).collect(),
                    note: String::new(),
                    near_miss: false,
                },
            }),
        }
    }

    fn run(case: &str, round: u32, verdict: ReviewDecision, codes: &[&str]) -> ReviewRun {
        ReviewRun {
            case: case.to_owned(),
            round,
            outcome: Some(ReviewOutcome {
                verdict,
                codes: codes.iter().map(|c| (*c).to_owned()).collect(),
            }),
        }
    }

    #[test]
    fn revise_and_concern_are_violations_and_pass_is_clean() {
        assert_eq!(Judgement::of(ReviewDecision::Revise), Judgement::Violation);
        assert_eq!(Judgement::of(ReviewDecision::Concern), Judgement::Violation);
        assert_eq!(Judgement::of(ReviewDecision::Pass), Judgement::Clean);
    }

    #[test]
    fn a_result_gives_the_codes_of_its_reasons_only_for_a_violation() {
        let revise = json!({
            "status": "completed", "verdict": "revise",
            "reasons": [{"text": "A-999 in the text", "codes": ["A-150", " A-152 "]},
                        {"text": "x", "codes": "A-163"}, "a text alone"],
            "summary": "A-155 did not apply",
        });
        let outcome = ReviewOutcome::from_agent_result(&revise).unwrap();
        assert_eq!(
            outcome.codes,
            BTreeSet::from(["A-150", "A-152", "A-163"].map(str::to_owned))
        );
        let pass = json!({"status": "completed", "verdict": "pass",
                          "reasons": [{"text": "x", "codes": ["A-150"]}]});
        assert!(
            ReviewOutcome::from_agent_result(&pass)
                .unwrap()
                .codes
                .is_empty()
        );
        for no_judgment in [
            json!({"status": "failed", "verdict": "pass"}),
            json!({"status": "completed"}),
            json!({"status": "completed", "verdict": "maybe"}),
        ] {
            assert_eq!(ReviewOutcome::from_agent_result(&no_judgment), None);
        }
    }

    #[test]
    fn verdict_and_codes_count_per_run_and_acceptable_codes_are_no_false_positive() {
        let cases = [
            case("v", Judgement::Violation, &["A-150"], &["A-152"]),
            case("c", Judgement::Clean, &[], &[]),
        ];
        let runs = [
            run("v", 0, ReviewDecision::Revise, &["A-150", "A-152"]),
            run("v", 1, ReviewDecision::Concern, &["A-163"]),
            run("v", 2, ReviewDecision::Pass, &[]),
            run("c", 0, ReviewDecision::Pass, &[]),
            run("c", 1, ReviewDecision::Revise, &["A-150"]),
            ReviewRun {
                case: "c".to_owned(),
                round: 2,
                outcome: None,
            },
            run("unknown", 0, ReviewDecision::Revise, &["A-150"]),
        ];
        let score = score(&cases, &runs);

        let p = score.primary;
        assert_eq!(
            (
                p.verdict.true_positives,
                p.verdict.false_positives,
                p.verdict.false_negatives,
                p.true_negatives
            ),
            (2, 1, 1, 1)
        );
        assert_eq!((p.runs, p.errors), (6, 1));
        // A-150: found once (v#0), missed twice (v#1, v#2), named wrongly on c#1.
        // A-152: acceptable on v, so not a false positive. A-163: wrong.
        assert_eq!(
            score.per_code["A-150"],
            Counts {
                true_positives: 1,
                false_positives: 1,
                false_negatives: 2
            }
        );
        assert_eq!(score.per_code["A-152"], Counts::default());
        assert_eq!(score.per_code["A-163"].false_positives, 1);
        assert_eq!(
            p.codes,
            Counts {
                true_positives: 1,
                false_positives: 2,
                false_negatives: 2
            }
        );
        assert_eq!(p.verdict.recall(), Some(2.0 / 3.0));
        assert_eq!(p.codes.precision(), Some(1.0 / 3.0));
        assert_eq!(score.with_disputed, None);
        let v = &score.cases[0];
        assert_eq!((v.runs, v.correct, v.codes_exact), (3, 2, 1));
        assert_eq!(
            score
                .failed_cases()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            ["v", "c"]
        );
    }

    #[test]
    fn a_disputed_case_is_left_out_of_the_primary_scores() {
        let mut disputed = case("d", Judgement::Clean, &[], &[]);
        disputed.disputed = Some(json!({"reason": "ambiguous"}));
        let cases = [case("v", Judgement::Violation, &["A-150"], &[]), disputed];
        let runs = [
            run("v", 0, ReviewDecision::Revise, &["A-150"]),
            run("d", 0, ReviewDecision::Revise, &["A-152"]),
        ];
        let score = score(&cases, &runs);

        assert_eq!(score.primary.verdict.precision(), Some(1.0));
        assert_eq!(score.primary.runs, 1);
        assert!(!score.per_code.contains_key("A-152"));
        let all = score.with_disputed.unwrap();
        assert_eq!(all.verdict.precision(), Some(0.5));
        assert_eq!(all.codes.false_positives, 1);
        assert!(score.cases[1].disputed);
        assert!(score.check(DEFAULT_THRESHOLD).passed);
    }

    #[test]
    fn the_threshold_needs_all_four_scores_and_no_errors() {
        let cases = [
            case("v", Judgement::Violation, &["A-150"], &[]),
            case("c", Judgement::Clean, &[], &[]),
        ];
        let mut runs: Vec<ReviewRun> = (0..9)
            .map(|round| run("v", round, ReviewDecision::Revise, &["A-150"]))
            .collect();
        runs.push(run("v", 9, ReviewDecision::Pass, &[]));
        runs.push(run("c", 0, ReviewDecision::Pass, &[]));
        let at_threshold = score(&cases, &runs).check(0.9);
        assert!(at_threshold.passed, "{at_threshold:?}");

        runs.push(run("c", 1, ReviewDecision::Revise, &["A-150", "A-163"]));
        let below = score(&cases, &runs).check(0.9);
        assert!(!below.passed);
        assert_eq!(
            below
                .below
                .iter()
                .map(|(metric, _)| metric.as_str())
                .collect::<Vec<_>>(),
            ["codes_precision"]
        );

        let only_clean = [case("c", Judgement::Clean, &[], &[])];
        let none = score(&only_clean, &[run("c", 0, ReviewDecision::Pass, &[])]).check(0.9);
        assert!(!none.passed, "a score without a value does not reach it");
        assert_eq!(none.below.len(), 4);

        let errors = score(
            &cases,
            &[
                run("v", 0, ReviewDecision::Revise, &["A-150"]),
                ReviewRun {
                    case: "c".to_owned(),
                    round: 0,
                    outcome: None,
                },
            ],
        )
        .check(0.9);
        assert!(errors.below.is_empty() && !errors.passed);
        assert_eq!(errors.errors, 1);
    }

    #[test]
    fn per_code_values_are_compared_with_the_threshold_too() {
        let cases = [
            case("a", Judgement::Violation, &["A-150"], &[]),
            case("b", Judgement::Violation, &["A-152"], &[]),
        ];
        let runs = [
            run("a", 0, ReviewDecision::Revise, &["A-150"]),
            run("b", 0, ReviewDecision::Revise, &["A-150"]),
        ];
        let checks = score(&cases, &runs).per_code_checks(0.9);
        let meets: Vec<(&str, bool)> = checks
            .iter()
            .map(|check| (check.code.as_str(), check.meets))
            .collect();
        assert_eq!(meets, [("A-150", false), ("A-152", false)]);
        assert_eq!(checks[0].precision, Some(0.5));
        assert_eq!(checks[1].recall, Some(0.0));
        assert_eq!(checks[1].precision, None);
    }

    /// The Spike's result `results/adr-rules/20261005T044010Z-0c00dde92d16-
    /// production-baseline-iter0.json` (dagq-agent-eval commit cfb8a4c):
    /// its cases' expectations and disputes in this shape, and each run's
    /// agent result with only the codes of its reasons kept. The values are
    /// the result's `metrics`.
    #[test]
    fn the_spikes_production_baseline_scores_the_same() {
        let text = include_str!("fixtures/spike-production-baseline-iter0.cases.json");
        let value: Value = serde_json::from_str(text).unwrap();
        let patches: BTreeSet<String> = value["cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|case| case["patch"].as_str().unwrap().to_owned())
            .collect();
        let file = read_case_file("adr-rules", "production.json", text, &patches).unwrap();
        let printed: Vec<Value> = serde_json::from_str(include_str!(
            "fixtures/spike-production-baseline-iter0.runs.json"
        ))
        .unwrap();
        let runs: Vec<ReviewRun> = printed
            .iter()
            .map(|run| ReviewRun {
                case: run["case"].as_str().unwrap().to_owned(),
                round: run["round"].as_u64().unwrap() as u32,
                outcome: ReviewOutcome::from_agent_result(&run["agent"]),
            })
            .collect();

        let score = ReviewHarness::score(&file.cases, &runs);

        let counts = |tp, fp, fn_| Counts {
            true_positives: tp,
            false_positives: fp,
            false_negatives: fn_,
        };
        assert_eq!(score.primary.verdict, counts(12, 0, 3));
        assert_eq!(score.primary.true_negatives, 11);
        assert_eq!(score.primary.codes, counts(13, 21, 5));
        assert_eq!(score.primary.errors, 0);
        assert_eq!(score.primary.verdict.recall(), Some(0.8));
        assert_eq!(score.primary.verdict.precision(), Some(1.0));
        assert_eq!(score.primary.codes.recall(), Some(0.7222222222222222));
        assert_eq!(score.primary.codes.precision(), Some(0.38235294117647056));
        let per_code: Vec<(&str, (usize, usize, usize))> = score
            .per_code
            .iter()
            .map(|(code, c)| {
                (
                    code.as_str(),
                    (c.true_positives, c.false_positives, c.false_negatives),
                )
            })
            .collect();
        assert_eq!(
            per_code,
            [
                ("A-150", (5, 2, 0)),
                ("A-151", (0, 3, 0)),
                ("A-152", (2, 2, 0)),
                ("A-155", (1, 2, 3)),
                ("A-156", (0, 2, 0)),
                ("A-159", (0, 2, 0)),
                ("A-160", (0, 1, 0)),
                ("A-161", (0, 1, 0)),
                ("A-162", (3, 5, 2)),
                ("A-163", (2, 1, 0)),
            ]
        );
        let all = score.with_disputed.unwrap();
        assert_eq!(all.verdict, counts(12, 2, 3));
        assert_eq!(all.codes, counts(13, 29, 5));
        let mut failed: Vec<&str> = score.failed_cases().map(|case| case.id.as_str()).collect();
        failed.sort_unstable();
        assert_eq!(
            failed,
            [
                "prod-adr-rules-t1232-a1",
                "prod-adr-rules-t1361-a1",
                "prod-adr-rules-t1529-a1",
                "prod-adr-rules-t1596-a1",
                "prod-adr-rules-t1632-a2",
            ]
        );
        let check = score.check(DEFAULT_THRESHOLD);
        assert!(!check.passed);
        assert_eq!(
            check
                .below
                .iter()
                .map(|(metric, _)| *metric)
                .collect::<Vec<_>>(),
            [
                Metric::VerdictRecall,
                Metric::CodesRecall,
                Metric::CodesPrecision
            ]
        );
    }
}
