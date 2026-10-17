//! The prompt of the job `setup-review` starts (ADR-t2259-1 decision 7):
//! read the project once and write the review's settings in the
//! temporary place, starting from the settings the checkout has now.

use crate::domain::{
    headless_job::JobAccess,
    review_builtins::{self, BuiltinAgent},
    review_programs::BuiltinProgram,
    setup_review::{CONFIG_FILE, PROGRAMS_DIR, ROOT_FROM_CWD},
    toolchain::RecommendedProgram,
};

/// What the job may do: read and edit the files of its temporary place,
/// and run no command.
pub const SETUP_REVIEW_ACCESS: JobAccess = JobAccess::EditFiles;

/// The most of the current settings the prompt carries, in bytes; a file
/// past it is named for the job to read in its place.
pub const SETUP_CURRENT_LIMIT: usize = 64 * 1024;

/// The review's settings of the checkout as they are: its `dagq.toml`
/// (`None` without one) and the files under `.dagq/review/`, each its
/// repository-relative path and its text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CurrentSetup {
    pub config: Option<String>,
    pub files: Vec<(String, String)>,
}

impl CurrentSetup {
    /// Whether the checkout has any of the review's settings: the prompt
    /// asks then for an update of them rather than new ones.
    pub fn is_empty(&self) -> bool {
        self.config.is_none() && self.files.is_empty()
    }
}

/// What a built-in agent judges, as the prompt says it.
const fn agent_purpose(agent: BuiltinAgent) -> &'static str {
    match agent {
        BuiltinAgent::Security => {
            "what the change does about sending data out, running commands and handling credentials"
        }
        BuiltinAgent::Tests => {
            "whether the tests cover the decisions and boundaries the change makes, and are stable"
        }
    }
}

/// What a built-in program detects, as the prompt says it.
const fn program_purpose(program: BuiltinProgram) -> &'static str {
    match program {
        BuiltinProgram::Secrets => "tokens and keys in the lines the change adds",
    }
}

/// The built-ins, each with what it judges, whether it is on by default
/// and its default paths.
fn builtins() -> String {
    let mut lines = vec![format!(
        "- `{}`: the whole review (the acceptance criteria and the receipt against the change, \
         and the documents against the changed behaviour). Always on, on every change: its \
         table takes `enabled = true` only. Its rules file is `{}`.",
        review_builtins::ACCEPTANCE,
        review_builtins::acceptance_rules_path()
    )];
    for agent in BuiltinAgent::ALL {
        lines.push(format!(
            "- `{}` (an agent; table `[review.subagents.\"{}\"]` with `enabled`, `paths`, \
             `provider` = \"claude\" or \"codex\"): {}. {} by default, on {:?}. Its rules file \
             is `{}`.",
            agent.name(),
            agent.name(),
            agent_purpose(agent),
            if agent.enabled_by_default() {
                "On"
            } else {
                "Off"
            },
            agent.default_paths(),
            review_builtins::rules_path(agent)
        ));
    }
    for program in BuiltinProgram::ALL {
        lines.push(format!(
            "- `{}` (a program; table `[review.programs.\"{}\"]` with `enabled` and `paths` \
             only): {}. {} by default, on {:?}.",
            program.name(),
            program.name(),
            program_purpose(program),
            if program.enabled_by_default() {
                "On"
            } else {
                "Off"
            },
            program.default_paths()
        ));
    }
    lines.join("\n")
}

/// The review programs the project's toolchain profiles recommend, each
/// its table and its script, for the job to write where they fit; none
/// says nothing.
fn recommended(programs: &[RecommendedProgram]) -> String {
    if programs.is_empty() {
        return String::new();
    }
    let mut parts = vec![
        "The toolchain profiles of the project's languages recommend these programs. Write each \
         that fits the project, its script at the path given and its table in `dagq.toml`, \
         adapted to where the project's files are (its `paths`, and where its script runs the \
         tool, such as the directory of each package when there is none at the root); leave out \
         one that does not fit or that the project already checks, and say why:"
            .to_owned(),
    ];
    for program in programs {
        parts.push(format!(
            "{}`{}`:\n{}",
            super::fenced("toml", &program.table()),
            program.script,
            super::fenced("sh", program.body)
        ));
    }
    format!("{}\n\n", parts.join("\n\n"))
}

/// The current settings as the prompt carries them, within
/// [`SETUP_CURRENT_LIMIT`].
fn current(current: &CurrentSetup) -> String {
    if current.is_empty() {
        return format!(
            "The repository has no `{CONFIG_FILE}` and nothing under `{}/` yet: write them new.",
            review_builtins::RULES_DIR
        );
    }
    let mut left = SETUP_CURRENT_LIMIT;
    let mut parts = vec![
        "The repository already has settings. Start from them and update them to what the \
         project is now: keep what still holds, change what does not, and say why in your \
         reply. They are, as committed:"
            .to_owned(),
    ];
    let files = current
        .config
        .as_ref()
        .map(|text| (CONFIG_FILE, text.as_str()))
        .into_iter()
        .chain(
            current
                .files
                .iter()
                .map(|(path, text)| (path.as_str(), text.as_str())),
        );
    for (path, text) in files {
        if text.len() > left {
            parts.push(format!(
                "`{path}`: too long for this prompt; read it in your working directory."
            ));
            continue;
        }
        left -= text.len();
        parts.push(format!("`{path}`:\n{}", super::fenced("text", text)));
    }
    parts.join("\n\n")
}

/// The prompt of the job: what to read, what to write and where, the
/// built-ins, the review programs the project's toolchain profiles
/// recommend (`programs`, only those it may write), and the settings the
/// checkout has now ([`CurrentSetup`]).
pub fn setup_review_prompt(setup: &CurrentSetup, programs: &[RecommendedProgram]) -> String {
    let rules_dir = review_builtins::RULES_DIR;
    format!(
        "You set up dagq's review of this repository, once. Your working directory is a \
         temporary checkout of the repository at its HEAD; a person reads the diff of what you \
         write there and applies it only if they approve it. Nothing you write is committed.\n\
         \n\
         Read the project first, once: its instructions (such as AGENTS.md, CLAUDE.md, README \
         and CONTRIBUTING), its documents, where its tests and documents live, and its languages \
         and tools. From then on dagq's review reads only the files you write, never the \
         instructions again, so put in them what a reviewer of a change of this project needs.\n\
         \n\
         Write only these, and change nothing else:\n\
         - In `{CONFIG_FILE}`, the built-ins' tables (`[review.subagents.\"builtin:<name>\"]`, \
         `[review.programs.\"builtin:<name>\"]`) and the project's programs \
         (`[review.programs.<name>]` with `script`, `paths` and optionally `args` and \
         `timeout_secs`). Leave every other table and key of `{CONFIG_FILE}` as it is.\n\
         - `{rules_dir}/builtin-<name>.md`: the project's rules for a built-in agent, plain text \
         it is given after its own definition. Write what is particular to this project (where \
         things are, its conventions, what a change must keep); do not restate the generic \
         judgement. Write one only for a built-in you leave on.\n\
         - `{PROGRAMS_DIR}/<name>.sh`: the project's mechanical rules, each a fast check of the \
         form of the changed files that runs no build and no test. Each starts with its `#!` \
         line, takes its root from the working directory's Git work tree (`{ROOT_FROM_CWD}`), \
         never from where the script is, since the review runs a copy of it from elsewhere, \
         exits 0 when the rule holds and non-zero with what broke it on its output otherwise. \
         List each in `[review.programs.<name>]` with `script = \"{PROGRAMS_DIR}/<name>.sh\"` \
         and the `paths` it checks. Write none when the project has no such rule.\n\
         Files elsewhere, and other tables of `{CONFIG_FILE}`, are refused: dagq checks your \
         changes and applies none of them if any is outside these.\n\
         \n\
         The built-ins (turn one off with `enabled = false`; `paths` replaces its default \
         paths, it is not added to them):\n\
         {builtins}\n\
         \n\
         {recommended}{current}\n\
         \n\
         When you are done, reply with a short summary: each built-in on or off and why, each \
         rules file and program you wrote and what it holds.\n",
        builtins = builtins(),
        recommended = recommended(programs),
        current = current(setup),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_without_settings_is_asked_for_new_ones() {
        let prompt = setup_review_prompt(&CurrentSetup::default(), &[]);
        assert!(prompt.contains("has no `dagq.toml`"), "{prompt}");
        for name in [
            "builtin:acceptance",
            "builtin:security",
            "builtin:tests",
            "builtin:secrets",
            ".dagq/review/builtin-security.md",
            ROOT_FROM_CWD,
        ] {
            assert!(prompt.contains(name), "{name}");
        }
    }

    #[test]
    fn the_current_settings_are_in_the_prompt() {
        let setup = CurrentSetup {
            config: Some("[review.subagents.\"builtin:tests\"]\nenabled = true\n".to_owned()),
            files: vec![(
                ".dagq/review/builtin-tests.md".to_owned(),
                "Tests live in t/.".to_owned(),
            )],
        };
        let prompt = setup_review_prompt(&setup, &[]);
        assert!(prompt.contains("already has settings"), "{prompt}");
        assert!(prompt.contains("[review.subagents.\"builtin:tests\"]\nenabled = true"));
        assert!(prompt.contains("`.dagq/review/builtin-tests.md`:\n"));
        assert!(prompt.contains("Tests live in t/."));
        // A file past the limit is named, not carried.
        let long = CurrentSetup {
            config: None,
            files: vec![(
                ".dagq/review/builtin-security.md".to_owned(),
                "x".repeat(SETUP_CURRENT_LIMIT + 1),
            )],
        };
        let prompt = setup_review_prompt(&long, &[]);
        assert!(prompt.contains("`.dagq/review/builtin-security.md`: too long"));
        assert!(!prompt.contains(&"x".repeat(SETUP_CURRENT_LIMIT)));
    }

    #[test]
    fn the_recommended_programs_are_in_the_prompt() {
        let program = RecommendedProgram {
            name: "lint",
            script: ".dagq/review/programs/lint.sh",
            paths: &["src/**"],
            body: "#!/bin/sh\nroot=$(git rev-parse --show-toplevel) || exit 2\n",
        };
        let prompt = setup_review_prompt(&CurrentSetup::default(), &[program]);
        assert!(prompt.contains("recommend these programs"), "{prompt}");
        assert!(prompt.contains(&program.table()));
        assert!(prompt.contains("`.dagq/review/programs/lint.sh`:\n"));
        assert!(prompt.contains(program.body));
        let none = setup_review_prompt(&CurrentSetup::default(), &[]);
        assert!(!none.contains("recommend these programs"));
    }
}
