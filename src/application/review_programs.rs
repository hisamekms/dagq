//! The program reviews of a run's review (ADR-t1895-2): which programs a
//! review attempt runs and what each runs, read from the landing branch's
//! committed tree when the attempt starts, never from the run's worktree
//! or the main checkout's files (decision 2). The programs then run as
//! program jobs ([`super::supervise::start_review_program`]) against the
//! run's worktree, on the backend of the review's actor
//! ([`ProgramBackends`]). The eval of an agent reads and runs a case's
//! programs the same way, at the commit its round reads its cases from
//! (ADR-t1728-1 (i)). A built-in program
//! ([`crate::domain::review_programs::BuiltinProgram`]) runs what
//! the binary carries instead of a script of the commit, and is given the
//! range it checks ([`review_programs::with_range`]).
//!
//! The parts any caller that runs them shares live here: which programs a
//! range requires ([`programs_for_range`], from the snapshot
//! [`snapshot_programs`] reads), what one runs and where
//! ([`prepare_program`]), how its process starts ([`spawn_program`]) and
//! how its end is read and classed ([`program_end`], [`ProgramRun`]).
//! The review stage runs them as asynchronous program jobs it starts and
//! polls; [`run_program`] runs one to its end in the calling thread.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::execution::{ExecutionConfig, ExecutorBackend};
use super::review::{CONFIG_FILE, ReviewRange};
use super::{
    Clock, CommandSpec, Exit, Repository, ReviewProgramBackend, RunFiles, Spawned, Spawner,
    Streams, tail,
};
use crate::domain::review_programs::{
    self, ProgramOutcome, ReviewProgram, ReviewPrograms, SelectedProgram,
};
use crate::domain::{ActorRole, CommitSha, RunId};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The backends a program job can run on, and which the review's actor
/// (`ExecutionConfig::backend_of(ActorRole::ReviewJob)`) runs on: a
/// program job is placed where the review's jobs are (ADR-t1895-2
/// decision 5), never chosen apart from them.
pub struct ProgramBackends<'a> {
    /// The host's: the program a process of this user on this host.
    pub host: &'a dyn ReviewProgramBackend,
    pub config: ExecutionConfig,
}

impl<'a> ProgramBackends<'a> {
    pub fn new(host: &'a dyn ReviewProgramBackend, config: ExecutionConfig) -> Self {
        Self { host, config }
    }

    /// The backend the review's actor runs on. One nothing can start a job
    /// on (the container) is the error of the program job's start and runs
    /// nothing: never the host instead (fail closed).
    pub fn backend(&self) -> Result<&'a dyn ReviewProgramBackend> {
        match self.config.backend_of(ActorRole::ReviewJob) {
            ExecutorBackend::Host => Ok(self.host),
            other => {
                other.ensure_implemented()?;
                bail!(
                    "no program job runs on the {} executor backend",
                    other.as_str()
                )
            }
        }
    }
}

/// One program a review attempt runs, as the landing branch's commit has
/// it: its configuration, the changed paths that required it, and its
/// script's text at that commit (a built-in's, the source the binary
/// carries of what it runs, `BuiltinProgram::source`), which is
/// what runs whatever the run's worktree holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotProgram {
    pub program: ReviewProgram,
    pub matched: Vec<String>,
    pub script: String,
}

impl SnapshotProgram {
    /// What `review_programs_started` records of which built-in program
    /// ran (ADR-t2259-1 decision 9): its name, the build identifier of
    /// this dagq and the SHA-256 of the source of what it runs; `None` for a
    /// project's program, whose script is the landing branch's commit's.
    pub fn builtin_record(&self) -> Option<Value> {
        self.program.builtin?;
        Some(json!({
            "builtin": self.program.name,
            "build": crate::VERSION,
            "digest": format!("{:x}", Sha256::digest(self.script.as_bytes())),
        }))
    }
}

/// The program reviews of one attempt read from the landing branch's
/// commit `commit`: those its range requires, none when the range touches
/// no configured glob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramSnapshot {
    pub commit: String,
    /// The range the programs were selected from.
    pub range: ReviewRange,
    pub programs: Vec<SnapshotProgram>,
}

/// The program reviews `dagq.toml` configures in `commit`'s tree, as
/// `parse` reads them: none written when the commit has no `dagq.toml` or
/// one without `[review.programs.*]` (the built-ins then go by their
/// defaults); `Err` when the file cannot be read or parsed.
pub fn programs_at(
    repository: &dyn Repository,
    commit: &str,
    parse: &dyn Fn(&str) -> Result<ReviewPrograms>,
) -> Result<ReviewPrograms> {
    let Some(text) = repository
        .file_in(commit, CONFIG_FILE)
        .with_context(|| format!("read {CONFIG_FILE} in the landing branch's commit {commit}"))?
    else {
        return Ok(ReviewPrograms::default());
    };
    parse(&text)
        .with_context(|| format!("parse {CONFIG_FILE} in the landing branch's commit {commit}"))
}

/// The programs of `configured` the paths `changed` require
/// ([`review_programs::select`]), each with its script read in `commit`'s
/// tree, whatever the tree they run against holds, or a built-in's from
/// the binary. `Err` when a selected script is not in the commit.
pub fn required_programs(
    repository: &dyn Repository,
    commit: &str,
    configured: &[ReviewProgram],
    changed: &[String],
) -> Result<Vec<SnapshotProgram>> {
    let mut programs = Vec::new();
    for selected in review_programs::select(configured, changed) {
        if let Some(builtin) = selected.program.builtin {
            programs.push(SnapshotProgram {
                program: selected.program,
                matched: selected.matched,
                script: builtin.source().to_owned(),
            });
            continue;
        }
        let path = &selected.program.script;
        let script = repository
            .file_in(commit, path)
            .with_context(|| format!("read {path} in the landing branch's commit {commit}"))?
            .with_context(|| {
                format!(
                    "the review program {} that {CONFIG_FILE} names runs {path}, which is not in the landing branch's commit {commit}",
                    selected.program.name
                )
            })?;
        programs.push(SnapshotProgram {
            program: selected.program,
            matched: selected.matched,
            script,
        });
    }
    Ok(programs)
}

/// Read the program reviews from the landing branch's committed tree and
/// select those the paths `<base>...<head>` changes require, each script
/// read at the same commit, the built-ins ([`ReviewPrograms::effective`])
/// first and each given the range from the merge base. `Ok(None)` when no
/// program is on: the landing branch's `dagq.toml` turns every built-in off
/// and names no program of its own, and the review goes as before. `Err`
/// when the file cannot be read or parsed, or a selected script is not in
/// the commit: what to run is not known, which is the review's failure,
/// not the worker's (ADR-t1895-2 decision 4). `range` is called only when
/// a program is on.
pub fn snapshot_programs(
    repository: &dyn Repository,
    parse: &dyn Fn(&str) -> Result<ReviewPrograms>,
    range: &dyn Fn(&CommitSha) -> Result<ReviewRange>,
) -> Result<Option<ProgramSnapshot>> {
    let main = repository.main_head()?;
    let commit = main.to_string();
    let configured = programs_at(repository, &commit, parse)?.effective();
    if configured.is_empty() {
        return Ok(None);
    }
    let range = range(&main)?;
    let from = repository
        .merge_base(&range.base, &range.head)?
        .map_or_else(|| range.base.clone(), CommitSha::into_string);
    let programs = programs_for_range(repository, &commit, &configured, (&from, &range.head))?;
    Ok(Some(ProgramSnapshot {
        commit,
        range,
        programs,
    }))
}

/// The programs of `configured` (in its order, [`ReviewPrograms::effective`]'s
/// with the built-ins first) whose paths the paths `from..head` changes
/// touch, each script read in `commit`'s tree ([`required_programs`]) and
/// a built-in given the range `from` `head` it checks
/// ([`review_programs::with_range`]). `Err` when the changed paths cannot
/// be read or a selected script is not in the commit.
pub fn programs_for_range(
    repository: &dyn Repository,
    commit: &str,
    configured: &[ReviewProgram],
    (from, head): (&str, &str),
) -> Result<Vec<SnapshotProgram>> {
    let changed = repository.changed_paths(from, head)?;
    Ok(required_programs(repository, commit, configured, &changed)?
        .into_iter()
        .map(|snapshot| {
            let SelectedProgram { program, matched } = review_programs::with_range(
                SelectedProgram {
                    program: snapshot.program,
                    matched: snapshot.matched,
                },
                from,
                head,
            );
            SnapshotProgram {
                program,
                matched,
                script: snapshot.script,
            }
        })
        .collect())
}

/// Where `run`'s program reviews write the scripts they run: its directory
/// of `dir`, the queue's `review-programs/`, outside what the run's worker
/// can write (its run directory and worktree): a script written there
/// could be replaced between its write and its exec, and the worker's text
/// would run instead of the landing branch's (ADR-t1895-2 decision 2).
pub fn program_scratch(dir: &Path, run: &RunId) -> PathBuf {
    dir.join(run.as_str())
}

/// The most of each of a program's stdout and stderr its end keeps
/// ([`ProgramEnd`]), from their ends.
pub const PROGRAM_OUTPUT_TAIL: usize = 4000;

/// How a program's process ended: its exit (`None` when it ran past its
/// timeout and was stopped with its group) and the ends of its stdout and
/// stderr, [`PROGRAM_OUTPUT_TAIL`] bytes of each at most.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramEnd {
    pub exit: Option<Exit>,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

/// The end of a program whose process ended as `exit` (`None`: stopped
/// past its time), with the ends of its output files `stdout` and
/// `stderr`. Only the end of each is read, and bytes that are not UTF-8
/// are replaced, so a long or binary output still shows how it ended; a
/// file that cannot be read is empty.
pub fn program_end(
    files: &dyn RunFiles,
    (stdout, stderr): (&Path, &Path),
    exit: Option<Exit>,
) -> ProgramEnd {
    let read = |path: &Path| {
        let bytes = files
            .read_tail(path, PROGRAM_OUTPUT_TAIL as u64)
            .unwrap_or_default();
        tail(&String::from_utf8_lossy(&bytes), PROGRAM_OUTPUT_TAIL).to_owned()
    };
    ProgramEnd {
        exit,
        stdout_tail: read(stdout),
        stderr_tail: read(stderr),
    }
}

/// How one program run ended, the same for a review stage's program job
/// and [`run_program`]: it passed, it rejected the change (a non-zero
/// exit, the worker's to fix: ADR-t1895-2 decision 3), it could not be
/// prepared, started or waited for, or it ran past its time and was
/// stopped with its process group (neither the worker's: decision 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramRun {
    Passed(ProgramEnd),
    /// `reason` names the program, its exit and the end of its output
    /// ([`review_programs::rejection_reason`]).
    Rejected {
        reason: String,
        end: ProgramEnd,
    },
    /// Why it did not run, naming the program.
    StartFailed(String),
    /// The limit it ran past.
    TimedOut {
        timeout: Duration,
        end: ProgramEnd,
    },
}

impl ProgramRun {
    /// The program `name` that ended as `end`, its limit `timeout`.
    pub fn ended(name: &str, end: ProgramEnd, timeout: Duration) -> Self {
        match &end.exit {
            Some(exit) if exit.success => Self::Passed(end),
            Some(exit) => Self::Rejected {
                reason: review_programs::rejection_reason(
                    name,
                    &exit.to_string(),
                    &end.stdout_tail,
                    &end.stderr_tail,
                ),
                end,
            },
            None => Self::TimedOut { timeout, end },
        }
    }

    /// The program `name` that could not start for `error`
    /// ([`Self::start_failure`]).
    pub fn start_failed(name: &str, error: &anyhow::Error) -> Self {
        Self::StartFailed(Self::start_failure(name, error))
    }

    /// Why the program `name` could not start for `error`, as its
    /// [`Self::StartFailed`] and a review's `review_program_finished` say.
    pub fn start_failure(name: &str, error: &anyhow::Error) -> String {
        format!("the review program {name} could not start: {error:#}")
    }

    /// Its `outcome` in `review_program_finished`.
    pub const fn outcome(&self) -> ProgramOutcome {
        match self {
            Self::Passed(_) => ProgramOutcome::Passed,
            Self::Rejected { .. } => ProgramOutcome::Rejected,
            Self::StartFailed(_) => ProgramOutcome::StartFailed,
            Self::TimedOut { .. } => ProgramOutcome::TimedOut,
        }
    }
}

/// A program ready to start ([`prepare_program`]): its command, the files
/// its stdout and stderr go to, and its time limit.
pub struct PreparedProgram {
    pub command: CommandSpec,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    pub timeout: Duration,
}

/// What `program` runs against `worktree`, on the backend of the review's
/// actor ([`ProgramBackends::backend`]: one not implemented is the error,
/// never the host instead), with `env` on top of what its backend sets
/// (the guard of a container run's landing worktree, ADR-t2179-1 decision
/// 8): its script written under `scratch` joined with `stem`, its output
/// to `<output>/<stem>.out` and `.err`, its limit its own `timeout_secs`
/// or `timeout` without one.
pub fn prepare_program(
    backends: &ProgramBackends<'_>,
    program: &SnapshotProgram,
    (worktree, env): (&Path, &[(String, String)]),
    (output, scratch, stem): (&Path, &Path, &str),
    timeout: Duration,
) -> Result<PreparedProgram> {
    let name = &program.program.name;
    let backend = backends
        .backend()
        .with_context(|| format!("start the review program {name}"))?;
    let mut command = backend
        .command(program, worktree, output, &scratch.join(stem))
        .with_context(|| format!("prepare the review program {name}"))?;
    for (key, value) in env {
        command.env(key, value);
    }
    Ok(PreparedProgram {
        command,
        stdout: output.join(format!("{stem}.out")),
        stderr: output.join(format!("{stem}.err")),
        timeout: program
            .program
            .timeout_secs
            .map_or(timeout, Duration::from_secs),
    })
}

/// Start `command` as a program's process in a process group of its own,
/// so its timeout stops it with what it started, its stdout and stderr to
/// `output`.
pub fn spawn_program(
    spawner: &dyn Spawner,
    command: &CommandSpec,
    (stdout, stderr): (&Path, &Path),
) -> Result<Box<dyn Spawned>> {
    let mut command = command.clone();
    command.new_session();
    spawner.spawn(&command, Streams::Files { stdout, stderr })
}

/// How often [`run_program`] looks whether its program ended.
const RUN_POLL: Duration = Duration::from_millis(100);

/// Run `program` to its end in the calling thread, as a review stage's
/// program job runs ([`prepare_program`], [`spawn_program`]), and class
/// its end ([`ProgramRun`]): past its limit, measured on `clock`, its
/// process group is stopped. Nothing of it is recorded in the queue.
pub fn run_program(
    (spawner, backends, files, clock): (
        &dyn Spawner,
        &ProgramBackends<'_>,
        &dyn RunFiles,
        &dyn Clock,
    ),
    program: &SnapshotProgram,
    tree: (&Path, &[(String, String)]),
    place: (&Path, &Path, &str),
    timeout: Duration,
) -> ProgramRun {
    let name = &program.program.name;
    let started = |prepared: &PreparedProgram| {
        spawn_program(
            spawner,
            &prepared.command,
            (&prepared.stdout, &prepared.stderr),
        )
    };
    let (prepared, mut child) = match prepare_program(backends, program, tree, place, timeout)
        .and_then(|prepared| {
            let child = started(&prepared)?;
            Ok((prepared, child))
        }) {
        Ok(started) => started,
        Err(error) => return ProgramRun::start_failed(name, &error),
    };
    let since = clock.monotonic();
    let exit = loop {
        match child.try_wait() {
            Ok(Some(exit)) => break Some(exit),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill_group();
                let _ = child.wait();
                return ProgramRun::StartFailed(format!(
                    "the review program {name} could not be waited for: {error:#}"
                ));
            }
        }
        let ran = clock.since(since);
        if ran >= prepared.timeout {
            let _ = child.kill_group();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(RUN_POLL.min(prepared.timeout - ran));
    };
    let end = program_end(files, (&prepared.stdout, &prepared.stderr), exit);
    ProgramRun::ended(name, end, prepared.timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::DiffNumbers;
    use crate::domain::{TaskId, TaskRun};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const CONFIG: &str = "the committed dagq.toml";
    const SCRIPT: &str = "#!/bin/sh\necho main's check\n";
    const FMT: &str = "#!/bin/sh\nexec cargo fmt --check\n";

    /// A repository whose landing branch is at [`SHA`] and holds `files`
    /// there only, and whose reviewed range changes `changed`. The run's
    /// worktree is never read: every read is at the landing branch's
    /// commit.
    struct Committed {
        files: Vec<(&'static str, &'static str)>,
        changed: Vec<String>,
    }

    impl Repository for Committed {
        fn is_dagq_source(&self) -> bool {
            false
        }
        fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
            assert_eq!((from, to), ("base", "head"));
            Ok(self.changed.clone())
        }
        fn current_branch(&self, _: &Path) -> Result<Option<String>> {
            unreachable!()
        }
        fn head(&self, _: &Path) -> Result<CommitSha> {
            unreachable!()
        }
        fn is_ancestor(&self, _: &str, _: &str) -> Result<bool> {
            unreachable!()
        }
        fn status(&self, _: &Path) -> Result<String> {
            unreachable!()
        }
        fn rebase_in_progress(&self, _: &Path) -> Result<bool> {
            unreachable!()
        }
        fn rebase_abort(&self, _: &Path) -> Result<()> {
            unreachable!()
        }
        fn rebase(&self, _: &Path, _: &str) -> Result<std::result::Result<(), String>> {
            unreachable!()
        }
        fn conflicted_files(&self, _: &Path) -> Result<Vec<String>> {
            unreachable!()
        }
        fn added_paths(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unreachable!()
        }
        fn paths_in(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unreachable!()
        }
        fn paths_containing(&self, _: &str, _: &str, _: &[String]) -> Result<Vec<String>> {
            unreachable!()
        }
        fn rename_and_commit(
            &self,
            _: &Path,
            _: &str,
            _: &str,
            _: &[String],
        ) -> Result<std::result::Result<CommitSha, String>> {
            unreachable!()
        }
        fn tree_of(&self, _: &str) -> Result<String> {
            unreachable!()
        }
        fn commit_tree(&self, _: &str, _: &str, _: &[String]) -> Result<CommitSha> {
            unreachable!()
        }
        fn update_ref(&self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        fn advance_main(
            &self,
            _: &crate::domain::landing_branch::LandingBranch,
            _: &str,
            _: &str,
        ) -> Result<()> {
            unreachable!()
        }
        fn repair_worktree(&self, _: &Path) -> Result<()> {
            unreachable!()
        }
        fn remove_worktree_and_branch(&self, _: &Path, _: &str) -> Result<()> {
            unreachable!()
        }
        fn branches(&self) -> Result<Vec<String>> {
            unreachable!()
        }
        fn delete_branch(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn tracks(&self, _: &Path, _: &str) -> Result<bool> {
            unreachable!()
        }
        fn main_checkout(&self) -> Result<Option<PathBuf>> {
            unreachable!()
        }
        fn create_worktree(&self, _: &TaskRun) -> Result<String> {
            unreachable!()
        }
        fn merge_conflicts(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unreachable!()
        }
        fn landed_task_ids(&self, _: &str, _: &str) -> Result<Vec<TaskId>> {
            unreachable!()
        }
        fn log_oneline(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }
        fn diff_stat(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }
        fn diff_numbers(&self, _: &str, _: &str) -> Result<DiffNumbers> {
            unreachable!()
        }
        fn diff_to_file(&self, _: &str, _: &str, _: &Path) -> Result<()> {
            unreachable!()
        }
    }

    impl crate::application::RepositoryReads for Committed {
        fn file_in(&self, commit: &str, path: &str) -> Result<Option<String>> {
            assert_eq!(commit, SHA, "only the landing branch's commit is read");
            Ok(self
                .files
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, text)| (*text).to_owned()))
        }
        fn main_head(&self) -> Result<CommitSha> {
            Ok(CommitSha::try_from(SHA).unwrap())
        }
        fn merge_base(&self, _: &str, _: &str) -> Result<Option<CommitSha>> {
            Ok(None)
        }
    }

    /// `fmt` runs the script `scripts/fmt.sh` on `src/**`; `links` runs
    /// `scripts/links.sh` on `docs/**`, and [`CONFIG`] turns
    /// builtin:secrets off. [`OFF`] turns it off and names no program. A
    /// config of `bad` does not parse; any other writes nothing.
    fn parse(text: &str) -> Result<ReviewPrograms> {
        let program = |name: &str, script: &str, args: &[&str], glob: &str| ReviewProgram {
            name: name.to_owned(),
            script: script.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            paths: vec![glob.to_owned()],
            timeout_secs: None,
            builtin: None,
        };
        let off = vec![review_programs::BuiltinProgramSetting {
            program: review_programs::BuiltinProgram::Secrets,
            enabled: Some(false),
            paths: None,
        }];
        match text {
            CONFIG => Ok(ReviewPrograms {
                programs: vec![
                    program("fmt", "scripts/fmt.sh", &[], "src/**"),
                    program("links", "scripts/links.sh", &["--quiet"], "docs/**"),
                ],
                builtins: off,
            }),
            OFF => Ok(ReviewPrograms {
                programs: Vec::new(),
                builtins: off,
            }),
            "bad" => anyhow::bail!("dagq.toml:1: [review.programs.fmt] has no paths"),
            _ => Ok(ReviewPrograms::default()),
        }
    }

    const OFF: &str = "builtin:secrets off";

    fn snapshot(
        files: Vec<(&'static str, &'static str)>,
        changed: &[&str],
    ) -> Result<Option<ProgramSnapshot>> {
        let repository = Committed {
            files,
            changed: changed.iter().map(|p| (*p).to_owned()).collect(),
        };
        let range = |main: &CommitSha| {
            assert_eq!(main.as_str(), SHA);
            Ok(ReviewRange {
                base: "base".to_owned(),
                head: "head".to_owned(),
            })
        };
        snapshot_programs(&repository, &parse, &range)
    }

    /// The programs and each script are read from the landing branch's
    /// commit (ADR-t1895-2 decision 2): those the range's changed paths
    /// require, a script with its committed text.
    #[test]
    fn the_programs_and_scripts_are_read_from_the_landing_branchs_commit() {
        let found = snapshot(
            vec![
                (CONFIG_FILE, CONFIG),
                ("scripts/fmt.sh", FMT),
                ("scripts/links.sh", SCRIPT),
            ],
            &["docs/a.md", "src/b.rs", "docs/c.md"],
        )
        .unwrap()
        .unwrap();
        assert_eq!(found.commit, SHA);
        let read: Vec<_> = found
            .programs
            .iter()
            .map(|p| (p.program.name.as_str(), p.matched.clone(), p.script.clone()))
            .collect();
        assert_eq!(
            read,
            [
                ("fmt", vec!["src/b.rs".to_owned()], FMT.to_owned()),
                (
                    "links",
                    vec!["docs/a.md".to_owned(), "docs/c.md".to_owned()],
                    SCRIPT.to_owned()
                ),
            ]
        );
        let only_src = snapshot(
            vec![(CONFIG_FILE, CONFIG), ("scripts/fmt.sh", FMT)],
            &["src/b.rs"],
        )
        .unwrap()
        .unwrap();
        assert_eq!(only_src.programs.len(), 1);
    }

    /// Without `[review.programs]` on the landing branch (no `dagq.toml`,
    /// or one without the table) builtin:secrets runs on every change,
    /// its script the binary's, given the range from the merge base, and
    /// its record names the build and the script's
    /// digest (ADR-t2259-1 decisions 5 and 9). With every built-in off and
    /// no program there is no snapshot and the review is as before; with
    /// programs but a range they do not select, the snapshot is empty.
    #[test]
    fn builtin_secrets_runs_without_a_configuration_and_the_rest_as_before() {
        use review_programs::BuiltinProgram;
        for files in [Vec::new(), vec![(CONFIG_FILE, "[run.env]")]] {
            let found = snapshot(files, &["README.md", "src/b.rs"])
                .unwrap()
                .unwrap();
            assert_eq!(found.programs.len(), 1);
            let secrets = &found.programs[0];
            assert_eq!(secrets.program.name, "builtin:secrets");
            assert_eq!(secrets.program.builtin, Some(BuiltinProgram::Secrets));
            assert_eq!(secrets.script, BuiltinProgram::Secrets.source());
            assert_eq!(secrets.program.args, ["base", "head"]);
            let record = secrets.builtin_record().unwrap();
            assert_eq!(record["builtin"], "builtin:secrets");
            assert_eq!(record["build"], crate::VERSION);
            assert_eq!(
                record["digest"],
                format!(
                    "{:x}",
                    Sha256::digest(BuiltinProgram::Secrets.source().as_bytes())
                )
            );
        }
        assert!(
            snapshot(vec![(CONFIG_FILE, OFF)], &["src/b.rs"])
                .unwrap()
                .is_none()
        );
        let none = snapshot(vec![(CONFIG_FILE, CONFIG)], &["README.md"])
            .unwrap()
            .unwrap();
        assert!(none.programs.is_empty());
        let project = snapshot(
            vec![(CONFIG_FILE, CONFIG), ("scripts/fmt.sh", FMT)],
            &["src/b.rs"],
        )
        .unwrap()
        .unwrap();
        assert_eq!(project.programs[0].builtin_record(), None);
        assert!(project.programs[0].program.args.is_empty());
    }

    /// What to run cannot be known: a committed `dagq.toml` that does not
    /// parse, or a selected script that is not in the landing branch's
    /// commit (though the run's worktree may have added it), is an error
    /// saying which.
    #[test]
    fn an_unreadable_config_or_a_script_missing_from_the_commit_is_an_error() {
        let error = format!(
            "{:#}",
            snapshot(vec![(CONFIG_FILE, "bad")], &["src/b.rs"]).unwrap_err()
        );
        assert!(
            error.contains(&format!(
                "parse dagq.toml in the landing branch's commit {SHA}"
            )) && error.contains("[review.programs.fmt] has no paths"),
            "{error}"
        );
        let error = format!(
            "{:#}",
            snapshot(vec![(CONFIG_FILE, CONFIG)], &["docs/a.md"]).unwrap_err()
        );
        assert!(
            error.contains(&format!(
                "the review program links that dagq.toml names runs scripts/links.sh, which is not in the landing branch's commit {SHA}"
            )),
            "{error}"
        );
    }

    /// A case of an eval reads its programs and their scripts at the
    /// round's commit, by the paths its change touches: a script or a
    /// `dagq.toml` its patch changes in its tree is never what runs
    /// (ADR-t1728-1 (i)); the fake fails any read at another commit.
    #[test]
    fn a_cases_programs_are_read_at_the_rounds_commit_whatever_its_patch_changes() {
        let repository = Committed {
            files: vec![
                (CONFIG_FILE, CONFIG),
                ("scripts/fmt.sh", FMT),
                ("scripts/links.sh", SCRIPT),
            ],
            changed: Vec::new(),
        };
        let configured = programs_at(&repository, SHA, &parse).unwrap().programs;
        assert_eq!(configured.len(), 2);
        // The case's patch rewrote the script and the configuration.
        let changed = ["src/b.rs", "scripts/fmt.sh", CONFIG_FILE].map(str::to_owned);
        let required = required_programs(&repository, SHA, &configured, &changed).unwrap();
        assert_eq!(required.len(), 1, "links matches no changed path");
        assert_eq!(required[0].program.name, "fmt");
        assert_eq!(required[0].script, FMT);
        let none = Committed {
            files: Vec::new(),
            changed: Vec::new(),
        };
        assert_eq!(
            programs_at(&none, SHA, &parse).unwrap(),
            ReviewPrograms::default()
        );
    }

    /// A program backend that counts the commands it was asked for.
    #[derive(Default)]
    struct Counting(std::sync::atomic::AtomicUsize);

    impl ReviewProgramBackend for Counting {
        fn command(
            &self,
            _: &SnapshotProgram,
            worktree: &Path,
            _: &Path,
            _: &Path,
        ) -> Result<crate::application::CommandSpec> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut command = crate::application::CommandSpec::new("/bin/true");
            command.current_dir(worktree);
            Ok(command)
        }
    }

    fn a_program() -> SnapshotProgram {
        SnapshotProgram {
            program: ReviewProgram {
                name: "fmt".to_owned(),
                script: "scripts/fmt.sh".to_owned(),
                args: Vec::new(),
                paths: vec!["**".to_owned()],
                timeout_secs: None,
                builtin: None,
            },
            matched: vec!["a".to_owned()],
            script: FMT.to_owned(),
        }
    }

    /// With the review's actor on the host, a program job's command comes
    /// from the host's adapter.
    #[test]
    fn a_review_on_the_host_runs_its_programs_with_the_hosts_adapter() {
        let host = Counting::default();
        let backends = ProgramBackends::new(&host, ExecutionConfig::default());
        let dir = Path::new("/w");
        let command = backends
            .backend()
            .unwrap()
            .command(&a_program(), dir, dir, dir)
            .unwrap();
        assert_eq!(command.get_current_dir(), Some(dir));
        assert_eq!(host.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// With the review's actor on the container, by its own setting or the
    /// default's, the program job's start is an error and the host's
    /// adapter is never asked: no program runs on the host instead.
    #[test]
    fn a_review_on_the_container_is_an_error_and_never_runs_on_the_host() {
        let host = Counting::default();
        for config in [
            ExecutionConfig {
                backend: ExecutorBackend::Host,
                actors: vec![(ActorRole::ReviewJob, ExecutorBackend::Container)],
            },
            ExecutionConfig {
                backend: ExecutorBackend::Container,
                actors: Vec::new(),
            },
        ] {
            let backends = ProgramBackends::new(&host, config);
            let error = format!("{:#}", backends.backend().err().unwrap());
            assert!(error.contains("container"), "{error}");
        }
        assert_eq!(host.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        // Another actor's backend does not move the review's programs.
        let other = ExecutionConfig {
            backend: ExecutorBackend::Host,
            actors: vec![(ActorRole::Worker, ExecutorBackend::Container)],
        };
        assert!(ProgramBackends::new(&host, other).backend().is_ok());
    }

    /// A process that ends as `exit` at its first look, or never
    /// (`None`), and tells whether its group was stopped.
    struct Process {
        exit: Option<Exit>,
        stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl Spawned for Process {
        fn id(&self) -> u32 {
            7
        }
        fn try_wait(&mut self) -> Result<Option<Exit>> {
            Ok(self.exit.clone())
        }
        fn kill(&mut self) -> Result<()> {
            panic!("a program is stopped with its group, not alone")
        }
        fn kill_group(&mut self) -> Result<()> {
            self.stopped
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn wait(&mut self) -> Result<Exit> {
            Ok(exit(false, "signal: 9 (SIGKILL)"))
        }
    }

    /// Starts a [`Process`] that writes `output` to its stdout and stderr
    /// files, keeping the commands it was given; `refuse` fails the spawn.
    struct Spawns<'f> {
        files: &'f crate::application::memory_files::MemoryFiles,
        exit: Option<Exit>,
        output: (&'static str, &'static str),
        refuse: bool,
        commands: std::sync::Mutex<Vec<CommandSpec>>,
        stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl<'f> Spawns<'f> {
        fn new(
            files: &'f crate::application::memory_files::MemoryFiles,
            exit: Option<Exit>,
        ) -> Self {
            Self {
                files,
                exit,
                output: ("checked\n", "src/b.rs: not formatted\n"),
                refuse: false,
                commands: std::sync::Mutex::default(),
                stopped: std::sync::Arc::default(),
            }
        }
    }

    impl Spawner for Spawns<'_> {
        fn spawn(&self, command: &CommandSpec, streams: Streams<'_>) -> Result<Box<dyn Spawned>> {
            self.commands.lock().unwrap().push(command.clone());
            if self.refuse {
                bail!("no such file or directory");
            }
            let Streams::Files { stdout, stderr } = streams else {
                panic!("a program's output goes to its files")
            };
            let at = std::time::SystemTime::UNIX_EPOCH;
            self.files.put(stdout, at, self.output.0);
            self.files.put(stderr, at, self.output.1);
            Ok(Box::new(Process {
                exit: self.exit.clone(),
                stopped: self.stopped.clone(),
            }))
        }
    }

    /// A clock that stands still: a program's limit is reached only when
    /// it is zero.
    struct Still(std::time::Instant);

    impl Clock for Still {
        fn system_time(&self) -> std::time::SystemTime {
            std::time::SystemTime::UNIX_EPOCH
        }
        fn monotonic(&self) -> std::time::Instant {
            self.0
        }
    }

    fn exit(success: bool, description: &str) -> Exit {
        Exit {
            success,
            code: success.then_some(0).or(Some(1)),
            signal: None,
            description: description.to_owned(),
        }
    }

    /// Run [`a_program`] against `/w` to its end with `spawns`, on the
    /// host's backend or the container's, within `timeout`.
    fn run(spawns: &Spawns<'_>, config: ExecutionConfig, timeout: Duration) -> ProgramRun {
        let host = Counting::default();
        let backends = ProgramBackends::new(&host, config);
        let clock = Still(std::time::Instant::now());
        let env = [("GIT_GUARD".to_owned(), "1".to_owned())];
        run_program(
            (spawns, &backends, spawns.files, &clock),
            &a_program(),
            (Path::new("/w"), &env),
            (Path::new("/run"), Path::new("/scratch"), "check-fmt"),
            timeout,
        )
    }

    /// A program that exits 0 passes, with the ends of its output; it ran
    /// from the host's command in a group of its own, with the guard's env
    /// on top, its output to `<output>/<stem>.out` and `.err`.
    #[test]
    fn a_program_run_to_its_end_that_exits_0_passes() {
        let files = crate::application::memory_files::MemoryFiles::default();
        let spawns = Spawns::new(&files, Some(exit(true, "exit status: 0")));
        let ended = run(&spawns, ExecutionConfig::default(), Duration::from_secs(60));
        let ProgramRun::Passed(end) = &ended else {
            panic!("{ended:?}")
        };
        assert_eq!(ended.outcome(), ProgramOutcome::Passed);
        assert_eq!(end.stdout_tail, "checked\n");
        assert_eq!(end.stderr_tail, "src/b.rs: not formatted\n");
        assert!(files.bytes(Path::new("/run/check-fmt.out")).is_ok());
        let commands = spawns.commands.lock().unwrap();
        assert!(commands[0].get_new_session());
        assert!(
            commands[0]
                .get_envs()
                .any(|(key, value)| key == "GIT_GUARD" && value == Some("1".as_ref()))
        );
        assert!(!spawns.stopped.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// A program that exits non-zero rejects the change, its reason
    /// naming it, its exit and the end of its output.
    #[test]
    fn a_program_run_to_its_end_that_exits_non_zero_rejects_with_its_name_and_output() {
        let files = crate::application::memory_files::MemoryFiles::default();
        let spawns = Spawns::new(&files, Some(exit(false, "exit status: 1")));
        let ended = run(&spawns, ExecutionConfig::default(), Duration::from_secs(60));
        assert_eq!(ended.outcome(), ProgramOutcome::Rejected);
        let ProgramRun::Rejected { reason, end } = ended else {
            panic!()
        };
        assert_eq!(
            reason,
            review_programs::rejection_reason(
                "fmt",
                "exit status: 1",
                "checked\n",
                "src/b.rs: not formatted\n"
            )
        );
        assert!(reason.contains("fmt") && reason.contains("src/b.rs: not formatted"));
        assert_eq!(end.exit.unwrap().description, "exit status: 1");
    }

    /// A program on the container's backend, or one whose process cannot
    /// be spawned, could not start: nothing runs on the host instead.
    #[test]
    fn a_program_run_to_its_end_that_cannot_start_says_why_and_never_runs_on_the_host() {
        let files = crate::application::memory_files::MemoryFiles::default();
        let spawns = Spawns::new(&files, Some(exit(true, "exit status: 0")));
        let container = ExecutionConfig {
            backend: ExecutorBackend::Container,
            actors: Vec::new(),
        };
        let ended = run(&spawns, container, Duration::from_secs(60));
        assert_eq!(ended.outcome(), ProgramOutcome::StartFailed);
        let ProgramRun::StartFailed(error) = &ended else {
            panic!("{ended:?}")
        };
        assert!(
            error.starts_with("the review program fmt could not start")
                && error.contains("container"),
            "{error}"
        );
        assert!(spawns.commands.lock().unwrap().is_empty());

        let mut refused = Spawns::new(&files, Some(exit(true, "exit status: 0")));
        refused.refuse = true;
        let ended = run(
            &refused,
            ExecutionConfig::default(),
            Duration::from_secs(60),
        );
        let ProgramRun::StartFailed(error) = &ended else {
            panic!("{ended:?}")
        };
        assert!(error.contains("no such file or directory"), "{error}");
    }

    /// A program still running at its limit (zero here, so the first look
    /// reaches it) is stopped with its process group and timed out, with
    /// the ends of its output.
    #[test]
    fn a_program_run_to_its_end_past_its_limit_is_stopped_with_its_group() {
        let files = crate::application::memory_files::MemoryFiles::default();
        let spawns = Spawns::new(&files, None);
        let ended = run(&spawns, ExecutionConfig::default(), Duration::ZERO);
        assert_eq!(ended.outcome(), ProgramOutcome::TimedOut);
        let ProgramRun::TimedOut { timeout, end } = ended else {
            panic!()
        };
        assert_eq!(timeout, Duration::ZERO);
        assert_eq!(end.exit, None);
        assert_eq!(end.stdout_tail, "checked\n");
        assert!(spawns.stopped.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// The end of a long output is what is kept, at most
    /// [`PROGRAM_OUTPUT_TAIL`] bytes of each stream.
    #[test]
    fn a_programs_end_keeps_the_end_of_its_output() {
        let files = crate::application::memory_files::MemoryFiles::default();
        let long = format!("{}the end", "x".repeat(PROGRAM_OUTPUT_TAIL));
        files.put(Path::new("/o"), std::time::SystemTime::UNIX_EPOCH, &long);
        let end = program_end(&files, (Path::new("/o"), Path::new("/missing")), None);
        assert_eq!(end.stdout_tail.len(), PROGRAM_OUTPUT_TAIL);
        assert!(end.stdout_tail.ends_with("the end"));
        assert_eq!(end.stderr_tail, "");
    }
}
