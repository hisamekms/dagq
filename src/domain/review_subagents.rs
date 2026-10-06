//! The review's subagents (ADR-t1453-1): `dagq.toml` names each one with
//! `[review.subagents.<agent>] paths`, and a review must run every agent
//! one of whose globs a path the reviewed commit changes matches. Which
//! agents a review needs is decided here, without side effects; the
//! supervisor reads the configuration and the definitions from the
//! landing branch's committed tree.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::concern::{
    self, ConcernDecision, ConcernReason, EscalatedBecause, LandingRecommendation,
};
use super::scope::glob_matches;
use super::{AskConfidence, ReviewDecision, review_reason};

/// Where the agents' definitions are, relative to the repository root:
/// one directory per agent (ADR-t1728-1, which amends ADR-t1453-1
/// decision 2).
pub const DEFINITION_DIR: &str = ".dagq/agents";

/// The definition's file in an agent's directory.
pub const DEFINITION_FILE: &str = "AGENT.md";

/// Where the definitions were before ADR-t1728-1, one `<agent>.md` each;
/// read while the definitions move, when an agent has none at
/// [`definition_path`].
pub const LEGACY_DEFINITION_DIR: &str = ".dagq/review-agents";

/// The configuration's section of the review's subagents.
pub const REVIEW_SECTION: &str = "review.subagents";

/// Every section of the configuration that gives an agent a role, each
/// `[<section>.<agent>]`: an agent has one role (ADR-t1728-1), so one
/// agent named in two of them is a mistake ([`config_problems`]).
pub const ROLE_SECTIONS: &[&str] = &[REVIEW_SECTION];

/// One `[review.subagents.<agent>]`: the agent's name and the globs that
/// make it required, each once, in the order written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSubagent {
    pub name: String,
    pub paths: Vec<String>,
}

/// An agent a review needs, with the changed paths that made it so, in
/// the order of the changed paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedAgent {
    pub name: String,
    pub matched: Vec<String>,
}

/// Whether `name` is kebab-case: lowercase ASCII letters and digits in
/// words joined by single `-`, as a file name of the definition can be.
pub fn valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// The repository-relative path of `agent`'s definition.
pub fn definition_path(agent: &str) -> String {
    format!("{DEFINITION_DIR}/{agent}/{DEFINITION_FILE}")
}

/// The repository-relative path of `agent`'s definition before
/// ADR-t1728-1.
pub fn legacy_definition_path(agent: &str) -> String {
    format!("{LEGACY_DEFINITION_DIR}/{agent}.md")
}

/// The paths `agent`'s definition is read from, in order: the first that
/// a commit's tree has is the definition, and none means it has none.
pub fn definition_paths(agent: &str) -> [String; 2] {
    [definition_path(agent), legacy_definition_path(agent)]
}

/// `agent`'s definition as `read` finds it at [`definition_paths`]: the
/// first path it has and that path's text, `None` when it has neither.
pub fn find_definition<E>(
    agent: &str,
    mut read: impl FnMut(&str) -> Result<Option<String>, E>,
) -> Result<Option<(String, String)>, E> {
    for path in definition_paths(agent) {
        if let Some(text) = read(&path)? {
            return Ok(Some((path, text)));
        }
    }
    Ok(None)
}

/// One agent a role's section of the configuration names: the section
/// (one of [`ROLE_SECTIONS`]) and the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedAgent {
    pub section: &'static str,
    pub agent: String,
}

/// The agents the review's sections name, as [`config_problems`] reads them.
pub fn named_for_review(configured: &[ReviewSubagent]) -> Vec<NamedAgent> {
    configured
        .iter()
        .map(|agent| NamedAgent {
            section: REVIEW_SECTION,
            agent: agent.name.clone(),
        })
        .collect()
}

/// The mistakes of the role sections' `named` agents (ADR-t1728-1): an
/// agent whose definition is not there (`defined` says whether it is) and
/// an agent named in more than one role's section, each once, in the order
/// named. None when there is none.
pub fn config_problems(named: &[NamedAgent], defined: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for entry in named {
        if seen.contains(&entry.agent.as_str()) {
            continue;
        }
        seen.push(&entry.agent);
        let mut sections: Vec<&str> = Vec::new();
        for other in named.iter().filter(|other| other.agent == entry.agent) {
            if !sections.contains(&other.section) {
                sections.push(other.section);
            }
        }
        if sections.len() > 1 {
            problems.push(format!(
                "the agent {} is named in more than one role's section: {}; an agent has one role",
                entry.agent,
                sections
                    .iter()
                    .map(|section| format!("[{section}.{}]", entry.agent))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !defined(&entry.agent) {
            let [path, legacy] = definition_paths(&entry.agent);
            problems.push(format!(
                "[{}.{}] names an agent without a definition: neither {path} nor {legacy} is committed",
                entry.section, entry.agent
            ));
        }
    }
    problems
}

/// The agents of `configured` one of whose globs matches a path of
/// `changed`, each once, in the order configured. `changed` is the
/// reviewed range's paths as Git lists them without rename detection:
/// both sides of a rename and the old path of a deletion, so either side
/// selects an agent (ADR-t1453-1 decision 3). Nothing configured selects
/// nothing.
pub fn select(configured: &[ReviewSubagent], changed: &[String]) -> Vec<SelectedAgent> {
    let mut selected: Vec<SelectedAgent> = Vec::new();
    for agent in configured {
        if selected.iter().any(|s| s.name == agent.name) {
            continue;
        }
        let mut matched: Vec<String> = Vec::new();
        for path in changed {
            if !matched.contains(path) && agent.paths.iter().any(|glob| glob_matches(glob, path)) {
                matched.push(path.clone());
            }
        }
        if !matched.is_empty() {
            selected.push(SelectedAgent {
                name: agent.name.clone(),
                matched,
            });
        }
    }
    selected
}

/// The `status` of an agent's result that finished its checks; any other
/// (`failed`, or what a job made up) is a result the review lacks.
pub const COMPLETED: &str = "completed";

/// The tools a review subagent may use, whatever its definition says: the
/// review job's own reads (ADR-t1453-1 decision 2).
pub const SUBAGENT_TOOLS: &[&str] = &["Read", "Grep", "Glob"];

/// A tool an agent may declare in its definition's frontmatter
/// (ADR-t1728-2): the runtime's list, whose names a definition picks from;
/// it cannot name a command. Each provider's own tools for it are the
/// provider's adapter's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentTool {
    /// Read a file.
    Read,
    /// Search files' contents.
    Grep,
    /// Find paths.
    Glob,
    /// Run a command.
    Shell,
    /// Edit a file.
    Edit,
    /// Write a file.
    Write,
}

impl AgentTool {
    /// The runtime's list, in the order a declaration is kept in.
    pub const ALL: [Self; 6] = [
        Self::Read,
        Self::Grep,
        Self::Glob,
        Self::Shell,
        Self::Edit,
        Self::Write,
    ];

    /// The name a definition declares it by.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Grep => "grep",
            Self::Glob => "glob",
            Self::Shell => "shell",
            Self::Edit => "edit",
            Self::Write => "write",
        }
    }

    /// The tool of the list named `name`, `None` for a name not in it.
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }
}

/// The role an agent is used in (ADR-t1728-1: one agent, one role), which
/// bounds the tools its definition may declare (ADR-t1728-2 decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    /// `[review.subagents.<agent>]`: it only reads.
    Review,
}

impl AgentRole {
    /// The configuration's section of the role.
    pub const fn section(self) -> &'static str {
        match self {
            Self::Review => REVIEW_SECTION,
        }
    }

    /// Whether the role may use `tool`: a review only reads.
    pub const fn allows(self, tool: AgentTool) -> bool {
        match self {
            Self::Review => matches!(tool, AgentTool::Read | AgentTool::Grep | AgentTool::Glob),
        }
    }

    /// The tools of a definition of the role that declares none: a
    /// review's reads, as the review job's own (ADR-t1453-1 decision 2).
    pub fn default_tools(self) -> AgentTools {
        match self {
            Self::Review => AgentTools(vec![AgentTool::Read, AgentTool::Grep, AgentTool::Glob]),
        }
    }
}

/// The tools an agent's job is given, as its definition declares them (or
/// its role's default): each once, in the order of [`AgentTool::ALL`]. A
/// provider's adapter turns it into the job's own setting (ADR-t1728-2
/// decision 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTools(Vec<AgentTool>);

impl AgentTools {
    pub fn new(tools: impl IntoIterator<Item = AgentTool>) -> Self {
        let mut tools: Vec<AgentTool> = tools.into_iter().collect();
        tools.sort();
        tools.dedup();
        Self(tools)
    }

    pub fn tools(&self) -> &[AgentTool] {
        &self.0
    }

    pub fn has(&self, tool: AgentTool) -> bool {
        self.0.contains(&tool)
    }

    /// The tools of the definition `text` of an agent of `role`
    /// (ADR-t1728-2): its frontmatter's `tools`, a list of names from
    /// [`AgentTool::ALL`] (`tools: [read, grep]` or one `- name` a line),
    /// or the role's [`AgentRole::default_tools`] without it. `Err` says
    /// why the declaration is a mistake: a name not in the list, a tool
    /// the role may not use, or a `tools` that is no list. Such a
    /// definition's agent is not started, and a review that requires it
    /// does not pass (ADR-t1453-1 decision 6).
    pub fn declared(role: AgentRole, text: &str) -> Result<Self, String> {
        let Some(names) = declared_tool_names(text)? else {
            return Ok(role.default_tools());
        };
        let mut tools = Vec::new();
        let mut problems = Vec::new();
        for name in &names {
            match AgentTool::named(name) {
                Some(tool) if role.allows(tool) => tools.push(tool),
                Some(tool) => problems.push(format!(
                    "{} is beyond what an agent of [{}] may use ({})",
                    tool.name(),
                    role.section(),
                    AgentTool::ALL
                        .into_iter()
                        .filter(|tool| role.allows(*tool))
                        .map(AgentTool::name)
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                None => problems.push(format!(
                    "{name:?} is not a tool of the runtime's list ({})",
                    AgentTool::ALL.map(AgentTool::name).join(", ")
                )),
            }
        }
        if problems.is_empty() {
            Ok(Self::new(tools))
        } else {
            Err(format!(
                "the frontmatter's tools declares {}",
                problems.join("; ")
            ))
        }
    }
}

/// The names the frontmatter's `tools` of `text` lists, `None` without a
/// frontmatter or a `tools`; `Err` when `tools` is no list.
fn declared_tool_names(text: &str) -> Result<Option<Vec<String>>, String> {
    let normalized = text.replace("\r\n", "\n");
    let Some((front, _)) = split_frontmatter(&normalized) else {
        return Ok(None);
    };
    let mut lines = front.lines();
    let Some(value) = lines.by_ref().find_map(|line| line.strip_prefix("tools:")) else {
        return Ok(None);
    };
    let unquote = |name: &str| {
        name.trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_owned()
    };
    // A YAML comment ends the line's value.
    let uncomment = |line: &str| -> String {
        let cut = line
            .char_indices()
            .find(|&(at, c)| c == '#' && (at == 0 || line[..at].ends_with(char::is_whitespace)))
            .map_or(line.len(), |(at, _)| at);
        line[..cut].trim().to_owned()
    };
    let value = uncomment(value);
    let value = value.as_str();
    if let Some(flow) = value.strip_prefix('[') {
        let inner = flow
            .strip_suffix(']')
            .ok_or_else(|| format!("the frontmatter's tools is not a list: [{flow}"))?;
        return Ok(Some(
            inner
                .split(',')
                .map(unquote)
                .filter(|name| !name.is_empty())
                .collect(),
        ));
    }
    if !value.is_empty() {
        return Err(format!(
            "the frontmatter's tools is not a list: {value} (write tools: [{}] with names of the runtime's list)",
            AgentTool::ALL.map(AgentTool::name).join(", ")
        ));
    }
    let items: Vec<String> = lines
        .map_while(|line| {
            line.trim_start()
                .strip_prefix("- ")
                .map(|item| unquote(&uncomment(item)))
        })
        .collect();
    if items.is_empty() {
        return Err(
            "the frontmatter's tools lists no tool: write tools: [] for none, or leave it out for the role's default"
                .to_owned(),
        );
    }
    Ok(Some(items))
}

/// The frontmatter and the body of `text` (with `\n` line ends): a `---`
/// line, the frontmatter, and a `---` line; `None` without one.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let (front_end, body_start) = rest.find("\n---\n").map(|at| (at, at + 5)).or_else(|| {
        rest.strip_suffix("\n---")
            .map(|front| (front.len(), rest.len()))
    })?;
    Some((&rest[..front_end], &rest[body_start..]))
}

/// One required agent's result in the review's verdict (ADR-t1453-1
/// decision 5): its name, whether it completed, and a judgment of the
/// verdict's own form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "PrintedAgentResult")]
pub struct AgentResult {
    pub agent: String,
    pub status: String,
    /// `None` when the agent gave no judgment (it did not complete).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<ReviewDecision>,
    pub reasons: Vec<String>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<LandingRecommendation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<AskConfidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_category: Option<ConcernReason>,
}

/// An agent's result as the job printed it; a field it does not know
/// makes the verdict unreadable, as the verdict's own do.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrintedAgentResult {
    agent: String,
    status: String,
    #[serde(default)]
    verdict: Option<ReviewDecision>,
    #[serde(default)]
    reasons: Vec<review_reason::PrintedReason>,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    recommendation: Option<String>,
    #[serde(default)]
    confidence: Option<String>,
    #[serde(default)]
    reason_category: Option<String>,
}

impl From<PrintedAgentResult> for AgentResult {
    fn from(printed: PrintedAgentResult) -> Self {
        let (reasons, _) = review_reason::split(printed.reasons);
        // Only a concern's are read, as the verdict's.
        let concern = printed.verdict == Some(ReviewDecision::Concern);
        Self {
            agent: printed.agent,
            status: printed.status,
            verdict: printed.verdict,
            reasons,
            summary: printed.summary,
            recommendation: concern
                .then(|| concern::known(printed.recommendation.as_deref()))
                .flatten(),
            confidence: concern
                .then(|| concern::known(printed.confidence.as_deref()))
                .flatten(),
            reason_category: concern
                .then(|| concern::reason(printed.reason_category.as_deref()))
                .flatten(),
        }
    }
}

/// Why `results` are not the completed results of exactly the agents
/// `required` (ADR-t1453-1 decision 6): agents without a result, results
/// that did not complete or gave no judgment, results of agents not
/// required and an agent's second result. `None` when they are; a review
/// that requires no agent must name none, so a verdict of a repository
/// without agents reads as before.
pub fn incomplete(required: &[String], results: &[AgentResult]) -> Option<String> {
    let mut problems = Vec::new();
    for agent in required {
        let mine: Vec<&AgentResult> = results.iter().filter(|r| &r.agent == agent).collect();
        match mine.as_slice() {
            [] => problems.push(format!("no result of {agent}")),
            [result] if result.status != COMPLETED => {
                problems.push(format!("{agent} did not complete ({})", result.status));
            }
            [result] if result.verdict.is_none() => {
                problems.push(format!("{agent} gave no verdict"));
            }
            [_] => {}
            _ => problems.push(format!("{} results of {agent}", mine.len())),
        }
    }
    for result in results {
        if !required.contains(&result.agent) {
            problems.push(format!(
                "a result of {}, which this review does not require",
                result.agent
            ));
        }
    }
    (!problems.is_empty()).then(|| {
        format!(
            "the verdict lacks the completed results of the review's required subagents: {}",
            problems.join("; ")
        )
    })
}

/// Where a judgment of a review sends the run (ADR-t1453-1 decision 7),
/// lightest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Destination {
    Land,
    SendBack,
    Ask,
}

impl Destination {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Land => "land",
            Self::SendBack => "send_back",
            Self::Ask => "ask",
        }
    }
}

/// Where one judgment goes by the rule of a `concern` (ADR-t451-1
/// decision 3): a `pass` lands, a `revise` goes back (its limit is the
/// revise's own), and a `concern` lands or goes back only when it may be
/// applied, else asks; with why it asks, for a concern.
pub fn destination(
    decision: ReviewDecision,
    recommendation: Option<LandingRecommendation>,
    confidence: Option<AskConfidence>,
    reason_category: Option<ConcernReason>,
    revise_left: bool,
) -> (Destination, Option<EscalatedBecause>) {
    match decision {
        ReviewDecision::Pass => (Destination::Land, None),
        ReviewDecision::Revise => (Destination::SendBack, None),
        ReviewDecision::Concern => {
            match concern::decide(recommendation, confidence, reason_category, revise_left) {
                ConcernDecision::Land => (Destination::Land, None),
                ConcernDecision::SendBack => (Destination::SendBack, None),
                ConcernDecision::Ask(why) => (Destination::Ask, Some(why)),
            }
        }
    }
}

/// One agent's judgment and where it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRoute {
    pub agent: String,
    pub destination: Destination,
    pub escalated: Option<EscalatedBecause>,
}

/// Where a review with its agents' results goes (ADR-t1453-1 decision 7):
/// the heaviest of where the parent's judgment and each agent's go, so
/// the parent's aggregate never makes it lighter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictRoute {
    pub destination: Destination,
    pub parent: Destination,
    pub agents: Vec<AgentRoute>,
}

impl VerdictRoute {
    /// The agents whose judgment goes where the review goes, when that is
    /// heavier than landing: their reasons go with it.
    pub fn deciding(&self) -> Vec<&AgentRoute> {
        if self.destination == Destination::Land {
            return Vec::new();
        }
        self.agents
            .iter()
            .filter(|a| a.destination == self.destination)
            .collect()
    }

    /// Whether the parent's aggregate was lighter than an agent's judgment.
    pub fn parent_lighter(&self) -> bool {
        self.parent < self.destination
    }

    /// What `review_finished` records of the route.
    pub fn event_value(&self) -> Value {
        json!({
            "destination": self.destination.as_str(),
            "parent": self.parent.as_str(),
            "parent_lighter": self.parent_lighter(),
            "agents": self.agents.iter().map(|a| json!({
                "agent": a.agent,
                "destination": a.destination.as_str(),
                "escalated_because": a.escalated,
            })).collect::<Vec<_>>(),
        })
    }
}

/// [`VerdictRoute`] of a verdict whose decision and concern fields are
/// `parent` and whose agents gave `results` (all completed, see
/// [`incomplete`]); `revise_left` says whether the round has a revise left.
pub fn route(
    parent: (
        ReviewDecision,
        Option<LandingRecommendation>,
        Option<AskConfidence>,
        Option<ConcernReason>,
    ),
    results: &[AgentResult],
    revise_left: bool,
) -> VerdictRoute {
    let (decision, recommendation, confidence, reason_category) = parent;
    let (parent, _) = destination(
        decision,
        recommendation,
        confidence,
        reason_category,
        revise_left,
    );
    let agents: Vec<AgentRoute> = results
        .iter()
        .map(|r| {
            // A result with no judgment never reaches here; read as a
            // concern without a recommendation, it would ask.
            let (destination, escalated) = match r.verdict {
                Some(decision) => destination(
                    decision,
                    r.recommendation,
                    r.confidence,
                    r.reason_category,
                    revise_left,
                ),
                None => (Destination::Ask, Some(EscalatedBecause::NoRecommendation)),
            };
            AgentRoute {
                agent: r.agent.clone(),
                destination,
                escalated,
            }
        })
        .collect();
    let destination = agents
        .iter()
        .map(|a| a.destination)
        .fold(parent, Destination::max);
    VerdictRoute {
        destination,
        parent,
        agents,
    }
}

/// An agent's definition as a provider is handed it (ADR-t1453-1
/// decision 2): its name, the frontmatter's `description`, and the body
/// (its checks and the documents it refers to) as its prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub prompt: String,
}

impl AgentDefinition {
    /// Read `text`, the committed definition: a `---` frontmatter with
    /// `description`, then the body. Without a frontmatter or a
    /// description, the description names the agent and the whole text is
    /// the prompt.
    pub fn read(name: &str, text: &str) -> Self {
        let fallback = || format!("The review subagent {name}");
        let normalized = text.replace("\r\n", "\n");
        let parsed = split_frontmatter(&normalized).map(|(front, body)| {
            let description = front.lines().find_map(|line| {
                line.strip_prefix("description:").map(|value| {
                    value
                        .trim()
                        .trim_matches(|c| c == '"' || c == '\'')
                        .to_owned()
                })
            });
            (description, body.trim_start_matches('\n').to_owned())
        });
        match parsed {
            Some((description, body)) => Self {
                name: name.to_owned(),
                description: description
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(fallback),
                prompt: body,
            },
            None => Self {
                name: name.to_owned(),
                description: fallback(),
                prompt: text.to_owned(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    fn agent(name: &str, paths: &[&str]) -> ReviewSubagent {
        ReviewSubagent {
            name: name.to_owned(),
            paths: strings(paths),
        }
    }

    #[test]
    fn an_agent_is_selected_once_with_every_path_it_matches() {
        let configured = [
            agent("design", &["src/**", "docs/design/**"]),
            agent("migrations", &["migrations/*.sql"]),
            agent("plugin", &["plugins/**"]),
        ];
        let changed = strings(&[
            "src/lib.rs",
            "docs/design/review.md",
            "src/a/b.rs",
            "README.md",
        ]);
        assert_eq!(
            select(&configured, &changed),
            vec![SelectedAgent {
                name: "design".into(),
                matched: strings(&["src/lib.rs", "docs/design/review.md", "src/a/b.rs"]),
            }]
        );
        // A path two globs of one agent match counts once.
        let overlapping = [agent("design", &["src/**", "**/*.rs"])];
        assert_eq!(
            select(&overlapping, &strings(&["src/lib.rs", "src/lib.rs"])),
            vec![SelectedAgent {
                name: "design".into(),
                matched: strings(&["src/lib.rs"]),
            }]
        );
        assert!(select(&configured, &strings(&["README.md"])).is_empty());
    }

    #[test]
    fn either_side_of_a_rename_and_a_deleted_path_select_an_agent() {
        let configured = [agent("migrations", &["migrations/**"])];
        // `git diff --no-renames` lists a rename as the deleted old path
        // and the added new one.
        let only_old = strings(&["migrations/0001_a.sql", "attic/0001_a.sql"]);
        let only_new = strings(&["attic/b.sql", "migrations/b.sql"]);
        let deleted = strings(&["migrations/0002_b.sql"]);
        for (changed, path) in [
            (only_old, "migrations/0001_a.sql"),
            (only_new, "migrations/b.sql"),
            (deleted, "migrations/0002_b.sql"),
        ] {
            assert_eq!(
                select(&configured, &changed),
                vec![SelectedAgent {
                    name: "migrations".into(),
                    matched: strings(&[path]),
                }]
            );
        }
    }

    #[test]
    fn nothing_configured_selects_nothing_and_names_are_kebab_case() {
        assert!(select(&[], &strings(&["src/lib.rs"])).is_empty());
        for name in ["design", "design-consistency", "a1-b2"] {
            assert!(valid_agent_name(name), "{name}");
        }
        for name in ["", "Design", "-a", "a-", "a--b", "a_b", "a.b", "a/b"] {
            assert!(!valid_agent_name(name), "{name}");
        }
    }

    /// The definition is read from `.dagq/agents/<agent>/AGENT.md`, else
    /// from the path before ADR-t1728-1, else there is none.
    #[test]
    fn a_definition_is_read_from_the_new_path_then_the_old_one() {
        assert_eq!(definition_path("design"), ".dagq/agents/design/AGENT.md");
        assert_eq!(
            legacy_definition_path("design"),
            ".dagq/review-agents/design.md"
        );
        let found = |files: &[(&str, &str)]| {
            find_definition("design", |path| {
                Ok::<_, ()>(
                    files
                        .iter()
                        .find(|(p, _)| *p == path)
                        .map(|(_, text)| (*text).to_owned()),
                )
            })
            .unwrap()
        };
        let both = [
            (".dagq/review-agents/design.md", "old"),
            (".dagq/agents/design/AGENT.md", "new"),
        ];
        assert_eq!(
            found(&both),
            Some((".dagq/agents/design/AGENT.md".to_owned(), "new".to_owned()))
        );
        assert_eq!(
            found(&both[..1]),
            Some((".dagq/review-agents/design.md".to_owned(), "old".to_owned()))
        );
        assert_eq!(found(&[(".dagq/agents/other/AGENT.md", "x")]), None);
        assert_eq!(
            find_definition("design", |_| Err::<Option<String>, _>("unreadable")),
            Err("unreadable")
        );
    }

    /// An agent a role's section names without a definition, and one agent
    /// named in two roles' sections, are mistakes (ADR-t1728-1); the role
    /// sections are [`ROLE_SECTIONS`], so a role added there is checked
    /// alike.
    #[test]
    fn an_undefined_agent_and_an_agent_of_two_roles_are_mistakes() {
        assert_eq!(ROLE_SECTIONS, ["review.subagents"]);
        let review =
            named_for_review(&[agent("design", &["src/**"]), agent("tests", &["tests/**"])]);
        assert_eq!(
            review[0],
            NamedAgent {
                section: "review.subagents",
                agent: "design".into()
            }
        );
        assert!(config_problems(&review, &|_| true).is_empty());
        assert!(config_problems(&[], &|_| false).is_empty());
        assert_eq!(
            config_problems(&review, &|agent| agent == "design"),
            [
                "[review.subagents.tests] names an agent without a definition: neither .dagq/agents/tests/AGENT.md nor .dagq/review-agents/tests.md is committed"
            ]
        );
        let mut two_roles = review.clone();
        two_roles.push(NamedAgent {
            section: "eval.subagents",
            agent: "design".into(),
        });
        assert_eq!(
            config_problems(&two_roles, &|agent| agent != "design"),
            [
                "the agent design is named in more than one role's section: [review.subagents.design], [eval.subagents.design]; an agent has one role",
                "[review.subagents.design] names an agent without a definition: neither .dagq/agents/design/AGENT.md nor .dagq/review-agents/design.md is committed",
            ]
        );
    }

    fn verdict(json: serde_json::Value) -> super::super::ReviewVerdict {
        super::super::ReviewVerdict::parse(&json.to_string()).unwrap()
    }

    fn completed(agent: &str, decision: &str) -> serde_json::Value {
        json!({"agent": agent, "status": "completed", "verdict": decision, "reasons": [], "summary": "ok"})
    }

    /// The verdict reads its agents' results when it has them, reads as
    /// before without, and a field an agent's result does not know makes
    /// it unreadable, as the verdict's own do.
    #[test]
    fn a_verdict_reads_its_agents_results() {
        let with = verdict(json!({
            "verdict": "pass", "reasons": [], "summary": "ok",
            "agents": [
                completed("design", "pass"),
                {"agent": "lint", "status": "completed", "verdict": "concern",
                 "reasons": [{"text": "a", "codes": ["x"]}, "b"], "summary": "look",
                 "recommendation": "send_back", "confidence": "high", "reason_category": "discard"},
            ],
        }));
        assert_eq!(with.agents.len(), 2);
        assert_eq!(with.agents[1].reasons, ["a", "b"]);
        assert_eq!(
            with.agents[1].recommendation,
            Some(LandingRecommendation::SendBack)
        );
        assert_eq!(with.agents[1].confidence, Some(AskConfidence::High));
        assert_eq!(with.agents[1].reason_category, Some(ConcernReason::Discard));
        // Serialized as recorded in review_finished.
        assert_eq!(
            serde_json::to_value(&with.agents[0]).unwrap(),
            json!({"agent": "design", "status": "completed", "verdict": "pass", "reasons": [], "summary": "ok"})
        );
        // A pass's concern fields are not read; a failed agent may give
        // no judgment.
        let failed = verdict(json!({
            "verdict": "pass", "reasons": [], "summary": "ok",
            "agents": [{"agent": "design", "status": "failed", "recommendation": "land"}],
        }));
        assert_eq!(failed.agents[0].verdict, None);
        assert_eq!(failed.agents[0].recommendation, None);
        let without = verdict(json!({"verdict": "pass", "reasons": [], "summary": "ok"}));
        assert!(without.agents.is_empty());
        assert!(
            serde_json::to_value(&without)
                .unwrap()
                .get("agents")
                .is_none()
        );
        for bad in [
            json!({"verdict": "pass", "reasons": [], "summary": "ok",
                   "agents": [{"agent": "design", "status": "completed", "verdict": "pass", "extra": 1}]}),
            json!({"verdict": "pass", "reasons": [], "summary": "ok", "agents": [{"status": "completed"}]}),
            json!({"verdict": "pass", "reasons": [], "summary": "ok", "agents": {"design": "pass"}}),
        ] {
            assert!(
                super::super::ReviewVerdict::parse(&bad.to_string()).is_err(),
                "{bad}"
            );
        }
    }

    /// Only the completed results of exactly the required agents make a
    /// verdict whole (ADR-t1453-1 decision 6).
    #[test]
    fn a_verdict_lacking_a_required_result_is_incomplete() {
        let required = strings(&["design", "lint"]);
        let results = |values: Vec<serde_json::Value>| {
            verdict(json!({"verdict": "pass", "reasons": [], "summary": "ok", "agents": values}))
                .agents
        };
        assert_eq!(
            incomplete(
                &required,
                &results(vec![
                    completed("lint", "pass"),
                    completed("design", "revise")
                ])
            ),
            None
        );
        let why = |values| incomplete(&required, &results(values)).unwrap();
        assert!(why(vec![completed("design", "pass")]).ends_with(": no result of lint"));
        let failed = why(vec![
            completed("design", "pass"),
            json!({"agent": "lint", "status": "failed", "summary": "timed out"}),
        ]);
        assert!(
            failed.ends_with(": lint did not complete (failed)"),
            "{failed}"
        );
        let silent = why(vec![
            completed("design", "pass"),
            json!({"agent": "lint", "status": "completed"}),
        ]);
        assert!(silent.ends_with(": lint gave no verdict"), "{silent}");
        let twice = why(vec![
            completed("design", "pass"),
            completed("lint", "pass"),
            completed("lint", "pass"),
        ]);
        assert!(twice.ends_with(": 2 results of lint"), "{twice}");
        let unknown = why(vec![
            completed("design", "pass"),
            completed("lint", "pass"),
            completed("other", "pass"),
        ]);
        assert!(
            unknown.ends_with(": a result of other, which this review does not require"),
            "{unknown}"
        );
        // A review that requires none must name none; naming none is whole.
        assert_eq!(incomplete(&[], &[]), None);
        assert!(incomplete(&[], &results(vec![completed("design", "pass")])).is_some());
        assert!(why(vec![]).starts_with(
            "the verdict lacks the completed results of the review's required subagents: no result of design; no result of lint"
        ));
    }

    /// The review goes to the heaviest of where its judgments go, each
    /// decided by the rule of a concern (ADR-t1453-1 decision 7): an
    /// agent's revise or a concern a person must decide is never wrapped
    /// into a landing by the verdict.
    #[test]
    fn the_heaviest_judgment_decides_where_the_review_goes() {
        let route_of =
            |parent: serde_json::Value, revise_left: bool| verdict(parent).route(revise_left);
        let base = |decision: &str, extra: serde_json::Value, agents: Vec<serde_json::Value>| {
            let mut v =
                json!({"verdict": decision, "reasons": ["r"], "summary": "s", "agents": agents});
            for (key, value) in extra.as_object().unwrap() {
                v[key] = value.clone();
            }
            v
        };
        let land_high = json!({"recommendation": "land", "confidence": "high"});
        // All pass: land, decided by nobody but the verdict.
        let all = route_of(
            base(
                "pass",
                json!({}),
                vec![completed("a", "pass"), completed("b", "pass")],
            ),
            true,
        );
        assert_eq!(
            (all.destination, all.parent),
            (Destination::Land, Destination::Land)
        );
        assert!(all.deciding().is_empty() && !all.parent_lighter());
        // An agent's revise under a pass sends it back, the verdict lighter.
        let revise = route_of(
            base(
                "pass",
                json!({}),
                vec![completed("a", "pass"), completed("b", "revise")],
            ),
            true,
        );
        assert_eq!(revise.destination, Destination::SendBack);
        assert!(revise.parent_lighter());
        assert_eq!(
            revise
                .deciding()
                .iter()
                .map(|a| a.agent.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );
        // An agent's revise wrapped in a concern that would land on high
        // confidence still goes back.
        let wrapped = route_of(
            base("concern", land_high.clone(), vec![completed("b", "revise")]),
            true,
        );
        assert_eq!(
            (wrapped.destination, wrapped.parent),
            (Destination::SendBack, Destination::Land)
        );
        // An agent's scope concern asks, whatever the verdict.
        let scope = json!({"agent": "b", "status": "completed", "verdict": "concern", "reasons": ["x"],
                           "summary": "s", "recommendation": "land", "confidence": "high", "reason_category": "scope"});
        let asks = route_of(base("revise", json!({}), vec![scope.clone()]), true);
        assert_eq!(
            (asks.destination, asks.parent),
            (Destination::Ask, Destination::SendBack)
        );
        assert_eq!(asks.agents[0].escalated, Some(EscalatedBecause::Scope));
        // An agent's send_back past the revise limit asks.
        let send_back = json!({"agent": "b", "status": "completed", "verdict": "concern", "reasons": [],
                               "summary": "s", "recommendation": "send_back", "confidence": "high"});
        assert_eq!(
            route_of(base("pass", json!({}), vec![send_back.clone()]), true).destination,
            Destination::SendBack
        );
        let limit = route_of(base("pass", json!({}), vec![send_back]), false);
        assert_eq!(limit.destination, Destination::Ask);
        assert_eq!(
            limit.agents[0].escalated,
            Some(EscalatedBecause::ReviseLimit)
        );
        // A verdict heavier than its agents decides by itself.
        let heavier = route_of(
            base("revise", json!({}), vec![completed("a", "pass")]),
            true,
        );
        assert_eq!(heavier.destination, Destination::SendBack);
        assert!(heavier.deciding().is_empty() && !heavier.parent_lighter());
        assert_eq!(
            asks.event_value(),
            json!({"destination": "ask", "parent": "send_back", "parent_lighter": true,
                   "agents": [{"agent": "b", "destination": "ask", "escalated_because": "scope"}]})
        );
    }

    #[test]
    fn a_definition_reads_its_description_and_body() {
        let read = AgentDefinition::read(
            "design",
            "---\r\ntitle: x\r\ndescription: \"Checks the design\"\r\n---\r\n\r\nCheck it.\r\n",
        );
        assert_eq!(read.description, "Checks the design");
        assert_eq!(read.prompt, "Check it.\n");
        let empty = AgentDefinition::read("design", "---\ndescription:\n---");
        assert_eq!(empty.description, "The review subagent design");
        assert_eq!(empty.prompt, "");
        let plain = AgentDefinition::read("design", "---not a frontmatter\n");
        assert_eq!(plain.description, "The review subagent design");
        assert_eq!(plain.prompt, "---not a frontmatter\n");
        let open = AgentDefinition::read("design", "---\ndescription: d\nno end\n");
        assert_eq!(open.prompt, "---\ndescription: d\nno end\n");
        assert_eq!(Destination::SendBack.as_str(), "send_back");
    }

    #[test]
    fn a_definition_declares_its_tools_from_the_runtimes_list() {
        let review = |text: &str| AgentTools::declared(AgentRole::Review, text);
        let reads = AgentTools::new([AgentTool::Read, AgentTool::Grep, AgentTool::Glob]);
        // No declaration: the role's default, a review's reads.
        assert_eq!(
            review("---\ndescription: d\n---\nCheck.\n"),
            Ok(reads.clone())
        );
        assert_eq!(review("No frontmatter.\n"), Ok(reads.clone()));
        assert_eq!(AgentRole::Review.default_tools(), reads);
        // A flow or a block list, each tool once in the list's order.
        assert_eq!(
            review("---\ntools: [glob, \"read\", glob]\n---\n"),
            Ok(AgentTools::new([AgentTool::Read, AgentTool::Glob]))
        );
        assert_eq!(
            review(
                "---\r\ndescription: d\r\ntools:\r\n  - grep\r\n  - 'read'\r\nother: x\r\n---\r\n"
            ),
            Ok(AgentTools::new([AgentTool::Read, AgentTool::Grep]))
        );
        assert_eq!(review("---\ntools: []\n---\n"), Ok(AgentTools::new([])));
        assert_eq!(
            review("---\ntools: [read]\n---\n").unwrap().tools(),
            [AgentTool::Read]
        );
        // A name not in the list and a tool beyond a review's reads are
        // mistakes, each named.
        assert_eq!(
            review("---\ntools: [read, shell, fetch, write]\n---\n"),
            Err("the frontmatter's tools declares shell is beyond what an agent of [review.subagents] may use (read, grep, glob); \"fetch\" is not a tool of the runtime's list (read, grep, glob, shell, edit, write); write is beyond what an agent of [review.subagents] may use (read, grep, glob)".to_owned())
        );
        assert!(review("---\ntools: [edit]\n---\n").is_err());
        // A `tools` that is no list.
        assert_eq!(
            review("---\ntools: read\n---\n"),
            Err("the frontmatter's tools is not a list: read (write tools: [read, grep, glob, shell, edit, write] with names of the runtime's list)".to_owned())
        );
        // A YAML comment is not part of the value.
        assert_eq!(
            review("---\ntools: [read]  # reads only\n---\n"),
            Ok(AgentTools::new([AgentTool::Read]))
        );
        assert_eq!(
            review("---\ntools: # narrower\n  - grep # search\n---\n"),
            Ok(AgentTools::new([AgentTool::Grep]))
        );
        assert!(review("---\ntools: [read\n---\n").is_err());
        assert!(review("---\ntools:\n---\n").is_err());
        for tool in AgentTool::ALL {
            assert_eq!(AgentTool::named(tool.name()), Some(tool));
        }
        assert_eq!(AgentTool::named("Read"), None);
        assert!(!AgentTools::new([AgentTool::Read]).has(AgentTool::Grep));
    }
}
