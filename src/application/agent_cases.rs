//! The production sets of an agent's cases as a use case of the queue
//! (ADR-t1728-1 (g)): `agent cases` records a request, which the
//! supervisor makes into a set (`supervise::agent_cases`); `agent decide`
//! records a person's decision on a case's label; `agent sets` and
//! `agent set` read them back from their events alone. Asking and deciding
//! need `eval.cases` (a person's, the inbox's or a planner's: never a
//! worker's, nor the supervisor's, which only makes the sets it is asked
//! for), reading `eval.read` on no run.
//!
//! A set's files on the queue's data dir ([`view`]) are a view: the case
//! list and the patches are made again from the events and Git
//! (`git diff <base>...<head>`), and a case whose commits are gone is left
//! out with why.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::commands::{DenialLog, Gate};
use super::{Repository, RunFiles};
use crate::domain::agent_eval::PATCH_EXTENSION;
use crate::domain::agent_eval::production::{
    self, CaseSet, CasesRequest, Decision, Label, check_label,
};
use crate::domain::agent_eval::review::Judgement;
use crate::domain::agent_eval::round::case_set_digest;
use crate::domain::review_subagents::valid_agent_name;
use crate::domain::{ActorContext, Authorizer, Capability, EventId, EventKind, Resource, RunEvent};

/// Where the production sets' events are written and read.
pub trait CaseSetStore: DenialLog {
    /// Record a queue event of the production sets.
    fn record_cases_event(&self, kind: EventKind, payload: Value) -> Result<EventId>;
    /// Every event of their kinds ([`production::KINDS`]), oldest first.
    fn cases_events(&self) -> Result<Vec<RunEvent>>;
}

/// The sets the events say, oldest first.
pub fn sets_of(events: &[RunEvent]) -> Vec<CaseSet> {
    production::sets(
        events
            .iter()
            .map(|event| (event.id.as_i64(), event.kind.as_str(), &event.payload)),
    )
}

/// The production sets' commands of one actor on one store.
pub struct AgentCaseSets<'a> {
    store: &'a dyn CaseSetStore,
    gate: Gate<'a>,
}

impl<'a> AgentCaseSets<'a> {
    pub fn new(
        store: &'a dyn CaseSetStore,
        actor: &'a ActorContext,
        authorizer: &'a dyn Authorizer,
    ) -> Self {
        Self {
            store,
            gate: Gate { actor, authorizer },
        }
    }

    fn actor(&self) -> &ActorContext {
        self.gate.actor
    }

    /// The sets as the actor names them: on its run, for a worker, whom
    /// the policy refuses all the same.
    fn own(&self) -> Resource {
        Resource::AgentEval {
            run: self.actor().run_id().cloned(),
        }
    }

    fn sets(&self) -> Result<Vec<CaseSet>> {
        Ok(sets_of(&self.store.cases_events()?))
    }

    /// `agent cases <agent>`: record the request as the actor. The
    /// supervisor makes the set.
    pub fn request(
        &self,
        agent: &str,
        since: Option<String>,
        until: Option<String>,
    ) -> Result<Value> {
        if !valid_agent_name(agent) {
            bail!("agent {agent:?} is not kebab-case (lowercase letters and digits joined by -)");
        }
        // Compared as instants, and recorded so: a date or another offset
        // names the same instant as the queue's times.
        let period = match production::period(since.as_deref(), until.as_deref()) {
            Ok(period) => period,
            Err(why) => bail!("{why}"),
        };
        self.gate
            .authorize(self.store, Capability::EvalCases, &self.own())?;
        let actor = self.actor();
        let request = CasesRequest {
            agent: agent.to_owned(),
            since: period.since.map(crate::domain::marks::utc_text),
            until: period.until.map(crate::domain::marks::utc_text),
            requested_by: actor.role().as_str().to_owned(),
            requested_by_id: actor.actor_id().to_owned(),
        };
        let id = self
            .store
            .record_cases_event(EventKind::AgentCasesRequested, request.record())?;
        Ok(json!({"set_id": id, "agent": agent, "status": "waiting"}))
    }

    /// `agent decide <set> <case>`: record a person's label of a case of
    /// the set, which clears its dispute; it must fit the set's codes.
    pub fn decide(
        &self,
        set_id: i64,
        case: &str,
        verdict: &str,
        codes: Vec<String>,
        reason: &str,
    ) -> Result<Value> {
        self.gate
            .authorize(self.store, Capability::EvalCases, &self.own())?;
        let Some(judgement) = Judgement::parse(verdict) else {
            bail!("--verdict is violation or clean, not {verdict:?}");
        };
        if reason.trim().is_empty() {
            bail!("--reason says why the label is right");
        }
        let sets = self.sets()?;
        let Some(set) = sets.iter().find(|set| set.id == set_id) else {
            bail!("no production set {set_id}");
        };
        if !set.cases().iter().any(|labeled| labeled.case.id() == case) {
            bail!("set {set_id} has no labeled case {case}");
        }
        let known = set
            .frame
            .as_ref()
            .map(|frame| frame.codes.clone())
            .unwrap_or_default();
        let mut codes = codes;
        codes.sort();
        codes.dedup();
        if let Err(why) = check_label(judgement, &codes, &known) {
            bail!("{why}");
        }
        let actor = self.actor();
        let decision = Decision {
            label: Label {
                judgement,
                codes,
                reason: reason.trim().to_owned(),
            },
            decided_by: actor.role().as_str().to_owned(),
            decided_by_id: actor.actor_id().to_owned(),
        };
        let id = self
            .store
            .record_cases_event(EventKind::AgentCasesDecided, decision.record(set_id, case))?;
        Ok(json!({"event_id": id, "set_id": set_id, "case": case, "verdict": verdict}))
    }

    /// `agent sets`: the sets, newest first, `agent`'s only when given, at
    /// most `limit`.
    pub fn list(&self, agent: Option<&str>, limit: usize) -> Result<Value> {
        self.gate.authorize(
            self.store,
            Capability::EvalRead,
            &Resource::AgentEval { run: None },
        )?;
        let sets = self.sets()?;
        let listed: Vec<Value> = sets
            .iter()
            .rev()
            .filter(|set| agent.is_none_or(|agent| set.request.agent == agent))
            .take(limit)
            .map(summary)
            .collect();
        Ok(json!({"sets": listed}))
    }

    /// `agent set <id>`: one set whole.
    pub fn show(&self, id: i64) -> Result<Value> {
        self.gate.authorize(
            self.store,
            Capability::EvalRead,
            &Resource::AgentEval { run: None },
        )?;
        let sets = self.sets()?;
        let Some(set) = sets.iter().find(|set| set.id == id) else {
            bail!("no production set {id}");
        };
        let mut value = summary(set);
        value["request"] = set.request.record();
        value["refusal"] = json!(set.refused);
        value["frame"] = json!(set.frame.as_ref().map(production::Frame::record));
        value["skipped"] = json!(set.skipped);
        value["labels"] = json!(
            set.labels
                .iter()
                .map(|(case, record)| json!({
                    "case": case,
                    "label": record.label.as_ref().map(Label::record),
                    "error": record.error,
                    "made_by": record.made_by,
                }))
                .collect::<Vec<_>>()
        );
        value["cases"] = json!(set.case_list().map(|list| list["cases"].clone()));
        Ok(value)
    }
}

/// A set in a list: its request, where it is, its digest and its disputed
/// cases.
fn summary(set: &CaseSet) -> Value {
    let cases = set.cases();
    json!({
        "set_id": set.id,
        "agent": set.request.agent,
        "since": set.request.since,
        "until": set.request.until,
        "status": set.status(),
        "owner": set.owner,
        "requested_by": set.request.requested_by,
        "refused": set.refused.as_ref().map(|refused| &refused["reason"]),
        "digest": set.built.as_ref().map(|built| &built.digest),
        "cases": cases.len(),
        "skipped": set.skipped.len(),
        "unlabeled": set.unlabeled().len(),
        "disputed": cases
            .iter()
            .filter_map(|case| case.disputed.as_ref().map(|why| json!({"case": case.case.id(), "reason": why})))
            .collect::<Vec<_>>(),
    })
}

/// The directory of set `id` under `root` (the queue's
/// [`production::SETS_DIR`]).
pub fn set_dir(root: &Path, id: i64) -> PathBuf {
    root.join(id.to_string())
}

fn patches_dir(dir: &Path) -> PathBuf {
    dir.join("patches")
}

fn patch_file(dir: &Path, hash: &str) -> PathBuf {
    patches_dir(dir).join(format!("{hash}.{PATCH_EXTENSION}"))
}

/// The SHA-256 of `bytes` in lowercase hex, a patch's name.
fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// Write the change `<base>...<head>` into set directory `dir`'s patches
/// by the SHA-256 of its content; the hash.
pub fn write_patch(
    repository: &dyn Repository,
    files: &dyn RunFiles,
    dir: &Path,
    (base, head): (&str, &str),
) -> Result<String> {
    let patches = patches_dir(dir);
    files
        .create_dir_all(&patches)
        .with_context(|| format!("create {}", patches.display()))?;
    let incoming = patches.join(format!("incoming-{base}-{head}"));
    if files.exists(&incoming) {
        files.remove_file(&incoming)?;
    }
    repository.diff_to_file(base, head, &incoming)?;
    let bytes = files
        .read(&incoming)
        .with_context(|| format!("read {}", incoming.display()))?;
    let hash = sha256(&bytes);
    files
        .rename(&incoming, &patch_file(dir, &hash))
        .with_context(|| format!("name the patch of {base}...{head}"))?;
    Ok(hash)
}

/// A set as its files hold it: its case list (that of `production.json`)
/// with the cases whose patch is there, each patch by its hash, the cases
/// left out with why, and its digest.
#[derive(Debug, Clone, PartialEq)]
pub struct SetView {
    pub list: Value,
    pub patches: BTreeMap<String, String>,
    pub left_out: Vec<(String, String)>,
    pub digest: String,
}

/// Why a case whose change is not UTF-8 text (a file in another encoding)
/// is left out: the agent job's material and the case list read text only.
fn not_text(base: &str, head: &str) -> String {
    format!("the change {base}...{head} is not UTF-8 text")
}

/// The file of set directory `dir` that holds its case list.
pub fn cases_file(dir: &Path) -> PathBuf {
    dir.join("cases.json")
}

/// Make set `set`'s files in `dir` again from its events and Git. A case
/// whose base or head no ref reaches is left out with why first, whether
/// or not its patch's file is still there, so the view is the same with
/// the files kept or lost (the case's tree starts from its base, too).
/// Each other case's patch is read from its file when its content still
/// has its hash, else written again from the diff `<base>...<head>`; a
/// case whose change cannot be read, reads otherwise now or is not UTF-8
/// text is left out with why, so one case never stops its set. The case
/// list is written as `cases.json`.
pub fn view(
    set: &CaseSet,
    repository: &dyn Repository,
    files: &dyn RunFiles,
    dir: &Path,
) -> Result<SetView> {
    let mut list = set
        .case_list()
        .with_context(|| format!("set {} is not prepared", set.id))?;
    let mut patches = BTreeMap::new();
    let mut left_out = Vec::new();
    let mut kept = Vec::new();
    for case in list["cases"].as_array().cloned().unwrap_or_default() {
        let id = case["id"].as_str().unwrap_or_default().to_owned();
        let hash = case["patch"].as_str().unwrap_or_default().to_owned();
        let base = case["base_commit"].as_str().unwrap_or_default();
        let head = case["review"]["production"]["head"]
            .as_str()
            .unwrap_or_default();
        let there = |commit: &str| repository.reachable_commit(commit).unwrap_or(false);
        if let Some(why) = production::unreachable((base, there(base)), (head, there(head))) {
            left_out.push((id, why));
            continue;
        }
        let path = patch_file(dir, &hash);
        let text = match files.read(&path) {
            Ok(bytes) if sha256(&bytes) == hash => String::from_utf8(bytes).ok(),
            _ => None,
        };
        let text = match text {
            Some(text) => text,
            None => {
                let again = match write_patch(repository, files, dir, (base, head)) {
                    Ok(again) => again,
                    Err(error) => {
                        left_out.push((
                            id,
                            format!("the change {base}...{head} could not be read: {error:#}"),
                        ));
                        continue;
                    }
                };
                if again != hash {
                    left_out.push((
                        id,
                        format!("the change {base}...{head} reads otherwise now (patch {again}, not {hash})"),
                    ));
                    continue;
                }
                match files
                    .read(&path)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                {
                    Some(text) => text,
                    None => {
                        left_out.push((id, not_text(base, head)));
                        continue;
                    }
                }
            }
        };
        patches.insert(hash, text);
        kept.push(case);
    }
    let digest = case_set_digest(kept.iter().filter_map(|case| {
        let patch = patches.get(case["patch"].as_str()?)?;
        Some((case["id"].as_str()?, patch.as_bytes()))
    }));
    list["cases"] = json!(kept);
    files
        .create_dir_all(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let file = cases_file(dir);
    files
        .write(&file, serde_json::to_string_pretty(&list)?.as_bytes())
        .with_context(|| format!("write {}", file.display()))?;
    Ok(SetView {
        list,
        patches,
        left_out,
        digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::StaticPolicy;
    use crate::domain::actor::ActorRole;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Store {
        events: Mutex<Vec<RunEvent>>,
        denials: Mutex<Vec<Value>>,
    }

    impl DenialLog for Store {
        fn record_denial(&self, payload: Value) -> Result<()> {
            self.denials.lock().unwrap().push(payload);
            Ok(())
        }
    }

    impl CaseSetStore for Store {
        fn record_cases_event(&self, kind: EventKind, payload: Value) -> Result<EventId> {
            let mut events = self.events.lock().unwrap();
            let id = EventId::new(i64::try_from(events.len()).unwrap() + 1);
            events.push(RunEvent {
                id,
                task_id: None,
                goal_id: None,
                run_id: None,
                kind: kind.as_str().to_owned(),
                payload,
                created_at: String::new(),
                actor: None,
            });
            Ok(id)
        }
        fn cases_events(&self) -> Result<Vec<RunEvent>> {
            Ok(self.events.lock().unwrap().clone())
        }
    }

    fn actor(role: ActorRole) -> ActorContext {
        ActorContext::instance(role, "1")
    }

    /// Asking for a set and deciding a label are the user's, the inbox's
    /// and a planner's; a worker and the supervisor are refused, and the
    /// refusal recorded (ADR-t1728-1 (e)).
    #[test]
    fn only_a_person_the_inbox_or_a_planner_asks_for_a_set() {
        for role in [ActorRole::User, ActorRole::Inbox, ActorRole::Planner] {
            let store = Store::default();
            let actor = actor(role);
            let sets = AgentCaseSets::new(&store, &actor, &StaticPolicy);
            let asked = sets
                .request("adr-rules", Some("2026-10-05".into()), None)
                .unwrap();
            assert_eq!(asked["status"], "waiting", "{role:?}");
            let recorded = &store.events.lock().unwrap()[0];
            assert_eq!(recorded.kind, "agent_cases_requested");
            assert_eq!(recorded.payload["requested_by"], role.as_str());
            assert_eq!(recorded.payload["since"], "2026-10-05T00:00:00.000Z");
        }
        let worker = ActorContext::worker(
            &crate::domain::RunId::new("r1").unwrap(),
            crate::domain::TaskId::new(1),
        );
        for actor in [worker, actor(ActorRole::Supervisor)] {
            let store = Store::default();
            let sets = AgentCaseSets::new(&store, &actor, &StaticPolicy);
            let error = sets.request("adr-rules", None, None).unwrap_err();
            assert!(
                error.to_string().contains("eval.cases"),
                "{:?}: {error:#}",
                actor.role()
            );
            assert!(store.events.lock().unwrap().is_empty());
            assert_eq!(store.denials.lock().unwrap().len(), 1);
            let error = sets
                .decide(1, "c", "clean", Vec::new(), "fine")
                .unwrap_err();
            assert!(error.to_string().contains("eval.cases"), "{error:#}");
        }
        let store = Store::default();
        let user = actor(ActorRole::User);
        let sets = AgentCaseSets::new(&store, &user, &StaticPolicy);
        assert!(sets.request("Not Kebab", None, None).is_err());
        assert!(
            sets.request("x", Some("2026-10-05".into()), Some("2026-10-01".into()))
                .is_err()
        );
    }

    /// A decision names a labeled case of the set and fits its codes; it
    /// is recorded with who decided.
    #[test]
    fn a_decision_fits_the_sets_codes_and_is_recorded() {
        let store = Store::default();
        let user = actor(ActorRole::User);
        let sets = AgentCaseSets::new(&store, &user, &StaticPolicy);
        let set = sets.request("demo", None, None).unwrap()["set_id"]
            .as_i64()
            .unwrap();
        let case = json!({"id": "prod-r1-a1", "run_id": "r1", "attempt": 1, "base": "b",
                          "head": "h", "production": {"verdict": "pass", "reasons": []},
                          "reviewed_at": "t", "patch": "a".repeat(64)});
        store
            .record_cases_event(
                EventKind::AgentCasesPrepared,
                json!({"set_id": set, "cases": [case], "skipped": [],
                       "frame": {"codes": ["D-1"], "k": 1, "rules": [], "commit": "c"}}),
            )
            .unwrap();
        store
            .record_cases_event(
                EventKind::AgentCasesLabeled,
                json!({"set_id": set, "case": "prod-r1-a1", "made_by": "codex",
                       "label": {"verdict": "violation", "codes": ["D-1"], "reason": "r"}}),
            )
            .unwrap();
        assert!(sets.decide(set, "other", "clean", vec![], "x").is_err());
        assert!(
            sets.decide(set, "prod-r1-a1", "violation", vec![], "x")
                .is_err()
        );
        assert!(
            sets.decide(set, "prod-r1-a1", "violation", vec!["X-1".into()], "x")
                .is_err()
        );
        assert!(
            sets.decide(set, "prod-r1-a1", "clean", vec![], " ")
                .is_err()
        );
        let shown = sets.show(set).unwrap();
        assert_eq!(shown["disputed"][0]["case"], "prod-r1-a1", "{shown}");
        sets.decide(set, "prod-r1-a1", "clean", vec![], "allowed by the rule")
            .unwrap();
        let shown = sets.show(set).unwrap();
        assert_eq!(shown["disputed"], json!([]), "{shown}");
        assert_eq!(shown["cases"][0]["adjudicated"]["by"], user.actor_id());
        let listed = sets.list(Some("demo"), 10).unwrap();
        assert_eq!(listed["sets"][0]["status"], "labeling");
    }
}
