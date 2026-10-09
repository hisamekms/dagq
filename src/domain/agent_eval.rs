//! The eval of an agent (ADR-t1728-1 decisions 3, 4 and 10): the case lists under
//! `.dagq/agents/<name>/evals/`, the role's harness that scores the agent's
//! runs on them, and the check that a definition does not borrow a case's
//! own words ([`leak`]). Everything here is without side effects: the
//! caller reads the files and launches the agent. The shapes, the counting
//! and the threshold rule are docs/design/agent-eval.md's.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::{Map, Value};

pub mod leak;
pub mod record;
pub mod review;
pub mod round;

/// The directory of an agent's case lists, in its definition's directory
/// (`.dagq/agents/<name>/evals/`).
pub const EVALS_DIR: &str = "evals";

/// Where the cases' patches are, relative to the repository root: one
/// `<sha256>.patch` per content, shared by every agent's cases.
pub const PATCH_DIR: &str = ".dagq/agent-cases/patches";

/// The extension of a patch's file in [`PATCH_DIR`].
pub const PATCH_EXTENSION: &str = "patch";

/// The threshold every adoption score is compared with when the
/// configuration names none.
pub const DEFAULT_THRESHOLD: f64 = 0.9;

/// A case list's split: which file of `evals/` holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Split {
    Dev,
    Holdout,
    Production,
}

impl Split {
    pub const ALL: [Split; 3] = [Split::Dev, Split::Holdout, Split::Production];

    pub const fn as_str(self) -> &'static str {
        match self {
            Split::Dev => "dev",
            Split::Holdout => "holdout",
            Split::Production => "production",
        }
    }

    /// The file of `evals/` that holds the split's cases.
    pub const fn file_name(self) -> &'static str {
        match self {
            Split::Dev => "dev.json",
            Split::Holdout => "holdout.json",
            Split::Production => "production.json",
        }
    }

    /// The split a file of `evals/` holds; `None` for any other file.
    pub fn from_file_name(name: &str) -> Option<Split> {
        Split::ALL
            .into_iter()
            .find(|split| split.file_name() == name)
    }
}

/// The role an agent is used in. Each role has its harness (the fields of
/// its cases, how a run is read and scored); the only one is the review's
/// (ADR-t1728-1 decision 3), and a role added here gets a [`Harness`] and a
/// variant of [`RoleCase`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    Review,
}

impl AgentRole {
    pub const ALL: [AgentRole; 1] = [AgentRole::Review];

    /// The role's name: a case list's `role` and the field of a case that
    /// holds the role's `input` and `expected`.
    pub const fn as_str(self) -> &'static str {
        match self {
            AgentRole::Review => "review",
        }
    }

    pub fn parse(name: &str) -> Option<AgentRole> {
        AgentRole::ALL
            .into_iter()
            .find(|role| role.as_str() == name)
    }

    /// The case's fields of this role (`case.<role>`), checked against the
    /// list's `codes`.
    fn read_case(
        self,
        fields: &Value,
        codes: &BTreeSet<String>,
    ) -> Result<RoleCase, Vec<ProblemKind>> {
        match self {
            AgentRole::Review => review::ReviewHarness::read(fields, codes).map(RoleCase::Review),
        }
    }
}

/// A role's harness: what the role's fields of a case are, what one run of
/// the agent gave, and how the runs of a case list are scored.
pub trait Harness {
    const ROLE: AgentRole;
    /// The role's fields of a case (`input` and `expected`).
    type Case;
    /// One run of the agent on one case.
    type Run;
    /// The scores of a round.
    type Score;

    /// Read `case.<role>`; `codes` is the list's `codes`, which the
    /// expected codes must be among.
    fn read(fields: &Value, codes: &BTreeSet<String>) -> Result<Self::Case, Vec<ProblemKind>>;

    /// Score `runs` of the agent on `cases` (the cases of one list, or the
    /// part of it the round ran).
    fn score(cases: &[Case], runs: &[Self::Run]) -> Self::Score;
}

/// The role's fields of a case.
#[derive(Debug, Clone, PartialEq)]
pub enum RoleCase {
    Review(review::ReviewCase),
}

/// Who made a case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Generated,
    Handmade,
    Production,
}

impl Source {
    pub const fn as_str(self) -> &'static str {
        match self {
            Source::Generated => "generated",
            Source::Handmade => "handmade",
            Source::Production => "production",
        }
    }

    fn parse(name: &str) -> Option<Source> {
        [Source::Generated, Source::Handmade, Source::Production]
            .into_iter()
            .find(|source| source.as_str() == name)
    }
}

/// One case of a list: the fields every role has, and the role's own.
/// `adjudicated`, `disputed` and `k` may be left out, which is the same as
/// `null`; the others are required. Fields the reader does not know (a
/// case's provenance from the Spike, such as `base_kind`, or a production
/// case's `review.production`) are kept in the file and not read.
#[derive(Debug, Clone, PartialEq)]
pub struct Case {
    /// Unique in its file; the same id may be in another split's file.
    pub id: String,
    pub source: Source,
    pub made_by: String,
    pub base_commit: String,
    /// The SHA-256 of the patch in [`PATCH_DIR`].
    pub patch: String,
    /// A person's decision on the label, `None` when there is none.
    pub adjudicated: Option<Value>,
    /// Why the label is disputed, `None` when it is not (`null`, `false`,
    /// `0` or an empty value); a disputed case is left out of the primary
    /// scores.
    pub disputed: Option<Value>,
    /// The runs of the case in a round, 1 or more; `None` for the list's
    /// `k`.
    pub k: Option<u32>,
    pub role: RoleCase,
}

impl Case {
    pub fn is_disputed(&self) -> bool {
        self.disputed.is_some()
    }

    /// The case's runs in a round of a list whose `k` is `list_k`.
    pub fn runs(&self, list_k: u32) -> u32 {
        self.k.unwrap_or(list_k)
    }

    pub fn review(&self) -> Option<&review::ReviewCase> {
        match &self.role {
            RoleCase::Review(case) => Some(case),
        }
    }
}

/// One file of `evals/`: `agent`, `role`, `codes`, `k` and `cases`, each
/// required; the split is the file's name, not a field.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseFile {
    /// The directory's `<name>`; another name is a problem.
    pub agent: String,
    pub role: AgentRole,
    pub split: Split,
    /// The rule codes the cases may expect.
    pub codes: Vec<String>,
    /// The runs of each case in a round, 1 or more.
    pub k: u32,
    pub cases: Vec<Case>,
}

/// What is wrong with a case list, one item each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseProblem {
    /// The file's name in `evals/`.
    pub file: String,
    /// The case's id, `None` for the file's own fields.
    pub case: Option<String>,
    pub kind: ProblemKind,
}

impl fmt::Display for CaseProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.case {
            Some(case) => write!(f, "{} case {case}: {}", self.file, self.kind),
            None => write!(f, "{}: {}", self.file, self.kind),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProblemKind {
    /// A file of `evals/` that holds no split.
    UnknownFile,
    NotJson(String),
    MissingField(String),
    WrongType {
        field: String,
        expected: &'static str,
    },
    UnknownRole(String),
    /// The list's `agent` is not the directory's.
    OtherAgent(String),
    UnknownSource(String),
    /// A `handmade` case outside `dev.json`.
    HandmadeOutsideDev,
    DuplicateId,
    /// `patch` is not a SHA-256 in lowercase hex.
    NotAPatchHash(String),
    /// No patch in [`PATCH_DIR`] has the hash.
    MissingPatch(String),
    UnknownVerdict(String),
    ViolationWithoutCodes,
    CleanWithCodes,
    /// Expected or acceptable codes the list's `codes` does not have.
    UnknownCodes(Vec<String>),
    /// Codes both expected and acceptable.
    ExpectedAndAcceptable(Vec<String>),
}

impl fmt::Display for ProblemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProblemKind::UnknownFile => {
                let files: Vec<&str> = Split::ALL.iter().map(|s| s.file_name()).collect();
                write!(f, "not a case list (one of {})", files.join(", "))
            }
            ProblemKind::NotJson(error) => write!(f, "not JSON: {error}"),
            ProblemKind::MissingField(field) => write!(f, "`{field}` is missing"),
            ProblemKind::WrongType { field, expected } => {
                write!(f, "`{field}` must be {expected}")
            }
            ProblemKind::UnknownRole(role) => write!(f, "unknown role {role:?}"),
            ProblemKind::OtherAgent(agent) => {
                write!(f, "`agent` is {agent:?}, not the directory's agent")
            }
            ProblemKind::UnknownSource(source) => write!(
                f,
                "`source` is {source:?}, not generated, handmade or production"
            ),
            ProblemKind::HandmadeOutsideDev => write!(f, "a handmade case belongs in dev.json"),
            ProblemKind::DuplicateId => write!(f, "the id is used twice"),
            ProblemKind::NotAPatchHash(patch) => {
                write!(f, "`patch` {patch:?} is not a lowercase SHA-256")
            }
            ProblemKind::MissingPatch(patch) => {
                write!(f, "no {PATCH_DIR}/{patch}.{PATCH_EXTENSION}")
            }
            ProblemKind::UnknownVerdict(verdict) => {
                write!(f, "expected verdict {verdict:?} is not violation or clean")
            }
            ProblemKind::ViolationWithoutCodes => {
                write!(f, "a violation names the codes it expects")
            }
            ProblemKind::CleanWithCodes => write!(f, "a clean case expects no codes"),
            ProblemKind::UnknownCodes(codes) => {
                write!(f, "codes not in the list's `codes`: {}", codes.join(", "))
            }
            ProblemKind::ExpectedAndAcceptable(codes) => {
                write!(
                    f,
                    "codes both expected and acceptable: {}",
                    codes.join(", ")
                )
            }
        }
    }
}

/// Whether a mark is set: not `false`, `0`, `""`, `[]` or `{}`.
fn is_set(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

/// The patch's hash a file of [`PATCH_DIR`] holds, `None` for a file that
/// is not a patch.
pub fn patch_hash(file_name: &str) -> Option<&str> {
    file_name
        .strip_suffix(PATCH_EXTENSION)
        .and_then(|stem| stem.strip_suffix('.'))
        .filter(|hash| is_sha256(hash))
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Read the file `file_name` of `agent`'s `evals/`, whose text is `text`.
/// `patches` is the hashes of [`PATCH_DIR`]'s patches. Every problem is
/// returned, not only the first.
pub fn read_case_file(
    agent: &str,
    file_name: &str,
    text: &str,
    patches: &BTreeSet<String>,
) -> Result<CaseFile, Vec<CaseProblem>> {
    let file_problem = |kind| CaseProblem {
        file: file_name.to_owned(),
        case: None,
        kind,
    };
    let Some(split) = Split::from_file_name(file_name) else {
        return Err(vec![file_problem(ProblemKind::UnknownFile)]);
    };
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => return Err(vec![file_problem(ProblemKind::NotJson(error.to_string()))]),
    };
    let Some(object) = value.as_object() else {
        return Err(vec![file_problem(ProblemKind::WrongType {
            field: "(the file)".to_owned(),
            expected: "an object",
        })]);
    };
    let mut fields = Fields::new(object, "");
    let named = fields.string("agent");
    let role_name = fields.string("role");
    let codes = fields.strings("codes");
    let k = fields.positive("k");
    let cases = fields.array("cases");
    let mut problems: Vec<CaseProblem> = fields.problems.into_iter().map(file_problem).collect();
    if let Some(named) = &named
        && named != agent
    {
        problems.push(file_problem(ProblemKind::OtherAgent(named.clone())));
    }
    let role = role_name.and_then(|name| {
        let role = AgentRole::parse(&name);
        if role.is_none() {
            problems.push(file_problem(ProblemKind::UnknownRole(name)));
        }
        role
    });
    let (Some(role), Some(codes), Some(k), Some(cases)) = (role, codes, k, cases) else {
        return Err(problems);
    };
    let code_set: BTreeSet<String> = codes.iter().cloned().collect();
    let mut seen = BTreeSet::new();
    let mut read = Vec::new();
    for (index, value) in cases.iter().enumerate() {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(|| format!("#{index}"), str::to_owned);
        let case_problem = |kind| CaseProblem {
            file: file_name.to_owned(),
            case: Some(id.clone()),
            kind,
        };
        if !seen.insert(id.clone()) {
            problems.push(case_problem(ProblemKind::DuplicateId));
        }
        match read_case(value, role, split, &code_set, patches) {
            Ok(case) => read.push(case),
            Err(kinds) => problems.extend(kinds.into_iter().map(case_problem)),
        }
    }
    if problems.is_empty() {
        Ok(CaseFile {
            agent: agent.to_owned(),
            role,
            split,
            codes,
            k,
            cases: read,
        })
    } else {
        Err(problems)
    }
}

fn read_case(
    value: &Value,
    role: AgentRole,
    split: Split,
    codes: &BTreeSet<String>,
    patches: &BTreeSet<String>,
) -> Result<Case, Vec<ProblemKind>> {
    let Some(object) = value.as_object() else {
        return Err(vec![ProblemKind::WrongType {
            field: "cases[]".to_owned(),
            expected: "an object",
        }]);
    };
    let mut fields = Fields::new(object, "");
    let id = fields.string("id");
    let source = fields.string("source");
    let made_by = fields.string("made_by");
    let base_commit = fields.string("base_commit");
    let patch = fields.string("patch");
    let adjudicated = fields.nullable("adjudicated");
    // An empty or false mark is no dispute, as in the Spike.
    let disputed = fields.nullable("disputed").filter(is_set);
    let k = fields.optional_positive("k");
    let role_fields = fields.required(role.as_str());
    let mut problems = fields.problems;
    let source = source.and_then(|name| {
        let source = Source::parse(&name);
        if source.is_none() {
            problems.push(ProblemKind::UnknownSource(name));
        }
        source
    });
    if source == Some(Source::Handmade) && split != Split::Dev {
        problems.push(ProblemKind::HandmadeOutsideDev);
    }
    if let Some(patch) = &patch {
        if !is_sha256(patch) {
            problems.push(ProblemKind::NotAPatchHash(patch.clone()));
        } else if !patches.contains(patch) {
            problems.push(ProblemKind::MissingPatch(patch.clone()));
        }
    }
    let role_case = role_fields.and_then(|fields| match role.read_case(fields, codes) {
        Ok(case) => Some(case),
        Err(kinds) => {
            problems.extend(kinds);
            None
        }
    });
    match (id, source, made_by, base_commit, patch, role_case) {
        (Some(id), Some(source), Some(made_by), Some(base_commit), Some(patch), Some(role))
            if problems.is_empty() =>
        {
            Ok(Case {
                id,
                source,
                made_by,
                base_commit,
                patch,
                adjudicated,
                disputed,
                k,
                role,
            })
        }
        _ => Err(problems),
    }
}

/// The case lists of one agent's `evals/`.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCases {
    pub agent: String,
    /// The lists there are, in [`Split::ALL`]'s order; a split may have none.
    pub files: Vec<CaseFile>,
}

impl AgentCases {
    pub fn split(&self, split: Split) -> Option<&CaseFile> {
        self.files.iter().find(|file| file.split == split)
    }
}

/// Read every file of `agent`'s `evals/`, given as (file name, text).
/// A file that holds no split is a problem, as is any of the lists'.
pub fn read_agent_cases(
    agent: &str,
    files: &[(&str, &str)],
    patches: &BTreeSet<String>,
) -> Result<AgentCases, Vec<CaseProblem>> {
    let mut problems = Vec::new();
    let mut read = Vec::new();
    for (name, text) in files {
        match read_case_file(agent, name, text, patches) {
            Ok(file) => read.push(file),
            Err(found) => problems.extend(found),
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    read.sort_by_key(|file| file.split);
    Ok(AgentCases {
        agent: agent.to_owned(),
        files: read,
    })
}

/// The patches of [`PATCH_DIR`] no case of `lists` refers to, in order.
pub fn unreferenced_patches<'a>(
    lists: impl IntoIterator<Item = &'a CaseFile>,
    patches: &BTreeSet<String>,
) -> Vec<String> {
    let referred: BTreeSet<&str> = lists
        .into_iter()
        .flat_map(|list| list.cases.iter().map(|case| case.patch.as_str()))
        .collect();
    patches
        .iter()
        .filter(|patch| !referred.contains(patch.as_str()))
        .cloned()
        .collect()
}

/// The fields of one JSON object, read one by one; each missing or
/// mistyped field is recorded in `problems` with its path.
pub(crate) struct Fields<'a> {
    object: &'a Map<String, Value>,
    prefix: &'a str,
    pub(crate) problems: Vec<ProblemKind>,
}

impl<'a> Fields<'a> {
    pub(crate) fn new(object: &'a Map<String, Value>, prefix: &'a str) -> Self {
        Fields {
            object,
            prefix,
            problems: Vec::new(),
        }
    }

    fn path(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    fn wrong(&mut self, key: &str, expected: &'static str) {
        let field = self.path(key);
        self.problems
            .push(ProblemKind::WrongType { field, expected });
    }

    pub(crate) fn required(&mut self, key: &str) -> Option<&'a Value> {
        let value = self.object.get(key).filter(|value| !value.is_null());
        if value.is_none() {
            let field = self.path(key);
            self.problems.push(ProblemKind::MissingField(field));
        }
        value
    }

    /// The value, `None` when it is missing or `null`.
    fn nullable(&self, key: &str) -> Option<Value> {
        self.object
            .get(key)
            .filter(|value| !value.is_null())
            .cloned()
    }

    pub(crate) fn string(&mut self, key: &str) -> Option<String> {
        let value = self.required(key)?;
        let text = value.as_str().map(str::to_owned);
        if text.is_none() {
            self.wrong(key, "a string");
        }
        text
    }

    fn string_list(&mut self, key: &str, value: &Value) -> Option<Vec<String>> {
        let list = value.as_array().and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        });
        if list.is_none() {
            self.wrong(key, "a list of strings");
        }
        list
    }

    pub(crate) fn strings(&mut self, key: &str) -> Option<Vec<String>> {
        let value = self.required(key)?;
        self.string_list(key, value)
    }

    /// A list of strings that may be missing or `null` (an empty list).
    pub(crate) fn optional_strings(&mut self, key: &str) -> Option<Vec<String>> {
        match self.object.get(key) {
            None | Some(Value::Null) => Some(Vec::new()),
            Some(value) => self.string_list(key, value),
        }
    }

    pub(crate) fn optional_string(&mut self, key: &str) -> Option<String> {
        match self.object.get(key) {
            None | Some(Value::Null) => Some(String::new()),
            Some(Value::String(text)) => Some(text.clone()),
            Some(_) => {
                self.wrong(key, "a string");
                None
            }
        }
    }

    pub(crate) fn optional_bool(&mut self, key: &str) -> Option<bool> {
        match self.object.get(key) {
            None | Some(Value::Null) => Some(false),
            Some(Value::Bool(flag)) => Some(*flag),
            Some(_) => {
                self.wrong(key, "true or false");
                None
            }
        }
    }

    fn positive_of(&mut self, key: &str, value: &Value) -> Option<u32> {
        let k = value
            .as_u64()
            .filter(|k| *k >= 1)
            .and_then(|k| u32::try_from(k).ok());
        if k.is_none() {
            self.wrong(key, "a positive integer");
        }
        k
    }

    fn positive(&mut self, key: &str) -> Option<u32> {
        let value = self.required(key)?;
        self.positive_of(key, value)
    }

    fn optional_positive(&mut self, key: &str) -> Option<u32> {
        let value = self.object.get(key).filter(|value| !value.is_null())?;
        self.positive_of(key, value)
    }

    fn array(&mut self, key: &str) -> Option<&'a Vec<Value>> {
        let value = self.required(key)?;
        let items = value.as_array();
        if items.is_none() {
            self.wrong(key, "a list");
        }
        items
    }

    pub(crate) fn object(&mut self, key: &str) -> Option<&'a Map<String, Value>> {
        let value = self.required(key)?;
        let object = value.as_object();
        if object.is_none() {
            self.wrong(key, "an object");
        }
        object
    }
}

/// `part / whole`, `None` when `whole` is 0.
pub(crate) fn ratio(part: usize, whole: usize) -> Option<f64> {
    (whole > 0).then(|| part as f64 / whole as f64)
}

/// The per-code values of a score, by code.
pub type PerCode<T> = BTreeMap<String, T>;

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_PATCH: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn patches() -> BTreeSet<String> {
        [PATCH.to_owned(), OTHER_PATCH.to_owned()].into()
    }

    fn case(id: &str, source: &str, patch: &str, expected: Value) -> Value {
        serde_json::json!({
            "id": id,
            "source": source,
            "made_by": "codex-cli 0.160.0",
            "base_commit": "f46a7963cf708d621bf529eb855ecfb552f78b33",
            "patch": patch,
            "adjudicated": null,
            "disputed": null,
            "base_kind": "exact",
            "review": {"input": {}, "expected": expected},
        })
    }

    fn list(cases: Vec<Value>) -> String {
        serde_json::json!({
            "agent": "adr-rules",
            "role": "review",
            "codes": ["A-150", "A-152"],
            "k": 3,
            "cases": cases,
        })
        .to_string()
    }

    fn violation() -> Value {
        serde_json::json!({
            "verdict": "violation", "codes": ["A-150"],
            "acceptable_codes": ["A-152"], "note": "an accepted ADR rewritten",
        })
    }

    fn clean() -> Value {
        serde_json::json!({"verdict": "clean", "codes": [], "note": "", "near_miss": true})
    }

    fn kinds(result: Result<CaseFile, Vec<CaseProblem>>) -> Vec<(Option<String>, ProblemKind)> {
        result
            .unwrap_err()
            .into_iter()
            .map(|problem| (problem.case, problem.kind))
            .collect()
    }

    #[test]
    fn a_list_in_the_designs_shape_reads_with_its_common_and_role_fields() {
        let mut disputed = case("gen-b", "production", OTHER_PATCH, clean());
        disputed["disputed"] = serde_json::json!({"reason": "ambiguous"});
        disputed["k"] = serde_json::json!(1);
        let mut unmarked = case("gen-c", "generated", PATCH, clean());
        unmarked["disputed"] = serde_json::json!(false);
        let text = list(vec![
            case("gen-a", "handmade", PATCH, violation()),
            disputed,
            unmarked,
        ]);

        let file = read_case_file("adr-rules", "dev.json", &text, &patches()).unwrap();

        assert_eq!(file.split, Split::Dev);
        assert_eq!(file.role, AgentRole::Review);
        assert_eq!(file.k, 3);
        let [a, b, c] = &file.cases[..] else {
            panic!("three cases")
        };
        assert!(!c.is_disputed());
        assert_eq!(a.source, Source::Handmade);
        assert_eq!((a.runs(file.k), b.runs(file.k)), (3, 1));
        assert!(!a.is_disputed() && b.is_disputed());
        let expected = &a.review().unwrap().expected;
        assert_eq!(expected.verdict, review::Judgement::Violation);
        assert_eq!(expected.codes, BTreeSet::from(["A-150".to_owned()]));
        assert_eq!(
            expected.acceptable_codes,
            BTreeSet::from(["A-152".to_owned()])
        );
        assert!(b.review().unwrap().expected.near_miss);
    }

    #[test]
    fn a_file_of_evals_that_holds_no_split_is_a_problem() {
        let text = list(vec![case("gen-a", "generated", PATCH, violation())]);
        assert_eq!(
            kinds(read_case_file("adr-rules", "evals.json", &text, &patches())),
            [(None, ProblemKind::UnknownFile)]
        );
        let err = read_agent_cases(
            "adr-rules",
            &[("dev.json", &text), ("notes.json", "{}")],
            &patches(),
        )
        .unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].file, "notes.json");
        assert!(
            err[0]
                .to_string()
                .contains("dev.json, holdout.json, production.json")
        );
    }

    #[test]
    fn missing_fields_are_problems_with_their_paths() {
        let mut no_patch = case("gen-a", "generated", PATCH, violation());
        no_patch.as_object_mut().unwrap().remove("patch");
        no_patch["review"]
            .as_object_mut()
            .unwrap()
            .remove("expected");
        let mut no_role = case("gen-b", "generated", PATCH, clean());
        no_role.as_object_mut().unwrap().remove("review");
        let text = list(vec![no_patch, no_role]);

        assert_eq!(
            kinds(read_case_file("adr-rules", "dev.json", &text, &patches())),
            [
                (
                    Some("gen-a".to_owned()),
                    ProblemKind::MissingField("patch".to_owned())
                ),
                (
                    Some("gen-a".to_owned()),
                    ProblemKind::MissingField("review.expected".to_owned())
                ),
                (
                    Some("gen-b".to_owned()),
                    ProblemKind::MissingField("review".to_owned())
                ),
            ]
        );

        let top = serde_json::json!({"agent": "adr-rules", "role": "review", "k": 0}).to_string();
        assert_eq!(
            kinds(read_case_file("adr-rules", "dev.json", &top, &patches())),
            [
                (None, ProblemKind::MissingField("codes".to_owned())),
                (
                    None,
                    ProblemKind::WrongType {
                        field: "k".to_owned(),
                        expected: "a positive integer"
                    }
                ),
                (None, ProblemKind::MissingField("cases".to_owned())),
            ]
        );
    }

    #[test]
    fn a_patch_the_store_does_not_have_is_a_problem() {
        let missing = "c".repeat(64);
        let text = list(vec![
            case("gen-a", "generated", &missing, violation()),
            case("gen-b", "generated", "not-a-hash", clean()),
        ]);
        assert_eq!(
            kinds(read_case_file(
                "adr-rules",
                "holdout.json",
                &text,
                &patches()
            )),
            [
                (Some("gen-a".to_owned()), ProblemKind::MissingPatch(missing)),
                (
                    Some("gen-b".to_owned()),
                    ProblemKind::NotAPatchHash("not-a-hash".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn an_id_used_twice_in_a_list_is_a_problem() {
        let text = list(vec![
            case("gen-a", "generated", PATCH, violation()),
            case("gen-a", "generated", OTHER_PATCH, clean()),
        ]);
        assert_eq!(
            kinds(read_case_file("adr-rules", "dev.json", &text, &patches())),
            [(Some("gen-a".to_owned()), ProblemKind::DuplicateId)]
        );
    }

    #[test]
    fn the_lists_own_agent_role_and_sources_are_checked() {
        let other = list(vec![case("gen-a", "handmade", PATCH, violation())])
            .replace("\"adr-rules\"", "\"migration-rules\"")
            .replace("\"role\":\"review\"", "\"role\":\"planner\"");
        assert_eq!(
            kinds(read_case_file(
                "adr-rules",
                "holdout.json",
                &other,
                &patches()
            )),
            [
                (None, ProblemKind::OtherAgent("migration-rules".to_owned())),
                (None, ProblemKind::UnknownRole("planner".to_owned())),
            ]
        );
        let text = list(vec![
            case("gen-a", "handmade", PATCH, violation()),
            case("gen-b", "scraped", PATCH, clean()),
        ]);
        assert_eq!(
            kinds(read_case_file(
                "adr-rules",
                "production.json",
                &text,
                &patches()
            )),
            [
                (Some("gen-a".to_owned()), ProblemKind::HandmadeOutsideDev),
                (
                    Some("gen-b".to_owned()),
                    ProblemKind::UnknownSource("scraped".to_owned())
                ),
            ]
        );
        assert_eq!(
            kinds(read_case_file("adr-rules", "dev.json", "[", &patches())).len(),
            1
        );
    }

    #[test]
    fn patches_no_case_refers_to_are_listed() {
        let text = list(vec![case("gen-a", "generated", PATCH, violation())]);
        let file = read_case_file("adr-rules", "dev.json", &text, &patches()).unwrap();
        assert_eq!(
            unreferenced_patches([&file], &patches()),
            [OTHER_PATCH.to_owned()]
        );
    }

    #[test]
    fn a_patch_file_name_gives_its_hash() {
        assert_eq!(patch_hash(&format!("{PATCH}.patch")), Some(PATCH));
        assert_eq!(patch_hash("README.md"), None);
        assert_eq!(patch_hash("abc.patch"), None);
    }
}
