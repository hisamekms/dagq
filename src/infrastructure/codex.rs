//! Codex CLI as a headless worker (ADR-t813-1, ADR-t813-3): each turn is
//! one `codex exec --json` (the first) or `codex exec resume --json
//! <thread id>` (every later one) in the run's worktree, in the
//! workspace-write sandbox with the writable roots, network and approval
//! policy of ADR-t813-3 given as `-c` on every call (a resume does not
//! keep the first call's), and the project rules that forbid `pkill` and
//! `killall` in the worktree. Nothing of the person's (`~/.codex`,
//! `CODEX_HOME`) is written or changed.

use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::application::{AgentProvider, CommandSpec, TurnReader};
use crate::domain::{TaskRun, turn::TurnSession};

use super::{adapters::output, codex_turns::CodexTurnReader};

/// Codex CLI (`codex`, resolved to its executable by [`executable`]:
/// cmux's shim would add hooks and trust flags of its own).
#[derive(Debug, Clone)]
pub struct Codex {
    pub executable: PathBuf,
}

/// The directories of cmux's per-surface shims (`$TMPDIR/cmux-cli-shims/
/// <surface>/codex`), which a cmux terminal puts first on PATH: the shim
/// adds cmux's hooks with `--dangerously-bypass-hook-trust`, and goes with
/// its surface.
const CMUX_SHIMS: &str = "cmux-cli-shims";

/// `path` resolved to the Codex executable (ADR-t813-3 decision 6): a
/// bare name is looked for on `search` (PATH) past cmux's shims, and the
/// result is canonicalized.
pub fn executable_on(path: &Path, search: &std::ffi::OsStr) -> Result<PathBuf> {
    let candidate = if path.components().count() > 1 || path.is_absolute() {
        path.to_owned()
    } else {
        std::env::split_paths(search)
            .filter(|dir| !dir.to_string_lossy().contains(CMUX_SHIMS))
            .map(|dir| dir.join(path))
            .find(|candidate| candidate.is_file())
            .with_context(|| {
                format!(
                    "{} was not found on PATH (outside cmux's shims)",
                    path.display()
                )
            })?
    };
    candidate
        .canonicalize()
        .with_context(|| format!("resolve executable {}", candidate.display()))
}

/// [`executable_on`] this process's PATH.
pub fn executable(path: &Path) -> Result<PathBuf> {
    executable_on(path, &std::env::var_os("PATH").unwrap_or_default())
}

/// The project rules the runtime puts in a Codex run's worktree (ADR-t813-3
/// decision 5): `pkill` and `killall` are forbidden, with why. The sandbox
/// is the main defence (a process outside it can be neither listed nor
/// signalled); the rules only answer a plain `pkill` with the reason, as
/// `sh -c 'pkill …'` goes round them.
pub const RULES_PATH: &str = ".codex/rules/dagq-deny.rules";

/// The content of [`RULES_PATH`].
pub const RULES: &str = r#"# Written by dagq for this run's Codex worker (ADR-t813-3); not committed.
prefix_rule(pattern=["pkill"], decision="forbidden", justification="dagq: stop only processes you started, by pid or task; pkill also stops other runs' sessions and checks")
prefix_rule(pattern=["killall"], decision="forbidden", justification="dagq: stop only processes you started, by pid or task; killall also stops other runs' sessions and checks")
"#;

/// The sandbox of every turn, before its writable roots: workspace-write
/// (the worktree, `/tmp` and `$TMPDIR`), no approval (what needs one
/// fails at once instead of waiting) and the network open for sccache's
/// server and the registry (ADR-t813-3 decisions 1 and 4).
pub const SANDBOX_CONFIG: [&str; 3] = [
    r#"sandbox_mode="workspace-write""#,
    r#"approval_policy="never""#,
    "sandbox_workspace_write.network_access=true",
];

/// The places a Codex worker may write besides its worktree (ADR-t813-3
/// decision 2): of the Git common directory `G` only the worktree's own
/// directory, the objects and the refs (and their logs) of the `dagq/`
/// run branches, so that the run branch takes commits and `main` does
/// not; the run directory (the receipt); and cargo's registry.
pub fn writable_roots(worktree: &Path, run_dir: &Path, cargo_home: &Path) -> Result<Vec<PathBuf>> {
    let admin = worktree_admin_dir(worktree)?;
    let common = common_dir(&admin)?;
    Ok(vec![
        admin,
        common.join("objects"),
        common.join("refs/heads/dagq"),
        common.join("logs/refs/heads/dagq"),
        run_dir.to_owned(),
        cargo_home.join("registry"),
    ])
}

/// The directory Git keeps a linked worktree's state in: the `gitdir:` of
/// its `.git` file.
fn worktree_admin_dir(worktree: &Path) -> Result<PathBuf> {
    let file = worktree.join(".git");
    let text = fs::read_to_string(&file)
        .with_context(|| format!("{} is not a linked worktree's", file.display()))?;
    let dir = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .with_context(|| format!("{} names no gitdir", file.display()))?;
    Ok(worktree.join(dir))
}

/// The common directory of the worktree whose own directory is `admin`:
/// its `commondir` file, relative to `admin`.
fn common_dir(admin: &Path) -> Result<PathBuf> {
    let file = admin.join("commondir");
    let text = fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
    let common = admin.join(text.trim());
    common
        .canonicalize()
        .with_context(|| format!("resolve {}", common.display()))
}

/// Cargo's home: `$CARGO_HOME`, else `~/.cargo`.
fn cargo_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME").filter(|home| !home.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".cargo"))
}

/// `-c sandbox_workspace_write.writable_roots=[…]`: the roots as a TOML
/// array of strings (JSON's escapes are TOML's).
pub fn writable_roots_config(roots: &[PathBuf]) -> Result<String> {
    let roots = roots
        .iter()
        .map(|root| {
            let text = root
                .to_str()
                .with_context(|| format!("{} is not UTF-8", root.display()))?;
            Ok(serde_json::to_string(text)?)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(format!(
        "sandbox_workspace_write.writable_roots=[{}]",
        roots.join(",")
    ))
}

/// Codex's reasoning effort for the effort a claim chose: Codex has no
/// `max`, its highest is `xhigh`.
pub fn reasoning_effort(effort: &str) -> &str {
    match effort {
        "max" => "xhigh",
        other => other,
    }
}

/// Write [`RULES`] to the worktree, and keep it out of Git through the
/// repository's `info/exclude` (shared by its worktrees; the line is added
/// once), so that the worktree stays clean for the receipt.
pub fn write_rules(worktree: &Path) -> Result<()> {
    let path = worktree.join(RULES_PATH);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    fs::write(&path, RULES).with_context(|| format!("write {}", path.display()))?;
    let exclude = common_dir(&worktree_admin_dir(worktree)?)?.join("info/exclude");
    let line = format!("/{RULES_PATH}");
    let text = match fs::read_to_string(&exclude) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", exclude.display())),
    };
    if !text.lines().any(|existing| existing.trim() == line) {
        if let Some(dir) = exclude.parent() {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let separator = if text.is_empty() || text.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        // Appended, never rewritten: the file is the repository's, read by
        // Git and by other runs' turns meanwhile. Two turns that append at
        // once leave the line twice, which Git takes as once.
        use std::io::Write;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&exclude)
            .and_then(|mut file| file.write_all(format!("{separator}{line}\n").as_bytes()))
            .with_context(|| format!("append to {}", exclude.display()))?;
    }
    Ok(())
}

impl AgentProvider for Codex {
    fn preflight(&self) -> Result<()> {
        output(Command::new(&self.executable).arg("--version"))?;
        Ok(())
    }
    fn command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        bail!("Codex runs headless only: it has no interactive session")
    }
    fn resume_command(&self, _: &TaskRun) -> Result<CommandSpec> {
        bail!("Codex runs headless only: it has no interactive session")
    }
    fn review_command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        bail!("Codex does not review: the review jobs stay Claude's")
    }
    /// `-m <model>` for a model of Codex's (a claim's Claude model is left
    /// to Codex's default) and `-c model_reasoning_effort="<effort>"`.
    fn select_model(&self, command: &mut CommandSpec, model: &str, effort: &str) {
        if !model.starts_with("claude") {
            command.option_args(["-m", model]);
        }
        command.option_args([
            "-c".to_owned(),
            format!(r#"model_reasoning_effort="{}""#, reasoning_effort(effort)),
        ]);
    }
    /// `codex exec --json -C <worktree>` for the first turn and `codex exec
    /// resume --json … <thread id>` after it (`resume` has no `-C`, so the
    /// worktree is its working directory either way), with
    /// [`SANDBOX_CONFIG`] and the run's [`writable_roots`] as `-c` on each,
    /// the prompt after `--`. It leads a session of its own: stopped, Codex
    /// leaves its commands running unless its group is stopped with it.
    fn turn_command(
        &self,
        run: &TaskRun,
        prompt: &str,
        session: TurnSession<'_>,
    ) -> Result<CommandSpec> {
        // Codex names a new thread itself.
        let resume = session.resumed();
        let worktree = Path::new(run.worktree_path().context("missing worktree")?);
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let roots = writable_roots(worktree, run_dir, &cargo_home()?)?;
        write_rules(worktree)?;
        let mut command = CommandSpec::new(&self.executable);
        command.current_dir(worktree).arg("exec");
        if resume.is_some() {
            command.arg("resume");
        }
        command.arg("--json");
        if resume.is_none() {
            command.arg("-C").arg(worktree);
        }
        for config in SANDBOX_CONFIG
            .into_iter()
            .map(str::to_owned)
            .chain([writable_roots_config(&roots)?])
        {
            command.arg("-c").arg(config);
        }
        command.arg("--");
        if let Some(thread) = resume {
            command.arg(thread);
        }
        command.arg(prompt).new_session();
        Ok(command)
    }
    fn turn_reader(&self) -> Result<Box<dyn TurnReader>> {
        Ok(Box::new(CodexTurnReader::default()))
    }
    fn turn_session_from_output(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository `G` with a linked worktree `wt` named `run`, as Git
    /// lays it out.
    fn layout(dir: &Path) -> (PathBuf, PathBuf) {
        let common = dir.join("repo/.git");
        let admin = common.join("worktrees/run");
        fs::create_dir_all(&admin).unwrap();
        fs::create_dir_all(common.join("info")).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        let worktree = dir.join("wt");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        (common.canonicalize().unwrap(), worktree)
    }

    #[test]
    fn the_writable_roots_are_the_run_branch_the_run_directory_and_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let (common, worktree) = layout(dir.path());
        let roots = writable_roots(&worktree, Path::new("/runs/r1"), Path::new("/c")).unwrap();
        assert_eq!(
            roots,
            [
                dir.path().join("repo/.git/worktrees/run"),
                common.join("objects"),
                common.join("refs/heads/dagq"),
                common.join("logs/refs/heads/dagq"),
                PathBuf::from("/runs/r1"),
                PathBuf::from("/c/registry"),
            ]
        );
        assert_eq!(
            writable_roots_config(&[PathBuf::from("/a b"), PathBuf::from("/q\"x")]).unwrap(),
            r#"sandbox_workspace_write.writable_roots=["/a b","/q\"x"]"#
        );
        // Not a linked worktree: no roots to guess.
        assert!(writable_roots(dir.path(), Path::new("/r"), Path::new("/c")).is_err());
    }

    #[test]
    fn codex_is_resolved_past_cmux_shims() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("cmux-cli-shims/s1");
        let bin = dir.path().join("bin");
        for d in [&shim, &bin] {
            fs::create_dir_all(d).unwrap();
            fs::write(d.join("codex"), "#!/bin/sh\n").unwrap();
        }
        let search = std::env::join_paths([&shim, &bin]).unwrap();
        assert_eq!(
            executable_on(Path::new("codex"), &search).unwrap(),
            bin.join("codex").canonicalize().unwrap()
        );
        let only_shim = std::env::join_paths([&shim]).unwrap();
        assert!(executable_on(Path::new("codex"), &only_shim).is_err());
        // A path is taken as it is.
        assert_eq!(
            executable_on(&shim.join("codex"), &search).unwrap(),
            shim.join("codex").canonicalize().unwrap()
        );
    }

    #[test]
    fn the_rules_are_written_once_and_kept_out_of_git() {
        let dir = tempfile::tempdir().unwrap();
        let (common, worktree) = layout(dir.path());
        fs::write(common.join("info/exclude"), "# excludes").unwrap();
        write_rules(&worktree).unwrap();
        write_rules(&worktree).unwrap();
        assert_eq!(
            fs::read_to_string(worktree.join(RULES_PATH)).unwrap(),
            RULES
        );
        assert_eq!(
            fs::read_to_string(common.join("info/exclude")).unwrap(),
            "# excludes\n/.codex/rules/dagq-deny.rules\n"
        );
    }

    #[test]
    fn a_claude_model_is_left_to_codex_and_the_effort_is_given() {
        let codex = Codex {
            executable: "codex".into(),
        };
        let mut command = CommandSpec::new("codex");
        command.args(["exec", "--json", "--", "prompt"]);
        codex.select_model(&mut command, "claude-opus-5-5", "max");
        codex.select_model(&mut command, "gpt-6-astra", "low");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "exec",
                "--json",
                "-c",
                r#"model_reasoning_effort="xhigh""#,
                "-m",
                "gpt-6-astra",
                "-c",
                r#"model_reasoning_effort="low""#,
                "--",
                "prompt"
            ]
        );
    }

    fn run(worktree: &Path, run_dir: &Path) -> TaskRun {
        use crate::domain::{CommitSha, Provider, RunId, RunStatus, TaskId};
        TaskRun::restore(crate::domain::RunRecord {
            id: RunId::new("0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47").unwrap(),
            task_id: TaskId::new(15),
            status: RunStatus::Running,
            requested_provider: Provider::Codex,
            actual_provider: Provider::Codex,
            worker_mode: crate::domain::worker::WorkerMode::Headless,
            base_commit: CommitSha::try_from("a".repeat(40)).unwrap(),
            branch: None,
            worktree_path: Some(worktree.display().to_string()),
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: Some(run_dir.display().to_string()),
            last_error: None,
            workspace_closed_at: None,
            created_at: "2026-09-28 00:00:00".into(),
        })
        .unwrap()
    }

    #[test]
    fn a_turn_starts_or_resumes_the_thread_with_the_same_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let (common, worktree) = layout(dir.path());
        let run_dir = dir.path().join("run");
        let run = run(&worktree, &run_dir);
        let codex = Codex {
            executable: "/bin/codex".into(),
        };
        let args = |command: &CommandSpec| -> Vec<String> {
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        let roots = writable_roots_config(
            &writable_roots(&worktree, &run_dir, &cargo_home().unwrap()).unwrap(),
        )
        .unwrap();
        let first = codex
            .turn_command(&run, "-do it", TurnSession::New("ignored"))
            .unwrap();
        assert_eq!(first.get_program(), "/bin/codex");
        assert_eq!(first.get_current_dir(), Some(worktree.as_path()));
        assert!(first.get_new_session());
        let sandbox = [
            "-c",
            SANDBOX_CONFIG[0],
            "-c",
            SANDBOX_CONFIG[1],
            "-c",
            SANDBOX_CONFIG[2],
            "-c",
            roots.as_str(),
        ];
        let worktree_text = worktree.display().to_string();
        let mut expected = vec!["exec", "--json", "-C", worktree_text.as_str()];
        expected.extend(sandbox);
        expected.extend(["--", "-do it"]);
        assert_eq!(args(&first), expected);
        let resumed = codex
            .turn_command(&run, "answer", TurnSession::Resume("th-1"))
            .unwrap();
        assert_eq!(resumed.get_current_dir(), Some(worktree.as_path()));
        let mut expected = vec!["exec", "resume", "--json"];
        expected.extend(sandbox);
        expected.extend(["--", "th-1", "answer"]);
        assert_eq!(args(&resumed), expected);
        // The rules are in the worktree, excluded.
        assert!(worktree.join(RULES_PATH).is_file());
        assert!(
            fs::read_to_string(common.join("info/exclude"))
                .unwrap()
                .contains(RULES_PATH)
        );
        assert!(codex.command(&run, "p").is_err());
        assert!(codex.resume_command(&run).is_err());
        assert!(codex.review_command(&run, "p").is_err());
        assert!(codex.turn_session_from_output());
        assert!(codex.turn_reader().is_ok());
        assert!(
            Codex {
                executable: "/nonexistent/codex".into()
            }
            .preflight()
            .is_err()
        );
    }
}
