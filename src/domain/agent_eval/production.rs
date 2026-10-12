//! Production cases (ADR-t1728-1 (g)): a set of cases made from the
//! production reviews that selected an agent, kept on the queue's side and
//! run whole as a hold-out (`agent eval --set`). The Spike's
//! `runner/production_cases.py` `prepare`, `judge` and `build`
//! (dagq-agent-eval commit cfb8a4c) as the supervisor runs them:
//!
//! - prepare: each review of the period whose `review_started` selected
//!   the agent and whose `review_finished` holds the agent's judgment
//!   ([`candidates`]) is a case of its `<base>...<head>`, unless one of the
//!   two commits is not in the repository ([`unreachable`]), which is
//!   recorded with why and left out.
//! - label: a job of a provider other than the improving side's judges each
//!   case by the rules' text alone ([`label_of`]); the production verdict
//!   is never the label (it came from the definition of its day). A case
//!   whose label and production verdict disagree is a disputed candidate
//!   ([`dispute`]) until a person decides it.
//! - build: the labeled cases are the set, named by its request's event id
//!   and digested as a hold-out's cases are
//!   ([`super::round::case_set_digest`]).
//!
//! Every step is a queue event, and a set's state is read back from them
//! alone ([`sets`]); the set's files on the queue's data dir are a view
//! made again from them and Git. The events, each a queue event whose
//! payload names the set by `set_id` (the id of its request's event):
//!
//! - `agent_cases_requested`: a request ([`CasesRequest`]).
//! - `agent_cases_taken`: a supervisor (`supervisor`, its token) took the
//!   set to make, from `from` when another one owned it and is gone; only
//!   the owner prepares, labels and builds it.
//! - `agent_cases_refused`: no set was made (`reason`, `detail`).
//! - `agent_cases_prepared`: the cases ([`PreparedCase`]), the reviews
//!   left out (`skipped`, each with its `reason`) and the frame the labels
//!   are judged in ([`Frame`]).
//! - `agent_cases_labeled`: one case's label ([`LabelRecord`]).
//! - `agent_cases_built`: the set (`digest`, `cases`, the `disputed`
//!   candidates and the `ask_id` that asks a person about them).
//! - `agent_cases_decided`: a person's decision on a case's label
//!   ([`Decision`]).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::review::{Judgement, ReviewOutcome};
use crate::domain::{EventKind, ReviewDecision};

/// The kinds of the production cases' events.
pub const KINDS: [EventKind; 7] = [
    EventKind::AgentCasesRequested,
    EventKind::AgentCasesTaken,
    EventKind::AgentCasesRefused,
    EventKind::AgentCasesPrepared,
    EventKind::AgentCasesLabeled,
    EventKind::AgentCasesBuilt,
    EventKind::AgentCasesDecided,
];

/// What a supervisor does with a production set it would make
/// ([`take`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Take {
    /// Built or refused: nobody makes it any more.
    Settled,
    /// Its own already: go on, nothing recorded.
    Keep,
    /// Take it (`agent_cases_taken`), from the owner that is gone when one
    /// had it.
    Take { from: Option<String> },
    /// Another supervisor that lives makes it: leave it alone.
    Leave,
}

/// Whether supervisor `token` may make a set (ADR-t1728-1 (g)): one owner
/// at a time, as a round of the eval has. `settled` is whether the set was
/// built or refused, `owner` the supervisor of its latest
/// `agent_cases_taken`, `registration` that owner's heartbeat and whether
/// its process lives (`None` when it is not registered), and `now` unix
/// seconds; the owner is gone as
/// [`super::round::owner_gone`] says.
pub fn take(
    settled: bool,
    owner: Option<&str>,
    token: &str,
    registration: Option<(i64, bool)>,
    now: i64,
) -> Take {
    if settled {
        return Take::Settled;
    }
    match owner {
        None => Take::Take { from: None },
        Some(owner) if owner == token => Take::Keep,
        Some(owner) if super::round::owner_gone(Some(owner), token, registration, now) => {
            Take::Take {
                from: Some(owner.to_owned()),
            }
        }
        Some(_) => Take::Leave,
    }
}

/// The directory of the sets on the queue's data dir, one `<set id>/` each
/// with its `cases.json` (the shape of a `production.json`) and its
/// `patches/<sha256>.patch`.
pub const SETS_DIR: &str = "agent-cases";

/// A request of a production set (`agent_cases_requested`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasesRequest {
    pub agent: String,
    /// `--since` and `--until`: the reviews started at or after `since` and
    /// before `until`, recorded as UTC RFC 3339 times with milliseconds
    /// ([`period`] reads them, and a date or another offset is normalized
    /// before it is recorded); none is open.
    pub since: Option<String>,
    pub until: Option<String>,
    /// The requester's role and actor id.
    pub requested_by: String,
    pub requested_by_id: String,
}

impl CasesRequest {
    pub fn record(&self) -> Value {
        json!({
            "agent": self.agent,
            "since": self.since,
            "until": self.until,
            "requested_by": self.requested_by,
            "requested_by_id": self.requested_by_id,
        })
    }

    pub fn read(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            agent: text("agent")?,
            since: text("since"),
            until: text("until"),
            requested_by: text("requested_by").unwrap_or_default(),
            requested_by_id: text("requested_by_id").unwrap_or_default(),
        })
    }
}

/// The instant (Unix milliseconds) a `--since` / `--until` names: a UTC
/// date (`2026-10-05`, its midnight) or an RFC 3339 time with or without
/// a fraction (`2026-10-05T12:00:00Z`, `...+09:00`); `None` otherwise.
pub fn boundary(text: &str) -> Option<i64> {
    let text = text.trim();
    let date = text.len() == 10
        && text.bytes().enumerate().all(|(at, b)| {
            if at == 4 || at == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    if date {
        crate::domain::stats::rfc3339_millis(&format!("{text}T00:00:00Z"))
    } else {
        crate::domain::stats::rfc3339_millis(text)
    }
}

/// The instant of an event's `created_at` (Unix milliseconds); `None`
/// when it does not read.
pub fn event_instant(created_at: &str) -> Option<i64> {
    crate::domain::stats::rfc3339_millis(created_at)
        .or_else(|| crate::domain::stats::timestamp_millis(created_at))
}

/// A request's period as instants, `[since, until)`: each boundary that
/// reads ([`boundary`]), and `since` before `until`; why not otherwise.
pub fn period(since: Option<&str>, until: Option<&str>) -> Result<Period, String> {
    let read = |flag: &str, text: Option<&str>| {
        text.map(|text| {
            boundary(text).ok_or_else(|| {
                format!("{flag} {text:?} is not a date (2026-10-05) or an RFC 3339 time (2026-10-05T12:00:00Z)")
            })
        })
        .transpose()
    };
    let period = Period {
        since: read("--since", since)?,
        until: read("--until", until)?,
    };
    if let (Some(since), Some(until)) = (period.since, period.until)
        && since >= until
    {
        return Err(format!(
            "--since {} is not before --until {}",
            crate::domain::marks::utc_text(since),
            crate::domain::marks::utc_text(until)
        ));
    }
    Ok(period)
}

/// The reviews a set is made of: those started at or after `since` and
/// before `until` (Unix milliseconds), each open when `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Period {
    pub since: Option<i64>,
    pub until: Option<i64>,
}

impl Period {
    /// Whether an event recorded at `created_at` is in the period, compared
    /// as instants; one whose time does not read is in an open period only.
    pub fn holds(&self, created_at: &str) -> bool {
        match event_instant(created_at) {
            Some(at) => {
                self.since.is_none_or(|since| at >= since)
                    && self.until.is_none_or(|until| at < until)
            }
            None => self.since.is_none() && self.until.is_none(),
        }
    }
}

/// A review event as [`candidates`] reads it.
#[derive(Debug, Clone, Copy)]
pub struct ReviewEvent<'a> {
    pub run_id: &'a str,
    pub kind: &'a str,
    pub payload: &'a Value,
    pub created_at: &'a str,
}

/// A production review of the agent: what it reviewed and what the agent
/// said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub run_id: String,
    pub attempt: u64,
    pub base: String,
    pub head: String,
    /// The agent's production verdict (`pass`, `revise` or `concern`).
    pub verdict: String,
    pub reasons: Vec<String>,
    /// The review's `review_started` time.
    pub reviewed_at: String,
}

impl Candidate {
    /// The case's id: its run and the review's attempt.
    pub fn id(&self) -> String {
        format!("prod-{}-a{}", self.run_id, self.attempt)
    }

    fn record(&self) -> Value {
        json!({
            "id": self.id(),
            "run_id": self.run_id,
            "attempt": self.attempt,
            "base": self.base,
            "head": self.head,
            "production": {"verdict": self.verdict, "reasons": self.reasons},
            "reviewed_at": self.reviewed_at,
        })
    }

    fn read(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            run_id: text("run_id")?,
            attempt: value["attempt"].as_u64()?,
            base: text("base")?,
            head: text("head")?,
            verdict: value["production"]["verdict"].as_str()?.to_owned(),
            reasons: value["production"]["reasons"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            reviewed_at: text("reviewed_at").unwrap_or_default(),
        })
    }
}

/// The production reviews of `agent` in the period, in the order they
/// started: each `review_started` whose `subagents` selected the agent and
/// names `base` and `head`, with the agent's judgment in the
/// `review_finished` of the same run and attempt. A review the agent gave
/// no judgment (it did not complete, or the review never finished) is no
/// case.
pub fn candidates<'a>(
    agent: &str,
    period: Period,
    events: impl IntoIterator<Item = ReviewEvent<'a>>,
) -> Vec<Candidate> {
    let started_kind = EventKind::ReviewStarted.as_str();
    let finished_kind = EventKind::ReviewFinished.as_str();
    // Each selected review's (run, attempt), with its base, head and start.
    type Started = ((String, u64), (String, String, String));
    let mut started: Vec<Started> = Vec::new();
    let mut found: BTreeMap<(String, u64), Candidate> = BTreeMap::new();
    for event in events {
        let Some(attempt) = event.payload["attempt"].as_u64() else {
            continue;
        };
        let key = (event.run_id.to_owned(), attempt);
        if event.kind == started_kind {
            let subagents = &event.payload["subagents"];
            let selected = subagents["agents"]
                .as_array()
                .is_some_and(|agents| agents.iter().any(|a| a["agent"] == agent));
            if !selected || !period.holds(event.created_at) {
                continue;
            }
            let (Some(base), Some(head)) = (subagents["base"].as_str(), subagents["head"].as_str())
            else {
                continue;
            };
            started.retain(|(seen, _)| *seen != key);
            started.push((
                key,
                (
                    base.to_owned(),
                    head.to_owned(),
                    event.created_at.to_owned(),
                ),
            ));
        } else if event.kind == finished_kind {
            let Some((_, (base, head, at))) = started.iter().find(|(seen, _)| *seen == key) else {
                continue;
            };
            let judged = event.payload["agents"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|result| result["agent"] == agent && result["verdict"].is_string());
            if let Some(result) = judged {
                found.insert(
                    key.clone(),
                    Candidate {
                        run_id: key.0.clone(),
                        attempt,
                        base: base.clone(),
                        head: head.clone(),
                        verdict: result["verdict"].as_str().unwrap_or_default().to_owned(),
                        reasons: result["reasons"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect(),
                        reviewed_at: at.clone(),
                    },
                );
            }
        }
    }
    started
        .iter()
        .filter_map(|(key, _)| found.remove(key))
        .collect()
}

/// Why a review whose `base` or `head` no ref of the repository reaches (a
/// run commit whose branch was deleted, even while its object is kept, or
/// one not there at all) is left out: its change cannot be read again
/// (REPORT 5.8). Each flag says the commit is reachable; `None` when both
/// are.
pub fn unreachable(base: (&str, bool), head: (&str, bool)) -> Option<String> {
    let missing: Vec<&str> = [base, head]
        .into_iter()
        .filter(|(_, there)| !there)
        .map(|(commit, _)| commit)
        .collect();
    (!missing.is_empty()).then(|| {
        format!(
            "commit {} is reached by no ref of the repository: its change cannot be read",
            missing.join(" and ")
        )
    })
}

/// A case of the set: its review and its patch (`git diff <base>...<head>`)
/// by the SHA-256 of its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCase {
    pub review: Candidate,
    pub patch: String,
}

impl PreparedCase {
    pub fn id(&self) -> String {
        self.review.id()
    }

    pub fn record(&self) -> Value {
        let mut value = self.review.record();
        value["patch"] = json!(self.patch);
        value
    }

    pub fn read(value: &Value) -> Option<Self> {
        Some(Self {
            review: Candidate::read(value)?,
            patch: value["patch"].as_str()?.to_owned(),
        })
    }
}

/// What the cases are judged in, read from the landing branch when the set
/// is prepared ([`frame`]): the rule codes the agent's case lists know,
/// their `k`, the rules' documents its definition links to (repository
/// paths), and where each code's rule is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub codes: Vec<String>,
    pub k: u32,
    pub rules: Vec<String>,
    /// Where each code's rule is, by code: the case lists' `rules` entry
    /// for it ([`super::CaseFile::rules`]), else the sections the
    /// definition's lines that name it link ([`rule_sections`]); a code
    /// with neither is absent.
    pub sections: BTreeMap<String, Vec<String>>,
    /// The landing branch's commit they were read from.
    pub commit: String,
}

impl Frame {
    pub fn record(&self) -> Value {
        json!({"codes": self.codes, "k": self.k, "rules": self.rules,
               "sections": self.sections, "commit": self.commit})
    }

    pub fn read(value: &Value) -> Option<Self> {
        let texts = |value: &Value| -> Option<Vec<String>> {
            Some(
                value
                    .as_array()?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            )
        };
        Some(Self {
            codes: texts(&value["codes"])?,
            k: u32::try_from(value["k"].as_u64()?).ok()?,
            rules: texts(&value["rules"])?,
            sections: value["sections"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(code, sections)| Some((code.clone(), texts(sections)?)))
                .collect(),
            commit: value["commit"].as_str()?.to_owned(),
        })
    }
}

/// A link's target in a definition at `.dagq/agents/<agent>/AGENT.md` as a
/// repository path, without its `#anchor`; `None` for a URL, a path out of
/// the repository or one into `.dagq/agents/` (which the labeling job's
/// tree hides).
fn rule_path(agent: &str, target: &str) -> Option<String> {
    if target.contains("://") {
        return None;
    }
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        vec![".dagq", "agents", agent]
    };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    let path = parts.join("/");
    let hidden = path == ".dagq/agents" || path.starts_with(".dagq/agents/");
    (!hidden && !path.is_empty()).then_some(path)
}

/// The rules' documents a definition at `.dagq/agents/<agent>/AGENT.md`
/// links to, as repository paths, each once in order ([`rule_path`]).
pub fn rule_paths(agent: &str, definition: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for target in super::leak::linked_paths(definition) {
        if let Some(path) = rule_path(agent, &target)
            && !paths.contains(&path)
        {
            paths.push(path);
        }
    }
    paths
}

/// The Markdown links of `line`: each `[text](target)`.
fn links(line: &str) -> Vec<(&str, &str)> {
    let mut found = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("](") {
        let text = rest[..at].rfind('[').map_or("", |open| &rest[open + 1..at]);
        let after = &rest[at + 2..];
        let end = after.find(')').unwrap_or(after.len());
        found.push((text, &after[..end]));
        rest = &after[end..];
    }
    found
}

/// Whether `code` is in `line` as a whole word (not part of a longer code).
fn names_code(line: &str, code: &str) -> bool {
    let part = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    line.match_indices(code).any(|(at, _)| {
        !line[..at].chars().next_back().is_some_and(part)
            && !line[at + code.len()..].chars().next().is_some_and(part)
    })
}

/// Which section of the rules' documents each of `codes` is, as the
/// definition's lines link them: for a line that names codes and links
/// documents, each link's repository path with its `#anchor` and its text
/// (`docs/development/documents.md#adr (documents.md「ADR」)`). Only this
/// mapping is taken, never the definition's own words of what to check,
/// so the labeling job judges by the documents' text: their codes are the
/// review's own and the documents need not name them.
pub fn rule_sections(
    agent: &str,
    definition: &str,
    codes: &[String],
) -> BTreeMap<String, Vec<String>> {
    let mut sections: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in definition.lines() {
        let named: Vec<&String> = codes.iter().filter(|code| names_code(line, code)).collect();
        if named.is_empty() {
            continue;
        }
        for (text, target) in links(line) {
            let (path, anchor) = target.split_once('#').unwrap_or((target, ""));
            let Some(path) = rule_path(agent, path) else {
                continue;
            };
            let mut section = path;
            if !anchor.is_empty() {
                section = format!("{section}#{anchor}");
            }
            if !text.is_empty() {
                section = format!("{section} ({text})");
            }
            for code in &named {
                let entry = sections.entry((*code).clone()).or_default();
                if !entry.contains(&section) {
                    entry.push(section.clone());
                }
            }
        }
    }
    sections
}

/// The frame of `agent`'s cases from its definition and its case lists
/// (`(file name, text)`, `dev.json` first): the codes of every list, the
/// first list's `k`, the documents the definition links to, and where
/// each code's rule is, a list's `rules` before the definition's links.
/// Why not, when no list names a code.
pub fn frame(
    agent: &str,
    definition: &str,
    lists: &[(&str, &str)],
    commit: &str,
) -> Result<Frame, String> {
    let mut codes = BTreeSet::new();
    let mut k = None;
    let mut places: BTreeMap<String, String> = BTreeMap::new();
    let mut ordered: Vec<&(&str, &str)> = lists.iter().collect();
    ordered.sort_by_key(|(name, _)| super::Split::from_file_name(name));
    for (_, text) in ordered {
        let Ok(list) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        codes.extend(
            list["codes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
        k = k.or_else(|| list["k"].as_u64().and_then(|k| u32::try_from(k).ok()));
        for (code, place) in list["rules"].as_object().into_iter().flatten() {
            if let Some(place) = place.as_str() {
                places
                    .entry(code.clone())
                    .or_insert_with(|| place.to_owned());
            }
        }
    }
    if codes.is_empty() {
        return Err(format!(
            "no case list of {agent} at {commit} names the rule codes its cases are labeled by"
        ));
    }
    let codes: Vec<String> = codes.into_iter().collect();
    let mut sections = rule_sections(agent, definition, &codes);
    for (code, place) in places {
        if codes.contains(&code) {
            sections.insert(code, vec![place]);
        }
    }
    Ok(Frame {
        codes,
        k: k.unwrap_or(1).max(1),
        rules: rule_paths(agent, definition),
        sections,
        commit: commit.to_owned(),
    })
}

/// The definition of the job that labels a case: the rules' documents,
/// codes and where each code's rule is alone, never the agent's definition
/// (which the case's tree hides too), as the Spike's `JUDGE` prompt gives
/// each code with where its rule is.
/// The frame comes from the landing branch, but the job reads the
/// documents in the case's tree (the review's base with its change): their
/// text is the rules of the review's time, as its change left them.
pub fn label_definition(agent: &str, frame: &Frame) -> String {
    let rules: Vec<String> = frame.rules.iter().map(|path| format!("- {path}")).collect();
    let codes: Vec<String> = frame
        .codes
        .iter()
        .map(|code| match frame.sections.get(code) {
            Some(sections) => format!("- {code}: {}", sections.join("; ")),
            None => format!("- {code}: (no place given; judge it by the documents above)"),
        })
        .collect();
    format!(
        "---\ndescription: labels a change by the rules of {agent} alone\n---\n\
You label one change for a reviewer that checks a repository's changes against some of its rules.\n\
Judge the change by these rules only, as they are written in this tree (not by any other rule of the repository, nor by whether the change is good otherwise).\n\
The rules are in:\n{}\n\
The rule codes and where each one's rule is in the documents:\n{}\n\
Answer `revise` with every rule code above that the change breaks in its reasons' `codes`, or `pass` with no reason when it breaks none.\n\
Each reason names the file and what breaks, in one to three sentences.\n",
        if rules.is_empty() {
            "- (the definition links no document)".to_owned()
        } else {
            rules.join("\n")
        },
        codes.join("\n")
    )
}

/// A case's label: what the labeling job judged, by the rules alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub judgement: Judgement,
    /// The codes the change breaks, among the frame's; none for `clean`.
    pub codes: Vec<String>,
    pub reason: String,
}

impl Label {
    pub fn record(&self) -> Value {
        json!({"verdict": self.judgement.as_str(), "codes": self.codes, "reason": self.reason})
    }

    pub fn read(value: &Value) -> Option<Self> {
        Some(Self {
            judgement: Judgement::parse(value["verdict"].as_str()?)?,
            codes: value["codes"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            reason: value["reason"].as_str().unwrap_or_default().to_owned(),
        })
    }
}

/// The label of a labeling job's result (one agent result, as
/// `agent_job::read_reply` reads it), judged against the frame's `codes`:
/// a violation that names none of them is `clean`, as the Spike's build
/// made it. `None` for a result without a judgment.
pub fn label_of(result: &Value, codes: &[String]) -> Option<Label> {
    let outcome = ReviewOutcome::from_agent_result(result)?;
    let known: BTreeSet<&str> = codes.iter().map(String::as_str).collect();
    let named: Vec<String> = outcome
        .codes
        .iter()
        .filter(|code| known.contains(code.as_str()))
        .cloned()
        .collect();
    let violation = outcome.judgement() == Judgement::Violation && !named.is_empty();
    let reasons: Vec<&str> = result["reasons"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|reason| reason["text"].as_str().or_else(|| reason.as_str()))
        .collect();
    let reason = if reasons.is_empty() {
        result["summary"].as_str().unwrap_or_default().to_owned()
    } else {
        reasons.join(" ")
    };
    Some(Label {
        judgement: if violation {
            Judgement::Violation
        } else {
            Judgement::Clean
        },
        codes: if violation { named } else { Vec::new() },
        reason,
    })
}

/// Why a case is a disputed candidate: its label and the production verdict
/// disagree on whether the change breaks a rule. `None` when they agree
/// (or the production verdict does not read).
pub fn dispute(production: &str, label: &Label) -> Option<String> {
    let decision: ReviewDecision = production.parse().ok()?;
    (Judgement::of(decision) != label.judgement).then(|| {
        format!(
            "production said {production}, the label says {}",
            label.judgement.as_str()
        )
    })
}

/// One case's label as `agent_cases_labeled` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelRecord {
    /// `None` when the job gave no judgment: the case is left out.
    pub label: Option<Label>,
    pub error: Option<String>,
    /// The provider that labeled it and how (`made_by` of the case).
    pub made_by: String,
}

/// A person's decision on a case's label (`agent_cases_decided`): the
/// label it keeps, which clears its dispute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub label: Label,
    pub decided_by: String,
    pub decided_by_id: String,
}

impl Decision {
    pub fn record(&self, set_id: i64, case: &str) -> Value {
        let mut value = self.label.record();
        value["set_id"] = json!(set_id);
        value["case"] = json!(case);
        value["decided_by"] = json!(self.decided_by);
        value["decided_by_id"] = json!(self.decided_by_id);
        value
    }
}

/// Whether a decided label fits the frame's codes: a violation names some,
/// all known; a clean names none.
pub fn check_label(judgement: Judgement, codes: &[String], known: &[String]) -> Result<(), String> {
    match judgement {
        Judgement::Violation if codes.is_empty() => {
            Err("a violation names the codes it breaks (--codes)".to_owned())
        }
        Judgement::Clean if !codes.is_empty() => Err("a clean label names no code".to_owned()),
        _ => match codes.iter().find(|code| !known.contains(code)) {
            Some(code) => Err(format!(
                "{code} is not one of the set's codes ({})",
                known.join(", ")
            )),
            None => Ok(()),
        },
    }
}

/// A built set as `agent_cases_built` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    pub digest: String,
    pub cases: Vec<String>,
}

/// One case of a set with its label: its label, the person's decision if
/// any, and why it is disputed while no decision is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetCase<'a> {
    pub case: &'a PreparedCase,
    pub label: &'a Label,
    pub made_by: &'a str,
    pub decision: Option<&'a Decision>,
    pub disputed: Option<String>,
}

impl SetCase<'_> {
    /// The label the case expects: the person's, or the job's.
    pub fn expected(&self) -> &Label {
        self.decision.map_or(self.label, |decision| &decision.label)
    }

    /// The case in a case list's shape (`source` `production`).
    pub fn record(&self) -> Value {
        let review = &self.case.review;
        let expected = self.expected();
        json!({
            "id": self.case.id(),
            "source": "production",
            "made_by": format!("production review of run {} attempt {}; label by {}", review.run_id, review.attempt, self.made_by),
            "base_commit": review.base,
            "patch": self.case.patch,
            "adjudicated": self.decision.map(|decision| json!({
                "by": decision.decided_by_id,
                "verdict": decision.label.judgement.as_str(),
                "codes": decision.label.codes,
                "reason": decision.label.reason,
                "label": self.label.record(),
            })),
            "disputed": self.disputed,
            "review": {
                "input": {},
                "expected": {
                    "verdict": expected.judgement.as_str(),
                    "codes": expected.codes,
                    "note": expected.reason,
                },
                "production": {"verdict": review.verdict, "reasons": review.reasons, "head": review.head},
            },
        })
    }
}

/// One production set: its request and what became of it.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseSet {
    /// The id of its request's event.
    pub id: i64,
    pub request: CasesRequest,
    pub refused: Option<Value>,
    pub frame: Option<Frame>,
    /// `None` before it was prepared.
    pub prepared: Option<Vec<PreparedCase>>,
    pub skipped: Vec<Value>,
    /// The latest label of each case, by its id.
    pub labels: BTreeMap<String, LabelRecord>,
    pub built: Option<Built>,
    /// The latest decision on each case, by its id.
    pub decisions: BTreeMap<String, Decision>,
    /// The supervisor that makes it: the latest `agent_cases_taken`'s.
    pub owner: Option<String>,
}

impl CaseSet {
    /// `refused`, `waiting` (to be prepared), `labeling` or `built`.
    pub fn status(&self) -> &'static str {
        if self.refused.is_some() {
            "refused"
        } else if self.built.is_some() {
            "built"
        } else if self.prepared.is_some() {
            "labeling"
        } else {
            "waiting"
        }
    }

    /// The prepared cases with no label yet, in order.
    pub fn unlabeled(&self) -> Vec<&PreparedCase> {
        self.prepared
            .iter()
            .flatten()
            .filter(|case| !self.labels.contains_key(&case.id()))
            .collect()
    }

    /// The cases of the set: those labeled with a judgment, in order, each
    /// disputed while its label and the production verdict disagree and no
    /// person decided it.
    pub fn cases(&self) -> Vec<SetCase<'_>> {
        self.prepared
            .iter()
            .flatten()
            .filter_map(|case| {
                let record = self.labels.get(&case.id())?;
                let label = record.label.as_ref()?;
                let decision = self.decisions.get(&case.id());
                Some(SetCase {
                    case,
                    label,
                    made_by: &record.made_by,
                    decision,
                    disputed: decision
                        .is_none()
                        .then(|| dispute(&case.review.verdict, label))
                        .flatten(),
                })
            })
            .collect()
    }

    /// The set in a case list's shape (that of `production.json`), which
    /// `agent eval --set` reads; `None` before it was prepared.
    pub fn case_list(&self) -> Option<Value> {
        let frame = self.frame.as_ref()?;
        Some(json!({
            "agent": self.request.agent,
            "role": "review",
            "codes": frame.codes,
            "k": frame.k,
            "cases": self.cases().iter().map(SetCase::record).collect::<Vec<_>>(),
        }))
    }
}

/// The sets the production cases' events (`(id, kind, payload)`, oldest
/// first) say, oldest first. An event of a set with no request, or one
/// that does not read, is left out; a set's first `prepared` and `built`
/// hold.
pub fn sets<'a>(events: impl IntoIterator<Item = (i64, &'a str, &'a Value)>) -> Vec<CaseSet> {
    let mut sets: BTreeMap<i64, CaseSet> = BTreeMap::new();
    for (id, kind, payload) in events {
        if kind == EventKind::AgentCasesRequested.as_str() {
            if let Some(request) = CasesRequest::read(payload) {
                sets.insert(
                    id,
                    CaseSet {
                        id,
                        request,
                        refused: None,
                        frame: None,
                        prepared: None,
                        skipped: Vec::new(),
                        labels: BTreeMap::new(),
                        built: None,
                        decisions: BTreeMap::new(),
                        owner: None,
                    },
                );
            }
            continue;
        }
        let Some(set) = payload["set_id"]
            .as_i64()
            .and_then(|set| sets.get_mut(&set))
        else {
            continue;
        };
        let case = payload["case"].as_str().unwrap_or_default().to_owned();
        if kind == EventKind::AgentCasesTaken.as_str() {
            set.owner = payload["supervisor"].as_str().map(str::to_owned);
        } else if kind == EventKind::AgentCasesRefused.as_str() {
            set.refused.get_or_insert_with(|| payload.clone());
        } else if kind == EventKind::AgentCasesPrepared.as_str() && set.prepared.is_none() {
            set.frame = Frame::read(&payload["frame"]);
            set.prepared = Some(
                payload["cases"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(PreparedCase::read)
                    .collect(),
            );
            set.skipped = payload["skipped"].as_array().cloned().unwrap_or_default();
        } else if kind == EventKind::AgentCasesLabeled.as_str() {
            set.labels.insert(
                case,
                LabelRecord {
                    label: Label::read(&payload["label"]),
                    error: payload["error"].as_str().map(str::to_owned),
                    made_by: payload["made_by"].as_str().unwrap_or_default().to_owned(),
                },
            );
        } else if kind == EventKind::AgentCasesBuilt.as_str() && set.built.is_none() {
            set.built = Some(Built {
                digest: payload["digest"].as_str().unwrap_or_default().to_owned(),
                cases: payload["cases"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            });
        } else if kind == EventKind::AgentCasesDecided.as_str()
            && let Some(label) = Label::read(payload)
        {
            set.decisions.insert(
                case,
                Decision {
                    label,
                    decided_by: payload["decided_by"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    decided_by_id: payload["decided_by_id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                },
            );
        }
    }
    sets.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(agents: &[&str], attempt: u64) -> Value {
        json!({"attempt": attempt, "subagents": {"commit": "m", "base": "b1", "head": "h1",
               "agents": agents.iter().map(|a| json!({"agent": a})).collect::<Vec<_>>()}})
    }

    fn finished(agent: &str, verdict: &str, attempt: u64) -> Value {
        json!({"attempt": attempt, "verdict": verdict, "agents": [
            {"agent": agent, "status": "completed", "verdict": verdict, "reasons": ["adds notes.txt"], "summary": "s"},
            {"agent": "other", "status": "completed", "verdict": "pass", "reasons": [], "summary": "s"}]})
    }

    /// The reviews that selected the agent in the period, with its
    /// judgment of the same attempt; another agent's, one outside the
    /// period, one without its judgment and one never finished are no case.
    #[test]
    fn the_candidates_are_the_agents_judged_reviews_of_the_period() {
        let s1 = started(&["demo", "other"], 1);
        let s2 = started(&["other"], 1);
        let s3 = started(&["demo"], 2);
        let s4 = started(&["demo"], 1);
        let f1 = finished("demo", "revise", 1);
        let f2 = finished("other", "pass", 1);
        let f3 = json!({"attempt": 2, "agents": [{"agent": "demo", "status": "failed", "reasons": [], "summary": ""}]});
        let early = started(&["demo"], 1);
        let f_early = finished("demo", "pass", 1);
        let events = [
            ("r0", "review_started", &early, "2026-10-01T00:00:00.000Z"),
            (
                "r0",
                "review_finished",
                &f_early,
                "2026-10-01T00:10:00.000Z",
            ),
            ("r1", "review_started", &s1, "2026-10-05T00:00:00.000Z"),
            ("r2", "review_started", &s2, "2026-10-05T01:00:00.000Z"),
            ("r1", "review_finished", &f1, "2026-10-05T00:10:00.000Z"),
            ("r2", "review_finished", &f2, "2026-10-05T01:10:00.000Z"),
            ("r1", "review_started", &s3, "2026-10-05T02:00:00.000Z"),
            ("r1", "review_finished", &f3, "2026-10-05T02:10:00.000Z"),
            ("r3", "review_started", &s4, "2026-10-06T00:00:00.000Z"),
        ];
        let found = candidates(
            "demo",
            period(Some("2026-10-05"), Some("2026-10-07")).unwrap(),
            events
                .iter()
                .map(|(run_id, kind, payload, created_at)| ReviewEvent {
                    run_id,
                    kind,
                    payload,
                    created_at,
                }),
        );
        assert_eq!(
            found,
            vec![Candidate {
                run_id: "r1".into(),
                attempt: 1,
                base: "b1".into(),
                head: "h1".into(),
                verdict: "revise".into(),
                reasons: vec!["adds notes.txt".into()],
                reviewed_at: "2026-10-05T00:00:00.000Z".into(),
            }]
        );
        assert_eq!(found[0].id(), "prod-r1-a1");
    }

    /// The period's boundaries are instants, compared with the events'
    /// times as instants: a boundary at the same instant as an event holds
    /// it for `--since` and not for `--until`, whatever the text's form (a
    /// date, a fraction, another offset); a boundary that does not read,
    /// or a `--since` not before `--until`, is refused with why.
    #[test]
    fn the_period_is_compared_as_instants() {
        let at = "2026-10-05T12:00:00.000Z";
        for same in [
            "2026-10-05T12:00:00Z",
            "2026-10-05T12:00:00.000Z",
            "2026-10-05T21:00:00+09:00",
        ] {
            assert_eq!(boundary(same), event_instant(at), "{same}");
            assert!(period(Some(same), None).unwrap().holds(at), "{same}");
            assert!(!period(None, Some(same)).unwrap().holds(at), "{same}");
        }
        assert!(period(Some("2026-10-05"), None).unwrap().holds(at));
        assert!(period(None, Some("2026-10-06")).unwrap().holds(at));
        assert!(!period(None, Some("2026-10-05")).unwrap().holds(at));
        assert!(
            !period(Some("2026-10-05T12:00:00.001Z"), None)
                .unwrap()
                .holds(at)
        );
        // The queue's SQLite form without a zone reads too.
        assert_eq!(event_instant("2026-10-05 12:00:00"), event_instant(at));
        // An event whose time does not read is in an open period only.
        assert!(Period::default().holds("yesterday"));
        assert!(!period(Some("2026-10-05"), None).unwrap().holds("yesterday"));

        let error = period(
            Some("2026-10-05T21:00:00+09:00"),
            Some("2026-10-05T12:00:00Z"),
        )
        .unwrap_err();
        assert!(
            error.contains(
                "2026-10-05T12:00:00.000Z is not before --until 2026-10-05T12:00:00.000Z"
            ),
            "{error}"
        );
        assert!(period(Some("2026-10-06"), Some("2026-10-05")).is_err());
        let error = period(Some("last week"), None).unwrap_err();
        assert!(error.contains("--since \"last week\""), "{error}");
        assert!(period(None, Some("2026-13-01")).is_err());
        assert_eq!(period(None, None), Ok(Period::default()));
    }

    /// A commit the repository no longer has leaves the review out with
    /// why.
    #[test]
    fn a_review_whose_commit_is_gone_is_left_out_with_why() {
        assert_eq!(unreachable(("b", true), ("h", true)), None);
        let why = unreachable(("b", true), ("h", false)).unwrap();
        assert!(why.contains("commit h is reached by no ref"), "{why}");
        let why = unreachable(("b", false), ("h", false)).unwrap();
        assert!(why.contains("b and h"), "{why}");
    }

    fn result(verdict: &str, codes: &[&str]) -> Value {
        json!({"agent": "demo", "status": "completed", "verdict": verdict,
               "reasons": [{"text": "notes.txt breaks it", "codes": codes}], "summary": "s"})
    }

    /// The label is the job's judgment by the frame's codes; a case whose
    /// label and production verdict disagree is a disputed candidate,
    /// either way.
    #[test]
    fn a_label_that_disagrees_with_production_is_disputed() {
        let codes = vec!["D-1".to_owned(), "D-2".to_owned()];
        let violation = label_of(&result("revise", &["D-1", "X-9"]), &codes).unwrap();
        assert_eq!(violation.judgement, Judgement::Violation);
        assert_eq!(violation.codes, vec!["D-1".to_owned()]);
        assert_eq!(violation.reason, "notes.txt breaks it");
        // A violation of no code of the frame is clean.
        let unknown = label_of(&result("concern", &["X-9"]), &codes).unwrap();
        assert_eq!(unknown.judgement, Judgement::Clean);
        assert!(unknown.codes.is_empty());
        let clean = label_of(&result("pass", &[]), &codes).unwrap();
        assert_eq!(clean.judgement, Judgement::Clean);
        assert_eq!(
            label_of(&json!({"agent": "demo", "status": "failed"}), &codes),
            None
        );

        assert_eq!(dispute("revise", &violation), None);
        assert_eq!(dispute("concern", &violation), None);
        assert_eq!(dispute("pass", &clean), None);
        assert_eq!(
            dispute("pass", &violation).as_deref(),
            Some("production said pass, the label says violation")
        );
        assert_eq!(
            dispute("revise", &clean).as_deref(),
            Some("production said revise, the label says clean")
        );
        assert_eq!(dispute("unknown", &clean), None);
    }

    #[test]
    fn a_decided_label_fits_the_sets_codes() {
        let known = vec!["D-1".to_owned()];
        assert!(check_label(Judgement::Violation, &["D-1".into()], &known).is_ok());
        assert!(check_label(Judgement::Clean, &[], &known).is_ok());
        assert!(check_label(Judgement::Violation, &[], &known).is_err());
        assert!(check_label(Judgement::Clean, &["D-1".into()], &known).is_err());
        let error = check_label(Judgement::Violation, &["X-1".into()], &known).unwrap_err();
        assert!(error.contains("X-1"), "{error}");
    }

    #[test]
    fn the_rules_are_the_documents_the_definition_links_to() {
        let definition = "- A-1: not A-123 [rules](../../../docs/development/documents.md#adr)\n\
             - again [rules](../../../docs/development/documents.md#frontmatter)\n\
             - [web](https://example.com/x) [near](notes.md) [out](../../../../etc/passwd)\n";
        assert_eq!(
            rule_paths("adr-rules", definition),
            vec!["docs/development/documents.md".to_owned()]
        );
        let codes = vec!["A-1".to_owned(), "A-12".to_owned(), "A-2".to_owned()];
        let frame = Frame {
            codes: codes.clone(),
            k: 3,
            rules: rule_paths("adr-rules", definition),
            sections: rule_sections("adr-rules", definition, &codes),
            commit: "c".into(),
        };
        assert_eq!(
            frame.sections,
            BTreeMap::from([(
                "A-1".to_owned(),
                vec!["docs/development/documents.md#adr (rules)".to_owned()]
            )])
        );
        let text = label_definition("adr-rules", &frame);
        assert!(text.contains("- docs/development/documents.md\n"), "{text}");
        assert!(
            text.contains("- A-1: docs/development/documents.md#adr (rules)\n"),
            "{text}"
        );
        assert!(text.contains("- A-2: (no place given"), "{text}");
        assert!(!text.contains("[rules]"), "the definition is not in it");
        assert_eq!(Frame::read(&frame.record()), Some(frame));
    }

    /// One owner per set: a set nobody took is taken, the owner keeps its
    /// own, another's is taken only from an owner that is gone (not
    /// registered, or its heartbeat old and its process gone), and nobody
    /// takes one built or refused.
    #[test]
    fn a_set_is_taken_once_and_only_from_a_gone_owner() {
        let now = 10_000;
        let fresh = Some((now, false));
        let stale_dead = Some((now - 600, false));
        let stale_alive = Some((now - 600, true));
        assert_eq!(
            take(false, None, "me", None, now),
            Take::Take { from: None }
        );
        assert_eq!(take(false, Some("me"), "me", fresh, now), Take::Keep);
        assert_eq!(take(false, Some("a"), "me", fresh, now), Take::Leave);
        assert_eq!(take(false, Some("a"), "me", stale_alive, now), Take::Leave);
        for gone in [None, stale_dead] {
            assert_eq!(
                take(false, Some("a"), "me", gone, now),
                Take::Take {
                    from: Some("a".to_owned())
                }
            );
        }
        for owner in [None, Some("me"), Some("a")] {
            assert_eq!(take(true, owner, "me", None, now), Take::Settled);
        }
    }

    /// This repository's agents, as they are laid out: `migration-rules`
    /// links its one document on a line apart from its codes, and
    /// `adr-rules` links one section for several codes. Each code gets a
    /// place of its own from the case lists' `rules`, none the same as
    /// another's, and the labeling job's definition carries those places
    /// and none of the definition's own lines of what to check.
    #[test]
    fn this_repositorys_agents_give_each_code_a_place_of_its_own() {
        for (agent, definition, dev) in [
            (
                "migration-rules",
                include_str!("../../../.dagq/agents/migration-rules/AGENT.md"),
                include_str!("../../../.dagq/agents/migration-rules/evals/dev.json"),
            ),
            (
                "adr-rules",
                include_str!("../../../.dagq/agents/adr-rules/AGENT.md"),
                include_str!("../../../.dagq/agents/adr-rules/evals/dev.json"),
            ),
        ] {
            let frame = frame(agent, definition, &[("dev.json", dev)], "c").unwrap();
            assert!(!frame.codes.is_empty(), "{agent}");
            let mut seen = BTreeSet::new();
            for code in &frame.codes {
                let places = frame
                    .sections
                    .get(code)
                    .unwrap_or_else(|| panic!("{agent} {code}"));
                assert_eq!(places.len(), 1, "{agent} {code}: {places:?}");
                assert!(
                    seen.insert(places[0].clone()),
                    "{agent} {code} shares {}",
                    places[0]
                );
            }
            let text = label_definition(agent, &frame);
            for code in &frame.codes {
                let line = format!("- {code}: {}\n", frame.sections[code][0]);
                assert!(text.contains(&line), "{agent}: {line} in {text}");
            }
            assert!(!text.contains("no place given"), "{text}");
            // No line of the definition's body reaches the job.
            let body = definition.splitn(3, "---").nth(2).unwrap();
            for line in body.lines().map(str::trim).filter(|line| line.len() > 12) {
                assert!(!text.contains(line), "{agent}: {line:?} reached the job");
            }
        }
        // Without `rules`, the definition's links are the places, as
        // `migration-rules` would have none of its own.
        let bare =
            r#"{"agent": "migration-rules", "codes": ["A-139", "A-140"], "k": 1, "cases": []}"#;
        let read = frame(
            "migration-rules",
            include_str!("../../../.dagq/agents/migration-rules/AGENT.md"),
            &[("dev.json", bare)],
            "c",
        )
        .unwrap();
        assert!(read.sections.is_empty(), "{:?}", read.sections);
        assert!(
            frame(
                "migration-rules",
                "",
                &[("dev.json", r#"{"codes": []}"#)],
                "c"
            )
            .is_err()
        );
    }

    /// A set's state is read back from its events alone: its cases, their
    /// labels, the disputed candidate until a person decides it, and its
    /// case list in a production list's shape.
    #[test]
    fn a_sets_state_is_read_back_from_its_events() {
        let request = CasesRequest {
            agent: "demo".into(),
            since: Some("2026-10-05".into()),
            until: None,
            requested_by: "user".into(),
            requested_by_id: "user".into(),
        };
        assert_eq!(CasesRequest::read(&request.record()), Some(request.clone()));
        let case = |run: &str, verdict: &str| PreparedCase {
            review: Candidate {
                run_id: run.into(),
                attempt: 1,
                base: "b".into(),
                head: "h".into(),
                verdict: verdict.into(),
                reasons: vec![],
                reviewed_at: "t".into(),
            },
            patch: "p".repeat(64),
        };
        let agree = case("r1", "revise");
        let differ = case("r2", "pass");
        let failed = case("r3", "pass");
        let frame = Frame {
            codes: vec!["D-1".into()],
            k: 3,
            rules: vec![],
            sections: BTreeMap::new(),
            commit: "c".into(),
        };
        let violation = Label {
            judgement: Judgement::Violation,
            codes: vec!["D-1".into()],
            reason: "breaks D-1".into(),
        };
        let labeled = |case: &PreparedCase, label: Option<&Label>| {
            json!({"set_id": 1, "case": case.id(), "label": label.map(Label::record),
                   "made_by": "codex", "error": label.is_none().then_some("no verdict")})
        };
        let events = [
            (1, "agent_cases_requested", request.record()),
            (
                2,
                "agent_cases_prepared",
                json!({"set_id": 1, "frame": frame.record(),
                       "cases": [agree.record(), differ.record(), failed.record()],
                       "skipped": [{"run_id": "r9", "reason": "gone"}]}),
            ),
            (3, "agent_cases_labeled", labeled(&agree, Some(&violation))),
        ];
        let read = |events: &[(i64, &str, Value)]| {
            sets(
                events
                    .iter()
                    .map(|(id, kind, payload)| (*id, *kind, payload)),
            )
            .remove(0)
        };
        let set = read(&events);
        assert_eq!(set.status(), "labeling");
        assert_eq!(set.unlabeled(), vec![&differ, &failed]);
        assert_eq!(set.skipped.len(), 1);
        let mut events = events.to_vec();
        events.push((4, "agent_cases_labeled", labeled(&differ, Some(&violation))));
        events.push((5, "agent_cases_labeled", labeled(&failed, None)));
        events.push((
            6,
            "agent_cases_built",
            json!({"set_id": 1, "digest": "d", "cases": [agree.id(), differ.id()]}),
        ));
        let set = read(&events);
        assert_eq!(set.status(), "built");
        assert!(set.unlabeled().is_empty());
        let cases = set.cases();
        assert_eq!(cases.len(), 2, "a case without a label is left out");
        assert_eq!(cases[0].disputed, None);
        assert_eq!(
            cases[1].disputed.as_deref(),
            Some("production said pass, the label says violation")
        );
        let list = set.case_list().unwrap();
        assert_eq!(list["k"], 3);
        assert_eq!(list["cases"][1]["source"], "production");
        assert_eq!(
            list["cases"][1]["review"]["expected"]["verdict"],
            "violation"
        );

        // A person's decision clears the dispute and becomes the label.
        let decision = Decision {
            label: Label {
                judgement: Judgement::Clean,
                codes: vec![],
                reason: "the rule allows it".into(),
            },
            decided_by: "user".into(),
            decided_by_id: "user".into(),
        };
        events.push((7, "agent_cases_decided", decision.record(1, &differ.id())));
        let set = read(&events);
        let cases = set.cases();
        assert_eq!(cases[1].disputed, None);
        assert_eq!(cases[1].expected().judgement, Judgement::Clean);
        let list = set.case_list().unwrap();
        assert_eq!(list["cases"][1]["review"]["expected"]["codes"], json!([]));
        assert_eq!(list["cases"][1]["adjudicated"]["verdict"], "clean");
        assert_eq!(list["cases"][1]["disputed"], Value::Null);
    }
}
