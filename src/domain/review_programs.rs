//! The program reviews of a run's review stage (ADR-t1895-2): `dagq.toml`
//! names each one with `[review.programs.<name>]`, the program it runs and
//! the globs that make it required, and a review runs every program one of
//! whose globs a path the reviewed commit changes matches. Which programs a
//! review needs is decided here, without side effects; the supervisor reads
//! the configuration and the scripts from the landing branch's committed
//! tree, never from the run's worktree (decision 2).
//!
//! The binary also carries built-in programs ([`BuiltinProgram`],
//! ADR-t2259-1 decisions 1, 4, 5 and 9), configured by
//! `[review.programs."builtin:<name>"]` ([`BuiltinProgramSetting`]) and run
//! as a project's programs are, ahead of them ([`ReviewPrograms::effective`]).

use super::review_builtins::PREFIX;
use super::scope::{glob_matches, validate_path_globs};

pub mod secrets;

/// The configuration's section of the program reviews.
pub const SECTION: &str = "review.programs";

/// One `[review.programs.<name>]`: the program's name, the script it
/// runs, the globs that make it required (each once, in the order
/// written), and `timeout_secs`, its own time limit in seconds, which
/// without the key is `[review.jobs] program_timeout_secs`'s.
///
/// `script = "scripts/check.sh"` with `args = [...]` is a script of the
/// repository, its text taken from the landing branch's commit and run
/// from a copy outside the worktree against the worktree as its working
/// directory, so a worker's change to it has no effect until it lands
/// (ADR-t1895-2 decision 2). It is executed as it is, so it starts with
/// its interpreter's `#!` line; an outside tool (cargo, a linter) is
/// `exec`ed from it. There is no form that runs a program named by the
/// configuration: whether such a program, or a file an interpreter is
/// given, is the worktree's cannot be told from its words. Another
/// repository script the script calls by a path from its working
/// directory is the worktree's, which nothing here prevents.
///
/// A built-in program ([`Self::builtin`]) is named `builtin:<name>`, and
/// its `script` is that name: its text is the binary's
/// ([`BuiltinProgram::script`]), never a file of the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewProgram {
    pub name: String,
    pub script: String,
    pub args: Vec<String>,
    pub paths: Vec<String>,
    pub timeout_secs: Option<u64>,
    /// The built-in it is, `None` for a project's program.
    pub builtin: Option<BuiltinProgram>,
}

/// A built-in program the binary carries (ADR-t2259-1 decision 1).
///
/// Each is a hidden subcommand of the dagq binary, `dagq review-program
/// <name> BASE HEAD [PATH...]`, whose judgement is a pure function of this
/// layer ([`secrets`]), run as a project's script is: a program job on the
/// review's backend, with the narrowed environment and `PATH`
/// (ADR-t1895-2 decisions 5 and 6), the job's record, its time limit and
/// the fail-fast of the programs' step. The program is the supervisor's own
/// binary, which neither the worker nor the project can change. A shell
/// script would leave its judgement to `awk`, which no unit test reads,
/// and a function run inside the supervisor would have needed a second
/// kind of program job beside the process one, with its own record, time
/// limit and stop. It reads the change with `git` only, so it assumes no
/// language or tool of the project.
///
/// What it checks it is given as arguments: the reviewed range's merge
/// base and head (commit names, no secret) and, when `dagq.toml` narrowed
/// its `paths`, the changed paths they matched ([`with_range`]), so it
/// needs no variable beyond the narrowed environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinProgram {
    /// `builtin:secrets`: the lines the reviewed range adds that look like
    /// a credential ([`secrets`], where the kinds and their patterns are).
    /// On by default, on every changed path.
    Secrets,
}

impl BuiltinProgram {
    /// Every built-in program, in the order a review runs them.
    pub const ALL: [Self; 1] = [Self::Secrets];

    /// Its name without [`PREFIX`].
    pub const fn short(self) -> &'static str {
        match self {
            Self::Secrets => "secrets",
        }
    }

    /// Its name, `builtin:<short>`: the name of its table, its job and its
    /// events.
    pub fn name(self) -> String {
        format!("{PREFIX}{}", self.short())
    }

    /// Whether it runs when `dagq.toml` does not say `enabled`.
    pub const fn enabled_by_default(self) -> bool {
        match self {
            Self::Secrets => true,
        }
    }

    /// The globs that select it when `dagq.toml` does not say `paths`:
    /// every path. `paths` written replaces them, it is not added to them.
    pub const fn default_paths(self) -> &'static [&'static str] {
        match self {
            Self::Secrets => &["**"],
        }
    }

    /// What it runs as the binary carries it: the source of the module
    /// that judges, whose digest the job's record keeps beside the build
    /// identifier (ADR-t2259-1 decision 9).
    pub const fn source(self) -> &'static str {
        match self {
            Self::Secrets => include_str!("review_programs/secrets.rs"),
        }
    }

    /// The built-in program `name` names: `Ok(None)` for a name without
    /// [`PREFIX`] (a project's program), `Err` with why for one with it
    /// that names no built-in program.
    pub fn named(name: &str) -> Result<Option<Self>, String> {
        let Some(short) = name.strip_prefix(PREFIX) else {
            return Ok(None);
        };
        Self::ALL
            .into_iter()
            .find(|program| program.short() == short)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "{name} is no built-in program (they are {}); names starting with {PREFIX} are the built-ins', so a project's program may not take one",
                    Self::ALL.map(Self::name).join(", ")
                )
            })
    }
}

/// One `[review.programs."builtin:<name>"]` as written: each key `None`
/// when the table does not have it. It has no `script`, `args` or
/// `timeout_secs`: what runs is the binary's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinProgramSetting {
    pub program: BuiltinProgram,
    pub enabled: Option<bool>,
    pub paths: Option<Vec<String>>,
}

/// The program reviews a `dagq.toml` configures: the project's programs in
/// file order, and its settings of the built-ins (none leaves each to its
/// defaults).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewPrograms {
    pub programs: Vec<ReviewProgram>,
    pub builtins: Vec<BuiltinProgramSetting>,
}

impl ReviewPrograms {
    /// The programs a review selects from, in the order they run: each
    /// enabled built-in in [`BuiltinProgram::ALL`]'s order, on the `paths`
    /// written or its defaults, then the project's programs. The built-ins
    /// go first: they are quick, need nothing of the project's tools, and a
    /// change that adds a credential goes back before any project's
    /// program runs over it.
    pub fn effective(&self) -> Vec<ReviewProgram> {
        BuiltinProgram::ALL
            .into_iter()
            .filter_map(|builtin| {
                let setting = self.builtins.iter().find(|s| s.program == builtin);
                setting
                    .and_then(|s| s.enabled)
                    .unwrap_or(builtin.enabled_by_default())
                    .then(|| ReviewProgram {
                        name: builtin.name(),
                        script: builtin.name(),
                        args: Vec::new(),
                        paths: setting.and_then(|s| s.paths.clone()).unwrap_or_else(|| {
                            builtin
                                .default_paths()
                                .iter()
                                .map(|glob| (*glob).to_owned())
                                .collect()
                        }),
                        timeout_secs: None,
                        builtin: Some(builtin),
                    })
            })
            .chain(self.programs.iter().cloned())
            .collect()
    }
}

/// What a built-in program of `selected` is given to check: `base` and
/// `head` of the reviewed range, then, when its globs are not its defaults
/// (which match every path), the changed paths they matched, as its
/// arguments. On its defaults it reads the whole range, so a change of
/// many files does not make its command line long. A project's program is
/// left as configured.
pub fn with_range(mut selected: SelectedProgram, base: &str, head: &str) -> SelectedProgram {
    if let Some(builtin) = selected.program.builtin {
        let narrowed = selected.program.paths != builtin.default_paths();
        selected.program.args = [base.to_owned(), head.to_owned()]
            .into_iter()
            .chain(selected.matched.iter().filter(|_| narrowed).cloned())
            .collect();
    }
    selected
}

/// A program a review needs, with the changed paths that made it so, in
/// the order of the changed paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedProgram {
    pub program: ReviewProgram,
    pub matched: Vec<String>,
}

/// Check a script's path: one repository file, relative to the root,
/// with no glob character.
pub fn check_script_path(path: &str) -> Result<(), String> {
    validate_path_globs(&[path.to_owned()]).map_err(|error| error.to_string())?;
    if path.contains(['*', '?']) {
        return Err(format!("{path:?} must name one file, not a glob"));
    }
    Ok(())
}

/// The programs `changed` requires, in the order configured, each with the
/// changed paths one of its globs matches.
pub fn select(configured: &[ReviewProgram], changed: &[String]) -> Vec<SelectedProgram> {
    configured
        .iter()
        .filter_map(|program| {
            let mut matched: Vec<String> = Vec::new();
            for path in changed {
                if !matched.contains(path) && program.paths.iter().any(|g| glob_matches(g, path)) {
                    matched.push(path.clone());
                }
            }
            (!matched.is_empty()).then(|| SelectedProgram {
                program: program.clone(),
                matched,
            })
        })
        .collect()
}

/// How one program of a run's review ended: the `outcome` of
/// `review_program_finished`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramOutcome {
    /// It exited 0: the change passes its check.
    Passed,
    /// It exited non-zero: the change breaks its check, which the worker
    /// fixes (ADR-t1895-2 decision 3).
    Rejected,
    /// It ran past its time and was stopped with its process group.
    TimedOut,
    /// It did not start: its backend refused it, or its process could not
    /// be spawned.
    StartFailed,
}

impl ProgramOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Rejected => "rejected",
            Self::TimedOut => "timed_out",
            Self::StartFailed => "start_failed",
        }
    }

    /// The outcome of a program whose process ended as `exit` says:
    /// `Some(success)` for an exit, `None` for one stopped past its time.
    pub const fn of_exit(exit: Option<bool>) -> Self {
        match exit {
            Some(true) => Self::Passed,
            Some(false) => Self::Rejected,
            None => Self::TimedOut,
        }
    }
}

/// Where a run's program reviews go, given how many programs the review
/// needs and how those that ran ended, in the configured order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramsStep {
    /// Start the program at this index next.
    Run(usize),
    /// Every program exited 0 (or none was needed): the agents' review
    /// starts.
    Passed,
    /// The program at this index exited non-zero: the worker is sent back
    /// with its output, and neither the programs after it nor an agent run.
    Rejected(usize),
    /// The program at this index could not start or ran past its time:
    /// the review failed, not the worker (ADR-t1895-2 decision 4), and no
    /// program after it runs.
    Failed(usize, ProgramOutcome),
}

/// The next [`ProgramsStep`] of the `selected` programs a review needs,
/// `outcomes` those that ended, in order: the first that did not pass
/// ends the step, else the next not run yet runs.
pub fn step(selected: usize, outcomes: &[ProgramOutcome]) -> ProgramsStep {
    for (index, outcome) in outcomes.iter().enumerate() {
        match outcome {
            ProgramOutcome::Passed => {}
            ProgramOutcome::Rejected => return ProgramsStep::Rejected(index),
            failed => return ProgramsStep::Failed(index, *failed),
        }
    }
    if outcomes.len() < selected {
        ProgramsStep::Run(outcomes.len())
    } else {
        ProgramsStep::Passed
    }
}

/// What a program review that [`ProgramsStep::Failed`] leads to: as a
/// review whose job failed, it runs once more from its first program,
/// unless it is that one retry already; then the review fails to a person
/// (`review_failed` and the `approve_landing` ask). It never sends the
/// worker back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterFailure {
    Again,
    ReviewFailed,
}

pub const fn after_failure(retried: bool) -> AfterFailure {
    if retried {
        AfterFailure::ReviewFailed
    } else {
        AfterFailure::Again
    }
}

/// The most of a rejecting program's output the reason sent to the worker
/// carries, from its end.
pub const REASON_OUTPUT_TAIL: usize = 2000;

/// The reason a worker is sent back with when the program `name` exited
/// with `exit` (its status as text): the end of its stdout and stderr,
/// [`REASON_OUTPUT_TAIL`] bytes at most, or that it printed nothing.
pub fn rejection_reason(name: &str, exit: &str, stdout: &str, stderr: &str) -> String {
    let output = [stdout.trim_end(), stderr.trim_end()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let mut start = output.len().saturating_sub(REASON_OUTPUT_TAIL);
    while !output.is_char_boundary(start) {
        start += 1;
    }
    let output = &output[start..];
    if output.is_empty() {
        format!("the review program {name} rejected the change ({exit}) and printed nothing")
    } else {
        format!(
            "the review program {name} rejected the change ({exit}); the end of its output:\n{output}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(name: &str, paths: &[&str]) -> ReviewProgram {
        ReviewProgram {
            name: name.to_owned(),
            script: "scripts/check.sh".to_owned(),
            args: Vec::new(),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            timeout_secs: None,
            builtin: None,
        }
    }

    fn setting(enabled: Option<bool>, paths: Option<&[&str]>) -> BuiltinProgramSetting {
        BuiltinProgramSetting {
            program: BuiltinProgram::Secrets,
            enabled,
            paths: paths.map(|paths| paths.iter().map(|p| (*p).to_owned()).collect()),
        }
    }

    /// Without a setting builtin:secrets runs first, on every changed
    /// path, before the project's programs (ADR-t2259-1 decision 5).
    #[test]
    fn builtin_secrets_runs_first_on_every_path_by_default() {
        let configured = ReviewPrograms {
            programs: vec![program("fmt", &["src/**"])],
            builtins: Vec::new(),
        };
        let effective = configured.effective();
        assert_eq!(
            effective
                .iter()
                .map(|p| (p.name.as_str(), p.builtin))
                .collect::<Vec<_>>(),
            [
                ("builtin:secrets", Some(BuiltinProgram::Secrets)),
                ("fmt", None)
            ]
        );
        assert_eq!(effective[0].script, "builtin:secrets");
        let changed = ["README.md", "a/b/c.txt", ".env"].map(str::to_owned);
        let selected = select(&effective, &changed);
        assert_eq!(selected[0].matched, changed);
        // On its defaults it reads the whole range: no path is passed.
        let given = with_range(selected[0].clone(), "base", "head");
        assert_eq!(given.program.args, ["base", "head"]);
        assert!(ReviewPrograms::default().effective()[0].builtin.is_some());
    }

    /// `enabled = false` leaves it out; `paths` written replaces its
    /// globs, so only the paths they match select it and are checked.
    #[test]
    fn enabled_and_paths_of_a_builtin_program_are_taken_as_written() {
        let off = ReviewPrograms {
            programs: Vec::new(),
            builtins: vec![setting(Some(false), Some(&["src/**"]))],
        };
        assert!(off.effective().is_empty());
        let narrowed = ReviewPrograms {
            programs: Vec::new(),
            builtins: vec![setting(None, Some(&["src/**"]))],
        };
        let effective = narrowed.effective();
        assert_eq!(effective[0].paths, ["src/**"]);
        assert!(select(&effective, &["docs/a.md".to_owned()]).is_empty());
        let selected = select(&effective, &["docs/a.md", "src/b.rs"].map(str::to_owned));
        assert_eq!(selected[0].matched, ["src/b.rs"]);
        // The range and the matched paths are its arguments; a project's
        // program keeps its own.
        let given = with_range(selected[0].clone(), "base", "head");
        assert_eq!(given.program.args, ["base", "head", "src/b.rs"]);
        let project = SelectedProgram {
            program: ReviewProgram {
                args: vec!["--quiet".to_owned()],
                ..program("fmt", &["**"])
            },
            matched: vec!["a".to_owned()],
        };
        assert_eq!(with_range(project, "b", "h").program.args, ["--quiet"]);
        let on = ReviewPrograms {
            programs: Vec::new(),
            builtins: vec![setting(Some(true), None)],
        };
        assert_eq!(on.effective()[0].paths, ["**"]);
    }

    /// A name with the prefix names a built-in program or is a mistake.
    #[test]
    fn names_with_the_prefix_are_the_builtin_programs() {
        assert_eq!(
            BuiltinProgram::named("builtin:secrets"),
            Ok(Some(BuiltinProgram::Secrets))
        );
        assert_eq!(BuiltinProgram::named("fmt"), Ok(None));
        for unknown in ["builtin:scope", "builtin:", "builtin:security"] {
            let error = BuiltinProgram::named(unknown).unwrap_err();
            assert!(
                error.contains("is no built-in program (they are builtin:secrets)"),
                "{error}"
            );
        }
    }

    /// What a built-in runs is the source of the module that judges, which
    /// assumes no project's language or tool.
    #[test]
    fn the_secrets_source_is_its_judging_module_and_generic() {
        let source = BuiltinProgram::Secrets.source();
        assert!(source.contains("pub fn scan("), "{source}");
        let lower = source.to_lowercase();
        for word in ["cargo", "rust", "python", "npm install"] {
            assert!(!lower.contains(word), "{word}");
        }
    }

    /// A program is required by the changed paths its globs match, in the
    /// order configured; one that matches nothing is not.
    #[test]
    fn the_changed_paths_select_the_programs() {
        let configured = [
            program("docs", &["docs/**", "*.md"]),
            program("never", &["nothing/**"]),
            program("src", &["src/**"]),
        ];
        let changed: Vec<String> = ["README.md", "src/a.rs", "docs/x.md", "README.md"]
            .map(str::to_owned)
            .to_vec();
        let selected = select(&configured, &changed);
        let names: Vec<(&str, Vec<&str>)> = selected
            .iter()
            .map(|s| {
                (
                    s.program.name.as_str(),
                    s.matched.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                ("docs", vec!["README.md", "docs/x.md"]),
                ("src", vec!["src/a.rs"])
            ]
        );
        assert!(select(&configured, &[]).is_empty());
    }

    /// The programs run one at a time in order while each exits 0, and the
    /// review goes on to its agents once all did (or none was needed).
    #[test]
    fn the_programs_run_in_order_and_all_passing_goes_on_to_the_agents() {
        use ProgramOutcome::Passed;
        assert_eq!(step(0, &[]), ProgramsStep::Passed);
        assert_eq!(step(3, &[]), ProgramsStep::Run(0));
        assert_eq!(step(3, &[Passed]), ProgramsStep::Run(1));
        assert_eq!(step(3, &[Passed, Passed]), ProgramsStep::Run(2));
        assert_eq!(step(3, &[Passed, Passed, Passed]), ProgramsStep::Passed);
        assert_eq!(ProgramOutcome::of_exit(Some(true)), Passed);
    }

    /// The first program that exits non-zero stops the step there: no
    /// program after it runs and the worker is sent back.
    #[test]
    fn the_first_rejecting_program_stops_the_rest_and_sends_the_worker_back() {
        use ProgramOutcome::{Passed, Rejected};
        assert_eq!(ProgramOutcome::of_exit(Some(false)), Rejected);
        assert_eq!(step(3, &[Rejected]), ProgramsStep::Rejected(0));
        assert_eq!(step(3, &[Passed, Rejected]), ProgramsStep::Rejected(1));
    }

    /// A program that could not start or ran past its time fails the
    /// review, never the worker: no program after it runs, the program
    /// review runs once more from its first program, and a failed retry
    /// fails the review to a person.
    #[test]
    fn a_start_failure_or_a_timeout_fails_the_review_once_more_then_to_a_person() {
        use ProgramOutcome::{Passed, StartFailed, TimedOut};
        assert_eq!(ProgramOutcome::of_exit(None), TimedOut);
        for failed in [TimedOut, StartFailed] {
            assert_eq!(step(3, &[failed]), ProgramsStep::Failed(0, failed));
            assert_eq!(step(3, &[Passed, failed]), ProgramsStep::Failed(1, failed));
        }
        assert_eq!(after_failure(false), AfterFailure::Again);
        assert_eq!(after_failure(true), AfterFailure::ReviewFailed);
        assert_eq!(
            [Passed, ProgramOutcome::Rejected, TimedOut, StartFailed].map(ProgramOutcome::as_str),
            ["passed", "rejected", "timed_out", "start_failed"]
        );
    }

    /// The worker reads the program's name, its exit and the end of its
    /// output, cut on a character boundary.
    #[test]
    fn the_reason_names_the_program_its_exit_and_the_end_of_its_output() {
        assert_eq!(
            rejection_reason("links", "exit status: 1", "a\nbroken link\n", "oops\n"),
            "the review program links rejected the change (exit status: 1); the end of its output:\na\nbroken link\noops"
        );
        assert_eq!(
            rejection_reason("links", "exit status: 2", "", "\n"),
            "the review program links rejected the change (exit status: 2) and printed nothing"
        );
        let long = format!("{}末尾", "é".repeat(REASON_OUTPUT_TAIL));
        let reason = rejection_reason("p", "exit status: 1", &long, "");
        let output = reason.split_once("output:\n").unwrap().1;
        assert!(output.len() <= REASON_OUTPUT_TAIL, "{}", output.len());
        assert!(output.ends_with("末尾"));
    }

    /// A script names one repository file.
    #[test]
    fn a_scripts_path_names_one_file_of_the_repository() {
        assert!(check_script_path("scripts/check.sh").is_ok());
        for bad in ["/bin/sh", "../x.sh", "scripts/*.sh", "a//b", " "] {
            assert!(check_script_path(bad).is_err(), "{bad}");
        }
    }
}
