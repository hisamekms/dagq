//! The review's judgment of an `already_satisfied` claim (ADR-t1818-1
//! decisions 3 and 4): the verdict read through its own entry, which keeps
//! the confidence of a `pass` as well (a run's review drops it, see
//! [`super::ReviewVerdict`]), and what the runtime does with it against
//! the snapshot and main: close, verify and review again on a newer head,
//! or ask a person.

use serde::Deserialize;

use super::{
    AskConfidence, CommitSha, ReviewDecision, concern, parse_json_object, review_reason,
    satisfied::SatisfiedAnswer,
};

/// How many times one run's judgment is made again on a newer head after
/// a `high` pass found main moved past its snapshot (ADR-t1818-1 decision
/// 3, which leaves this small limit to the design). Past it the claim goes
/// to a person instead of being closed.
///
/// Two: each new judgment first runs the task's whole verification, the
/// coverage gate included, on the new head, and main lands several runs an
/// hour, so a single chance would often end in an ask only because main
/// moved during one verification, while a third would mostly spend another
/// full verification and review on a race the runtime keeps losing. A
/// person then decides with the review's reasons in front of them.
pub const MAX_JUDGMENT_RETRIES: usize = 2;

/// The review's judgment of a claim, as the job printed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimJudgment {
    pub verdict: ReviewDecision,
    /// How sure the job is, whatever the verdict; `None` when it gave none
    /// or a value this binary does not know, which counts as not `high`.
    pub confidence: Option<AskConfidence>,
    /// The `approve_close` option the job recommends; `None` when it gave
    /// none or one that is not an option.
    pub recommendation: Option<SatisfiedAnswer>,
    pub reasons: Vec<String>,
    pub summary: String,
}

/// What the judgment's job prints: `verdict`, `reasons` and `summary` are
/// required, the rest optional, and a field it does not know is ignored.
/// `confidence` and `recommendation` are read as any value, so one that is
/// not a text reads as unknown rather than failing the judgment.
#[derive(Deserialize)]
struct PrintedJudgment {
    verdict: ReviewDecision,
    reasons: Vec<review_reason::PrintedReason>,
    summary: String,
    #[serde(default)]
    confidence: serde_json::Value,
    #[serde(default)]
    recommendation: serde_json::Value,
}

impl ClaimJudgment {
    /// The judgment in the job's stdout: the whole text, or else the
    /// outermost `{...}` in it. An unknown verdict, a missing required
    /// field or no JSON at all fails to read.
    pub fn parse(stdout: &str) -> Result<Self, String> {
        let printed = parse_json_object::<PrintedJudgment>(stdout)
            .map_err(|error| format!("the claim's review printed no judgment JSON: {error}"))?;
        Ok(Self {
            verdict: printed.verdict,
            confidence: concern::known(printed.confidence.as_str()),
            recommendation: printed
                .recommendation
                .as_str()
                .and_then(SatisfiedAnswer::parse),
            reasons: review_reason::split(printed.reasons).0,
            summary: printed.summary,
        })
    }
}

/// What the runtime does with a judgment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgmentStep {
    /// Close the task as `completed` without a commit.
    Close,
    /// Verify on `head`, main's head now, as the new snapshot, and review
    /// the claim again there.
    Rejudge { head: CommitSha },
    /// Ask a person in `approve_close`.
    Ask(JudgmentAsk),
}

/// Why a judgment went to a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskBecause {
    /// Not a `high` pass: a `low` or unknown confidence, a `revise` or a
    /// `concern`.
    NotHighPass,
    /// A `high` pass on a snapshot main moved past, with no new judgment
    /// left ([`MAX_JUDGMENT_RETRIES`]).
    RetryLimit,
}

/// The ask a judgment opens, with what the review gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgmentAsk {
    pub because: AskBecause,
    pub verdict: ReviewDecision,
    pub confidence: Option<AskConfidence>,
    pub recommendation: Option<SatisfiedAnswer>,
    pub reasons: Vec<String>,
    pub summary: String,
}

/// Decide a `judgment` made on `snapshot`, main being at `main` when the
/// claim would close, after `retries` new judgments of this run out of
/// `limit`. Only a `high` pass closes, and only while main is still at the
/// snapshot; main having moved, it is judged again on main's head while a
/// new judgment is left, and asked of a person past them. Anything else
/// asks a person.
pub fn decide(
    judgment: &ClaimJudgment,
    snapshot: &CommitSha,
    main: &CommitSha,
    retries: usize,
    limit: usize,
) -> JudgmentStep {
    let high_pass = judgment.verdict == ReviewDecision::Pass
        && judgment.confidence == Some(AskConfidence::High);
    if !high_pass {
        return JudgmentStep::Ask(ask(judgment, AskBecause::NotHighPass));
    }
    if snapshot == main {
        JudgmentStep::Close
    } else if retries < limit {
        JudgmentStep::Rejudge { head: main.clone() }
    } else {
        JudgmentStep::Ask(ask(judgment, AskBecause::RetryLimit))
    }
}

fn ask(judgment: &ClaimJudgment, because: AskBecause) -> JudgmentAsk {
    JudgmentAsk {
        because,
        verdict: judgment.verdict,
        confidence: judgment.confidence,
        recommendation: judgment.recommendation,
        reasons: judgment.reasons.clone(),
        summary: judgment.summary.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(c: char) -> CommitSha {
        CommitSha::parse(c.to_string().repeat(40), "test").unwrap()
    }

    fn judgment(verdict: &str, confidence: Option<&str>) -> ClaimJudgment {
        let confidence = confidence.map_or(String::new(), |c| format!(r#","confidence":"{c}""#));
        ClaimJudgment::parse(&format!(
            r#"{{"verdict":"{verdict}","reasons":["r1"],"summary":"s","recommendation":"retry"{confidence}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn every_verdict_keeps_its_confidence() {
        for verdict in ["pass", "revise", "concern"] {
            let read = judgment(verdict, Some("high"));
            assert_eq!(read.verdict.as_str(), verdict);
            assert_eq!(read.confidence, Some(AskConfidence::High), "{verdict}");
            assert_eq!(read.recommendation, Some(SatisfiedAnswer::Retry));
            assert_eq!(read.reasons, ["r1"]);
            assert_eq!(read.summary, "s");
        }
        assert_eq!(
            judgment("pass", Some("low")).confidence,
            Some(AskConfidence::Low)
        );
    }

    #[test]
    fn a_runs_review_still_drops_the_confidence_of_a_pass() {
        let stdout = r#"{"verdict":"pass","reasons":[],"summary":"s","confidence":"high"}"#;
        assert_eq!(
            super::super::ReviewVerdict::parse(stdout)
                .unwrap()
                .confidence,
            None
        );
        assert_eq!(
            ClaimJudgment::parse(stdout).unwrap().confidence,
            Some(AskConfidence::High)
        );
    }

    #[test]
    fn a_missing_or_unknown_confidence_reads_as_none() {
        assert_eq!(judgment("pass", None).confidence, None);
        assert_eq!(judgment("pass", Some("medium")).confidence, None);
        assert_eq!(judgment("pass", Some(" ")).confidence, None);
        let numeric = ClaimJudgment::parse(
            r#"{"verdict":"pass","reasons":[],"summary":"s","confidence":0.9,"recommendation":1}"#,
        )
        .unwrap();
        assert_eq!((numeric.confidence, numeric.recommendation), (None, None));
    }

    #[test]
    fn coded_reasons_keep_their_texts_and_an_unknown_recommendation_reads_as_none() {
        let read = ClaimJudgment::parse(
            r#"Here: ```json
            {"verdict":"revise","reasons":[{"text":"t","codes":["acceptance_unmet"]}],
             "summary":"s","recommendation":"land","extra":1}
            ```"#,
        )
        .unwrap();
        assert_eq!(read.reasons, ["t"]);
        assert_eq!(read.recommendation, None);
    }

    #[test]
    fn an_unreadable_judgment_fails() {
        for bad in [
            r#"{"verdict":"maybe","reasons":[],"summary":"s"}"#,
            r#"{"reasons":[],"summary":"s"}"#,
            r#"{"verdict":"pass","summary":"s"}"#,
            r#"{"verdict":"pass","reasons":[]}"#,
            "not json at all",
        ] {
            let error = ClaimJudgment::parse(bad).unwrap_err();
            assert!(error.contains("no judgment JSON"), "{bad}: {error}");
        }
    }

    #[test]
    fn a_high_pass_on_the_snapshot_closes_whatever_judgments_were_made_again() {
        let high = judgment("pass", Some("high"));
        for retries in [0, MAX_JUDGMENT_RETRIES, MAX_JUDGMENT_RETRIES + 1] {
            assert_eq!(
                decide(&high, &sha('a'), &sha('a'), retries, MAX_JUDGMENT_RETRIES),
                JudgmentStep::Close,
                "{retries}"
            );
        }
    }

    #[test]
    fn a_high_pass_main_moved_past_is_judged_again_until_the_limit() {
        let high = judgment("pass", Some("high"));
        for retries in 0..MAX_JUDGMENT_RETRIES {
            assert_eq!(
                decide(&high, &sha('a'), &sha('b'), retries, MAX_JUDGMENT_RETRIES),
                JudgmentStep::Rejudge { head: sha('b') },
                "{retries}"
            );
        }
        let JudgmentStep::Ask(ask) = decide(
            &high,
            &sha('a'),
            &sha('b'),
            MAX_JUDGMENT_RETRIES,
            MAX_JUDGMENT_RETRIES,
        ) else {
            panic!("past the limit asks");
        };
        assert_eq!(ask.because, AskBecause::RetryLimit);
        assert_eq!(ask.confidence, Some(AskConfidence::High));
        assert_eq!(ask.reasons, ["r1"]);
    }

    #[test]
    fn anything_but_a_high_pass_asks_with_what_the_review_gave() {
        for (verdict, confidence, expected) in [
            ("pass", Some("low"), Some(AskConfidence::Low)),
            ("pass", None, None),
            ("pass", Some("medium"), None),
            ("revise", Some("high"), Some(AskConfidence::High)),
            ("concern", Some("high"), Some(AskConfidence::High)),
        ] {
            let read = judgment(verdict, confidence);
            for main in [sha('a'), sha('b')] {
                let JudgmentStep::Ask(ask) =
                    decide(&read, &sha('a'), &main, 0, MAX_JUDGMENT_RETRIES)
                else {
                    panic!("{verdict} {confidence:?} asks");
                };
                assert_eq!(
                    ask,
                    JudgmentAsk {
                        because: AskBecause::NotHighPass,
                        verdict: read.verdict,
                        confidence: expected,
                        recommendation: Some(SatisfiedAnswer::Retry),
                        reasons: vec!["r1".into()],
                        summary: "s".into(),
                    }
                );
            }
        }
    }
}
