//! The headless run review and the recovery job (triage) of an ended run
//! (execution and landing).

use super::*;

/// The bytes the whole run review prompt takes at most, the language's
/// instruction included (task 1571, ADR-t1566-1 decision 4): in production
/// it took 6,385 bytes at the median, 10,218 at p90 and 18,535 at most.
pub const RUN_REVIEW_PROMPT_LIMIT: usize = 32_000;

/// The bytes of the task's title, of its acceptance (1,544 at most in
/// production) and of the required subagents' list in the review prompt.
pub const RUN_REVIEW_TITLE_BYTES: usize = 1_000;
pub const RUN_REVIEW_ACCEPTANCE_BYTES: usize = 8_000;
pub const RUN_REVIEW_SUBAGENTS_BYTES: usize = 8_000;

/// The bytes the whole prompt of an agent job takes at most, the
/// language's instruction included (ADR-t1566-1 decision 4): one agent's
/// definition, the lines that say where the change is, and the
/// instructions with the verdict's schema. The job of the eval of an agent
/// and that of an agent of a run's review are the same job and share it.
/// The run review's own limit, which the definitions are far within (8
/// review agents of at most 2,529 bytes, 9,382 together).
pub const AGENT_JOB_PROMPT_LIMIT: usize = 32_000;
/// The bytes of the definition in an agent job's prompt at most. A
/// definition is never cut (ADR-t1869-1): one past this is an error of the
/// job's assembly, and no job starts for it. Over six times today's
/// largest definition.
pub const AGENT_JOB_DEFINITION_BYTES: usize = 16_000;
/// The bytes of the lines that say where the change is (its base and head
/// commits and the material file's path, about 300 bytes): cut from their
/// end past this, as any section that is not the definition.
pub const AGENT_JOB_MATERIAL_BYTES: usize = 2_000;
/// The bytes of the agent job's own instructions and its verdict's schema
/// with the agent's name (about 2,000 bytes): cut past this. With the
/// definition, the material and the language's room the sections stay
/// within [`AGENT_JOB_PROMPT_LIMIT`].
pub const AGENT_JOB_INSTRUCTIONS_BYTES: usize = 6_000;

/// What the headless reviewer is asked (ADR-0023 decision 2, ADR-0027
/// decision 2 as ADR-t451-1 decision 3 amends it): where the material is,
/// the task's acceptance, the verdict schema, where `revise` ends and
/// `concern` begins, and how a concern is judged and when it reaches a
/// person; then the required subagents (`subagents`, from
/// [`crate::application::review::review_subagents_prompt`]), when there are any. The
/// title, the acceptance and the subagents are held to their limits and
/// the whole to [`RUN_REVIEW_PROMPT_LIMIT`]; what is cut says where in
/// the worktree or the run directory the job reads it whole (task 1571,
/// ADR-t1566-1 decision 3).
pub fn review_prompt(
    task: &Task,
    run: &TaskRun,
    review_path: &str,
    subagents: Option<&str>,
) -> FittedPrompt {
    let mut fit = Fit::new(RUN_REVIEW_PROMPT_LIMIT);
    let material = format!("read it whole in the review material at {review_path}");
    let title = fit.text(
        "title",
        task.title(),
        RUN_REVIEW_TITLE_BYTES,
        Keep::Start,
        &material,
    );
    fit.section("title", &title);
    let acceptance = fit.required(
        "acceptance",
        or_none(task.acceptance()),
        RUN_REVIEW_ACCEPTANCE_BYTES,
        &format!("{material}, its section Acceptance"),
    );
    fit.section("acceptance", &acceptance);
    // The list of agents is held to its limit; how to run them and report
    // their results is never cut.
    let subagents = subagents.map_or_else(String::new, |text| {
        let instruction = crate::application::review::SUBAGENTS_INSTRUCTION;
        let list = text.strip_suffix(instruction).unwrap_or(text);
        let mut kept = fit.text(
            "subagents",
            list,
            RUN_REVIEW_SUBAGENTS_BYTES - instruction.len(),
            Keep::Start,
            "every agent and the paths that selected it are in the file the list names",
        );
        if kept.len() != list.len() {
            kept.push('\n');
        }
        if list.len() != text.len() {
            kept.push_str(instruction);
        }
        fit.section("subagents", &kept);
        kept
    });
    let mut text = format!(
        "You review run {run_id} of dagq task {task_id} ({title}) before it lands.\n\
         Read the review material at {review_path}: the task, its goal, the receipt, the commits and the full diff. Read the worktree if you need more. Do not change any file.\n\
         {rules}\n\
         {docs}\n\
         Acceptance criteria of the task:\n{acceptance}\n\n\
         Decide one verdict:\n\
         - pass: the diff meets the acceptance criteria and the task's instructions and nothing needs fixing.\n\
         - revise: findings the worker can fix without a person's judgment: missing tests or evidence, findings of the repository's formatter, linter or other checks, a receipt that disagrees with the diff where fixing the diff settles it, or an obvious gap inside the instructed scope.\n\
         - concern: findings that call for a judgment rather than a mechanical fix: a mismatch with the acceptance criteria, changes the task did not ask for, or a finding that involves a judgment call. A concern does not by itself go to a person: you judge it below, recommending land or send_back with your confidence, and the runtime applies a sure judgment that needs no person (high, reason_category null) itself; only the rest (low, scope, discard) reaches a person.\n\n\
         {concern}\
         {codes}\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [{{\"text\": string, \"codes\": [string]}}], \"summary\": string, \"recommendation\": \"land\" | \"send_back\" | null, \"confidence\": \"high\" | \"low\" | null, \"reason_category\": \"scope\" | \"discard\" | null}}\n\
         reasons lists each finding (empty for pass); summary is one or two sentences; recommendation, confidence and reason_category are for a concern only (null for pass and revise).\n",
        run_id = run.id(),
        task_id = task.id(),
        docs = REVIEW_DOCS_CHECK,
        concern = CONCERN_RECOMMENDATION,
        codes = reason_codes_section(review_reason::REVIEW_CODES),
        rules = REVIEW_RULES,
    );
    text.push_str(&subagents);
    fit.finish(text)
}

/// How a review checks the documents on the changed behavior
/// (ADR-t1428-1 decision 5, ADR-t1942-2 decision 4): against the diff and
/// the summary, never taking a document's diff alone as proof, in both
/// directions: stale ones as before, and what the diff adds to a document
/// that copies the code or carries history. A name missing from a document
/// alone is not drift. The verdict's shape and the codes stay as they are.
pub const REVIEW_DOCS_CHECK: &str = "Read the documents on the behavior the diff changes (named by the task's description or context or by the summary, or found as you read) against the diff and the summary. A document's diff alone does not show the change is right; check the summary's reason for leaving a document as it is like any other claim. Report as docs_drift a stale document, or a changed flow, boundary, invariant or promise it misses, whether or not the task named it; a name missing from a document alone is not stale. Also report text the diff adds to a document when that text copies the code (lists of fields, flags, defaults, function or test names) or carries history (task numbers, what was before).\n";

/// Where the review finds the repository's rules: its instructions in the
/// worktree, which a Claude review that loads no setting sources no longer
/// gets as its memory (ADR-t1470-1 decision 2).
pub(super) const REVIEW_RULES: &str = "The repository's rules are in its instructions at the root of the worktree (AGENTS.md and CLAUDE.md, whichever it has) and the documents they name: read the instructions, and of what they name the rules that bear on this change, and judge the diff by them.";

/// How a review recommends what to do with its `concern`, and which
/// judgements it leaves to a person (ADR-t451-1 decisions 1 and 3).
pub(super) const CONCERN_RECOMMENDATION: &str = "For a concern, also recommend what to do, and how sure you are:\n\
- recommendation: land (the run may land as it is: the findings are minor or acceptable within the task and its acceptance) or send_back (the worker should fix the findings in the same run).\n\
- confidence: high when the task, its acceptance, the repository's decision records and rules settle it and you would bet on a person choosing the same; low when you hesitate, the material is not enough, or a person could reasonably choose otherwise.\n\
- reason_category: scope when landing would accept a departure from the acceptance criteria, a recorded decision of the repository or the goal's decisions (or meeting them would need a change of scope); discard when the judgement is whether to cancel the task or throw the work away; null otherwise.\n\
The runtime applies a high recommendation whose reason_category is null without asking: send_back goes to the worker's session like a revise, and land lands the run after its usual checks. Anything else (low, scope, discard) goes to a person with your recommendation. Leave scope and discard to the person rather than deciding them; when in doubt, say low.\n\n";

/// How a review job labels each finding (ADR-t947-1): the codes, their
/// definitions heaviest first, and how the primary one is chosen.
pub(super) fn reason_codes_section(codes: &[(&str, &str)]) -> String {
    format!(
        "Give each finding one or more reason codes, the main one first: when a finding fits two, the one whose fix needs the heavier judgment (the list is heaviest first); other when none fits, explained in the text. \
         Put first the finding that decides the verdict; a note that would not stop it is never first. \
         The codes are recorded for statistics only and change nothing of how the verdict is applied. The codes:\n{}\n\n",
        review_reason::prompt_lines(codes)
    )
}

/// What the headless triage (the recovery job) may do beyond what needs no
/// permission: read files only (ADR-t1063-1 decision 2).
pub const TRIAGE_ACCESS: JobAccess = JobAccess::ReadFiles;

/// What the headless review of a run may do: read files only, since the
/// live worker session owns the worktree (ADR-0027, ADR-t1063-1 decision 2).
pub const REVIEW_ACCESS: JobAccess = JobAccess::ReadFiles;

/// Bytes of each log, receipt and screen the triage prompt carries (their
/// ends).
pub(super) const TRIAGE_TAIL_BYTES: usize = 3000;

/// Logs of a run directory the triage reads: the latest integrate
/// attempt's `integrate-<attempt>-verify-N.log` (see [`integrate_logs`]) and
/// `verify-N.log`, at most this many.
pub(super) const TRIAGE_LOGS: usize = 8;

/// What the recovery job of a run that ended `failed` or `interrupted`
/// reads beyond a live session's material (ADR-0047 decision 39, as the
/// triage read it before): the run's error, receipt, verification logs,
/// final screen and events, the task's earlier runs with their rounds, and
/// the rules the runtime holds `retry` and `resume` to. `dir` is where the
/// run's files are.
pub fn ended_run_material(
    files: &dyn RunFiles,
    detail: &TaskDetail,
    run: &TaskRun,
    resumes: crate::domain::resume::ResumeCount,
    config: ResumeConfig,
    dir: &Path,
) -> String {
    let failures = detail
        .runs
        .iter()
        .filter(|r| matches!(r.status(), RunStatus::Failed | RunStatus::Interrupted))
        .count();
    let read = |path: &Path| {
        files
            .read(path)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    let mut material = format!(
        "Last error of the run:\n{}\n\n",
        or_none(run.last_error().unwrap_or_default())
    );
    let receipt = run.receipt_path().map(Path::new).and_then(read);
    material.push_str(&format!(
        "Receipt ({}):\n{}\n",
        run.receipt_path().unwrap_or("none"),
        fenced(
            "json",
            or_none(tail(
                receipt.as_deref().unwrap_or_default().trim(),
                TRIAGE_TAIL_BYTES
            ))
        )
    ));
    let (latest, earlier) = integrate_logs(files, dir);
    let mut logs = latest;
    // `verify-N.log` is what validation wrote before ADR-0023.
    let mut validation: Vec<PathBuf> = log_names(files, dir)
        .into_iter()
        .filter(|(name, _)| name.starts_with("verify-") && name.ends_with(".log"))
        .map(|(_, path)| path)
        .collect();
    validation.sort();
    logs.extend(validation);
    logs.truncate(TRIAGE_LOGS);
    if !earlier.is_empty() {
        material.push_str(&format!(
            "Logs of earlier integrate attempts (not shown): {}\n",
            earlier
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if logs.is_empty() {
        material.push_str("Verification logs: none\n");
    }
    for log in &logs {
        let text = read(log).unwrap_or_default();
        material.push_str(&format!(
            "Verification log {} (end):\n{}\n",
            log.display(),
            fenced("text", or_none(tail(text.trim(), TRIAGE_TAIL_BYTES)))
        ));
    }
    let screen = read(&dir.join("terminal-final.txt"));
    material.push_str(&format!(
        "Final screen of the session (end of terminal-final.txt; a headless session has none, its turns follow):\n{}\n",
        fenced(
            "text",
            or_none(tail(
                screen.as_deref().unwrap_or_default().trim(),
                TRIAGE_TAIL_BYTES
            ))
        )
    ));
    // A headless session's turns (ADR-t813-1): how each ended, and why the
    // runtime stopped one.
    let turns: Vec<String> = detail
        .events
        .iter()
        .filter(|e| {
            e.run_id.as_ref() == Some(run.id())
                && e.kind == crate::domain::event_kind::TURN_FINISHED
        })
        .map(|e| e.payload.to_string())
        .collect();
    if !turns.is_empty() {
        let turns = &turns[turns.len().saturating_sub(5)..];
        material.push_str(&format!(
            "Turns of the headless session (the last {}; `stopped` says why the runtime stopped one):\n{}\n",
            turns.len(),
            fenced("json", &turns.join("\n"))
        ));
    }
    let events: Vec<Value> = detail
        .events
        .iter()
        .filter(|e| e.run_id.as_ref() == Some(run.id()))
        .map(crate::application::health::compact_event)
        .collect();
    let events = &events[events.len().saturating_sub(40)..];
    material.push_str(&format!(
        "Events of the run (the last {}):\n{}\n",
        events.len(),
        fenced(
            "json",
            &events
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        )
    ));
    let earlier: Vec<String> = detail
        .runs
        .iter()
        .filter(|r| *r.id() != *run.id())
        .map(|r| {
            let actions: Vec<String> = detail
                .events
                .iter()
                .filter(|e| {
                    e.run_id.as_ref() == Some(r.id()) && e.kind == event_kind::TRIAGE_FINISHED
                })
                .map(|e| format!("{}", e.payload.get("action").unwrap_or(&Value::Null)))
                .collect();
            format!(
                "- run {} {}: {}{}",
                r.id(),
                r.status().as_str(),
                or_none(tail(r.last_error().unwrap_or_default(), 300)),
                if actions.is_empty() {
                    String::new()
                } else {
                    format!(" (recovered: {})", actions.join(", "))
                }
            )
        })
        .collect();
    material.push_str(&format!(
        "\nEarlier runs of the task:\n{}\n\n",
        if earlier.is_empty() {
            "none".to_owned()
        } else {
            earlier.join("\n")
        }
    ));
    let retry_rule = if failures >= TRIAGE_RETRY_FAILURES {
        format!(
            "This task has {failures} failed or interrupted runs, this one included: do not choose retry (the runtime escalates it)."
        )
    } else {
        format!(
            "This task has {failures} failed or interrupted run(s), this one included; from {TRIAGE_RETRY_FAILURES} on, the runtime does not apply retry."
        )
    };
    let resume_rule = if resumes.exhausted(config) {
        format!(
            "The run was resumed {} time(s) ({} of at most {MAX_RESUME_ATTEMPTS} counted, {} of at most {} conflict-only attempts with the conflict precheck's requests, {} of at most {} after a signal from outside killed its session) and its resumes are used up: do not choose resume.",
            resumes.total(),
            resumes.counted,
            resumes.conflict_attempts(),
            config.conflict_only_limit,
            resumes.kill_only,
            crate::domain::resume::KILL_ONLY_RESUME_LIMIT
        )
    } else {
        format!(
            "The run was resumed {} time(s) ({} of at most {MAX_RESUME_ATTEMPTS} counted, {} of at most {} after a signal from outside killed its session); resume needs the run's worktree.",
            resumes.total(),
            resumes.counted,
            resumes.kill_only,
            crate::domain::resume::KILL_ONLY_RESUME_LIMIT
        )
    };
    material.push_str(&format!("Rules: {retry_rule} {resume_rule}\n"));
    material
}

/// What the runtime read for a recovery job (ADR-0047 decision 39), at the
/// time of the alert.
pub struct RecoveryMaterial<'a> {
    pub alert: RecoveryAlert,
    /// For a run that ended: [`ended_run_material`]. `None` for a live
    /// session.
    pub ended: Option<String>,
    /// The alert's own facts (`recovery_requested`'s payload).
    pub facts: &'a Value,
    pub workspace: &'a str,
    /// The screen's excerpt, or why it could not be read.
    pub screen: &'a str,
    /// The processes that belong to the run (see
    /// [`crate::domain::recovery::run_processes`]), or why they could not
    /// be listed.
    pub processes: std::result::Result<Vec<ProcessInfo>, String>,
    pub git_status: &'a str,
    pub head: &'a str,
    /// The receipt's `commit`, when there is a receipt.
    pub receipt_commit: Option<&'a str>,
    /// The run's earlier recovery verdicts and automatic repairs.
    pub history: &'a [Value],
    /// The actions that apply to this alert.
    pub allowed: &'a [&'a str],
    /// The binary the supervisor runs and its replacements (task 1633).
    pub binary: &'a BinaryFacts,
}

/// What each allowed action does, for the recovery prompt of a worker
/// run: an instruction is the prompt of the session's next turn (every
/// worker run is headless since task 1437).
pub(super) fn recovery_action_help(action: &str) -> &'static str {
    match action {
        "send_instruction" => {
            "{\"action\": \"send_instruction\", \"instruction\": string}: send this instruction once, as the prompt of the session's next turn (a resume of the same session; its previous turn has ended), for example to rerun the tests in the foreground, commit and write the receipt."
        }
        "stop_processes" => {
            "{\"action\": \"stop_processes\", \"pids\": [pid, ...]}: stop these processes (SIGTERM, then SIGKILL after a grace). Only processes listed below as the run's own are allowed; any other pid makes the whole verdict an escalation. Use it for a background process the session waits for that will not end by itself (an orphan holding a pipe, a hung test). Never the session's own wrapper or agent."
        }
        "wait" => {
            "{\"action\": \"wait\", \"recheck_after_secs\": n}: do nothing now; if the alert still holds after n seconds (at most 3600), another recovery job runs. The work looks healthy and is only slow, or what holds it passes by itself."
        }
        "retry" => {
            "{\"action\": \"retry\"}: make the task ready again for a new run from the current main. Only for a run whose branch holds no commit of its own (nothing is thrown away); a run with commits needs retry_inherit or a person. Use it when the failure came from the environment (the machine slept, a process was killed, the session never started, an outage)."
        }
        "retry_inherit" => {
            "{\"action\": \"retry_inherit\"}: make the task ready again for a new run that carries this run's branch over onto the current main. Only for a run whose branch has commits, and once per task."
        }
        "resume" => {
            "{\"action\": \"resume\", \"instruction\": string}: send the run back to a session of its own in its worktree, with the instruction (what to do: fix the failing test, commit and rewrite the receipt, rebase) added to the resolution request. Only while its resumes are not used up."
        }
        _ => "",
    }
}

/// The recovery actions that answered the retired interactive session's
/// screen, its dialogs and its `/exit`: never offered, and refused in a
/// verdict (task 1437).
pub(crate) const HEADLESS_NEVER: [&str; 2] = ["answer_known_dialog", "close_and_proceed"];

/// The bytes the whole recovery job prompt takes at most, the language's
/// instruction included (task 1571, ADR-t1566-1 decision 4): in production
/// it took 19,773 bytes at the median, 27,553 at p90 and 66,814 at most,
/// of which a headless session's last turns took 46,559. The limit is the
/// sum of the sections' limits below (81,000 bytes) and the instructions
/// (about 6,000) with room for the language's instruction.
pub const RECOVERY_PROMPT_LIMIT: usize = 96_000;

/// The bytes of the task's title, description (2,166 at most in
/// production), acceptance and verification commands.
pub const RECOVERY_TITLE_BYTES: usize = 1_000;
pub const RECOVERY_DESCRIPTION_BYTES: usize = 6_000;
pub const RECOVERY_ACCEPTANCE_BYTES: usize = 4_000;
pub const RECOVERY_VERIFY_BYTES: usize = 2_000;

/// The bytes of the alert's facts (6,806 at most in production).
pub const RECOVERY_FACTS_BYTES: usize = 12_000;

/// The bytes of the screen's end, or of a headless session's last turns
/// (46,559 at most in production), newest first.
pub const RECOVERY_SCREEN_BYTES: usize = 16_000;

/// The bytes of the run's processes and of its worktree's `git status`.
pub const RECOVERY_PROCESSES_BYTES: usize = 4_000;
pub const RECOVERY_STATUS_BYTES: usize = 4_000;

/// The bytes of the earlier verdicts, repairs and `task_edited` (6,748 at
/// most in production), newest first, and of one of them.
pub const RECOVERY_HISTORY_BYTES: usize = 8_000;
pub const RECOVERY_HISTORY_ITEM_BYTES: usize = 2_000;

/// The bytes of what a run that ended adds ([`ended_run_material`]: its
/// error, receipt, logs, final screen, turns and events).
pub const RECOVERY_ENDED_BYTES: usize = 24_000;

/// The bytes of the supervisor's build identifier and its commit (task
/// 1633): one line of about 100 bytes.
pub const RECOVERY_BINARY_BYTES: usize = 500;

/// The bytes of the task's dependencies with their landing commit and
/// whether the build holds it (task 1633), and of one of them: a line takes
/// about 100 bytes (a full commit), or up to the item's limit with why it
/// cannot be told, so about 30 lines fit. Those the build does not hold (or
/// cannot be told to) are kept first.
pub const RECOVERY_DEPENDENCIES_BYTES: usize = 3_000;
pub const RECOVERY_DEPENDENCY_BYTES: usize = 400;

/// The count and bytes of the binary's replacements since the run was
/// claimed (`update_installed`, `supervisor_handed_off`; task 1633), newest
/// first, and the bytes of one: a line takes about 250 bytes (two build
/// identifiers and a commit).
pub const RECOVERY_REPLACEMENTS: usize = 10;
pub const RECOVERY_REPLACEMENTS_BYTES: usize = 3_000;
pub const RECOVERY_REPLACEMENT_BYTES: usize = 400;

/// The file in the run directory that holds a recovery job's whole
/// [`BinaryFacts`] (task 1633), next to its `prompt.txt`.
pub fn recovery_binary_file(alert: RecoveryAlert, attempt: usize) -> String {
    format!("recovery-{}-{attempt}.binary.json", alert.as_str())
}

/// A dependency of the task and its landing, for the recovery job (task
/// 1633).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DependencyLanding {
    pub task: TaskId,
    /// The commit its latest `run_integrated` landed; `None` when it has
    /// not landed.
    pub landed: Option<String>,
    /// Whether the supervisor's build holds `landed`; `None` when that
    /// cannot be told, with `why`.
    pub held: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// What the recovery job reads of the binary the supervisor runs (task
/// 1633): its build identifier and commit now, whether that build holds
/// the landing of each of the task's dependencies, and the binary's
/// replacements since the run was claimed, oldest first. A run that failed
/// because the fixed binary did not yet hold a landing can then be retried
/// once it does, without a person.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BinaryFacts {
    /// The supervisor's build identifier (`dagq::VERSION` of its binary).
    pub version: String,
    /// The commit `version` names ([`crate::application::update::build_commit`]).
    pub commit: Option<String>,
    pub dependencies: Vec<DependencyLanding>,
    /// `update_installed` (of the binary, not of the plugin only) and the
    /// run's `supervisor_handed_off`: `event_id`, `kind`, `at`,
    /// `previous_version`, `version` and, for `update_installed`, `commit`.
    pub replacements: Vec<Value>,
}

/// The [`BinaryFacts`] of `run` for a supervisor of build `version`:
/// `events` are the task's (the run's among them), `updates` the queue's
/// `update_*` events, `landings` each dependency with the commit it landed,
/// and `holds(commit, build)` whether `build` contains `commit`.
pub fn binary_facts(
    version: &str,
    run: &TaskRun,
    events: &[RunEvent],
    updates: &[RunEvent],
    landings: &[(TaskId, Option<String>)],
    holds: &dyn Fn(&str, &str) -> Result<bool>,
) -> BinaryFacts {
    let commit = crate::application::update::build_commit(version).map(str::to_owned);
    let own = |e: &&RunEvent| e.run_id.as_ref() == Some(run.id());
    // The run's first event is its claim.
    let claimed = events.iter().filter(own).map(|e| e.id).min();
    let after_claim = |e: &RunEvent| match claimed {
        Some(id) => e.id > id,
        None => e.created_at.as_str() >= run.created_at(),
    };
    let mut replaced: Vec<&RunEvent> = updates
        .iter()
        .filter(|e| {
            e.kind == crate::domain::UPDATE_INSTALLED
                && !crate::domain::stats::updates::plugin_only(e)
                && after_claim(e)
        })
        .chain(
            events
                .iter()
                .filter(own)
                .filter(|e| e.kind == event_kind::EventKind::SupervisorHandedOff.as_str()),
        )
        .collect();
    replaced.sort_by_key(|e| e.id);
    replaced.dedup_by_key(|e| e.id);
    let replacements = replaced
        .into_iter()
        .map(|e| {
            let mut line = serde_json::json!({
                "event_id": e.id,
                "kind": e.kind,
                "at": e.created_at,
                "previous_version": e.payload.get("previous_version").cloned().unwrap_or(Value::Null),
                "version": e.payload.get("version").cloned().unwrap_or(Value::Null),
            });
            if let Some(commit) = e.payload.get("commit") {
                line["commit"] = commit.clone();
            }
            line
        })
        .collect();
    let dependencies = landings
        .iter()
        .map(|(task, landed)| {
            let (held, why) = match (landed, &commit) {
                (None, _) => (None, Some("it has not landed".to_owned())),
                (Some(_), None) => (
                    None,
                    Some("the build names no commit (a release or an unknown build)".to_owned()),
                ),
                (Some(landed), Some(build)) => match holds(landed, build) {
                    Ok(held) => (Some(held), None),
                    Err(error) => (None, Some(format!("{error:#}").trim().to_owned())),
                },
            };
            DependencyLanding {
                task: *task,
                landed: landed.clone(),
                held,
                why,
            }
        })
        .collect();
    BinaryFacts {
        version: version.to_owned(),
        commit,
        dependencies,
        replacements,
    }
}

/// How the recovery job judges a run the fixed binary failed (task 1633),
/// for the jobs that may retry.
pub(super) const RECOVERY_BINARY_RULE: &str = "A run that failed because the fixed binary did not yet hold a task's landing (its error, receipt or a verification gate says the build identifier's commit does not contain that landing) is fixed by the binary's replacement: when the current build above holds that landing (held true, or the replacements above show a build at or after it), choose retry (retry_inherit for a run with commits of its own) with confidence high and do not ask a person, unless the rules above say the runtime does not apply retry (then recommend it in an escalation); while the current build does not hold it, a retry fails the same way, so choose wait or escalate.";

/// The recovery prompt's part on the supervisor's binary (task 1633): the
/// build now (section `binary`), the dependencies' landings (section
/// `dependencies`, those the build does not hold or cannot be told to
/// first) and the replacements since the claim (section `replacements`,
/// newest first), each within its limit, what is left out counted and
/// named with the run directory's file that holds it all
/// ([`recovery_binary_file`]); for a job that may retry, the rule on a run
/// the binary failed. It comes right after the alert's facts, so a prompt
/// cut in its middle at the whole limit keeps it.
pub(super) fn binary_sections(
    fit: &mut Fit,
    material: &RecoveryMaterial<'_>,
    attempt: usize,
) -> Result<String> {
    let facts = material.binary;
    let read = format!(
        "read {} of the run directory",
        recovery_binary_file(material.alert, attempt)
    );
    let build = fit.text(
        "binary",
        &format!(
            "{} ({})",
            facts.version,
            facts.commit.as_deref().map_or_else(
                || "it names no commit: a release or an unknown build".to_owned(),
                |commit| format!("commit {commit}")
            )
        ),
        RECOVERY_BINARY_BYTES,
        Keep::Start,
        &read,
    );
    fit.section("binary", &build);
    let deps = &facts.dependencies;
    let values = deps
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    let unheld = |index: &usize| deps[*index].held != Some(true);
    let order: Vec<usize> = (0..deps.len())
        .filter(unheld)
        .chain((0..deps.len()).filter(|index| !unheld(index)))
        .collect();
    let (kept, left_out) = fit.lines(
        "dependencies",
        &values,
        order,
        (
            usize::MAX,
            RECOVERY_DEPENDENCIES_BYTES,
            RECOVERY_DEPENDENCY_BYTES,
        ),
        &read,
    );
    let mut dependencies = if deps.is_empty() {
        "none".to_owned()
    } else {
        kept.join("\n")
    };
    if !left_out.is_empty() {
        let ids: Vec<String> = left_out
            .iter()
            .map(|&index| format!("task {}", deps[index].task))
            .collect();
        dependencies.push('\n');
        dependencies.push_str(&left_out_note(
            "of them (those the build does not hold were chosen first)",
            &ids,
            &read,
        ));
    }
    fit.section("dependencies", &dependencies);
    let lines = &facts.replacements;
    let (kept, left_out) = fit.lines(
        "replacements",
        lines,
        (0..lines.len()).rev(),
        (
            RECOVERY_REPLACEMENTS,
            RECOVERY_REPLACEMENTS_BYTES,
            RECOVERY_REPLACEMENT_BYTES,
        ),
        &read,
    );
    let mut replacements = if kept.is_empty() {
        "none".to_owned()
    } else {
        kept.join("\n")
    };
    if !left_out.is_empty() {
        let ids: Vec<String> = left_out
            .iter()
            .map(|&index| {
                lines[index]
                    .get("event_id")
                    .map_or_else(|| format!("#{}", index + 1), Value::to_string)
            })
            .collect();
        replacements.push('\n');
        replacements.push_str(&left_out_note("of them (the oldest)", &ids, &read));
    }
    fit.section("replacements", &replacements);
    let rule = if material
        .allowed
        .iter()
        .any(|action| matches!(*action, "retry" | "retry_inherit"))
    {
        format!("{RECOVERY_BINARY_RULE}\n")
    } else {
        String::new()
    };
    Ok(format!(
        "Binary of the supervisor (the fixed binary the runtime runs) now: {build}\n\
         Dependencies of the task, the commit each landed and whether the current build holds it (held null when that cannot be told, with why), one JSON line each:\n{}\n\n\
         Replacements of the binary since the run was claimed (update_installed and supervisor_handed_off, oldest first; version is the build it put in place), one JSON line each:\n{}\n\n\
         {rule}\n",
        dependencies.trim_end(),
        replacements.trim_end(),
    ))
}

/// What the recovery job of an alert is asked (ADR-0047 decisions 39 and
/// 40): the alert, the task, the screen, the run's processes, the
/// worktree's state and the run's earlier repairs, for a run that ended
/// also its error, receipt, logs and events, then the allowed actions and
/// the verdict schema. Each section is held to its limit and the whole to
/// [`RECOVERY_PROMPT_LIMIT`]; what is cut names the file in the run
/// directory or the worktree it is in, or says the job cannot read it
/// (the job reads files only, ADR-t1566-1 decision 3; task 1571).
pub fn recovery_prompt(
    task: &Task,
    run: &TaskRun,
    attempt: usize,
    material: &RecoveryMaterial<'_>,
) -> Result<FittedPrompt> {
    let mut fit = Fit::new(RECOVERY_PROMPT_LIMIT);
    let claimed = "the task as the run claimed it is in prompt.txt of the run directory, and a later edit is a task_edited below";
    let (title, title_cut) =
        prompt_fit::cut_part(task.title(), RECOVERY_TITLE_BYTES, Keep::Start, claimed);
    let (description, description_cut) = fit.required_part(
        "task",
        or_none(task.description()),
        RECOVERY_DESCRIPTION_BYTES,
        claimed,
    );
    let (acceptance, acceptance_cut) = fit.required_part(
        "task",
        or_none(task.acceptance()),
        RECOVERY_ACCEPTANCE_BYTES,
        claimed,
    );
    let (verification, verification_cut) = fit.required_part(
        "task",
        &task.verification_commands().join("\n"),
        RECOVERY_VERIFY_BYTES,
        claimed,
    );
    // The task is one item, counted once whichever of its parts was cut.
    fit.omit(
        "task",
        usize::from(title_cut || description_cut || acceptance_cut || verification_cut),
    );
    let pretty = serde_json::to_string_pretty(material.facts)?;
    // Cut as a value, so that what is kept stays JSON.
    let facts = if pretty.len() > RECOVERY_FACTS_BYTES - 16 {
        fenced(
            "json",
            &fit.json(
                "facts",
                material.facts,
                RECOVERY_FACTS_BYTES - 16,
                NOT_READABLE,
            ),
        )
    } else {
        fenced("json", &pretty)
    };
    fit.section("facts", &facts);
    let binary = binary_sections(&mut fit, material, attempt)?;
    // Every worker run is headless since task 1437, a historical
    // interactive one too: its material is its last turns, newest first.
    let screen = fenced(
        "text",
        &fit.text(
            "screen",
            or_none(material.screen.trim()),
            RECOVERY_SCREEN_BYTES,
            Keep::Start,
            "each turn's whole output is in turns/turn-NNNNNN.jsonl of the run directory",
        ),
    );
    fit.section("screen", &screen);
    let ended = material.ended.as_deref().map_or_else(String::new, |ended| {
        let ended = fit.text(
            "ended",
            ended,
            RECOVERY_ENDED_BYTES,
            Keep::Start,
            "the run directory has the receipt, the verification logs (integrate-*-verify-*.log, verify-*.log), terminal-final.txt and turns/",
        );
        fit.section("ended", &ended);
        ended
    });
    let processes = match &material.processes {
        Ok(processes) if processes.is_empty() => "none".to_owned(),
        Ok(processes) => processes
            .iter()
            .map(|p| {
                format!(
                    "- pid {} (parent {}, running {}s, cpu {}, cwd {}): {}",
                    p.pid,
                    p.ppid,
                    p.elapsed_secs,
                    p.cpu_ms.map_or_else(
                        || "unknown".to_owned(),
                        |ms| format!("{}.{:03}s", ms / 1000, ms % 1000)
                    ),
                    p.cwd.as_deref().unwrap_or("unknown"),
                    tail(&p.command, 300)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Err(error) => format!("(the processes could not be listed: {error})"),
    };
    let processes = fit.text(
        "processes",
        &processes,
        RECOVERY_PROCESSES_BYTES,
        Keep::Start,
        NOT_READABLE,
    );
    fit.section("processes", &processes);
    let status = fenced(
        "text",
        &fit.text(
            "git_status",
            or_none(material.git_status.trim()),
            RECOVERY_STATUS_BYTES,
            Keep::Start,
            &format!(
                "read the files of the worktree at {}",
                run.worktree_path().unwrap_or("none")
            ),
        ),
    );
    fit.section("git_status", &status);
    let (kept, left_out) = fit.lines(
        "history",
        material.history,
        (0..material.history.len()).rev(),
        (
            usize::MAX,
            RECOVERY_HISTORY_BYTES,
            RECOVERY_HISTORY_ITEM_BYTES,
        ),
        NOT_READABLE,
    );
    let mut history = if kept.is_empty() {
        "none".to_owned()
    } else {
        kept.join("\n")
    };
    if !left_out.is_empty() {
        let ids: Vec<String> = left_out
            .iter()
            .map(|&index| {
                material.history[index]
                    .get("id")
                    .map_or_else(|| format!("#{}", index + 1), Value::to_string)
            })
            .collect();
        history.push('\n');
        history.push_str(&left_out_note("of them (the oldest)", &ids, NOT_READABLE));
    }
    fit.section("history", &history);
    // A headless session has no screen, dialog or `/exit`: the actions
    // that answer them are never offered (ADR-t813-1 decision 9).
    let actions = material
        .allowed
        .iter()
        .filter(|action| !HEADLESS_NEVER.contains(action))
        .map(|action| format!("- {}", recovery_action_help(action)))
        .collect::<Vec<_>>()
        .join("\n");
    let text = format!(
        "You are dagq's recovery job (attempt {attempt}) for run {run_id} of task {task_id} ({title}), {state}. The supervisor raised the alert {alert}: {meaning}\n\
         Decide whether the runtime can repair it with one of the allowed actions below, or whether a person has to look.\n\
         Read only: the material below, and the files it names if you need more (the worktree is {worktree}, the run directory {run_dir}). Do not change any file and do not run commands; the runtime applies your verdict. The material is held to limits: what was cut says how much and in which file it is, or that you cannot read it.\n\n\
         Task description:\n{description}\n\n\
         Acceptance criteria:\n{acceptance}\n\n\
         Current task verification commands (use these, including after a person's correction):\n{verification}\n\n\
         Alert facts:\n{facts}\n\n\
         {binary}\
         {ended}\
         Last turns of the headless session (it has no screen):\n{screen}\n\n\
         Processes of the run (working directory in the worktree, or under the session's wrapper; the wrapper and the agent themselves are not listed):\n{processes}\n\n\
         Worktree: HEAD {head}, receipt commit {receipt}, git status:\n{status}\n\n\
         Earlier recovery verdicts, repairs, and task_edited events:\n{history}\n\n\
         Allowed actions:\n{actions}\n\
         Not allowed, ever: cancelling the task, retrying a run that has commits, editing the task, landing without review, writing to main, pushing, deleting branches or worktrees, touching anything outside this run's worktree and workspace, writing the queue database, typing into the session (a headless session takes no keys; an instruction goes as its next turn). If the repair needs any of these, escalate.\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"repair\" | \"escalate\", \"confidence\": \"high\" | \"low\", \"diagnosis\": string, \"actions\": [action, ...], \"question\": string, \"options\": [string, ...], \"reason_category\": \"recovery_failed\" | \"discard\" | \"scope\"}}\n\
         diagnosis says what you found in one or two sentences. repair needs at least one action and is applied only with confidence high; with confidence low, or with escalate, a person is asked, with your actions as the recommendation, question as the question and options added to theirs. If a broken verification command caused an ended run to fail, offer `edit the task's --verify, then retry_inherit` to the person: user or inbox can edit only verification commands after the run ends; you cannot edit. Once the task_edited event and current commands show the correction, retry_inherit carries the committed work forward and integration uses the corrected commands. reason_category says why a person is needed: recovery_failed when you cannot repair it or are not sure, discard when the work would be thrown away, scope when it needs a permission you do not have.\n",
        run_id = run.id(),
        task_id = task.id(),
        state = match &material.ended {
            Some(_) => format!("which ended {}; its session is gone", run.status().as_str()),
            None => format!(
                "whose session in workspace {} is still running",
                material.workspace
            ),
        },
        alert = material.alert.as_str(),
        meaning = match material.alert {
            RecoveryAlert::Failed =>
                "the run failed (its receipt said failed, its validation or landing failed, or its session exited without finishing).",
            RecoveryAlert::Interrupted =>
                "the run's session died and the supervisor recovered the run as interrupted.",
            RecoveryAlert::ResumeExhausted =>
                "the run still needed a session after its last resume, so the supervisor stopped resuming it.",
            RecoveryAlert::Stalled =>
                "the headless session does not get on: its turns end with neither a receipt nor an open question after the supervisor's nudges, each sent as the next turn (reason turn_without_receipt), or a turn was refused permissions too often to get on (reason permission_denied). The alert facts say which. The session has no screen, input box or dialog: an instruction is the prompt of its next turn, and resume parks the run for a session of its own.",
            RecoveryAlert::IdleProcess =>
                "processes of the run (listed in the alert facts with how long they have used almost no CPU time) are alive but have not made progress for longer than the threshold; the session may be waiting for them.",
            RecoveryAlert::StuckExit
            | RecoveryAlert::PromptWaiting
            | RecoveryAlert::LongBackground =>
                "an alert of the retired interactive worker run, which the supervisor no longer raises.",
        },
        worktree = run.worktree_path().unwrap_or("none"),
        run_dir = run.run_dir().unwrap_or("none"),
        verification = fenced("sh", &verification),
        head = material.head,
        receipt = material.receipt_commit.unwrap_or("(no receipt)"),
    );
    for counted in [&title, &description, &acceptance, &verification] {
        fit.section("task", counted);
    }
    Ok(fit.finish(text))
}
