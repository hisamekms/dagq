//! The landing's verification a repository sets in `dagq.toml`
//! (ADR-t1925-1 decision 4): which of a task's verification commands
//! `integrate` replaces by the repository's own command after its rebase,
//! and what the runtime hands that command in its environment. The
//! registered commands stay as they are; the replacement is decided anew
//! at each landing. How the command chooses the tests, and which commands
//! it replaces, are the repository's: the runtime knows neither its tests
//! nor its tables.

use serde_json::{Value, json};

/// The table of `dagq.toml` that sets the landing's verification. Without
/// it `integrate` runs the task's commands as registered.
///
/// ```toml
/// [landing_verification]
/// replaces = ["cargo llvm-cov nextest"]
/// command = "sh scripts/landing-verify.sh"
/// ```
pub const TABLE: &str = "landing_verification";

/// The key of [`LandingVerification::replaces`]: an array of non-blank
/// strings, no one twice; required.
pub const REPLACES: &str = "replaces";

/// The key of [`LandingVerification::command`]: a non-blank string;
/// required.
pub const COMMAND: &str = "command";

/// The keys [`TABLE`] takes.
pub const KEYS: [&str; 2] = [REPLACES, COMMAND];

/// The environment variable holding the commit of main the run was
/// rebased onto: the base of the diff that lands (`git diff
/// $DAGQ_LANDING_BASE..HEAD`), which the command chooses its tests from.
pub const BASE_ENV: &str = "DAGQ_LANDING_BASE";

/// The environment variable holding the path of the file in the run's
/// directory ([`KNOWN_FAILURES_FILE`]) with the tests that fail already
/// on the watched branch (ADR-t1920-1 decision 5): the JSON of `dagq ci
/// failures --task <the run's task>`, whose `failures` the command leaves
/// out. A run of a CI fix task finds the items of the findings it fixes
/// under `kept_for_task` instead, so they are run. The list is what the
/// watch has recorded in the queue, read whether or not `[ci_watch]` is
/// set now; the file is written either way, and `failures` is empty only
/// when nothing is recorded (or every recorded test passed again).
pub const KNOWN_FAILURES_ENV: &str = "DAGQ_CI_KNOWN_FAILURES";

/// The environment variable that says whether the run is one of a task
/// that fixes a `ci_failure` finding (ADR-t1920-1 decision 5): `1` when
/// it is, `0` when not.
pub const FIX_RUN_ENV: &str = "DAGQ_CI_FIX_RUN";

/// The name of the file [`KNOWN_FAILURES_ENV`] names, in the run's
/// directory; each landing writes it anew before the command runs.
pub const KNOWN_FAILURES_FILE: &str = "ci-known-failures.json";

/// `[landing_verification]` of `dagq.toml` (ADR-t1925-1 decision 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandingVerification {
    /// What marks a task's verification command as one to replace: a
    /// command that contains any of these texts, as written, is replaced
    /// (the repository's coverage gate, say).
    pub replaces: Vec<String>,
    /// The shell command run in place of the replaced ones, in the run's
    /// worktree like the others, with `[run.env]` and the variables
    /// [`BASE_ENV`], [`KNOWN_FAILURES_ENV`] and [`FIX_RUN_ENV`].
    pub command: String,
}

/// One command the landing runs: `command`, and the task's commands it
/// runs in place of (`replaces`, empty for a command run as registered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub command: String,
    pub replaces: Vec<String>,
}

impl Planned {
    /// Whether this is the repository's command run in place of the task's.
    pub fn replaced(&self) -> bool {
        !self.replaces.is_empty()
    }

    /// `replaces` of the `verification_command` event: the task's commands
    /// this one runs in place of, null for a command run as registered.
    pub fn replaces_json(&self) -> Value {
        if self.replaced() {
            json!(self.replaces)
        } else {
            Value::Null
        }
    }

    /// The line put at the top of the command's log when it was replaced.
    pub fn log_header(&self) -> Option<String> {
        self.replaced().then(|| {
            format!(
                "# dagq: [{TABLE}] of dagq.toml runs this command in place of the task's {} (ADR-t1925-1)\n",
                self.replaces
                    .iter()
                    .map(|command| format!("{command:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }
}

/// The commands the landing runs for a task whose verification commands
/// are `commands`: each as registered when `config` is none or the
/// command contains none of its `replaces`; the first that does becomes
/// `config.command`, and any later one that does is run by it too rather
/// than once more. The order of the others is kept.
pub fn plan(config: Option<&LandingVerification>, commands: &[String]) -> Vec<Planned> {
    let mut planned: Vec<Planned> = Vec::with_capacity(commands.len());
    let mut replacement: Option<usize> = None;
    for command in commands {
        let hit = config.filter(|config| {
            config
                .replaces
                .iter()
                .any(|text| command.contains(text.as_str()))
        });
        match (hit, replacement) {
            (None, _) => planned.push(Planned {
                command: command.clone(),
                replaces: Vec::new(),
            }),
            (Some(_), Some(index)) => planned[index].replaces.push(command.clone()),
            (Some(config), None) => {
                replacement = Some(planned.len());
                planned.push(Planned {
                    command: config.command.clone(),
                    replaces: vec![command.clone()],
                });
            }
        }
    }
    planned
}

/// The variables the replacing command gets on top of `[run.env]`: the
/// main commit `base` the run was rebased onto, the path of the known
/// failures `known_failures`, and whether the run fixes a CI failure.
pub fn env(base: &str, known_failures: &str, fix_run: bool) -> Vec<(String, String)> {
    vec![
        (BASE_ENV.to_owned(), base.to_owned()),
        (KNOWN_FAILURES_ENV.to_owned(), known_failures.to_owned()),
        (
            FIX_RUN_ENV.to_owned(),
            if fix_run { "1" } else { "0" }.to_owned(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> LandingVerification {
        LandingVerification {
            replaces: vec!["cargo llvm-cov nextest".into()],
            command: "sh scripts/landing-verify.sh".into(),
        }
    }

    fn commands(list: &[&str]) -> Vec<String> {
        list.iter().map(|command| (*command).to_owned()).collect()
    }

    fn as_registered(command: &str) -> Planned {
        Planned {
            command: command.into(),
            replaces: Vec::new(),
        }
    }

    #[test]
    fn without_the_table_every_command_runs_as_registered() {
        let task = commands(&[
            "cargo fmt --all --check",
            "cargo llvm-cov nextest --workspace",
        ]);
        assert_eq!(
            plan(None, &task),
            vec![
                as_registered("cargo fmt --all --check"),
                as_registered("cargo llvm-cov nextest --workspace"),
            ]
        );
    }

    #[test]
    fn the_matching_command_is_replaced_and_the_others_run_as_registered() {
        let task = commands(&[
            "cargo fmt --all --check",
            "cargo llvm-cov nextest --locked --workspace --fail-under-lines 80",
            "sh scripts/check-layer-deps.sh",
        ]);
        let planned = plan(Some(&config()), &task);
        assert_eq!(
            planned,
            vec![
                as_registered("cargo fmt --all --check"),
                Planned {
                    command: "sh scripts/landing-verify.sh".into(),
                    replaces: vec![
                        "cargo llvm-cov nextest --locked --workspace --fail-under-lines 80".into()
                    ],
                },
                as_registered("sh scripts/check-layer-deps.sh"),
            ]
        );
        assert!(planned[1].replaced());
        assert_eq!(
            planned[1].replaces_json(),
            json!(["cargo llvm-cov nextest --locked --workspace --fail-under-lines 80"])
        );
        assert_eq!(planned[0].replaces_json(), Value::Null);
        assert!(planned[0].log_header().is_none());
        let header = planned[1].log_header().unwrap();
        assert!(
            header.starts_with("# dagq: [landing_verification]"),
            "{header}"
        );
        assert!(
            header.contains("\"cargo llvm-cov nextest --locked"),
            "{header}"
        );
        assert!(header.ends_with('\n'));
    }

    #[test]
    fn a_task_without_a_matching_command_runs_as_registered() {
        let task = commands(&["cargo fmt --all --check", "cargo test --lib"]);
        assert_eq!(
            plan(Some(&config()), &task),
            vec![
                as_registered("cargo fmt --all --check"),
                as_registered("cargo test --lib"),
            ]
        );
        assert!(plan(Some(&config()), &[]).is_empty());
    }

    #[test]
    fn several_matching_commands_run_the_replacement_once() {
        let config = LandingVerification {
            replaces: vec!["llvm-cov".into(), "nextest run".into()],
            command: "landing".into(),
        };
        let task = commands(&[
            "cargo nextest run -p broker",
            "cargo clippy",
            "cargo llvm-cov nextest",
        ]);
        assert_eq!(
            plan(Some(&config), &task),
            vec![
                Planned {
                    command: "landing".into(),
                    replaces: vec![
                        "cargo nextest run -p broker".into(),
                        "cargo llvm-cov nextest".into()
                    ],
                },
                as_registered("cargo clippy"),
            ]
        );
    }

    #[test]
    fn the_command_gets_the_base_the_known_failures_and_whether_it_fixes_ci() {
        assert_eq!(
            env("abc123", "/runs/r/ci-known-failures.json", true),
            vec![
                ("DAGQ_LANDING_BASE".to_owned(), "abc123".to_owned()),
                (
                    "DAGQ_CI_KNOWN_FAILURES".to_owned(),
                    "/runs/r/ci-known-failures.json".to_owned()
                ),
                ("DAGQ_CI_FIX_RUN".to_owned(), "1".to_owned()),
            ]
        );
        assert_eq!(env("abc", "f", false)[2].1, "0");
    }
}
