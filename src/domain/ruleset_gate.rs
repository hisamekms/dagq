//! The ruleset gate of the team version's runner assignment (ADR-t4332-1,
//! amending ADR-t3987-1 decision 6). The control side assigns runs and jobs
//! to runners, and issues the runner App's tokens, only while the rulesets
//! it read from GitHub hold every prohibition R1–R6
//! (docs/deploy/github-rulesets.md) by rule, target ref and `active`
//! enforcement. Anything else is unverified and fails closed.
//!
//! The bypass is judged as ADR-t3987-1 decision 6 sets it, for every
//! actor: nobody bypasses R2 and R5, only the main App bypasses R1 and R6,
//! and the runner App bypasses nothing (R3 and R4 may name others the
//! project chooses). GitHub's `current_user_can_bypass` on the runner App's
//! own read must be `never`, and `bypass_actors` must be returned and keep
//! to the prohibition's bypass. A ruleset whose `bypass_actors` is not
//! returned (GitHub returns it only to a caller with write access to the
//! ruleset) holds nothing: the bypass cannot be verified, so assignment
//! stays held. A ruleset's name is never judged (it is provisional, a
//! person may rename it).

use serde::Deserialize;
use serde_json::{Value, json};

use super::EventKind;
use super::views::RunEvent;

/// The branch namespaces of run branches and landing candidates.
pub const RUN_NAMESPACE: &str = "refs/heads/dagq/**";
pub const LANDING_NAMESPACE: &str = "refs/heads/dagq-landing/**";
pub const MAIN: &str = "refs/heads/main";
/// GitHub's pattern for every ref of the ruleset's target.
pub const ALL: &str = "~ALL";

/// What the gate reads of one ruleset (`GET /repos/{owner}/{repo}/rulesets/{id}`).
/// Every other field, `name` included, is ignored. A missing `target`,
/// enforcement, condition or `current_user_can_bypass` holds nothing.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Ruleset {
    pub id: u64,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub enforcement: Option<String>,
    #[serde(default)]
    pub conditions: Option<Conditions>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// Returned only to a caller with write access to the ruleset; kept as
    /// read so that a malformed list holds nothing.
    #[serde(default)]
    pub bypass_actors: Option<Value>,
    /// The bypass of the caller, the runner App's read token: `always`,
    /// `pull_requests_only`, `never` or `exempt`. Only `never` holds.
    #[serde(default)]
    pub current_user_can_bypass: Option<Value>,
}

/// Only `ref_name` is read. An Organization ruleset's repository condition
/// is not: the repository-level list returns only the ones that apply.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Conditions {
    #[serde(default)]
    pub ref_name: Option<RefName>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct RefName {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Rule {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub parameters: Option<Value>,
}

/// The prohibitions of docs/deploy/github-rulesets.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prohibition {
    R1,
    R2,
    R3,
    R4,
    R5,
    R6,
}

/// What a prohibition requires. Every pattern of `include` must be named by
/// the ruleset's include (literally, or by `~ALL`), and every pattern the
/// ruleset excludes must be one of `may_exclude`, patterns disjoint from
/// the refs the prohibition covers. Matching patterns more loosely than
/// this is not attempted: an unknown pattern fails closed.
struct Requirement {
    target: &'static str,
    include: &'static [&'static str],
    may_exclude: &'static [&'static str],
    rules: &'static [&'static str],
    bypass: Bypass,
}

/// Who may bypass a prohibition's ruleset (ADR-t3987-1 decision 6).
#[derive(Clone, Copy)]
enum Bypass {
    Nobody,
    MainAppOnly,
    /// Any actor the project chooses but the runner App.
    NotRunnerApp,
}

/// The two GitHub Apps' ids: `bypass_actors` names an App by its id
/// (`actor_type: Integration`). `main` is `None` when the deployment names
/// no main App; then only an empty bypass list keeps to R1 and R6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Apps {
    pub runner: u64,
    pub main: Option<u64>,
}

const NAMESPACES: &[&str] = &[RUN_NAMESPACE, LANDING_NAMESPACE];
const OUTSIDE: &[&str] = &[MAIN, RUN_NAMESPACE, LANDING_NAMESPACE];

impl Prohibition {
    pub const ALL: [Self; 6] = [Self::R1, Self::R2, Self::R3, Self::R4, Self::R5, Self::R6];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::R1 => "R1",
            Self::R2 => "R2",
            Self::R3 => "R3",
            Self::R4 => "R4",
            Self::R5 => "R5",
            Self::R6 => "R6",
        }
    }

    const fn requirement(self) -> Requirement {
        match self {
            Self::R1 => Requirement {
                target: "branch",
                include: &[MAIN],
                may_exclude: NAMESPACES,
                rules: &["update"],
                bypass: Bypass::MainAppOnly,
            },
            Self::R2 => Requirement {
                target: "branch",
                include: &[MAIN],
                may_exclude: NAMESPACES,
                rules: &["deletion", "non_fast_forward"],
                bypass: Bypass::Nobody,
            },
            Self::R3 => Requirement {
                target: "branch",
                include: &[ALL],
                may_exclude: OUTSIDE,
                rules: &["creation", "update", "deletion"],
                bypass: Bypass::NotRunnerApp,
            },
            Self::R4 => Requirement {
                target: "tag",
                include: &[ALL],
                may_exclude: &[],
                rules: &["creation", "update", "deletion"],
                bypass: Bypass::NotRunnerApp,
            },
            Self::R5 => Requirement {
                target: "branch",
                include: NAMESPACES,
                may_exclude: &[MAIN],
                rules: &["non_fast_forward"],
                bypass: Bypass::Nobody,
            },
            Self::R6 => Requirement {
                target: "branch",
                include: NAMESPACES,
                may_exclude: &[MAIN],
                rules: &["deletion"],
                bypass: Bypass::MainAppOnly,
            },
        }
    }
}

/// What was read from GitHub, by the infrastructure's adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum Read {
    Rulesets(Vec<Ruleset>),
    /// Any failure to read or parse the list or a ruleset.
    Unreadable,
    /// GitHub refused rulesets for the repository's plan.
    PlanUnavailable,
}

/// One rule of a prohibition no active ruleset holds, with the rulesets
/// read that have that rule (and why each does not count) as `read`.
#[derive(Debug, Clone, PartialEq)]
pub struct Missing {
    pub prohibition: Prohibition,
    pub rule: &'static str,
    pub read: Vec<Value>,
}

/// Why assignment is held. Each reason is safe to publish: it carries no
/// credential, URL or upstream body, only ruleset ids and their values.
#[derive(Debug, Clone, PartialEq)]
pub enum Unverified {
    /// No gate is configured for this deployment.
    NotConfigured,
    Unreadable,
    PlanUnavailable,
    /// No result read yet, or the kept one is past [`usable`].
    Stale,
    Missing(Vec<Missing>),
}

impl Unverified {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured => "ruleset_gate_unconfigured",
            Self::Unreadable => "ruleset_unreadable",
            Self::PlanUnavailable => "ruleset_plan_unavailable",
            Self::Stale => "ruleset_unread",
            Self::Missing(_) => "ruleset_prohibition_missing",
        }
    }

    /// The reason as one line for `status` and the operations ask.
    pub fn message(&self) -> String {
        match self {
            Self::NotConfigured => "no ruleset gate is configured: runners get no work".into(),
            Self::Unreadable => "the repository's rulesets could not be read".into(),
            Self::Stale => "no recent read of the repository's rulesets".into(),
            Self::PlanUnavailable => "GitHub refused rulesets for the repository's plan".into(),
            Self::Missing(missing) => {
                let parts = missing
                    .iter()
                    .map(|m| {
                        format!(
                            "{} {}: read {}",
                            m.prohibition.as_str(),
                            m.rule,
                            Value::Array(m.read.clone())
                        )
                    })
                    .collect::<Vec<_>>();
                format!("prohibitions missing: {}", parts.join("; "))
            }
        }
    }

    pub fn payload(&self) -> Value {
        let mut payload = json!({"reason": self.code(), "message": self.message()});
        if let Self::Missing(missing) = self {
            payload["missing"] = missing
                .iter()
                .map(|m| json!({"prohibition": m.prohibition.as_str(), "rule": m.rule, "read": m.read}))
                .collect();
        }
        payload
    }
}

/// Whether the read rulesets hold every prohibition, each by a ruleset
/// whose bypass keeps to the prohibition's ([`Apps`] names the two Apps). A
/// prohibition's rules may be held by different rulesets.
pub fn judge(read: &Read, apps: Apps) -> Result<(), Unverified> {
    let rulesets = match read {
        Read::Rulesets(rulesets) => rulesets,
        Read::Unreadable => return Err(Unverified::Unreadable),
        Read::PlanUnavailable => return Err(Unverified::PlanUnavailable),
    };
    let mut missing = Vec::new();
    for prohibition in Prohibition::ALL {
        let requirement = prohibition.requirement();
        for &rule in requirement.rules {
            let mut with_rule = Vec::new();
            let mut without = Vec::new();
            let mut held = false;
            for ruleset in rulesets {
                match ruleset.rules.iter().find(|r| r.kind == rule) {
                    None => without.push(values(ruleset, None, "does not have the rule")),
                    Some(found) => match why_not(ruleset, found, &requirement, apps) {
                        None => held = true,
                        Some(why) => with_rule.push(values(ruleset, Some(found), why)),
                    },
                }
            }
            if !held {
                // The rulesets that have the rule first; bound the size.
                let mut read = with_rule;
                read.extend(without);
                read.truncate(MAX_READ);
                missing.push(Missing {
                    prohibition,
                    rule,
                    read,
                });
            }
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Unverified::Missing(missing))
    }
}

/// How many rulesets a missing rule's reason names, and how many patterns
/// and rules of each.
const MAX_READ: usize = 5;

/// Why a ruleset with the rule does not hold it for the requirement.
fn why_not(
    ruleset: &Ruleset,
    rule: &Rule,
    requirement: &Requirement,
    apps: Apps,
) -> Option<&'static str> {
    if ruleset.enforcement.as_deref() != Some("active") {
        return Some("enforcement is not active");
    }
    if ruleset.target.as_deref() != Some(requirement.target) {
        return Some("target differs");
    }
    let refs = ruleset
        .conditions
        .as_ref()
        .and_then(|c| c.ref_name.as_ref())
        .cloned()
        .unwrap_or_default();
    let named = |pattern: &str| refs.include.iter().any(|p| p == ALL || p == pattern);
    if !requirement.include.iter().all(|p| named(p)) {
        return Some("does not include the refs");
    }
    if !refs
        .exclude
        .iter()
        .all(|p| requirement.may_exclude.contains(&p.as_str()))
    {
        return Some("excludes refs it must cover");
    }
    // An update rule that lets the branch take its upstream's changes holds
    // less than the prohibition.
    if rule
        .parameters
        .as_ref()
        .and_then(|p| p.get("update_allows_fetch_and_merge"))
        .is_some_and(|v| v != &Value::Bool(false))
    {
        return Some("update allows fetch and merge");
    }
    if ruleset
        .current_user_can_bypass
        .as_ref()
        .and_then(Value::as_str)
        != Some("never")
    {
        return Some("the runner App may bypass it (current_user_can_bypass is not never)");
    }
    let Some(bypass) = &ruleset.bypass_actors else {
        return Some("bypass_actors is not returned, so the bypass cannot be verified");
    };
    let Value::Array(actors) = bypass else {
        return Some("bypass_actors cannot be read");
    };
    let app = |actor: &Value, id: u64| {
        actor["actor_type"] == "Integration" && actor["actor_id"].as_u64() == Some(id)
    };
    match requirement.bypass {
        Bypass::Nobody if !actors.is_empty() => {
            return Some("a bypass actor is set where nobody may bypass");
        }
        Bypass::MainAppOnly
            if !actors
                .iter()
                .all(|actor| apps.main.is_some_and(|main| app(actor, main))) =>
        {
            return Some("a bypass actor other than the main App is set");
        }
        Bypass::NotRunnerApp if actors.iter().any(|actor| app(actor, apps.runner)) => {
            return Some("the runner App is a bypass actor");
        }
        _ => {}
    }
    None
}

/// At most [`MAX_READ`] entries of a list, so a ruleset with many patterns
/// or rules cannot make the reason, its events and the ask unbounded.
fn bounded<T>(list: &[T]) -> &[T] {
    &list[..list.len().min(MAX_READ)]
}

/// The values read of a ruleset, for the reason: its target, enforcement,
/// refs, rule types, the found rule's parameters and the bypass values.
/// Not its name.
fn values(ruleset: &Ruleset, rule: Option<&Rule>, why: &str) -> Value {
    let refs = ruleset
        .conditions
        .as_ref()
        .and_then(|c| c.ref_name.as_ref());
    let rules: Vec<&str> = ruleset.rules.iter().map(|r| r.kind.as_str()).collect();
    let mut value = json!({
        "id": ruleset.id,
        "target": ruleset.target,
        "enforcement": ruleset.enforcement,
        "include": refs.map(|r| bounded(&r.include)),
        "exclude": refs.map(|r| bounded(&r.exclude)),
        "rules": bounded(&rules),
        "current_user_can_bypass": ruleset.current_user_can_bypass,
        "why": why,
    });
    if let Some(parameters) = rule.and_then(|r| r.parameters.as_ref()) {
        value["parameters"] = parameters.clone();
    }
    if let Some(Value::Array(actors)) = &ruleset.bypass_actors {
        value["bypass_actors"] = json!(bounded(actors));
    }
    value
}

/// How long a verified result is believed before GitHub is read again.
pub const VERIFIED_SECS: i64 = 300;
/// How long an unverified result is kept, so that a fixed ruleset resumes
/// assignment soon without reading GitHub on every pull.
pub const UNVERIFIED_SECS: i64 = 30;

/// Whether to read GitHub again, given when the last result was read and
/// whether it verified. A clock that went back reads again.
pub fn reread_due(last: Option<(i64, bool)>, now: i64) -> bool {
    last.is_none_or(|(at, verified)| {
        let keep = if verified {
            VERIFIED_SECS
        } else {
            UNVERIFIED_SECS
        };
        now < at || now.saturating_sub(at) >= keep
    })
}

/// Whether a kept result may still be used at `now`: up to one more
/// period past its own, so a read that is late or stuck (the reads run off
/// the write line) leaves assignment held rather than trusting an old result.
pub fn usable(last: (i64, bool), now: i64) -> bool {
    let keep = if last.1 {
        VERIFIED_SECS
    } else {
        UNVERIFIED_SECS
    };
    now >= last.0 && now.saturating_sub(last.0) < 2 * keep
}

/// The pair of queue events of the hold.
pub const HOLD_KINDS: [EventKind; 2] = [
    EventKind::RunnerRulesetsHeld,
    EventKind::RunnerRulesetsResumed,
];

/// A change of the hold to record.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub kind: EventKind,
    pub payload: Value,
    /// A hold begins: open the operations ask, once for the whole hold,
    /// however its reason changes until it is resumed.
    pub open_ask: bool,
}

/// The change from the latest hold record (`last`, of [`HOLD_KINDS`]) to
/// the gate's result now: `held` when it starts or its reason changes,
/// `resumed` when it ends, else nothing.
pub fn transition(now: Result<(), &Unverified>, last: Option<&RunEvent>) -> Option<Change> {
    let held = last.filter(|e| e.kind == EventKind::RunnerRulesetsHeld);
    match now {
        Ok(()) => held.map(|e| Change {
            kind: EventKind::RunnerRulesetsResumed,
            payload: json!({"reason": e.payload.get("reason")}),
            open_ask: false,
        }),
        Err(reason) => {
            let payload = reason.payload();
            if held.is_some_and(|e| {
                e.payload.get("reason") == payload.get("reason")
                    && e.payload.get("message") == payload.get("message")
            }) {
                return None;
            }
            Some(Change {
                kind: EventKind::RunnerRulesetsHeld,
                payload,
                open_ask: held.is_none(),
            })
        }
    }
}

/// The operations ask's question, opened as a hold begins.
pub fn ask_question(reason: &Unverified) -> String {
    format!(
        "The control side assigns no run or job to runners and issues no runner App token: GitHub's rulesets could not be verified to hold the prohibitions R1-R6 of docs/deploy/github-rulesets.md ({}: {}). A person who administers the repository fixes the rulesets or the deployment settings; assignment resumes by itself once they verify. `dagq status` shows the reason while it lasts, and the runtime closes this ask when assignment resumes; answer `done` only to acknowledge it.",
        reason.code(),
        reason.message()
    )
}

/// The reference rulesets R1–R6 of docs/deploy/github-rulesets.md as
/// GitHub answers them, for the tests of the gate and its users.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn reference() -> Vec<Value> {
        let branch = |id: u64, include: &[&str], exclude: &[&str], rules: &[&str]| {
            json!({
                "id": id, "name": format!("dagq-{id}"), "target": "branch", "enforcement": "active",
                "source_type": "Repository", "current_user_can_bypass": "never",
                "conditions": {"ref_name": {"include": include, "exclude": exclude}},
                "rules": rules.iter().map(|r| json!({"type": r})).collect::<Vec<_>>(),
            })
        };
        vec![
            branch(1, &[MAIN], &[], &["update"]),
            branch(2, &[MAIN], &[], &["deletion", "non_fast_forward"]),
            branch(
                3,
                &[ALL],
                &[MAIN, RUN_NAMESPACE, LANDING_NAMESPACE],
                &["creation", "update", "deletion"],
            ),
            json!({
                "id": 4, "name": "dagq-tags", "target": "tag", "enforcement": "active",
                "current_user_can_bypass": "never",
                "conditions": {"ref_name": {"include": [ALL], "exclude": []}},
                "rules": [{"type": "creation"}, {"type": "update"}, {"type": "deletion"}],
            }),
            branch(
                5,
                &[RUN_NAMESPACE, LANDING_NAMESPACE],
                &[],
                &["non_fast_forward"],
            ),
            branch(6, &[RUN_NAMESPACE, LANDING_NAMESPACE], &[], &["deletion"]),
        ]
        .into_iter()
        .zip([
            json!([{"actor_type": "Integration", "actor_id": MAIN_APP, "bypass_mode": "always"}]),
            json!([]),
            json!([{"actor_type": "RepositoryRole", "actor_id": 5, "bypass_mode": "always"}]),
            json!([{"actor_type": "RepositoryRole", "actor_id": 5, "bypass_mode": "always"}]),
            json!([]),
            json!([{"actor_type": "Integration", "actor_id": MAIN_APP, "bypass_mode": "always"}]),
        ])
        .map(|(mut ruleset, bypass)| {
            ruleset["bypass_actors"] = bypass;
            ruleset
        })
        .collect()
    }

    pub(crate) fn read(values: &[Value]) -> Read {
        Read::Rulesets(
            values
                .iter()
                .map(|v| serde_json::from_value(v.clone()).unwrap())
                .collect(),
        )
    }

    /// The runner App's id the tests read with.
    pub(crate) const RUNNER_APP: u64 = 11;
    /// The main App's id of the reference rulesets' bypass.
    pub(crate) const MAIN_APP: u64 = 22;
    pub(crate) const APPS: Apps = Apps {
        runner: RUNNER_APP,
        main: Some(MAIN_APP),
    };

    pub(crate) fn reference_read() -> Read {
        read(&reference())
    }
}

#[cfg(test)]
mod tests;
