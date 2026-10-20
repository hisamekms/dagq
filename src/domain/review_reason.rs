//! Reason codes of the review and plan review verdicts (ADR-t947-1): each
//! item of a verdict's `reasons` is a text and one or more codes, the first
//! its primary one. A missing code is `unlabeled` and a code outside the
//! lists below is kept as it was printed. The codes are labels for `stats`
//! and `kpi`, and one judgment reads them (ADR-t2599-1, amending decisions
//! 3 and 6): whether a run's review that sends the run back past its
//! revise limit has any code finding left ([`converged`]), where an
//! item's optional `paths` and the repository's [`CodePaths`] tell an
//! `acceptance_unmet` on code from one elsewhere. The verdict's
//! decision, how a concern is applied and the plan review read none of
//! them. The lists and their definitions are the design's, so a code is
//! added or removed without an ADR; a new code counts as a code finding
//! until an ADR puts it in [`CONVERGED_CODES`] (fail closed).

use crate::domain::EventKind;
use serde::Deserialize;

use super::RunEvent;

/// The code of an item that carried none: an item in the old form (text
/// only), an empty `codes`, and a sent-back verdict with no reasons.
pub const UNLABELED: &str = "unlabeled";

/// The codes of a run's review with their definitions, heaviest first:
/// the order the job picks the primary code of an item that fits two.
/// `adr_conflict`, `acceptance_conflict` and `acceptance_infeasible` are
/// for a worker that wrote that meeting a criterion breaks another rule or
/// a fact; a criterion simply not reached is `acceptance_unmet`.
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
/// (the form before the codes) or a text with its codes and, for a run's
/// review, the paths it is on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub(crate) enum PrintedReason {
    Text(String),
    Coded(CodedReason),
}

/// The codes feed `stats`, `kpi` and the judgment past the revise limit
/// ([`converged`], ADR-t2599-1), yet their shape never fails the verdict:
/// `codes` may be one string or an array, a value of another shape leaves
/// the item unlabeled (a code finding there, fail closed), and other
/// fields are ignored. `paths`, the repository-relative paths the item is
/// on, is read the same way: a value of another shape leaves the item
/// without paths, which the judgment past the revise limit counts as a
/// code finding for an `acceptance_unmet` ([`Finding::is_code_finding`]).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct CodedReason {
    text: String,
    #[serde(default)]
    codes: serde_json::Value,
    #[serde(default)]
    paths: serde_json::Value,
}

/// The strings of a loose `codes` or `paths`: one string or an array's
/// strings, trimmed, blank ones dropped; none for a value of another shape.
fn strings(value: serde_json::Value) -> Vec<String> {
    let values = match value {
        serde_json::Value::String(value) => vec![value],
        serde_json::Value::Array(values) => values
            .into_iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    values
        .into_iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

/// The texts of `printed` and each item's codes (empty for an item without
/// them), blank codes dropped.
pub(crate) fn split(printed: Vec<PrintedReason>) -> (Vec<String>, Vec<Vec<String>>) {
    let split = split_with_paths(printed);
    (split.texts, split.codes)
}

/// The items of a verdict's `reasons` taken apart, index by index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SplitReasons {
    pub(crate) texts: Vec<String>,
    /// Each item's codes, empty for an item without them.
    pub(crate) codes: Vec<Vec<String>>,
    /// Each item's paths (a leading `./` dropped), empty for an item
    /// without them.
    pub(crate) paths: Vec<Vec<String>>,
}

/// [`split`] with each item's `paths` too: what a run's review reads.
pub(crate) fn split_with_paths(printed: Vec<PrintedReason>) -> SplitReasons {
    let mut split = SplitReasons::default();
    for reason in printed {
        let (text, codes, paths) = match reason {
            PrintedReason::Text(text) => (text, Vec::new(), Vec::new()),
            PrintedReason::Coded(CodedReason { text, codes, paths }) => (
                text,
                strings(codes),
                strings(paths)
                    .into_iter()
                    .map(|path| path.strip_prefix("./").unwrap_or(&path).to_owned())
                    .filter(|path| !path.is_empty())
                    .collect(),
            ),
        };
        split.texts.push(text);
        split.codes.push(codes);
        split.paths.push(paths);
    }
    split
}

/// Each of `count` items' paths, empty for one without them; empty
/// altogether when no item has any, as `review_finished` leaves
/// `reason_paths` out then.
pub fn recorded_paths(paths: &[Vec<String>], count: usize) -> Vec<Vec<String>> {
    if paths.iter().all(Vec::is_empty) {
        return Vec::new();
    }
    (0..count)
        .map(|index| paths.get(index).cloned().unwrap_or_default())
        .collect()
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

/// The code paths of a repository (ADR-t2599-1): the globs of `[review.code]
/// paths` in the landing branch's commit's `dagq.toml`, which a run's review
/// reads at the start of each attempt and records on its `review_started`.
/// The key is an array of non-blank globs written as `add --paths` takes
/// them ([`super::scope`]: from the repository root, `*` within one
/// segment, `**` at any depth), the same glob twice kept once. Without the
/// key, or with an empty array, the repository has none, and every
/// `acceptance_unmet` is a code finding. With them, an `acceptance_unmet`
/// whose paths all lie outside the globs is not one
/// ([`Finding::is_code_finding`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodePaths {
    pub globs: Vec<String>,
}

impl CodePaths {
    /// The code paths of `globs`, `None` for none.
    pub fn of(globs: Vec<String>) -> Option<Self> {
        (!globs.is_empty()).then_some(Self { globs })
    }

    /// Whether `path` is code: some glob matches it.
    pub fn covers(&self, path: &str) -> bool {
        self.globs
            .iter()
            .any(|glob| super::scope::glob_matches(glob, path))
    }

    /// The code paths `review_started` recorded for its attempt in
    /// `payload` (`code_paths`), `None` without them: a review started by
    /// a binary that read none counts every `acceptance_unmet` as a code
    /// finding.
    pub fn recorded(payload: &serde_json::Value) -> Option<Self> {
        let globs: Vec<String> =
            serde_json::from_value(payload["code_paths"].clone()).unwrap_or_default();
        Self::of(globs)
    }
}

/// The code a reason gives when part of what the task asks is not done:
/// a code finding unless its paths say it is not on code
/// ([`Finding::is_code_finding`]).
pub const ACCEPTANCE_UNMET: &str = "acceptance_unmet";

/// The codes of a run's review that are no code finding (ADR-t2599-1),
/// each with the follow_up category (ADR-t947-3) of the draft a reason
/// left with it becomes when the run lands past its revise limit. Every
/// other code (`code_defect`, `test_gap`, `adr_conflict`,
/// `adr_design_mismatch`, `acceptance_unmet`, `other`, [`UNLABELED`] and a
/// code no list knows) is a code finding: what cannot be told apart counts
/// as one.
pub const CONVERGED_CODES: &[(&str, &str)] = &[
    ("acceptance_ambiguous", "decision"),
    ("acceptance_conflict", "decision"),
    ("acceptance_infeasible", "decision"),
    ("out_of_scope_change", "decision"),
    ("repo_rule_violation", "improvement"),
    ("docs_drift", "docs_drift"),
    ("local_slip", "improvement"),
];

/// One reason a review sends a run back with: its text, its recorded
/// codes (at least one, [`UNLABELED`] for none), the paths it is on (none
/// when the job gave none) and whether a review agent of the security role
/// gave it ([`super::review_subagents::is_security_agent`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub text: String,
    pub codes: Vec<String>,
    pub paths: Vec<String>,
    pub security: bool,
}

impl Finding {
    /// Whether the reason is a code finding with the repository's `code`
    /// paths: one from a security agent, or with any code (not only its
    /// primary one) outside [`CONVERGED_CODES`], or with none. An
    /// [`ACCEPTANCE_UNMET`] is not one only when the repository has code
    /// paths and the reason has paths all of which lie outside them;
    /// without either, or with one path on code, it is. A path that is not
    /// a plain repository-relative one (absolute, with a `.` or `..`
    /// segment, or a `:` such as a line number) cannot be told off code,
    /// so it counts as on code.
    pub fn is_code_finding(&self, code: Option<&CodePaths>) -> bool {
        let plain = |path: &String| {
            !path.contains(':')
                && super::scope::validate_path_globs(std::slice::from_ref(path)).is_ok()
        };
        let off_code = code.is_some_and(|code| {
            !self.paths.is_empty()
                && self
                    .paths
                    .iter()
                    .all(|path| plain(path) && !code.covers(path))
        });
        self.security
            || self.codes.is_empty()
            || self.codes.iter().any(|known| {
                !(CONVERGED_CODES
                    .iter()
                    .any(|(converged, _)| converged == known)
                    || (off_code && known == ACCEPTANCE_UNMET))
            })
    }

    /// The category of the follow_up draft the reason becomes when the run
    /// lands with it left: that of its first code, `other` for a code
    /// finding (which never lands so).
    pub fn follow_up_category(&self) -> &'static str {
        follow_up_category(&self.codes)
    }
}

/// The follow_up category of a reason with `codes` left when the run
/// landed ([`Finding::follow_up_category`]): an [`ACCEPTANCE_UNMET`] off
/// code is `remaining_scope`.
pub fn follow_up_category(codes: &[String]) -> &'static str {
    codes.first().map_or("other", |first| {
        if first == ACCEPTANCE_UNMET {
            return "remaining_scope";
        }
        CONVERGED_CODES
            .iter()
            .find(|(known, _)| known == first)
            .map_or("other", |(_, category)| category)
    })
}

/// Whether a review that sends the run back past its revise limit has no
/// code finding left (ADR-t2599-1) with the repository's `code` paths: it
/// has reasons and none of them is a code finding
/// ([`Finding::is_code_finding`]). The run then lands with the reasons as
/// follow_up drafts; otherwise a person decides.
pub fn converged(findings: &[Finding], code: Option<&CodePaths>) -> bool {
    !findings.is_empty() && !findings.iter().any(|f| f.is_code_finding(code))
}

/// Whether `event` is a `revise_limit_judged` that found the review it
/// judged converged ([`converged`]), which lets the run land as a passed
/// review does.
pub fn judged_converged(event: &RunEvent) -> bool {
    event.kind == EventKind::ReviseLimitJudged && event.payload["converged"] == true
}

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

    fn finding(codes: &[&str], security: bool) -> Finding {
        Finding {
            text: "t".to_owned(),
            codes: codes.iter().map(|code| (*code).to_owned()).collect(),
            paths: Vec::new(),
            security,
        }
    }

    /// The judgment of a repository without code paths.
    fn converged(findings: &[Finding]) -> bool {
        super::converged(findings, None)
    }

    #[test]
    fn a_run_reviews_items_carry_their_paths_and_the_old_forms_read() {
        let printed: Vec<PrintedReason> = serde_json::from_str(
            r#"["old", {"text": "new", "codes": ["acceptance_unmet"], "paths": ["./docs/a.md", " ", "src/b.rs"]}, {"text": "one", "paths": "docs/c.md"}, {"text": "loose", "paths": 7}]"#,
        )
        .unwrap();
        let split = split_with_paths(printed);
        assert_eq!(split.texts, ["old", "new", "one", "loose"]);
        assert_eq!(
            split.paths,
            [
                vec![],
                vec!["docs/a.md".to_owned(), "src/b.rs".to_owned()],
                vec!["docs/c.md".to_owned()],
                vec![]
            ]
        );
        assert_eq!(
            recorded_paths(&split.paths, 4)[1],
            ["docs/a.md".to_owned(), "src/b.rs".to_owned()]
        );
        // None with paths records none.
        assert!(recorded_paths(&[vec![], vec![]], 2).is_empty());
        assert_eq!(
            recorded_paths(&[vec!["a".to_owned()]], 2)[1],
            Vec::<String>::new()
        );
    }

    fn unmet(paths: &[&str]) -> Finding {
        Finding {
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            ..finding(&[ACCEPTANCE_UNMET], false)
        }
    }

    /// An `acceptance_unmet` is no code finding only with code paths and
    /// paths all off them (ADR-t2599-1); without the setting or without
    /// paths it stays one, and the other codes keep their judgment.
    #[test]
    fn an_acceptance_unmet_off_the_code_paths_alone_is_no_code_finding() {
        let code = CodePaths::of(vec!["src/**".to_owned(), "tests/**".to_owned()]).unwrap();
        let code = Some(&code);
        let docs = unmet(&["docs/design/review.md", "plugins/a/SKILL.md"]);
        assert!(!docs.is_code_finding(code));
        assert!(super::converged(
            &[
                docs.clone(),
                finding(&["local_slip"], false),
                finding(&["docs_drift"], false)
            ],
            code
        ));
        assert_eq!(docs.follow_up_category(), "remaining_scope");
        // One path on code, no paths, or no setting: a code finding.
        assert!(unmet(&["docs/a.md", "src/b.rs"]).is_code_finding(code));
        assert!(unmet(&[]).is_code_finding(code));
        // A path that cannot be told off code counts as on it.
        for odd in [
            "/w/src/lib.rs",
            "docs/../src/lib.rs",
            "./src/lib.rs",
            "src/lib.rs:12",
            "docs//a.md",
        ] {
            assert!(unmet(&["docs/a.md", odd]).is_code_finding(code), "{odd}");
        }
        assert!(docs.is_code_finding(None));
        assert!(!super::converged(std::slice::from_ref(&docs), None));
        assert_eq!(CodePaths::of(Vec::new()), None);
        // Paths never lift another code nor a security agent's finding.
        let defect = Finding {
            paths: vec!["docs/a.md".to_owned()],
            ..finding(&["code_defect"], false)
        };
        assert!(defect.is_code_finding(code));
        let secure = Finding {
            security: true,
            ..docs.clone()
        };
        assert!(secure.is_code_finding(code));
        let mixed = Finding {
            codes: vec![ACCEPTANCE_UNMET.to_owned(), "test_gap".to_owned()],
            ..docs
        };
        assert!(mixed.is_code_finding(code));
        // What review_started recorded.
        assert_eq!(
            CodePaths::recorded(&serde_json::json!({"code_paths": ["src/**"]})),
            CodePaths::of(vec!["src/**".to_owned()])
        );
        assert_eq!(
            CodePaths::recorded(&serde_json::json!({"attempt": 1})),
            None
        );
        assert_eq!(
            CodePaths::recorded(&serde_json::json!({"code_paths": []})),
            None
        );
    }

    /// Only reasons all of whose codes are among the converged ones, none
    /// from a security agent, converge (ADR-t2599-1); whatever cannot be
    /// told apart is a code finding.
    #[test]
    fn a_review_converges_only_when_every_code_of_every_reason_is_no_code_finding() {
        let converged_codes: Vec<&str> = CONVERGED_CODES.iter().map(|(code, _)| *code).collect();
        assert_eq!(
            converged_codes,
            [
                "acceptance_ambiguous",
                "acceptance_conflict",
                "acceptance_infeasible",
                "out_of_scope_change",
                "repo_rule_violation",
                "docs_drift",
                "local_slip",
            ]
        );
        for code in &converged_codes {
            assert!(
                REVIEW_CODES.iter().any(|(known, _)| known == code),
                "{code}"
            );
            assert!(converged(&[finding(&[code], false)]), "{code}");
        }
        assert!(converged(&[
            finding(&["local_slip", "docs_drift"], false),
            finding(&["repo_rule_violation"], false),
        ]));
        for code in [
            "code_defect",
            "test_gap",
            "adr_conflict",
            "adr_design_mismatch",
            "acceptance_unmet",
            "other",
            UNLABELED,
            "code_defekt",
        ] {
            // Alone, after a converged primary code, and in a second reason.
            assert!(!converged(&[finding(&[code], false)]), "{code}");
            assert!(
                !converged(&[finding(&["local_slip", code], false)]),
                "{code}"
            );
            assert!(
                !converged(&[finding(&["docs_drift"], false), finding(&[code], false)]),
                "{code}"
            );
        }
        // A security agent's finding never converges, whatever its codes.
        assert!(!converged(&[finding(&["local_slip"], true)]));
        assert!(!converged(&[
            finding(&["docs_drift"], false),
            finding(&["docs_drift"], true)
        ]));
        // No reason, or a reason without codes, is nothing to judge by.
        assert!(!converged(&[]));
        assert!(!converged(&[finding(&[], false)]));
    }

    #[test]
    fn a_converged_reason_becomes_a_draft_of_its_first_codes_category() {
        assert_eq!(
            finding(&["docs_drift", "local_slip"], false).follow_up_category(),
            "docs_drift"
        );
        assert_eq!(
            finding(&["local_slip"], false).follow_up_category(),
            "improvement"
        );
        assert_eq!(
            finding(&["acceptance_ambiguous"], false).follow_up_category(),
            "decision"
        );
        for (_, category) in CONVERGED_CODES {
            assert!(
                super::super::follow_up::FOLLOW_UP_CATEGORIES
                    .iter()
                    .any(|(known, _)| known == category),
                "{category}"
            );
        }
    }

    #[test]
    fn a_judgment_lets_the_run_land_only_when_it_converged() {
        let event = |kind: &str, payload| RunEvent {
            id: crate::domain::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        };
        assert!(judged_converged(&event(
            "revise_limit_judged",
            serde_json::json!({"converged": true})
        )));
        for (kind, payload) in [
            (
                "revise_limit_judged",
                serde_json::json!({"converged": false}),
            ),
            (
                "revise_limit_judged",
                serde_json::json!({"converged": "true"}),
            ),
            ("revise_limit_judged", serde_json::json!({})),
            ("review_finished", serde_json::json!({"converged": true})),
        ] {
            assert!(!judged_converged(&event(kind, payload)), "{kind}");
        }
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
