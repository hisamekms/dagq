//! Cases and scoring for whole verdict roles. Snapshot deserialization and
//! production prompt assembly belong to the application harness.
use super::{Case, CaseBase, RoleCase, Source, Split, ratio};
use crate::domain::plan_review::{PlanReviewDecision, PlanReviewVerdict};
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerdictCase {
    pub id: String,
    pub source: String,
    pub made_by: String,
    pub snapshot: Value,
    pub expected: Expected,
    pub k: Option<u32>,
    pub disputed: Option<Value>,
    pub adjudicated: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    pub verdict: PlanReviewDecision,
    pub codes: BTreeSet<String>,
    #[serde(default)]
    pub acceptable_codes: BTreeSet<String>,
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
    let list: List = serde_json::from_str(text).map_err(|e| e.to_string())?;
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
        {
            return Err(format!("invalid id, source, made_by or k: {}", c.id));
        }
        let e = &c.expected;
        if !e.codes.is_subset(&list.codes)
            || !e.acceptable_codes.is_subset(&list.codes)
            || !e.codes.is_disjoint(&e.acceptable_codes)
            || (e.verdict == PlanReviewDecision::Pass && !e.codes.is_empty())
            || (e.verdict != PlanReviewDecision::Pass && e.codes.is_empty())
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
    /// cases. `patch` indexes the snapshot bytes, never a change patch.
    /// Provenance, expectations and repetition counts do not mint a new
    /// held-out key for the same inputs.
    pub fn into_case(self, main: String) -> Case {
        let bytes = serde_json::to_vec(&self.snapshot).expect("JSON snapshot");
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

/// Exact production decisions: revise and concern remain distinct. Errors
/// count in neither decision nor reason counts, and fail the round.
pub fn score(
    cases: &[Case],
    runs: &[(&str, Option<&Value>)],
    threshold: f64,
    disputed: bool,
) -> Value {
    let mut decisions = [Counts::default(), Counts::default(), Counts::default()];
    let values = [
        PlanReviewDecision::Pass,
        PlanReviewDecision::Revise,
        PlanReviewDecision::Concern,
    ];
    let mut codes = std::collections::BTreeMap::<String, Counts>::new();
    let (mut errors, mut total) = (0, 0);
    let mut failed_cases = Vec::new();
    for (id, result) in runs {
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
        let Some(v) = result.and_then(|v| PlanReviewVerdict::parse(&v.to_string()).ok()) else {
            errors += 1;
            failed_cases.push(json!({"id":id,"error":"no readable production verdict"}));
            continue;
        };
        for (counts, decision) in decisions.iter_mut().zip(values) {
            counts.add(c.expected.verdict == decision, v.verdict == decision);
        }
        let actual: BTreeSet<String> = v.reason_codes.into_iter().flatten().collect();
        if v.verdict != c.expected.verdict
            || !c.expected.codes.is_subset(&actual)
            || actual.iter().any(|code| {
                !c.expected.codes.contains(code) && !c.expected.acceptable_codes.contains(code)
            })
        {
            failed_cases
                .push(json!({"id":id,"verdict":v.verdict,"codes":actual,"reasons":v.reasons}));
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
            ("pass", Some(&pass)),
            ("revise", Some(&revise)),
            ("concern", Some(&concern)),
        ];
        let scores = score(&cases, &good, 0.9, false);
        assert_eq!(scores["passed"], true, "{scores}");
        assert_eq!(scores["per_code"]["P-1"]["recall"], 1.0);
        let wrong = score(
            &cases,
            &[
                ("pass", Some(&pass)),
                ("revise", Some(&concern)),
                ("concern", Some(&concern)),
            ],
            0.9,
            false,
        );
        assert_eq!(wrong["passed"], false);
        assert_eq!(wrong["per_verdict"]["revise"]["recall"], 0.0);
        let mut cases = cases;
        cases[1].disputed = Some(json!("disagree"));
        let error = [
            ("pass", Some(&pass)),
            ("revise", None),
            ("concern", Some(&concern)),
        ];
        assert_eq!(score(&cases, &error, 0.9, false)["errors"], 0);
        assert_eq!(score(&cases, &error, 0.9, true)["errors"], 1);
        assert_eq!(
            score(
                &cases,
                &[("pass", Some(&json!({"verdict":"pass"})))],
                0.9,
                false
            )["errors"],
            1
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
