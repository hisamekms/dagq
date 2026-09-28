//! Reason codes of the review and plan review verdicts (ADR-t947-1): each
//! item of a verdict's `reasons` is a text and one or more codes, the first
//! its primary one. The codes are labels for `stats` and `kpi` only: a
//! missing code is `unlabeled`, a code outside the lists below is kept as
//! it was printed, and neither the verdict's decision nor how it is applied
//! reads them (decision 3). The lists and their definitions are the
//! design's (decision 6), so a code is added or removed without an ADR.

use serde::Deserialize;

/// The code of an item that carried none: an item in the old form (text
/// only), an empty `codes`, and a sent-back verdict with no reasons.
pub const UNLABELED: &str = "unlabeled";

/// The codes of a run's review with their definitions, heaviest first:
/// the order the job picks the primary code of an item that fits two.
pub const REVIEW_CODES: &[(&str, &str)] = &[
    (
        "adr_conflict",
        "the task's acceptance or description, or the only way to meet it, contradicts an accepted decision record, a design document or a person's decision (the goal's constraints, the repository instructions' decisions), and both cannot hold: the worker chose one and departed from the other",
    ),
    (
        "acceptance_conflict",
        "two acceptance criteria of the same task, or a criterion and the description, cannot both hold",
    ),
    (
        "acceptance_infeasible",
        "a criterion cannot be met as written because of a fact (permissions, environment, how a tool or the existing code behaves)",
    ),
    (
        "adr_design_mismatch",
        "the task agrees with the decision records and a way to meet both exists, yet the implementation contradicts an accepted decision record or design (no new decision record or amendment)",
    ),
    (
        "acceptance_ambiguous",
        "a criterion reads two or more ways and the implementation chose one",
    ),
    (
        "acceptance_unmet",
        "part of what the criteria or the description state is not done (missed, or left to a follow-up)",
    ),
    (
        "out_of_scope_change",
        "a behavior nobody asked for was changed (production behavior, another task's scope)",
    ),
    (
        "repo_rule_violation",
        "a procedural rule of the repository (its instructions) was not followed",
    ),
    ("code_defect", "a bug or a regression in the implementation"),
    (
        "test_gap",
        "a test the criteria ask for, or of a changed path, is missing",
    ),
    (
        "docs_drift",
        "a design document, README, skill or comment that describes the changed behavior is left stale, or only one of its copies was fixed",
    ),
    (
        "local_slip",
        "a local slip that changes no meaning (placement, order, format, a wrong number in text)",
    ),
    ("other", "none of the above; the text explains it"),
];

/// The codes of a plan review with their definitions, heaviest first. The
/// codes both lists have are defined alike (decision 2).
pub const PLAN_REVIEW_CODES: &[(&str, &str)] = &[
    (
        "adr_conflict",
        "a task's acceptance or description, or the only way to meet it, contradicts an accepted decision record, a design document or a person's decision (the goal's constraints, the repository instructions' decisions), and both cannot hold",
    ),
    (
        "acceptance_conflict",
        "two acceptance criteria of the same task, or a criterion and the description or context, contradict each other",
    ),
    (
        "acceptance_infeasible",
        "a criterion cannot be met as written because of a fact (permissions, environment, how a tool or the existing code behaves)",
    ),
    (
        "operational_hazard",
        "landing the task would stop the queue or production operation",
    ),
    (
        "wrong_premise",
        "the task misjudges a fact of the current code or documents",
    ),
    (
        "task_overlap",
        "the task overlaps an open task or a change already landed",
    ),
    (
        "missing_dependency",
        "a dependency on a task or goal that has to land first is missing",
    ),
    (
        "stale_adr_reference",
        "the task cites a superseded decision record, or amends the wrong one",
    ),
    ("acceptance_ambiguous", "a criterion reads two or more ways"),
    (
        "incomplete_spec",
        "a place to change or a condition is left out of the task",
    ),
    (
        "paths_insufficient",
        "the declared paths do not include a path the change needs",
    ),
    (
        "verification_rule",
        "verify, evidence or kind do not match the repository's recommended combination",
    ),
    ("lint_violation", "a finding of `dagq lint`"),
    ("other", "none of the above; the text explains it"),
];

/// One item of a verdict's `reasons` as the job printed it: a text alone
/// (the form before the codes) or a text with its codes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub(crate) enum PrintedReason {
    Text(String),
    Coded(CodedReason),
}

/// The codes are labels for `stats` only (ADR-t947-1 decision 3), so their
/// shape never fails the verdict: `codes` may be one string or an array, a
/// value of another shape leaves the item unlabeled, and other fields are
/// ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct CodedReason {
    text: String,
    #[serde(default)]
    codes: serde_json::Value,
}

/// The texts of `printed` and each item's codes (empty for an item without
/// them), blank codes dropped.
pub(crate) fn split(printed: Vec<PrintedReason>) -> (Vec<String>, Vec<Vec<String>>) {
    printed
        .into_iter()
        .map(|reason| match reason {
            PrintedReason::Text(text) => (text, Vec::new()),
            PrintedReason::Coded(CodedReason { text, codes }) => {
                let codes = match codes {
                    serde_json::Value::String(code) => vec![code],
                    serde_json::Value::Array(codes) => codes
                        .into_iter()
                        .filter_map(|code| code.as_str().map(str::to_owned))
                        .collect(),
                    _ => Vec::new(),
                };
                (
                    text,
                    codes
                        .into_iter()
                        .map(|code| code.trim().to_owned())
                        .filter(|code| !code.is_empty())
                        .collect(),
                )
            }
        })
        .unzip()
}

/// The codes recorded for each of `count` items: an item's own codes, or
/// [`UNLABELED`] for one without them.
pub fn recorded(codes: &[Vec<String>], count: usize) -> Vec<Vec<String>> {
    (0..count)
        .map(|index| match codes.get(index) {
            Some(codes) if !codes.is_empty() => codes.clone(),
            _ => vec![UNLABELED.to_owned()],
        })
        .collect()
}

/// The primary code of a verdict that sent the work back: that of its
/// first item, which the job puts first as the one that decided the
/// verdict; [`UNLABELED`] without an item or a code.
pub fn primary(codes: &[Vec<String>]) -> String {
    codes
        .first()
        .and_then(|codes| codes.first())
        .cloned()
        .unwrap_or_else(|| UNLABELED.to_owned())
}

/// What a person's answer to the ask of a `concern` says of the review's
/// findings (decision 4): `land` and `ready` accepted the departure (a
/// review's error among them), `send_back` rejected it, `cancel` dropped
/// the work.
pub fn answer_outcome(answer: &str) -> Option<&'static str> {
    match answer.split(':').next().unwrap_or_default().trim() {
        "land" | "ready" => Some(DEVIATION_ACCEPTED),
        "send_back" => Some(DEVIATION_REJECTED),
        "cancel" => Some(CANCELED),
        _ => None,
    }
}

pub const DEVIATION_ACCEPTED: &str = "deviation_accepted";
pub const DEVIATION_REJECTED: &str = "deviation_rejected";
pub const CANCELED: &str = "canceled";
/// Every outcome, in the order `stats` lists them.
pub const OUTCOMES: [&str; 3] = [DEVIATION_ACCEPTED, DEVIATION_REJECTED, CANCELED];

/// The codes of `list` and their definitions, one line each, for a
/// job's prompt.
pub fn prompt_lines(list: &[(&str, &str)]) -> String {
    list.iter()
        .map(|(code, meaning)| format!("- {code}: {meaning}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_forms_of_an_item_split_into_texts_and_codes() {
        let printed: Vec<PrintedReason> = serde_json::from_str(
            r#"["old", {"text": "new", "codes": ["test_gap", " ", "code_defect"]}, {"text": "bare"}]"#,
        )
        .unwrap();
        let (texts, codes) = split(printed);
        assert_eq!(texts, ["old", "new", "bare"]);
        assert_eq!(
            codes,
            [
                vec![],
                vec!["test_gap".to_owned(), "code_defect".to_owned()],
                vec![]
            ]
        );
        assert_eq!(
            recorded(&codes, 3),
            [
                vec![UNLABELED.to_owned()],
                vec!["test_gap".to_owned(), "code_defect".to_owned()],
                vec![UNLABELED.to_owned()]
            ]
        );
        assert_eq!(primary(&recorded(&codes, 3)), UNLABELED);
        assert_eq!(primary(&recorded(&codes[1..], 2)), "test_gap");
        assert_eq!(primary(&[]), UNLABELED);
        // The codes' shape never fails the item: one string, a value of
        // another shape (unlabeled) and other fields are taken.
        let loose: Vec<PrintedReason> = serde_json::from_str(
            r#"[{"text":"a","codes":"test_gap"},{"text":"b","codes":7,"why":1},{"text":"c","codes":["x",2]}]"#,
        )
        .unwrap();
        assert_eq!(
            split(loose).1,
            [vec!["test_gap".to_owned()], vec![], vec!["x".to_owned()]]
        );
        // An item without its text is not one.
        assert!(serde_json::from_str::<Vec<PrintedReason>>(r#"[{"codes":["x"]}]"#).is_err());
    }

    #[test]
    fn the_answers_map_to_outcomes_and_the_lists_end_in_other() {
        assert_eq!(answer_outcome("land"), Some(DEVIATION_ACCEPTED));
        assert_eq!(answer_outcome(" ready "), Some(DEVIATION_ACCEPTED));
        assert_eq!(
            answer_outcome("send_back: split it"),
            Some(DEVIATION_REJECTED)
        );
        assert_eq!(answer_outcome("cancel"), Some(CANCELED));
        assert_eq!(answer_outcome("maybe"), None);
        for list in [REVIEW_CODES, PLAN_REVIEW_CODES] {
            assert_eq!(list.last().unwrap().0, "other");
            assert!(prompt_lines(list).starts_with("- adr_conflict: "));
        }
        let shared = [
            "adr_conflict",
            "acceptance_conflict",
            "acceptance_infeasible",
            "acceptance_ambiguous",
        ];
        for code in shared {
            assert!(REVIEW_CODES.iter().any(|(c, _)| *c == code));
            assert!(PLAN_REVIEW_CODES.iter().any(|(c, _)| *c == code));
        }
    }
}
