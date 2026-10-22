//! The review's judgment of an `already_satisfied` claim (ADR-t1818-1
//! decisions 3 and 4): the verdict read through its own entry, which keeps
//! the confidence of a `pass` as well (a run's review drops it, see
//! [`super::ReviewVerdict`]), and what the runtime does with it against
//! the snapshot and main: close, verify and review again on a newer head,
//! or ask a person.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    AskConfidence, CommitSha, EventKind, ReviewDecision, RunEvent, concern, parse_json_object,
    receipt::BaseCheck,
    review_reason,
    satisfied::{self, SatisfiedAnswer, SnapshotVerification, Verified},
};

/// How many reasons of a judgment its `approve_close` ask names, the first
/// ones, each within [`ASK_REASON_BYTES`], and its summary within
/// [`ASK_SUMMARY_BYTES`]: the ask is read whole by a person, so a job that
/// printed much is cut, and its `run_satisfied_judged` keeps it all.
pub const ASK_REASONS: usize = 10;
pub const ASK_REASON_BYTES: usize = 600;
pub const ASK_SUMMARY_BYTES: usize = 1_000;

/// How many times one run's judgment is made again on a newer head after
/// a `high` pass found main moved past its snapshot (ADR-t1818-1 decision
/// 3, which leaves this small limit to the design). Past it the claim goes
/// to a person instead of being closed.
///
/// Two: each new judgment first runs the task's verification, as the
/// landing runs it, on the new head, and main lands several runs an
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

/// The `step` of a judgment made again on a newer head.
const REJUDGE: &str = "rejudge";

impl JudgmentStep {
    /// The `step` of its `run_satisfied_judged`.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Close => "close",
            Self::Rejudge { .. } => REJUDGE,
            Self::Ask(_) => "ask",
        }
    }
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

impl AskBecause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotHighPass => "not_high_pass",
            Self::RetryLimit => "retry_limit",
        }
    }
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

/// How many times a run's claim was judged again on a newer head after a
/// `high` pass found main moved past its snapshot, by its `events` (the
/// `run_satisfied_judged` whose `step` is `rejudge`): the `retries` of
/// [`decide`]. A judgment made again because its supervisor stopped (the
/// next one validates, verifies and judges the claim afresh) is no retry.
pub fn rejudged(events: &[RunEvent]) -> usize {
    events
        .iter()
        .filter(|e| e.kind == EventKind::RunSatisfiedJudged && e.payload["step"] == REJUDGE)
        .count()
}

/// The payload of `run_satisfied_judged` for the judgment `attempt` on the
/// snapshot `head`: what the job printed with the step the runtime took on
/// it, or why it gave none (`error`).
pub fn judged_payload(
    attempt: usize,
    head: &CommitSha,
    judged: Result<(&ClaimJudgment, &JudgmentStep), &str>,
) -> Value {
    match judged {
        Ok((judgment, step)) => {
            let mut payload = json!({
                "attempt": attempt,
                "head": head,
                "verdict": judgment.verdict.as_str(),
                "confidence": judgment.confidence.map(|c| c.as_str().to_owned()),
                "recommendation": judgment.recommendation.map(SatisfiedAnswer::as_str),
                "reasons": judgment.reasons,
                "summary": judgment.summary,
                "step": step.as_str(),
            });
            match step {
                JudgmentStep::Close => {}
                JudgmentStep::Rejudge { head } => payload["main"] = json!(head),
                JudgmentStep::Ask(ask) => payload["because"] = json!(ask.because.as_str()),
            }
            payload
        }
        Err(error) => json!({"attempt": attempt, "head": head, "error": error}),
    }
}

/// The payload of `run_satisfied_closed` for a claim the review's
/// judgment `attempt` closed (`closed_by` `judgment`), a `high` pass on the
/// head of `verified` while main was still there: the head, the
/// verification, the verdict and confidence, and the receipt's grounds
/// (its summary and `base_checks`). No ask was answered.
pub fn closed_payload(
    verified: &Verified,
    attempt: usize,
    judgment: &ClaimJudgment,
    (summary, base_checks): (&str, &[BaseCheck]),
) -> Value {
    json!({
        "head": verified.head,
        "verified_event": verified.event,
        "closed_by": "judgment",
        "attempt": attempt,
        "verdict": judgment.verdict.as_str(),
        "confidence": judgment.confidence.map(|c| c.as_str().to_owned()),
        "receipt_summary": summary,
        "base_checks": base_checks,
    })
}

/// The question of the `approve_close` ask a judgment opens: the claim and
/// its verification as [`satisfied::question`] gives them, then what the
/// review judged on the same head, its verdict, confidence, recommendation,
/// summary and reasons (cut to [`ASK_REASONS`], [`ASK_REASON_BYTES`] and
/// [`ASK_SUMMARY_BYTES`]), and why the runtime did not close the task on
/// it (`because`): it is no `high` pass, or main kept moving past the
/// snapshot while the claim was judged again. The person decides.
pub fn judged_question(
    task: &str,
    summary: &str,
    verification: &SnapshotVerification,
    (judgment, because): (&ClaimJudgment, AskBecause),
) -> String {
    let mut reasons = String::new();
    for (index, reason) in judgment.reasons.iter().take(ASK_REASONS).enumerate() {
        reasons.push_str(&format!(
            "\n{}. {}",
            index + 1,
            clip(reason, ASK_REASON_BYTES)
        ));
    }
    if judgment.reasons.len() > ASK_REASONS {
        reasons.push_str(&format!(
            "\n({} more reasons are in the run's run_satisfied_judged event.)",
            judgment.reasons.len() - ASK_REASONS
        ));
    }
    format!(
        "{}\nThe review judged the claim on the same head {}: {} with {} confidence, recommending {}. Its summary: {}{}{}\n{} you decide.",
        satisfied::question(task, summary, verification, None),
        verification.head,
        judgment.verdict.as_str(),
        judgment.confidence.map_or("no known", |c| c.as_str()),
        judgment
            .recommendation
            .map_or("nothing", SatisfiedAnswer::as_str),
        clip(judgment.summary.trim(), ASK_SUMMARY_BYTES),
        if reasons.is_empty() {
            ""
        } else {
            "\nIts reasons:"
        },
        reasons,
        match because {
            AskBecause::NotHighPass =>
                "The runtime closes the task by itself only on a pass with high confidence, so it does not close it on this review:",
            AskBecause::RetryLimit =>
                "Main moved past the snapshot after a pass with high confidence each time the claim was verified and judged again, up to the limit, so the runtime does not close the task on it:",
        },
    )
}

/// The question of the `approve_close` ask of a claim whose judgment's
/// job gave no judgment (`error`, cut to [`ASK_SUMMARY_BYTES`]), its one
/// run once more included: the verification passed, so `close` is still
/// offered.
pub fn unjudged_question(
    task: &str,
    summary: &str,
    verification: &SnapshotVerification,
    error: &str,
) -> String {
    format!(
        "{}\nThe review of the claim on the same head {} gave no judgment, its one run once more included: {}",
        satisfied::question(task, summary, verification, None),
        verification.head,
        clip(error.trim(), ASK_SUMMARY_BYTES),
    )
}

/// The question of the `approve_close` ask of a claim whose judgment's
/// job never started because no provider of the reviews can run it
/// (`error`, why, cut to [`ASK_SUMMARY_BYTES`]): nothing judged the claim,
/// and the verification passed, so `close` is still offered.
pub fn unstarted_question(
    task: &str,
    summary: &str,
    verification: &SnapshotVerification,
    error: &str,
) -> String {
    format!(
        "{}\nThe review did not judge the claim on the same head {}: no provider of the reviews can run its job: {}",
        satisfied::question(task, summary, verification, None),
        verification.head,
        clip(error.trim(), ASK_SUMMARY_BYTES),
    )
}

/// `text` within `max` bytes, cut at a character's boundary with `…`.
fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
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
    use crate::domain::event_kind;

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

    fn verification() -> SnapshotVerification {
        SnapshotVerification {
            head: sha('a'),
            commands: 2,
            failure: None,
        }
    }

    /// The ask of a judgment names the head, the verdict, the confidence,
    /// the recommendation, the summary and the reasons, cut to their
    /// limits, and says the person decides; the payload keeps them whole.
    #[test]
    fn a_judged_ask_says_what_the_review_found_on_the_head_and_its_event_keeps_it_all() {
        let mut read = judgment("pass", Some("high"));
        read.reasons = (0..=ASK_REASONS).map(|i| format!("reason {i}")).collect();
        read.reasons[0] = "é".repeat(ASK_REASON_BYTES);
        let question = judged_question(
            "7",
            "the base meets it",
            &verification(),
            (&read, AskBecause::RetryLimit),
        );
        for said in [
            sha('a').as_str(),
            "pass with high confidence, recommending retry",
            "Its summary: s",
            "\n2. reason 1",
            "(1 more reasons",
            "you decide",
            "the base meets it",
            "Main moved past the snapshot",
        ] {
            assert!(question.contains(said), "{said}: {question}");
        }
        assert!(
            !question.contains(&format!("reason {ASK_REASONS}")),
            "{question}"
        );
        let first = question
            .split("\n1. ")
            .nth(1)
            .unwrap()
            .split('\n')
            .next()
            .unwrap();
        assert!(
            first.len() <= ASK_REASON_BYTES + '…'.len_utf8(),
            "{}",
            first.len()
        );
        let none = judgment("revise", None);
        let question = judged_question("7", "", &verification(), (&none, AskBecause::NotHighPass));
        assert!(
            question.contains("revise with no known confidence")
                && question.contains("only on a pass with high confidence"),
            "{question}"
        );
        let step = JudgmentStep::Ask(ask(&read, AskBecause::RetryLimit));
        let payload = judged_payload(2, &sha('a'), Ok((&read, &step)));
        assert_eq!(payload["step"], "ask");
        assert_eq!(payload["because"], "retry_limit");
        assert_eq!(payload["attempt"], 2);
        assert_eq!(payload["head"], json!(sha('a')));
        assert_eq!(payload["verdict"], "pass");
        assert_eq!(payload["confidence"], "high");
        assert_eq!(payload["recommendation"], "retry");
        assert_eq!(
            payload["reasons"].as_array().unwrap().len(),
            ASK_REASONS + 1
        );
        assert_eq!(payload["summary"], "s");
        assert_eq!(
            judged_payload(1, &sha('a'), Err("no JSON")),
            json!({"attempt": 1, "head": sha('a'), "error": "no JSON"})
        );
        let question = unjudged_question("7", "", &verification(), "no JSON");
        assert!(
            question.contains("gave no judgment")
                && question.contains("once more included")
                && question.contains("no JSON"),
            "{question}"
        );
        // A job that never started: no judgment given, nothing run again.
        let question = unstarted_question("7", "", &verification(), "disabled by --no-claude");
        assert!(
            question.contains("did not judge the claim")
                && question.contains("no provider of the reviews")
                && question.contains("disabled by --no-claude"),
            "{question}"
        );
        assert!(
            !question.contains("gave no judgment") && !question.contains("once more included"),
            "{question}"
        );
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

    /// A run's retries are its judgments the runtime made again on a newer
    /// head, read back from their `step`: not a close, an ask, a judgment
    /// that gave none, or another event.
    #[test]
    fn only_a_judgment_made_again_on_a_newer_head_counts_as_a_retry() {
        let high = judgment("pass", Some("high"));
        let low = judgment("pass", Some("low"));
        let event = |id: i64, kind: &str, payload: Value| RunEvent {
            id: super::super::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        };
        let judged = |id: i64, step: &JudgmentStep| {
            event(
                id,
                event_kind::RUN_SATISFIED_JUDGED,
                judged_payload(1, &sha('a'), Ok((&high, step))),
            )
        };
        let rejudge = decide(&high, &sha('a'), &sha('b'), 0, MAX_JUDGMENT_RETRIES);
        let rejudged_payload = judged(1, &rejudge).payload;
        assert_eq!(rejudged_payload["step"], "rejudge");
        assert_eq!(rejudged_payload["main"], json!(sha('b')));
        assert_eq!(judged(1, &JudgmentStep::Close).payload["step"], "close");
        let events = [
            judged(1, &rejudge),
            judged(2, &JudgmentStep::Close),
            judged(3, &decide(&low, &sha('a'), &sha('a'), 0, 2)),
            event(
                4,
                event_kind::RUN_SATISFIED_JUDGED,
                judged_payload(4, &sha('a'), Err("no JSON")),
            ),
            event(
                5,
                event_kind::RUN_SATISFIED_SNAPSHOT,
                json!({"step": "rejudge"}),
            ),
            judged(6, &rejudge),
        ];
        assert_eq!(rejudged(&events), 2);
        assert_eq!(rejudged(&events[1..5]), 0);
    }

    /// A close by the judgment names the head, the verification, the
    /// verdict and confidence and the receipt's grounds, and no ask.
    #[test]
    fn a_close_by_the_judgment_keeps_the_head_the_verdict_and_the_grounds() {
        let verified = Verified {
            head: sha('a'),
            passed: true,
            ask_id: None,
            event: super::super::EventId::new(9),
        };
        let checks = [BaseCheck {
            command: "test -f seed.txt".into(),
            result: "passed".into(),
        }];
        let closed = closed_payload(
            &verified,
            3,
            &judgment("pass", Some("high")),
            ("the base meets it", &checks),
        );
        assert_eq!(
            closed,
            json!({"head": sha('a'), "verified_event": 9, "closed_by": "judgment",
                "attempt": 3, "verdict": "pass", "confidence": "high",
                "receipt_summary": "the base meets it",
                "base_checks": [{"command": "test -f seed.txt", "result": "passed"}]})
        );
    }
}
