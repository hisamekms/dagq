//! Cases and scoring for whole verdict roles. Snapshot deserialization and
//! production prompt assembly belong to the application harness.
use super::{Case, CaseBase, RoleCase, Source, Split, ratio};
use crate::domain::goal_review::{GoalReviewDecision, GoalReviewVerdict};
use crate::domain::plan_review::{PlanReviewDecision, PlanReviewVerdict};
use crate::domain::{ReviewDecision, ReviewVerdict};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// All verdict-role case lists live under `.dagq/role-evals/<role>/`;
/// `dev.json`, `holdout.json`, and `production.json` select the split.
/// Each list has `role`, `codes`, `k`, and `cases`. Each case embeds the
/// complete production `input-snapshot.json` as `snapshot`, plus `id`,
/// `source`, `made_by`, `expected`, optional `k`, `disputed` and `adjudicated`.
/// Expectations use the production decision and reason codes; acceptable
/// codes may be reported without a false positive. No expectation is replayed.
/// A run review case also names its change copy by `patch`, the SHA-256 of
/// a file in [`super::PATCH_DIR`] as an agent case does; the other roles
/// read main and have none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerdictCase {
    pub id: String,
    pub source: String,
    pub made_by: String,
    pub snapshot: Value,
    #[serde(default)]
    pub patch: Option<String>,
    pub expected: Expected,
    pub k: Option<u32>,
    pub disputed: Option<Value>,
    pub adjudicated: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    pub verdict: Decision,
    pub codes: BTreeSet<String>,
    #[serde(default)]
    pub acceptable_codes: BTreeSet<String>,
}

/// The role's production decision, serialized without a wrapper or renaming.
/// Goal reason codes in expectations are the exact unmet criterion strings.
/// The run review's values are spelled like plan review's, so [`read`]
/// gives them their role's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Decision {
    Plan(PlanReviewDecision),
    Goal(GoalReviewDecision),
    Run(ReviewDecision),
}
impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Plan(d) => d.as_str(),
            Self::Goal(d) => d.as_str(),
            Self::Run(d) => d.as_str(),
        }
    }
    fn positive(self) -> bool {
        matches!(
            self,
            Self::Plan(PlanReviewDecision::Pass)
                | Self::Goal(GoalReviewDecision::Achieved)
                | Self::Run(ReviewDecision::Pass)
        )
    }
    fn belongs_to(self, role: &str) -> bool {
        matches!(
            (self, role),
            (Self::Plan(_), "plan_review")
                | (Self::Goal(_), "goal_review")
                | (Self::Run(_), "review")
        )
    }
    /// The same spelling as `role`'s decision, for the roles that share it.
    fn of_role(self, role: &str) -> Self {
        match (self, role) {
            (Self::Plan(d), "review") => d.as_str().parse().map_or(self, Self::Run),
            _ => self,
        }
    }
}

/// Hidden in every role's case tree, including trees for agent cases.
pub const CASE_DIR: &str = ".dagq/role-evals";
pub const HIDDEN: [&str; 3] = [".dagq/agents/*/evals", ".dagq/agent-cases", CASE_DIR];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct List {
    pub role: String,
    pub codes: BTreeSet<String>,
    pub k: u32,
    pub cases: Vec<VerdictCase>,
}

/// Validate one split without accessing files. The application validates
/// the embedded production snapshot before the round can start.
pub fn read(text: &str, split: Split, role: &str) -> Result<List, String> {
    let mut list: List = serde_json::from_str(text).map_err(|e| e.to_string())?;
    for c in &mut list.cases {
        c.expected.verdict = c.expected.verdict.of_role(role);
    }
    if list.role != role || list.k == 0 || list.cases.is_empty() {
        return Err("wrong role, zero k, or empty cases".into());
    }
    let mut ids = BTreeSet::new();
    for c in &list.cases {
        if c.id.trim().is_empty()
            || !ids.insert(&c.id)
            || c.k == Some(0)
            || c.made_by.trim().is_empty()
            || !matches!(c.source.as_str(), "generated" | "handmade" | "production")
            || (c.source == "handmade" && split != Split::Dev)
            || (c.patch.is_some() && role != "review")
        {
            return Err(format!("invalid id, source, made_by or k: {}", c.id));
        }
        let e = &c.expected;
        if !e.verdict.belongs_to(role)
            || !e.codes.is_subset(&list.codes)
            || !e.acceptable_codes.is_subset(&list.codes)
            || !e.codes.is_disjoint(&e.acceptable_codes)
            || (e.verdict.positive() && !e.codes.is_empty())
            || (!e.verdict.positive() && e.codes.is_empty())
        {
            return Err(format!("invalid expected codes: {}", c.id));
        }
    }
    Ok(list)
}

/// Check the directory entries and required split before reading any jobs.
pub fn check_files<'a>(
    names: impl IntoIterator<Item = &'a str>,
    split: Split,
) -> Result<(), String> {
    let mut found = false;
    for name in names {
        let part =
            Split::from_file_name(name).ok_or_else(|| format!("unknown role case file {name}"))?;
        found |= part == split;
    }
    if found {
        Ok(())
    } else {
        Err(format!("no {}", split.file_name()))
    }
}

impl VerdictCase {
    /// Share the round lifecycle, slots, keys and tree cleanup with agent
    /// cases. `patch` indexes the snapshot bytes, with a run review's change
    /// copy named after them, never the change copy alone. Provenance,
    /// expectations and repetition counts do not mint a new held-out key
    /// for the same inputs.
    pub fn into_case(self, main: String) -> Case {
        let mut bytes = serde_json::to_vec(&self.snapshot).expect("JSON snapshot");
        if let Some(patch) = &self.patch {
            bytes.extend_from_slice(patch.as_bytes());
        }
        use sha2::{Digest, Sha256};
        Case {
            id: self.id.clone(),
            source: match self.source.as_str() {
                "production" => Source::Production,
                "handmade" => Source::Handmade,
                _ => Source::Generated,
            },
            made_by: self.made_by.clone(),
            base: CaseBase::Commit(main),
            patch: format!("{:x}", Sha256::digest(bytes)),
            adjudicated: self.adjudicated.clone(),
            disputed: self.disputed.clone().filter(super::is_set),
            k: self.k,
            role: RoleCase::Verdict(Box::new(self)),
        }
    }
}

#[derive(Default)]
struct Counts {
    tp: usize,
    fp: usize,
    missed: usize,
}
impl Counts {
    fn record(&self) -> Value {
        json!({"recall":ratio(self.tp, self.tp+self.missed),
               "precision":ratio(self.tp, self.tp+self.fp)})
    }
    fn add(&mut self, expected: bool, actual: bool) {
        match (expected, actual) {
            (true, true) => self.tp += 1,
            (true, false) => self.missed += 1,
            (false, true) => self.fp += 1,
            _ => (),
        }
    }
}

/// One ended run of a case: its printed verdict, or the reason it has none
/// (a job that did not start or printed nothing readable).
pub type RoleRun<'a> = (&'a str, Option<&'a Value>, Option<&'a str>);

/// The role whose production verdict a score reads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scored {
    Plan,
    Goal,
    Run,
}

/// Exact production decisions: revise and concern remain distinct. Errors
/// count in neither decision nor reason counts, and fail the round.
pub fn score(cases: &[Case], runs: &[RoleRun<'_>], threshold: f64, disputed: bool) -> Value {
    score_role(cases, runs, threshold, disputed, Scored::Plan)
}

/// Score goal decisions and the acceptance items explicitly marked unmet.
/// Gap titles and prose summaries are not a second classification.
pub fn score_goal(cases: &[Case], runs: &[RoleRun<'_>], threshold: f64, disputed: bool) -> Value {
    score_role(cases, runs, threshold, disputed, Scored::Goal)
}

/// Score the whole run review's decisions and the codes of its reasons,
/// as [`ReviewVerdict::parse`] reads the printed verdict; the agents'
/// results never enter it.
pub fn score_review(cases: &[Case], runs: &[RoleRun<'_>], threshold: f64, disputed: bool) -> Value {
    score_role(cases, runs, threshold, disputed, Scored::Run)
}

/// The production decision, reason codes and reasons of a printed verdict.
fn parse(role: Scored, v: &Value) -> Option<(Decision, BTreeSet<String>, Value)> {
    let text = v.to_string();
    match role {
        Scored::Goal => {
            let v = GoalReviewVerdict::parse(&text).ok()?;
            let codes = v
                .criteria
                .iter()
                .filter(|c| !c.met)
                .map(|c| c.criterion.clone())
                .collect();
            Some((
                Decision::Goal(v.verdict),
                codes,
                serde_json::to_value(v.criteria).ok()?,
            ))
        }
        Scored::Plan => {
            let v = PlanReviewVerdict::parse(&text).ok()?;
            let codes = v.reason_codes.into_iter().flatten().collect();
            Some((
                Decision::Plan(v.verdict),
                codes,
                serde_json::to_value(v.reasons).ok()?,
            ))
        }
        Scored::Run => {
            let v = ReviewVerdict::parse(&text).ok()?;
            let codes = v.reason_codes.into_iter().flatten().collect();
            Some((Decision::Run(v.verdict), codes, json!(v.reasons)))
        }
    }
}

fn score_role(
    cases: &[Case],
    runs: &[RoleRun<'_>],
    threshold: f64,
    disputed: bool,
    role: Scored,
) -> Value {
    let mut decisions = [Counts::default(), Counts::default(), Counts::default()];
    let values = match role {
        Scored::Goal => [
            GoalReviewDecision::Achieved,
            GoalReviewDecision::Gaps,
            GoalReviewDecision::Ask,
        ]
        .map(Decision::Goal),
        Scored::Plan => [
            PlanReviewDecision::Pass,
            PlanReviewDecision::Revise,
            PlanReviewDecision::Concern,
        ]
        .map(Decision::Plan),
        Scored::Run => [
            ReviewDecision::Pass,
            ReviewDecision::Revise,
            ReviewDecision::Concern,
        ]
        .map(Decision::Run),
    };
    let mut codes = std::collections::BTreeMap::<String, Counts>::new();
    let (mut errors, mut total) = (0, 0);
    let mut failed_cases = Vec::new();
    for (id, result, error) in runs {
        let Some(c) = cases.iter().find(|c| c.id == *id) else {
            continue;
        };
        if c.is_disputed() && !disputed {
            continue;
        }
        let RoleCase::Verdict(c) = &c.role else {
            continue;
        };
        total += 1;
        let Some((decision, actual, reasons)) = result.and_then(|v| parse(role, v)) else {
            errors += 1;
            failed_cases
                .push(json!({"id":id,"error":error.unwrap_or("no readable production verdict")}));
            continue;
        };
        for (counts, value) in decisions.iter_mut().zip(values) {
            counts.add(c.expected.verdict == value, decision == value);
        }
        if decision != c.expected.verdict
            || !c.expected.codes.is_subset(&actual)
            || actual.iter().any(|code| {
                !c.expected.codes.contains(code) && !c.expected.acceptable_codes.contains(code)
            })
        {
            failed_cases.push(json!({"id":id,"verdict":decision,"codes":actual,"reasons":reasons}));
        }
        for code in c.expected.codes.union(&actual) {
            if !c.expected.acceptable_codes.contains(code) {
                codes
                    .entry(code.clone())
                    .or_default()
                    .add(c.expected.codes.contains(code), actual.contains(code));
            }
        }
    }
    let per_verdict: serde_json::Map<String, Value> = values
        .iter()
        .zip(&decisions)
        .map(|(d, c)| (d.as_str().into(), c.record()))
        .collect();
    let per_code: serde_json::Map<String, Value> =
        codes.iter().map(|(k, c)| (k.clone(), c.record())).collect();
    let reasons = Counts {
        tp: codes.values().map(|c| c.tp).sum(),
        fp: codes.values().map(|c| c.fp).sum(),
        missed: codes.values().map(|c| c.missed).sum(),
    }
    .record();
    let meets = |v: &Value| v.as_f64().is_some_and(|n| n >= threshold);
    // An absent class is not evidence of its quality. All three decisions
    // must be represented to adopt a configuration.
    let passed = total > 0
        && errors == 0
        && per_verdict
            .values()
            .all(|v| meets(&v["recall"]) && meets(&v["precision"]))
        && meets(&reasons["recall"])
        && meets(&reasons["precision"]);
    json!({"runs":total,"errors":errors,"per_verdict":per_verdict,"per_code":per_code,"reasons":reasons,"failed_cases":failed_cases,"passed":passed})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn list() -> Value {
        json!({"role":"plan_review","codes":["P-1","P-2"],"k":1,"cases":[
            {"id":"pass","source":"handmade","made_by":"test","snapshot":{},"expected":{"verdict":"pass","codes":[]}},
            {"id":"revise","source":"handmade","made_by":"test","snapshot":{},"expected":{"verdict":"revise","codes":["P-1"],"acceptable_codes":["P-2"]}},
            {"id":"concern","source":"handmade","made_by":"test","snapshot":{},"expected":{"verdict":"concern","codes":["P-1"]}}
        ]})
    }
    #[test]
    fn case_shapes_reject_mistakes_before_running() {
        assert!(read(&list().to_string(), Split::Dev, "plan_review").is_ok());
        let case = read(&list().to_string(), Split::Dev, "plan_review")
            .unwrap()
            .cases
            .remove(0);
        let original = case.clone().into_case("a".repeat(40));
        let mut relabeled = case;
        relabeled.made_by = "another labeler".into();
        relabeled.k = Some(3);
        assert_eq!(original.patch, relabeled.into_case("a".repeat(40)).patch);

        assert!(check_files(["dev.json", "holdout.json"], Split::Dev).is_ok());
        for names in [
            vec![],
            vec!["production.json"],
            vec!["dev.json", "extra.json"],
            vec!["nested/dev.json"],
        ] {
            assert!(check_files(names, Split::Dev).is_err());
        }
        for bad in [
            {
                let mut v = list();
                v["extra"] = json!(1);
                v
            },
            {
                let mut v = list();
                v["k"] = json!(0);
                v
            },
            {
                let mut v = list();
                v["cases"][1]["id"] = json!("pass");
                v
            },
            {
                let mut v = list();
                v["cases"][0]["expected"]["verdict"] = json!("clean");
                v
            },
            {
                let mut v = list();
                v["cases"][0]["expected"]["codes"] = json!(["P-1"]);
                v
            },
            {
                let mut v = list();
                v["cases"][1]["expected"]["codes"] = json!(["unknown"]);
                v
            },
            {
                let mut v = list();
                v["cases"][1]["expected"]["acceptable_codes"] = json!(["P-1"]);
                v
            },
        ] {
            assert!(
                read(&bad.to_string(), Split::Dev, "plan_review").is_err(),
                "{bad}"
            );
        }
        assert!(read(&list().to_string(), Split::Holdout, "plan_review").is_err());
        assert!(read(&list().to_string(), Split::Dev, "goal_review").is_err());
    }
    #[test]
    fn decisions_are_distinct_and_codes_errors_and_disputes_are_counted() {
        let cases: Vec<Case> = read(&list().to_string(), Split::Dev, "plan_review")
            .unwrap()
            .cases
            .into_iter()
            .map(|c| c.into_case("a".repeat(40)))
            .collect();
        let pass = json!({"verdict":"pass","reasons":[],"summary":"ok"});
        let revise = json!({"verdict":"revise","reasons":[{"text":"problem","codes":["P-1","P-2"]}],"summary":"fix"});
        let concern = json!({"verdict":"concern","reasons":[{"text":"problem","codes":["P-1"]}],"summary":"ask"});
        let good = [
            ("pass", Some(&pass), None),
            ("revise", Some(&revise), None),
            ("concern", Some(&concern), None),
        ];
        let scores = score(&cases, &good, 0.9, false);
        assert_eq!(scores["passed"], true, "{scores}");
        assert_eq!(scores["per_code"]["P-1"]["recall"], 1.0);
        let wrong = score(
            &cases,
            &[
                ("pass", Some(&pass), None),
                ("revise", Some(&concern), None),
                ("concern", Some(&concern), None),
            ],
            0.9,
            false,
        );
        assert_eq!(wrong["passed"], false);
        assert_eq!(wrong["per_verdict"]["revise"]["recall"], 0.0);
        let mut cases = cases;
        cases[1].disputed = Some(json!("disagree"));
        let error = [
            ("pass", Some(&pass), None),
            ("revise", None, None),
            ("concern", Some(&concern), None),
        ];
        assert_eq!(score(&cases, &error, 0.9, false)["errors"], 0);
        assert_eq!(score(&cases, &error, 0.9, true)["errors"], 1);
        assert_eq!(
            score(
                &cases,
                &[("pass", Some(&json!({"verdict":"pass"})), None)],
                0.9,
                false
            )["errors"],
            1
        );
    }
    #[test]
    fn goal_cases_keep_production_decisions_and_score_only_unmet_criteria() {
        let list = json!({"role":"goal_review","codes":["works","docs"],"k":1,"cases":[
            {"id":"done","source":"handmade","made_by":"test","snapshot":{},"expected":{"verdict":"achieved","codes":[]}},
            {"id":"gap","source":"handmade","made_by":"test","snapshot":{},"expected":{"verdict":"gaps","codes":["works"],"acceptable_codes":["docs"]}},
            {"id":"ask","source":"handmade","made_by":"test","snapshot":{},"expected":{"verdict":"ask","codes":["works"]}}
        ]});
        let read_goal = |v: &Value| read(&v.to_string(), Split::Dev, "goal_review");
        let cases: Vec<_> = read_goal(&list)
            .unwrap()
            .cases
            .into_iter()
            .map(|c| c.into_case("a".repeat(40)))
            .collect();
        for bad in [
            {
                let mut v = list.clone();
                v["cases"][0]["expected"]["verdict"] = json!("pass");
                v
            },
            {
                let mut v = list.clone();
                v["cases"][0]["expected"]["codes"] = json!(["works"]);
                v
            },
            {
                let mut v = list.clone();
                v["cases"][1]["expected"]["codes"] = json!([]);
                v
            },
            {
                let mut v = list.clone();
                v["cases"][1]["expected"]["codes"] = json!(["unknown"]);
                v
            },
        ] {
            assert!(read_goal(&bad).is_err(), "{bad}");
        }
        let done = json!({"verdict":"achieved","criteria":[{"criterion":"docs","met":true}]});
        let gap = json!({"verdict":"gaps","criteria":[{"criterion":"works","met":false},{"criterion":"docs","met":false}],"gaps":[{"title":"a different title"}]});
        let ask = json!({"verdict":"ask","criteria":[{"criterion":"works","met":false}],"question":"scope?"});
        let good = [
            ("done", Some(&done), None),
            ("gap", Some(&gap), None),
            ("ask", Some(&ask), None),
        ];
        let scored = score_goal(&cases, &good, 0.9, false);
        assert_eq!(scored["passed"], true, "{scored}");
        assert_eq!(scored["per_code"]["works"]["recall"], 1.0);
        assert!(scored["per_code"].get("docs").is_none());
        let wrong = score_goal(
            &cases,
            &[
                ("done", Some(&done), None),
                ("gap", Some(&ask), None),
                ("ask", Some(&ask), None),
            ],
            0.9,
            false,
        );
        assert_eq!(wrong["passed"], false);
        assert_eq!(wrong["per_verdict"]["gaps"]["recall"], 0.0);
        let missing = json!({"verdict":"gaps","gaps":[{"title":"works"}]});
        let wrong = score_goal(&cases, &[("gap", Some(&missing), None)], 0.9, false);
        assert_eq!(wrong["per_code"]["works"]["recall"], 0.0);
        let extra = json!({"verdict":"ask","criteria":[{"criterion":"works","met":false},{"criterion":"unknown","met":false}]});
        assert_eq!(
            score_goal(&cases, &[("ask", Some(&extra), None)], 0.9, false)["per_code"]["unknown"]["precision"],
            0.0
        );
        let mut disputed = cases.clone();
        disputed[1].disputed = Some(json!("disagree"));
        for bad in [
            json!({"verdict":"gaps"}),
            json!({"verdict":"pass","reasons":[],"summary":"ok"}),
            json!({"verdict":"achieved","extra":true}),
        ] {
            let runs = [("gap", Some(&bad), None)];
            assert_eq!(score_goal(&disputed, &runs, 0.9, false)["runs"], 0);
            assert_eq!(score_goal(&disputed, &runs, 0.9, true)["errors"], 1);
        }
    }

    #[test]
    fn run_review_cases_keep_production_decisions_patches_and_run_errors() {
        let list = json!({"role":"review","codes":["acceptance_unmet","docs_drift"],"k":1,"cases":[
            {"id":"pass","source":"handmade","made_by":"test","snapshot":{},"patch":"p1","expected":{"verdict":"pass","codes":[]}},
            {"id":"revise","source":"production","made_by":"codex","snapshot":{},"patch":"p2","expected":{"verdict":"revise","codes":["acceptance_unmet"],"acceptable_codes":["docs_drift"]}},
            {"id":"concern","source":"handmade","made_by":"test","snapshot":{},"expected":{"verdict":"concern","codes":["acceptance_unmet"]}}
        ]});
        let read_run = |v: &Value| read(&v.to_string(), Split::Dev, "review");
        let parsed = read_run(&list).unwrap();
        assert_eq!(
            parsed.cases[1].expected.verdict,
            Decision::Run(ReviewDecision::Revise)
        );
        // The change copy is part of the case's key; plan cases name none.
        let key = |patch: Option<&str>| {
            let mut c = parsed.cases[0].clone();
            c.patch = patch.map(Into::into);
            c.into_case("a".repeat(40)).patch
        };
        assert_ne!(key(Some("p1")), key(Some("other")));
        assert_ne!(key(Some("p1")), key(None));
        let mut plan = list.clone();
        plan["role"] = json!("plan_review");
        assert!(read(&plan.to_string(), Split::Dev, "plan_review").is_err());
        plan["cases"][0].as_object_mut().unwrap().remove("patch");
        plan["cases"][1].as_object_mut().unwrap().remove("patch");
        assert!(read(&plan.to_string(), Split::Dev, "plan_review").is_ok());
        for bad in [
            {
                let mut v = list.clone();
                v["cases"][0]["expected"]["verdict"] = json!("achieved");
                v
            },
            {
                let mut v = list.clone();
                v["cases"][1]["expected"]["codes"] = json!([]);
                v
            },
        ] {
            assert!(read_run(&bad).is_err(), "{bad}");
        }
        let cases: Vec<_> = parsed
            .cases
            .into_iter()
            .map(|c| c.into_case("a".repeat(40)))
            .collect();
        let pass = json!({"verdict":"pass","reasons":[],"summary":"ok"});
        let revise = json!({"verdict":"revise","reasons":[{"text":"fix","codes":["acceptance_unmet","docs_drift"]}],"summary":"fix"});
        let concern = json!({"verdict":"concern","reasons":[{"text":"ask","codes":["acceptance_unmet"]}],"summary":"ask","recommendation":"land","confidence":"low","reason_category":"scope"});
        let good = [
            ("pass", Some(&pass), None),
            ("revise", Some(&revise), None),
            ("concern", Some(&concern), None),
        ];
        let scored = score_review(&cases, &good, 0.9, false);
        assert_eq!(scored["passed"], true, "{scored}");
        assert_eq!(scored["per_verdict"]["concern"]["recall"], 1.0);
        // Concern and revise stay distinct.
        let wrong = score_review(&cases, &[("revise", Some(&concern), None)], 0.9, false);
        assert_eq!(wrong["per_verdict"]["revise"]["recall"], 0.0);
        // A run that never started keeps why in the scores.
        let refused = score_review(
            &cases,
            &[
                (
                    "pass",
                    None,
                    Some("rebuilt tree differs from recorded tree"),
                ),
                ("revise", Some(&json!({"verdict":"revise"})), None),
            ],
            0.9,
            false,
        );
        assert_eq!(refused["errors"], 2);
        assert_eq!(refused["passed"], false);
        assert_eq!(
            refused["failed_cases"][0],
            json!({"id":"pass","error":"rebuilt tree differs from recorded tree"})
        );
        assert_eq!(
            refused["failed_cases"][1]["error"],
            "no readable production verdict"
        );
    }

    #[test]
    fn every_case_tree_hides_every_expectation_store() {
        assert_eq!(
            HIDDEN,
            [
                ".dagq/agents/*/evals",
                ".dagq/agent-cases",
                ".dagq/role-evals"
            ]
        );
    }
}
