//! The fixture's worker mode (goal 92): the fixture's tasks are headless,
//! and [`TestWorkspace`] runs each one's agent script as the turns of a stub
//! `claude` of the run's own, so that the runtime tests that check nothing
//! of an interactive session go through the headless worker without
//! changing their scripts; [`interactive_workers`] chooses an interactive
//! session for the tests of what is particular to one.
use super::*;

thread_local! {
    /// The worker mode of the tasks the fixture makes in this test:
    /// headless unless the test chose [`interactive_workers`].
    static WORKER_MODE: std::cell::Cell<dagq::domain::worker::WorkerMode> =
        const { std::cell::Cell::new(dagq::domain::worker::WorkerMode::Headless) };
}

/// Run this test's workers in interactive sessions: from now on the tasks
/// the fixture makes ([`fixture`], [`add_ready_task`], [`run_agent`],
/// [`awaiting_run`], [`parked_conflict`]) are interactive, and a
/// [`TestWorkspace`] made after it types into the fake cmux session that
/// runs each one's script. Only the tests of what is particular to an
/// interactive session (its screen, dialogs, `/exit`, Enter, the answer
/// prompt, a stuck exit, the typed texts) call it, before the fixture
/// (goal 92); every other test's workers are headless: a [`TestWorkspace`]
/// runs each one's script (its [`TestWorkspace::script_for`] or default
/// script, with [`TURN_PRELUDE`]) as every turn of the stub `claude` of
/// [`headless_claude`], and a resumed run's script (with
/// [`RESUME_TURN_PRELUDE`]) as its turns after the resume.
pub fn interactive_workers() {
    WORKER_MODE.set(dagq::domain::worker::WorkerMode::Interactive);
}

/// The worker mode of the tasks the fixture makes in this test.
pub fn worker_mode() -> dagq::domain::worker::WorkerMode {
    WORKER_MODE.get()
}

/// A headless turn has no session to go idle in or to be asked to exit:
/// its `idle` helpers and `await_exit` return at once, and the turn ends
/// when its script does (goal 92).
macro_rules! turn_session_helpers {
    () => {
        r#"
idle() { :; }
idle_bg() { :; }
idle_bg_done() { :; }
await_exit() { :; }
"#
    };
}

/// The prelude of a fake agent's script run as a headless turn (goal 92):
/// the stub `claude` of [`headless_claude`] sources it after its own
/// helpers, so the script's `receipt` and `commit` are the
/// [`worker_helpers`] of an interactive session, and `idle` and
/// `await_exit` return at once ([`turn_session_helpers`]).
pub const TURN_PRELUDE: &str = concat!(worker_helpers!(), turn_session_helpers!());

/// The prelude of a resumed session's script run as a headless resume turn
/// (goal 92): the resolution request is the turn's prompt, so
/// `await_message` sets `$MAIN` from `$PROMPT` at once (a later turn's,
/// from the last request's); `receipt` and
/// `resolve` are a resumed session's.
pub const RESUME_TURN_PRELUDE: &str = concat!(
    resume_receipt!(),
    turn_session_helpers!(),
    r#"
await_message() {
  MAIN=$(printf '%s\n' "$PROMPT" | sed -n 's/.*main is now \([0-9a-f]*\) .*/\1/p' | head -n 1)
  if [ -n "$MAIN" ]; then printf '%s' "$MAIN" > "$RUN_DIR/resume-main"; else MAIN=$(cat "$RUN_DIR/resume-main" 2>/dev/null); fi
}
"#,
    resolve_helpers!()
);

impl TestWorkspace {
    /// The stub `claude` of `run`'s headless turns: [`Self::headless`], or
    /// with [`Self::turn_scripts`] a stub of the run's own (one per run on
    /// the queue, whichever backend starts it) whose turns run, from each
    /// start on, this backend's script for the task ([`Self::script_for`]
    /// or the default) after [`TURN_PRELUDE`], and from a resume on its
    /// [`Self::resume_script_for`] after [`RESUME_TURN_PRELUDE`]; a resume
    /// without one fails, as an interactive one does (goal 92).
    pub fn claude_for(&self, run: &TaskRun, resume: bool) -> Result<Option<PathBuf>> {
        if self.headless.is_some() || !self.turn_scripts {
            return Ok(self.headless.clone());
        }
        let dir = self.db.with_file_name("turn-stubs").join(run.id().as_str());
        let stub = dir.join("claude-headless");
        if !stub.exists() {
            fs::create_dir_all(&dir)?;
            headless_claude(&dir, &self.db);
        }
        let turns = if resume {
            let script = self
                .resume_scripts
                .lock()
                .unwrap()
                .get(&run.task_id())
                .cloned();
            let script = script
                .ok_or_else(|| anyhow::anyhow!("no resume script for task {}", run.task_id()))?;
            turn_script(run, RESUME_TURN_PRELUDE, &script)
        } else {
            let script = self
                .scripts
                .lock()
                .unwrap()
                .get(&run.task_id())
                .cloned()
                .unwrap_or_else(|| self.script.clone());
            turn_script(run, TURN_PRELUDE, &script)
        };
        set_turns(&dir, &turns);
        Ok(Some(stub))
    }
}

/// `script` as the turns of `run`'s stub `claude` (goal 92): after
/// `prelude`, with the variables an interactive session's script is given.
/// Each turn keeps its prompt as `turn-<n>.prompt` in the run directory
/// ([`session_texts`]) and says something, so that the wrapper resumes the
/// session for the next turn.
fn turn_script(run: &TaskRun, prelude: &str, script: &str) -> String {
    let run_dir = run.run_dir().unwrap();
    let mut vars = String::new();
    for (name, value) in [
        ("RUN_ID", run.id().to_string()),
        ("BASE", run.base_commit().as_str().to_owned()),
        (
            "IDLE",
            run.idle_marker_path().unwrap().display().to_string(),
        ),
        ("EXIT", exit_request_path(run_dir).display().to_string()),
        (
            "MESSAGE",
            resume_message_path(run_dir).display().to_string(),
        ),
    ] {
        vars.push_str(&format!("{name}={}\n", shell_join(&[value])));
    }
    vars.push_str("printf '%s' \"$PROMPT\" > \"$RUN_DIR/turn-$TURN.prompt\"\n");
    // The model answers every turn, so the session is created and a later
    // turn resumes it with only its own text, as Claude's does.
    vars.push_str("say \"turn $TURN\"\n");
    format!("{vars}{prelude}\n{script}\n")
}

/// What the supervisor told `run`'s worker after starting it (a resume's
/// request, an answer), in order: the texts typed into an interactive
/// session ([`TestWorkspace::texts`], of every workspace), or the prompts of
/// a headless run's turns after its first (goal 92).
pub fn session_texts(backend: &TestWorkspace, run: &TaskRun) -> Vec<String> {
    if run.worker_mode() != dagq::domain::worker::WorkerMode::Headless {
        return backend.texts().into_iter().map(|(_, text)| text).collect();
    }
    let run_dir = Path::new(run.run_dir().unwrap());
    (2..)
        .map_while(|turn| fs::read_to_string(run_dir.join(format!("turn-{turn}.prompt"))).ok())
        .collect()
}

/// The model and effort each turn of the headless runs on `db` was started
/// with, as [`session_models`] has an interactive session's (goal 92):
/// `run_id`, `resume`, `model`, `effort`, from the arguments the stub of
/// [`headless_claude`] logged, in the order of the runs' directories.
pub fn turn_models(db: &Path) -> Vec<Value> {
    let runs = db.canonicalize().unwrap().with_file_name("runs");
    let mut dirs: Vec<PathBuf> = fs::read_dir(&runs)
        .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
        .unwrap_or_default();
    dirs.sort();
    let mut models = Vec::new();
    for dir in dirs {
        let Ok(log) = fs::read_to_string(dir.join("stub-args.log")) else {
            continue;
        };
        let run_id = dir.file_name().unwrap().to_str().unwrap().to_owned();
        for line in log.lines() {
            // The arguments before the prompt.
            let words: Vec<&str> = line.split(' ').take_while(|word| *word != "--").collect();
            let after = |flag: &str| {
                words
                    .iter()
                    .position(|word| *word == flag)
                    .map(|at| words[at + 1])
            };
            models.push(json!({
                "run_id": run_id,
                "resume": words.contains(&"--resume"),
                "model": after("--model"),
                "effort": after("--effort"),
            }));
        }
    }
    models
}

/// The supervisor asked `run`'s worker to exit, or with `times` 0 did not:
/// for an interactive session, `/exit` was typed `times` times into the
/// backend's sessions ([`TestWorkspace::exits_sent`], all runs together);
/// for a headless one, nothing was typed and its wrapper got an exit request
/// in the run's turns directory (or none), whose count the run's
/// `exit_requested` events give (goal 92).
pub fn assert_exit_sent(backend: &TestWorkspace, run: &TaskRun, times: usize) {
    if run.worker_mode() == dagq::domain::worker::WorkerMode::Headless {
        assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
        let exit = dagq::domain::turn::exit_path(Path::new(run.run_dir().unwrap()));
        assert_eq!(
            exit.exists(),
            times > 0,
            "the exit request at {}",
            exit.display()
        );
    } else {
        assert_eq!(backend.exits_sent.load(Ordering::SeqCst), times);
    }
}

/// The steps the supervisor raised the live worker to before its revises,
/// in order: the `/effort` texts it switched an interactive session with
/// ([`TestWorkspace::switches`]), or the `--effort` of every resumed turn
/// of the headless runs on the backend's queue, which took no switch (goal
/// 92). It reads the revises only for a queue whose one run's resumed
/// turns are all revises.
pub fn revise_efforts(backend: &TestWorkspace) -> Vec<String> {
    let switches: Vec<String> = backend.switches().into_iter().map(|(_, s)| s).collect();
    let turns = turn_models(&backend.db);
    if turns.is_empty() {
        return switches;
    }
    assert_eq!(switches, Vec::<String>::new());
    turns
        .iter()
        .filter(|turn| turn["resume"] == true)
        .map(|turn| format!("/effort {}", turn["effort"].as_str().unwrap()))
        .collect()
}
