//! The assembly of one agent job (ADR-t1895-1's agent job, ADR-t1728-1
//! decision 5): one agent's own headless job, whose prompt carries the
//! agent's definition as the snapshot of the landing branch returned it,
//! says where the change is, and asks for one verdict of the shape of one
//! agent's result in the review's verdict. The eval of an agent starts it
//! for each run of a case; it takes nothing of a run's review (the
//! definition, where the material is, the provider's launch and the tools
//! are its input), so a run's review starts its agents' jobs with it too.
//!
//! The prompt (ADR-t1566-1 decisions 1 to 6, ADR-t1869-1) goes on the
//! job's standard input. It holds the instructions and the verdict's
//! schema, the definition whole, the material's lines (the change's base
//! and head commits and the material file, where the commits and the full
//! diff are) and the language's instruction, each held to its limit of
//! [`super::prompt`] (`AGENT_JOB_*`). The definition is never cut: one
//! past its limit is an error ([`DefinitionOverLimit`]) and no job starts.
//! What a case expects (its verdict, codes, split and id) is never in it.
//! The job reads only the material file and the files of the tree it runs
//! in, with the tools its definition declares; it is given no way to read
//! the queue.

use std::fmt;
use std::path::Path;

use super::AgentJobLaunch;
use super::prompt::{
    AGENT_JOB_DEFINITION_BYTES, AGENT_JOB_INSTRUCTIONS_BYTES, AGENT_JOB_MATERIAL_BYTES,
    AGENT_JOB_PROMPT_LIMIT, FittedPrompt,
};
use super::prompt_fit::{Fit, Keep};
use crate::domain::language::Language;
use crate::domain::review_subagents::AgentTools;

/// What an agent job is built from.
pub struct AgentJob<'a> {
    /// The agent's name, which its verdict names.
    pub agent: &'a str,
    /// The definition's text as the landing branch's snapshot returned it,
    /// frontmatter included.
    pub definition: &'a str,
    /// The tools its definition declares, or its role's default.
    pub tools: &'a AgentTools,
    /// The change: `<base>...<head>`.
    pub base: &'a str,
    pub head: &'a str,
    /// The material file (the commits and the full diff), in `dir`.
    pub material: &'a Path,
    /// The tree of the change, where the job runs.
    pub cwd: &'a Path,
    /// The job's own directory.
    pub dir: &'a Path,
    pub language: Option<&'a Language>,
}

/// A definition past the agent job's limit (ADR-t1869-1): no prompt is
/// made, and no job starts for the agent. The eval records it as
/// `definition_over_limit` and starts no round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefinitionOverLimit {
    pub bytes: usize,
    pub limit: usize,
}

impl fmt::Display for DefinitionOverLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the definition takes {} bytes, past the agent job's limit of {}: it is never cut, so no job starts for it",
            self.bytes, self.limit
        )
    }
}

impl std::error::Error for DefinitionOverLimit {}

/// The agent job built: its prompt and what it takes, and its launch.
#[derive(Debug, Clone)]
pub struct BuiltAgentJob {
    pub prompt: FittedPrompt,
    pub launch: AgentJobLaunch,
}

/// The verdict's schema of an agent job: one agent's result of the
/// review's verdict, as the review reads it
/// (`review_subagents::AgentResult`).
fn schema(agent: &str) -> String {
    format!(
        "{{\"agent\": \"{agent}\", \"status\": \"completed\" | \"failed\", \"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [{{\"text\": string, \"codes\": [string]}}], \"summary\": string, \"recommendation\": \"land\" | \"send_back\" | null, \"confidence\": \"high\" | \"low\" | null, \"reason_category\": \"scope\" | \"discard\" | null}}"
    )
}

/// Whether `definition` fits the agent job's prompt whole
/// ([`AGENT_JOB_DEFINITION_BYTES`]); the eval checks it before a round
/// starts.
pub fn check_definition(definition: &str) -> Result<(), DefinitionOverLimit> {
    if definition.len() > AGENT_JOB_DEFINITION_BYTES {
        return Err(DefinitionOverLimit {
            bytes: definition.len(),
            limit: AGENT_JOB_DEFINITION_BYTES,
        });
    }
    Ok(())
}

/// Build the agent job of `job`: its prompt held to its limits, and its
/// launch. `Err` for a definition past [`AGENT_JOB_DEFINITION_BYTES`].
pub fn build(job: &AgentJob<'_>) -> Result<BuiltAgentJob, DefinitionOverLimit> {
    check_definition(job.definition)?;
    let material_path = job.material.display().to_string();
    let read = format!("read the whole change in the material file {material_path}");
    let mut fit = Fit::new(AGENT_JOB_PROMPT_LIMIT);
    let material = fit.text(
        "material",
        &format!(
            "The change: the commits from {base} to {head}, whose files are in your working directory as of {head}.\n\
             The material file {material_path} lists the commits and holds the full diff `{base}...{head}`.\n",
            base = job.base,
            head = job.head,
        ),
        AGENT_JOB_MATERIAL_BYTES,
        Keep::Start,
        &read,
    );
    fit.section("material", &material);
    fit.section("definition", job.definition);
    let instructions = fit.text(
        "instructions",
        &format!(
            "You are the review agent `{agent}`. Review one change by its definition below, and by nothing else.\n\
             Read the material file and the files of your working directory only; change no file, and run no command that changes one.\n",
            agent = job.agent,
        ),
        AGENT_JOB_INSTRUCTIONS_BYTES / 2,
        Keep::Start,
        &read,
    );
    let answer = fit.text(
        "instructions",
        &format!(
            "Answer with one JSON object and nothing else, matching this schema:\n{schema}\n\
             reasons lists each finding with the rule codes it breaks (empty for pass); summary is one or two sentences; recommendation, confidence and reason_category are for a concern only (null otherwise). Say status failed only when you could not review the change.\n",
            schema = schema(job.agent),
        ),
        AGENT_JOB_INSTRUCTIONS_BYTES / 2,
        Keep::Start,
        &read,
    );
    let text = format!(
        "{instructions}\n{material}\nThe definition of `{agent}`:\n<definition>\n{definition}\n</definition>\n\n{answer}",
        agent = job.agent,
        definition = job.definition,
    );
    let prompt = fit.finish(text).with_language(job.language);
    Ok(BuiltAgentJob {
        launch: AgentJobLaunch {
            cwd: job.cwd.to_owned(),
            dir: job.dir.to_owned(),
            material: job.material.to_owned(),
            prompt: prompt.text.clone(),
            tools: job.tools.clone(),
        },
        prompt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::prompt::PromptBytes;
    use crate::domain::review_subagents::{AgentRole, AgentTool};
    use std::path::PathBuf;

    struct Paths {
        material: PathBuf,
        cwd: PathBuf,
        dir: PathBuf,
    }

    fn paths() -> Paths {
        Paths {
            material: PathBuf::from("/q/agent-evals/7/3/material.md"),
            cwd: PathBuf::from("/q/agent-evals/7/3/tree"),
            dir: PathBuf::from("/q/agent-evals/7/3"),
        }
    }

    fn job<'a>(definition: &'a str, tools: &'a AgentTools, paths: &'a Paths) -> AgentJob<'a> {
        AgentJob {
            agent: "adr-rules",
            definition,
            tools,
            base: "1111111111111111111111111111111111111111",
            head: "2222222222222222222222222222222222222222",
            material: &paths.material,
            cwd: &paths.cwd,
            dir: &paths.dir,
            language: None,
        }
    }

    fn sections_sum(bytes: &PromptBytes) -> usize {
        bytes.sections.values().sum()
    }

    /// A definition of exactly the definition's limit is carried whole,
    /// not a byte cut, and the prompt stays within its whole limit with the
    /// language's room; the sections add up to the prompt (ADR-t1869-1
    /// decision 4, ADR-t1566-1 decision 6).
    #[test]
    fn a_definition_at_its_limit_is_carried_whole_within_the_whole_limit() {
        let head = "---\ndescription: checks the ADRs\ntools: [read, grep]\n---\n";
        let definition = format!(
            "{head}{}",
            "あ".repeat((AGENT_JOB_DEFINITION_BYTES - head.len()) / 3)
        );
        let definition = format!(
            "{definition}{}",
            "x".repeat(AGENT_JOB_DEFINITION_BYTES - definition.len())
        );
        assert_eq!(definition.len(), AGENT_JOB_DEFINITION_BYTES);
        let tools = AgentTools::declared(AgentRole::Review, &definition).unwrap();
        let paths = paths();
        let built = build(&job(&definition, &tools, &paths)).unwrap();
        let prompt = &built.prompt;
        assert!(prompt.text.contains(&definition), "the definition is whole");
        assert!(prompt.text.len() <= AGENT_JOB_PROMPT_LIMIT);
        assert!(
            prompt.text.len() <= AGENT_JOB_PROMPT_LIMIT - super::super::prompt_fit::LANGUAGE_ROOM
        );
        assert_eq!(prompt.bytes.total, prompt.text.len());
        assert_eq!(prompt.bytes.limit, AGENT_JOB_PROMPT_LIMIT);
        assert_eq!(sections_sum(&prompt.bytes), prompt.text.len());
        assert_eq!(prompt.bytes.sections["definition"], definition.len());
        assert!(prompt.bytes.omitted.is_empty(), "{:?}", prompt.bytes);
        assert_eq!(prompt.bytes.over_limit, None);
        // Its launch carries the same prompt, the tools, and no path of a
        // definition: the definition's text is what it is given.
        assert_eq!(built.launch.prompt, prompt.text);
        assert_eq!(
            built.launch.tools,
            AgentTools::new([AgentTool::Read, AgentTool::Grep])
        );
        assert!(!prompt.text.contains(".dagq/agents"));
        assert!(!prompt.text.contains("AGENT.md"));
        // With the language's instruction it is still within the limit.
        let language = Language {
            tag: "ja".into(),
            source: crate::domain::language::LanguageSource::Repository,
        };
        let spoken = build(&AgentJob {
            language: Some(&language),
            ..job(&definition, &tools, &paths)
        })
        .unwrap()
        .prompt;
        assert!(spoken.text.len() <= AGENT_JOB_PROMPT_LIMIT);
        assert!(spoken.bytes.sections["language"] > 0);
        assert_eq!(sections_sum(&spoken.bytes), spoken.text.len());
    }

    /// A definition one byte past its limit makes no prompt: it is never
    /// cut (ADR-t1869-1 decision 3).
    #[test]
    fn a_definition_past_its_limit_is_an_error_and_makes_no_prompt() {
        let definition = "d".repeat(AGENT_JOB_DEFINITION_BYTES + 1);
        let tools = AgentRole::Review.default_tools();
        let paths = paths();
        assert_eq!(
            build(&job(&definition, &tools, &paths)).unwrap_err(),
            DefinitionOverLimit {
                bytes: AGENT_JOB_DEFINITION_BYTES + 1,
                limit: AGENT_JOB_DEFINITION_BYTES,
            }
        );
    }

    /// The material's lines past their limit keep their start, and say how
    /// many bytes were left out and to read the material file
    /// (ADR-t1566-1 decisions 4 and 5).
    #[test]
    fn material_past_its_limit_keeps_its_start_and_names_the_file() {
        let tools = AgentRole::Review.default_tools();
        let paths = Paths {
            material: PathBuf::from(format!("/q/{}/material.md", "m".repeat(1_800))),
            ..paths()
        };
        let built = build(&job("check the ADRs", &tools, &paths)).unwrap();
        let prompt = &built.prompt;
        assert_eq!(prompt.bytes.omitted["material"], 1);
        assert!(
            prompt.bytes.sections["material"] <= AGENT_JOB_MATERIAL_BYTES,
            "{:?}",
            prompt.bytes
        );
        assert!(prompt.text.contains("The change: the commits from 1111"));
        assert!(prompt.text.contains(
            "bytes left out by the prompt's limit; read the whole change in the material file /q/"
        ));
        assert!(prompt.text.contains("check the ADRs"));
        assert_eq!(sections_sum(&prompt.bytes), prompt.text.len());
    }

    /// The prompt says the change and the schema, and nothing of what a
    /// case expects: the eval gives it neither the expected verdict, its
    /// codes, the split nor the case's id.
    #[test]
    fn the_prompt_carries_the_change_and_the_schema_and_no_expectation() {
        let tools = AgentRole::Review.default_tools();
        let paths = paths();
        let prompt = build(&job("Check A-150.", &tools, &paths))
            .unwrap()
            .prompt
            .text;
        for part in [
            "You are the review agent `adr-rules`",
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            "/q/agent-evals/7/3/material.md",
            "\"agent\": \"adr-rules\"",
            "\"codes\": [string]",
            "<definition>\nCheck A-150.\n</definition>",
        ] {
            assert!(prompt.contains(part), "{part} in {prompt}");
        }
        for word in ["expected", "holdout", "dev.json", "violation", "split"] {
            assert!(!prompt.contains(word), "{word} in {prompt}");
        }
        assert!(!prompt.contains("dagq "), "no queue command: {prompt}");
        // One agent's own job: nothing of a review job's subagents.
        assert!(!prompt.contains(crate::application::review::SUBAGENTS_INSTRUCTION));
        assert!(!prompt.contains("subagent"), "{prompt}");
    }
}
