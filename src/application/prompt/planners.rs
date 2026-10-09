//! The initial prompts of the inbox session and of the runtime's planners
//! (planning).

use super::*;

/// The initial prompt of the inbox session that `up` opens in the
/// `[<repo>]inbox` workspace (ADR-0022): it relays each open ask to a person
/// and writes the person's answer back, deciding nothing itself. Every
/// other attention is the inbox's too (ADR-0024 decision 6): it reports it
/// and does only what the person says.
pub fn inbox_prompt(db: &Path) -> Result<String> {
    Ok(format!(
        "You are the inbox of the dagq queue at {db}: you relay its asks and attention to a person and never decide anything yourself.\n\
         Start with `dagq status --role inbox` and follow the dagq-inbox skill of the dagq plugin: run `dagq watch --role inbox --after <cursor>` in the background, wake when it returns and watch again from the cursor it returns.\n\
         On ask_opened, read the ask with `dagq asks --open --role inbox`, show the person its question and options (use AskUserQuestion when it is available), then write the person's answer with `dagq answer ID --text '<answer>'`. Report any other attention (an answered ask, a stopped supervisor, a failed review or triage) to the person and do only what they say, as the skill describes.\n\
         Never open the queue database directly; use the dagq CLI only.\n",
        db = crate::application::path_text(db)?,
    ))
}

/// Where a planner takes a task's verification commands, declared paths
/// and required evidence from. The runtime has no such rules of its own:
/// they are the repository's, and a repository without an AGENTS.md still
/// shows them in its CLAUDE.md, README, CI and build configuration. `ask`
/// is the last step, when none of them settles it: for a planner the
/// runtime opened, a `planner_question` ask.
pub(crate) fn repository_rules(ask: &str) -> String {
    format!(
        "Take a task's verification commands (`--verify`), declared paths (`--paths`) and required evidence (`--evidence`) from the repository's instructions and the documents and rules they name, in this order: its AGENTS.md; without one, its CLAUDE.md; without either, what its README, CI configuration and build configuration show; when none of them settles it, {ask}."
    )
}

/// The last step of [`repository_rules`] for a planner the runtime opens:
/// it decides from the rest of its material as the Basic policy says
/// (ADR-t451-1 decision 5), and asks the inbox, since no person watches
/// it, only what that leaves to a person or to a low confidence.
pub(super) const RUNTIME_PLANNER_ASK: &str = "decide them yourself from the source, the decisions the repository records and a person's precedents, and ask a person with a `planner_question` ask as below only when that material cannot settle them and the decision is a person's (`scope` or `discard`) or your confidence in it is low";

/// The CLI that reads the queue's record (ADR-0044 decision 22), named by
/// the prompts of the planners the runtime opens and of the plan review so
/// they read evidence from the events rather than from prose.
pub const RECORD_READING: &str = "To see what happened, read the record rather than prose: \
`dagq events --full --task ID` gives a task's events with their run_id and whole payload, narrowed by `--run ID`, `--goal ID`, `--kind KIND` (repeatable), `--since TIME` and `--until TIME` (UTC, YYYY-MM-DD or YYYY-MM-DDTHH:MM:SSZ); \
without `--kind` it lists attention events only, so add `--all` for every kind; it gives the oldest 100 first, so page on with `--after <cursor>` or narrow with `--since`. \
`dagq timeline RUN` gives a run's events oldest first with each gap and its reason (idle, waiting_ask, background, after_receipt, ...).";

/// The read-only dagq commands the prompts of the planners the runtime
/// opens name to read what their limits left out, as they write them
/// (`ID` for a number): each is one a planner's role may run (task 1571,
/// ADR-t1566-1 decision 3).
pub const PLANNER_READS: &[&str] = &[
    "dagq show ID --full",
    "dagq goal show ID --full",
    "dagq proposal show ID",
    "dagq findings ID --full",
    "dagq requests ID",
    "dagq asks --all",
    "dagq events --full --task ID --kind plan_review_finished",
    "dagq events --full --task ID --kind integration_receipt",
    "dagq events --full --run ID --kind integration_receipt",
    "dagq events --full --all --after ID --limit 1",
];

/// The bytes of one text field of a goal in a planner's prompt (its
/// description or acceptance; its constraints take half), of the lines of
/// its tasks, and of all the goals of one prompt.
pub const PLANNER_GOAL_TEXT_BYTES: usize = 4_000;
pub const PLANNER_GOAL_TASKS_BYTES: usize = 4_000;
pub const PLANNER_GOALS_BYTES: usize = 16_000;

/// The bytes of the asks a planner's prompt lists (the newest first) and of
/// one question or answer in them.
pub const PLANNER_ASKS_BYTES: usize = 8_000;
pub const PLANNER_ASK_TEXT_BYTES: usize = 1_000;

/// The bytes of the answered question a planner carries for the planner
/// before it, its question and its answer each.
pub const PLANNER_ANSWER_BYTES: usize = 3_000;

/// Characters of a task's title in a list of a planner's prompt.
pub(super) const PLANNER_TITLE_CHARS: usize = 200;

/// `title` cut to [`PLANNER_TITLE_CHARS`] characters.
pub(super) fn short_title(title: &str) -> String {
    crate::application::health::truncate(title, PLANNER_TITLE_CHARS)
        .unwrap_or_else(|| title.to_owned())
}

/// The lines of `tasks`, the newest first within `bytes`, kept in their
/// order, with a note of how many were left out and how to read them, and
/// that many: the caller counts them, as lines or as a cut of the item
/// they are a part of.
pub(super) fn planner_task_lines(tasks: &[GoalTask], bytes: usize, read: &str) -> (String, usize) {
    if tasks.is_empty() {
        return ("(none)".to_owned(), 0);
    }
    let lines: Vec<String> = tasks
        .iter()
        .map(|t| {
            format!(
                "- task {} ({}): {}",
                t.id,
                t.status.as_str(),
                short_title(&t.title)
            )
        })
        .collect();
    let sizes: Vec<usize> = lines.iter().map(String::len).collect();
    let kept = prompt_fit::pick(&sizes, (0..lines.len()).rev(), usize::MAX, bytes);
    let left_out: Vec<String> = tasks
        .iter()
        .zip(&kept)
        .filter(|(_, kept)| !**kept)
        .map(|(t, _)| t.id.to_string())
        .collect();
    let mut text = lines
        .into_iter()
        .zip(&kept)
        .filter(|(_, kept)| **kept)
        .map(|(line, _)| line)
        .collect::<Vec<_>>()
        .join("\n");
    if !left_out.is_empty() {
        text.push('\n');
        text.push_str(&left_out_note("tasks (the oldest)", &left_out, read));
    }
    (text, left_out.len())
}

/// The section of a goal in a planner's prompt: `heading`, its text
/// fields, its doc when `doc`, and the lines of `tasks` (headed
/// `tasks_label`), each held to its limit, and whether any was cut (a
/// task line left out among them): [`planner_goals`] counts the goal, not
/// its parts.
pub(super) fn planner_goal(
    heading: &str,
    goal: &Goal,
    (tasks, tasks_label): (&[GoalTask], &str),
    doc: bool,
) -> (String, bool) {
    let read = format!("read it whole with `dagq goal show {} --full`", goal.id());
    let (description, description_cut) = prompt_fit::cut_part(
        or_none(goal.description()),
        PLANNER_GOAL_TEXT_BYTES,
        Keep::Start,
        &read,
    );
    let (acceptance, acceptance_cut) = prompt_fit::cut_part(
        or_none(goal.acceptance()),
        PLANNER_GOAL_TEXT_BYTES,
        Keep::Start,
        &read,
    );
    let (constraints, constraints_cut) = prompt_fit::cut_part(
        or_none(goal.constraints()),
        PLANNER_GOAL_TEXT_BYTES / 2,
        Keep::Start,
        &read,
    );
    let (tasks, tasks_left_out) = planner_task_lines(
        tasks,
        PLANNER_GOAL_TASKS_BYTES,
        &format!("`dagq goal show {} --full`", goal.id()),
    );
    let cut = description_cut || acceptance_cut || constraints_cut || tasks_left_out > 0;
    let text = format!(
        "{heading}\n\n{description}\n\nAcceptance:\n{acceptance}\n\nConstraints:\n{constraints}\n\n{doc}{tasks_label}:\n{tasks}\n",
        doc = if doc {
            format!("Doc: {}\n\n", goal.doc().unwrap_or("(none)"))
        } else {
            String::new()
        },
    );
    (text, cut)
}

/// The goals' sections of a planner's prompt within
/// [`PLANNER_GOALS_BYTES`] in their order, with a note of the goals left
/// out. Each goal counts once in `goals`: left out, or kept and cut
/// ([`planner_goal`]).
pub(super) fn planner_goals(fit: &mut Fit, sections: Vec<(GoalId, (String, bool))>) -> String {
    let sizes: Vec<usize> = sections.iter().map(|(_, (text, _))| text.len()).collect();
    let cuts: Vec<bool> = sections.iter().map(|(_, (_, cut))| *cut).collect();
    let kept = prompt_fit::pick(&sizes, 0..sections.len(), usize::MAX, PLANNER_GOALS_BYTES);
    fit.picked("goals", &kept, &cuts);
    let left_out: Vec<String> = sections
        .iter()
        .zip(&kept)
        .filter(|(_, kept)| !**kept)
        .map(|((id, _), _)| id.to_string())
        .collect();
    let mut text: String = sections
        .into_iter()
        .zip(&kept)
        .filter(|(_, kept)| **kept)
        .map(|((_, (text, _)), _)| text)
        .collect();
    if !left_out.is_empty() {
        text.push_str(&format!(
            "\n## Goals left out\n\n{}",
            left_out_note("goals", &left_out, "`dagq goal show ID --full`")
        ));
    }
    fit.section("goals", &text);
    text
}

/// The lines of `asks`, the newest first within [`PLANNER_ASKS_BYTES`] and
/// kept in their order, each question and answer cut, with a note of the
/// asks left out. `kind` adds each ask's kind. Each ask counts once in
/// `asks`: left out, or kept and cut.
pub(super) fn planner_asks(fit: &mut Fit, asks: &[Ask], kind: bool) -> String {
    let read = "read it whole with `dagq asks --all`";
    let mut cuts = Vec::new();
    let lines: Vec<String> = asks
        .iter()
        .map(|ask| {
            let (question, question_cut) =
                prompt_fit::cut_part(&ask.question, PLANNER_ASK_TEXT_BYTES, Keep::Start, read);
            let (answer, answer_cut) = prompt_fit::cut_part(
                ask.answer.as_deref().unwrap_or("(none yet)"),
                PLANNER_ASK_TEXT_BYTES,
                Keep::Start,
                read,
            );
            cuts.push(question_cut || answer_cut);
            format!(
                "- ask {aid}{kind}: {question}\n  answer: {answer}\n",
                aid = ask.id,
                kind = if kind {
                    format!(" ({})", ask.kind.as_str())
                } else {
                    String::new()
                },
                question = question.replace('\n', "\n  "),
            )
        })
        .collect();
    let sizes: Vec<usize> = lines.iter().map(String::len).collect();
    let kept = prompt_fit::pick(
        &sizes,
        (0..lines.len()).rev(),
        usize::MAX,
        PLANNER_ASKS_BYTES,
    );
    fit.picked("asks", &kept, &cuts);
    let left_out: Vec<String> = asks
        .iter()
        .zip(&kept)
        .filter(|(_, kept)| !**kept)
        .map(|(ask, _)| ask.id.to_string())
        .collect();
    let mut text: String = lines
        .into_iter()
        .zip(&kept)
        .filter(|(_, kept)| **kept)
        .map(|(line, _)| line)
        .collect();
    if !left_out.is_empty() {
        text.push_str(&left_out_note(
            "asks (the oldest)",
            &left_out,
            "`dagq asks --all`",
        ));
    }
    fit.section("asks", &text);
    text
}

/// The question and the answer of the ask a planner carries for the
/// planner before it, each held to [`PLANNER_ANSWER_BYTES`]: the ask is
/// one item of `answer`, counted once whichever of the two was cut.
pub(super) fn planner_answer(fit: &mut Fit, answer: &Ask) -> (String, String) {
    let read = format!("read ask {} whole with `dagq asks --all`", answer.id);
    let (question, question_cut) =
        prompt_fit::cut_part(&answer.question, PLANNER_ANSWER_BYTES, Keep::Start, &read);
    let (text, text_cut) = prompt_fit::cut_part(
        answer.answer.as_deref().unwrap_or_default(),
        PLANNER_ANSWER_BYTES,
        Keep::Start,
        &read,
    );
    fit.omit("answer", usize::from(question_cut || text_cut));
    (question, text)
}

/// What a planner the runtime opens is told to leave before it stops at
/// its `planner_question` (ADR-t1704-1 decisions 1 and 3): the runtime
/// ends a planner whose only wait is the answer, and the next planner
/// goes on from the queue's records, not from this session.
pub const BEFORE_YOU_STOP_AT_A_QUESTION: &str = "Before you stop at a planner_question, decide everything you can without its answer (the other drafts, the fixes the answer does not touch) and leave the rest in the queue: save your edits of the drafts with `dagq edit`, and record with `dagq note` (`--task ID`, or `--goal ID`) what you decided, what you were about to decide, what is left open and why, and what the answer will decide. While only a person's answer is left, the runtime ends this session to free its place; the answer then goes to a new planner with your question, your notes and your drafts in its prompt, which goes on from them.";

/// The bytes of what the planner before a planner the runtime opens left
/// for it (ADR-t1704-1 decision 3): its notes, the newest first, and the
/// lines of the drafts it created or edited; and of one note.
pub const PLANNER_HANDOVER_BYTES: usize = 8_000;
pub const PLANNER_HANDOVER_NOTE_BYTES: usize = 2_000;

/// What a planner the runtime opens carries from the planner before it
/// (ADR-t1704-1 decision 3): the answered `planner_question`s that
/// planner stopped at, and what it left in the queue.
#[derive(Debug, Clone, Copy, Default)]
pub struct Carried<'a> {
    pub answers: &'a [Ask],
    pub handover: Option<&'a PlannerHandover>,
}

/// The section of what the planner before this one left as it ended for a
/// person's answer alone (ADR-t1704-1 decision 3): its notes within
/// [`PLANNER_HANDOVER_BYTES`] (the newest first, kept in their order, each
/// cut to [`PLANNER_HANDOVER_NOTE_BYTES`]), then the lines of its drafts
/// within a quarter of it, with how to read what was left out. Each note
/// counts once in `handover` (left out, or kept and cut), and each draft
/// line left out once.
pub(super) fn handover_section(fit: &mut Fit, handover: &PlannerHandover) -> String {
    let read_note = |id: i64| {
        format!(
            "read it whole with `dagq events --full --all --after {} --limit 1`",
            id - 1
        )
    };
    let (lines, cuts): (Vec<String>, Vec<bool>) = handover
        .notes
        .iter()
        .map(|(id, text)| {
            let (text, cut) = prompt_fit::cut_part(
                text,
                PLANNER_HANDOVER_NOTE_BYTES,
                Keep::Start,
                &read_note(*id),
            );
            (
                format!("- (event {id}) {}\n", text.replace('\n', "\n  ")),
                cut,
            )
        })
        .unzip();
    let sizes: Vec<usize> = lines.iter().map(String::len).collect();
    let kept = prompt_fit::pick(
        &sizes,
        (0..lines.len()).rev(),
        usize::MAX,
        PLANNER_HANDOVER_BYTES - PLANNER_HANDOVER_BYTES / 4,
    );
    fit.picked("handover", &kept, &cuts);
    let left_out: Vec<String> = handover
        .notes
        .iter()
        .zip(&kept)
        .filter(|(_, kept)| !**kept)
        .map(|((id, _), _)| id.to_string())
        .collect();
    let mut notes: String = lines
        .into_iter()
        .zip(&kept)
        .filter(|(_, kept)| **kept)
        .map(|(line, _)| line)
        .collect();
    if handover.notes.is_empty() {
        notes.push_str("(none)\n");
    }
    if !left_out.is_empty() {
        notes.push_str(&left_out_note(
            "notes (the oldest; event IDs)",
            &left_out,
            "`dagq events --full --all --after ID --limit 1` with ID one less than each",
        ));
    }
    let (drafts, drafts_left_out) = planner_task_lines(
        &handover.drafts,
        PLANNER_HANDOVER_BYTES / 4,
        "`dagq show ID --full` for each",
    );
    fit.omit("handover", drafts_left_out);
    let text = format!(
        "\n## What planner {planner} before you left\n\nIt stopped at the question above, and the runtime ended it while only the person's answer was left; its session is not resumed. Go on from these records and the queue as it is now, and do not redo what it decided.\n\nIts notes, oldest first:\n{notes}\nThe drafts it created or edited, still drafts (as it saved them; read each with `dagq show ID --full`):\n{drafts}\n",
        planner = handover.planner_id,
    );
    fit.section("handover", &text);
    text
}

/// The bytes the whole prompt of a planner the runtime opens for a revise
/// takes at most, the language's instruction included (task 1571): in
/// production it took 2,849 bytes at the median, 5,084 at p90 and 10,976
/// at most, mostly the reasons and the tasks' lines. The 32,000 that held
/// those grows by the answers it carries and what the planner before it
/// left ([`RUNTIME_PLANNER_ANSWERS_BYTES`], [`PLANNER_HANDOVER_BYTES`],
/// ADR-t1704-1), so that the sections' limits and the instructions add up
/// to no more than the whole.
pub const RUNTIME_PLANNER_PROMPT_LIMIT: usize =
    32_000 + RUNTIME_PLANNER_ANSWERS_BYTES + PLANNER_HANDOVER_BYTES;

/// The bytes of the answered questions a planner opened for a revise
/// carries (the oldest first, each question and answer cut to
/// [`PLANNER_ANSWER_BYTES`]); the rest are named with `dagq asks --all`.
pub const RUNTIME_PLANNER_ANSWERS_BYTES: usize = 6_000;

/// The bytes of the reasons plan review gave and of one reason, and of
/// the lines of the proposal's tasks.
pub const RUNTIME_PLANNER_REASONS_BYTES: usize = 12_000;
pub const RUNTIME_PLANNER_REASON_BYTES: usize = 4_000;
pub const RUNTIME_PLANNER_TASKS_BYTES: usize = 8_000;

/// The initial prompt of a planner the runtime opens for a proposal plan
/// review sent back while its own planner was closed (ADR-0041 decision
/// 12): the proposal, its tasks, and the reasons to fix. No person watches
/// the session, so what needs one goes to the inbox as an ask (decision 13).
/// The reasons and the tasks are held to their limits and the whole to
/// [`RUNTIME_PLANNER_PROMPT_LIMIT`] (task 1571, ADR-t1566-1).
pub fn runtime_planner_prompt(
    db: &Path,
    proposal: ProposalId,
    tasks: &[Task],
    reasons: &[String],
    review_anchor: Option<TaskId>,
    carried: Carried<'_>,
) -> Result<FittedPrompt> {
    let mut fit = Fit::new(RUNTIME_PLANNER_PROMPT_LIMIT);
    let goal_tasks: Vec<GoalTask> = tasks
        .iter()
        .map(|task| GoalTask {
            id: task.id(),
            title: task.title().to_owned(),
            status: task.status(),
            priority: task.priority(),
            priority_source: task.priority_source(),
            priority_by: task.priority_by(),
        })
        .collect();
    let (tasks, tasks_left_out) = planner_task_lines(
        &goal_tasks,
        RUNTIME_PLANNER_TASKS_BYTES,
        &format!("`dagq proposal show {proposal}`"),
    );
    fit.omit("tasks", tasks_left_out);
    fit.section("tasks", &tasks);
    let reason_read = review_anchor.map_or_else(
        || "the plan review anchor is unknown, so there is no known way to read the full reasons".to_owned(),
        |anchor| format!("read it whole with `dagq events --full --task {anchor} --kind plan_review_finished`"),
    );
    let (lines, cuts): (Vec<String>, Vec<bool>) = reasons
        .iter()
        .map(|reason| {
            let (text, cut) = prompt_fit::cut_part(
                reason,
                RUNTIME_PLANNER_REASON_BYTES,
                Keep::Start,
                &reason_read,
            );
            (format!("- {text}"), cut)
        })
        .unzip();
    let sizes: Vec<usize> = lines.iter().map(String::len).collect();
    let kept = prompt_fit::pick(
        &sizes,
        0..lines.len(),
        usize::MAX,
        RUNTIME_PLANNER_REASONS_BYTES,
    );
    fit.picked("reasons", &kept, &cuts);
    let left_out = kept.iter().filter(|kept| !**kept).count();
    let mut reasons = if lines.is_empty() {
        "(none given)".to_owned()
    } else {
        lines
            .into_iter()
            .zip(&kept)
            .filter(|(_, kept)| **kept)
            .map(|(line, _)| line)
            .collect::<Vec<_>>()
            .join("\n")
    };
    if left_out > 0 {
        reasons.push_str(&format!(
            "\n({left_out} more reasons left out by this section's limit. {reason_read}.)"
        ));
    }
    fit.section("reasons", &reasons);
    // The answers the planner before it stopped at, with what it left
    // (ADR-t1704-1 decisions 3 and 4).
    let read = "`dagq asks --all`";
    let (lines, cuts): (Vec<String>, Vec<bool>) = carried
        .answers
        .iter()
        .map(|answer| {
            let (question, question_cut) =
                prompt_fit::cut_part(&answer.question, PLANNER_ANSWER_BYTES, Keep::Start, read);
            let (text, text_cut) = prompt_fit::cut_part(
                answer.answer.as_deref().unwrap_or_default(),
                PLANNER_ANSWER_BYTES,
                Keep::Start,
                read,
            );
            (
                format!(
                    "\nThe planner before you asked a person (ask {aid}) about task {task} of this proposal while it fixed it, and was ended while it waited:\n{question}\n\nanswer to ask {aid}: {text}\n",
                    aid = answer.id,
                    task = answer.task_id.map_or("?".to_owned(), |t| t.to_string()),
                ),
                question_cut || text_cut,
            )
        })
        .unzip();
    let sizes: Vec<usize> = lines.iter().map(String::len).collect();
    let kept = prompt_fit::pick(
        &sizes,
        0..lines.len(),
        usize::MAX,
        RUNTIME_PLANNER_ANSWERS_BYTES,
    );
    fit.picked("answer", &kept, &cuts);
    let left_out: Vec<String> = carried
        .answers
        .iter()
        .zip(&kept)
        .filter(|(_, kept)| !**kept)
        .map(|(ask, _)| ask.id.to_string())
        .collect();
    let mut carried_text: String = lines
        .into_iter()
        .zip(&kept)
        .filter(|(_, kept)| **kept)
        .map(|(line, _)| line)
        .collect();
    if !left_out.is_empty() {
        carried_text.push_str(&left_out_note("answered asks", &left_out, read));
    }
    if !carried.answers.is_empty() {
        carried_text.push_str("\nApply these answers together with the reasons above.\n");
    }
    fit.section("answer", &carried_text);
    if let Some(handover) = carried.handover {
        carried_text.push_str(&handover_section(&mut fit, handover));
    }
    Ok(fit.finish(format!(
        "You are a planner the dagq runtime opened for proposal {proposal} of the queue at {db}; no person watches this session.\n\
         Plan review sent the proposal back. Its reasons:\n{reasons}\nFull reasons: {reason_read}.\n\
         Its tasks:\n{tasks}\n\
         Follow the dagq-planner skill of the dagq plugin: read the proposal with `dagq proposal show {proposal}` and each task with `dagq show ID`, fix what the reasons point at, and submit it again with `dagq submit --proposal {proposal}`.\n\
         {RECORD_READING}\n\
         {rules}\n\
         A fix that changes the plan's intent (acceptance, scope, the relation to the goal) needs a person: raise it to the inbox with `dagq ask --task ID --kind planner_question --because scope` as the skill describes, stop, and continue from the answer, which arrives as the prompt of your next turn. {BEFORE_YOU_STOP_AT_A_QUESTION}\n\
         Never open the queue database directly; use the dagq CLI only.\n{carried_text}",
        db = crate::application::path_text(db)?,
        rules = repository_rules(RUNTIME_PLANNER_ASK),
    )))
}

/// What the initial prompt of a planner the runtime opens for a bundle of
/// drafts (ADR-0041 decision 16, bundled by ADR-t807-1) shows it: the
/// drafts and where they came from, the source task and its landed receipt
/// for follow_ups (once), the goals and their other tasks, and the answer
/// of a `planner_question` it carries when the planner that asked is gone.
pub struct DraftPlannerMaterial<'a> {
    pub db: &'a Path,
    /// What made the drafts one bundle.
    pub key: &'a BundleKey,
    /// The drafts, oldest first, each with which planner of the runtime's
    /// this is for it (1-based).
    pub members: &'a [(DraftTarget, usize)],
    /// The task whose run's receipt proposed the follow_ups.
    pub source: Option<&'a Task>,
    /// That run's landed receipt.
    pub receipt: Option<&'a Value>,
    /// The drafts' goals, each with whether it is closed and its tasks
    /// other than the bundle's.
    pub goals: &'a [(Goal, bool, Vec<GoalTask>)],
    pub answer: Option<&'a Ask>,
    /// What the planner that asked it left as it ended for the answer
    /// alone (ADR-t1704-1 decision 3).
    pub handover: Option<&'a PlannerHandover>,
    /// The last decision about each draft whose revisit time came
    /// (ADR-t1540-1).
    pub revisits: &'a [RevisitHistory],
}

/// The last decision about a draft whose revisit time came (ADR-t1540-1):
/// its `planner_question` asks and its notes, oldest first.
#[derive(Debug, Clone)]
pub struct RevisitHistory {
    pub task: TaskId,
    pub asks: Vec<Ask>,
    pub notes: Vec<String>,
}

/// The bytes of the last decisions about the drafts whose revisit time
/// came, and of one question or note in them: a draft kept with a time
/// has a question or two and a note or two (draft 1537: one question, one
/// note), and the sections' limits with the instructions stay within
/// [`DRAFT_PLANNER_PROMPT_LIMIT`].
pub const DRAFT_REVISIT_BYTES: usize = 8_000;
pub const DRAFT_REVISIT_ITEM_BYTES: usize = 2_000;

/// The section of the drafts whose revisit time came (ADR-t1540-1): the
/// time, who set it and why, and the last decision about each (its
/// `planner_question` asks with their recommendation and answer, and its
/// notes), the newest first; empty when none came. The section counts
/// once in `revisit` when any question or note in it or the section as a
/// whole was cut.
pub(super) fn revisit_section(fit: &mut Fit, material: &DraftPlannerMaterial<'_>) -> String {
    let mut out = String::new();
    let mut cut = false;
    let mut cut_text = |text: &str, max: usize, read: &str| {
        let (text, was_cut) = prompt_fit::cut_part(text, max, Keep::Start, read);
        cut |= was_cut;
        text
    };
    for (target, _) in material.members {
        let Some(revisit) = &target.revisit else {
            continue;
        };
        let id = target.task.id();
        let read = format!("read it whole with `dagq show {id} --full`");
        let mut text = format!(
            "\n## Revisit of draft {id}\n\nIts revisit time came: {at} (set by {by} {actor}). What to look at then: {note}\nIt was kept as a draft (or added by a person) to be decided at this time; the time is used now. The last decision about it, the newest first:\n",
            at = revisit.revisit_at_utc,
            by = revisit.set_by,
            actor = revisit.set_by_id,
            note = revisit.note.as_deref().unwrap_or("(none)"),
        );
        let history = material.revisits.iter().find(|h| h.task == id);
        let asks: Vec<&Ask> = history.map_or_else(Vec::new, |h| h.asks.iter().rev().collect());
        let notes: Vec<&String> = history.map_or_else(Vec::new, |h| h.notes.iter().rev().collect());
        text.push_str("\n### Its planner_question asks\n\n");
        if asks.is_empty() {
            text.push_str("(none)\n");
        }
        for ask in asks {
            text.push_str(&format!(
                "- ask {aid}: {question}\n  Recommended: {recommend} ({confidence}). Answer: {answer}\n",
                aid = ask.id,
                question = cut_text(&ask.question, DRAFT_REVISIT_ITEM_BYTES, &read),
                recommend = ask.recommendation.as_deref().unwrap_or("(none)"),
                confidence = ask
                    .confidence
                    .map_or("no confidence", |confidence| confidence.as_str()),
                answer = ask.answer.as_deref().unwrap_or("(not answered)"),
            ));
        }
        text.push_str("\n### Its notes\n\n");
        if notes.is_empty() {
            text.push_str("(none)\n");
        }
        for note in notes {
            text.push_str(&format!(
                "- {}\n",
                cut_text(note, DRAFT_REVISIT_ITEM_BYTES, &read)
            ));
        }
        out.push_str(&text);
    }
    if out.is_empty() {
        return out;
    }
    let out = cut_text(
        &out,
        DRAFT_REVISIT_BYTES,
        "`dagq show ID --full` for each draft",
    );
    fit.omit("revisit", usize::from(cut));
    fit.section("revisit", &out);
    out
}

/// The line of a follow_up draft's section that shows the category its
/// worker gave it (ADR-t947-3), with what the category means when it is
/// one of the list; empty for a draft of another origin.
pub(super) fn follow_up_category_line(target: &DraftTarget) -> String {
    if target.origin != DraftOrigin::FollowUp {
        return String::new();
    }
    let category = crate::domain::follow_up_category(&target.material);
    let meaning = crate::domain::FOLLOW_UP_CATEGORIES
        .iter()
        .find(|(code, _)| *code == category)
        .map_or_else(
            || {
                if category == crate::domain::UNLABELED_CATEGORY {
                    " (the worker gave none)".to_owned()
                } else {
                    " (not one of the runtime's categories)".to_owned()
                }
            },
            |(_, meaning)| format!(": {meaning}"),
        );
    format!(
        "\nCategory (the worker's; keep it as it is, and judge the draft on its merits): {category}{meaning}\n"
    )
}

/// The line of a follow_up draft's section that shows the worker's
/// membership proposal (ADR-t1504-2 decision 11) as the material keeps
/// it; empty for a draft of another origin.
pub(super) fn follow_up_proposal_line(target: &DraftTarget) -> Result<String> {
    if target.origin != DraftOrigin::FollowUp {
        return Ok(String::new());
    }
    let proposal = crate::domain::follow_up_membership_proposal(&target.material);
    let material = &target.material;
    let source_goal = match material["source_goal_id"].as_i64() {
        Some(goal) => format!(
            "goal {goal} ({}, {})",
            material["source_goal_state"].as_str().unwrap_or("unknown"),
            material["source_goal_provenance"]
                .as_str()
                .unwrap_or("unknown")
        ),
        None if material["source_goal_state"].as_str() == Some("none") => "none".to_owned(),
        None => "unknown".to_owned(),
    };
    Ok(format!(
        "Source goal (at registration; judge against its acceptance with `dagq goal show <id>`, not the current goal's): {source_goal}\n\
         Membership proposal (the worker's; where you start, not a judgement): {}\n",
        if proposal.is_null() {
            "(none)".to_owned()
        } else {
            serde_json::to_string(&proposal)?
        }
    ))
}

/// The step of a follow_up draft's planner before it adopts, drops or asks
/// (ADR-t1504-1 decisions 1 to 3 and 6, ADR-t1504-2 decisions 1 and 6):
/// judge from the worker's proposal whether the source goal's acceptance
/// can be met without the draft and record it, membership apart from
/// adoption and priority, another goal found before one is made, and no
/// acceptance weakened to leave a draft out.
pub(super) fn follow_up_membership_step(t: &str) -> String {
    format!(
        "For a follow_up draft whose source goal is not none, first judge where it belongs, apart from whether it is worth doing and from its priority. Start from the worker's membership proposal and decide the meaning yourself: can the source goal's acceptance be met without this draft? When it cannot, it is required and belongs to the source goal; when it can, it is out_of_scope and belongs to another goal: look for a fitting existing goal with `dagq search` first, make one only when none fits, and never park it in an unrelated large goal. Record the judgement before you adopt, drop or ask: `dagq judge-follow-up {t} --classification <required|out_of_scope|undecided> --acceptance-item '<the acceptance item>' --reason '<why that acceptance can or cannot be met without it>' --evidence '<a receipt, commit, document section or task>'`, with `--destination-goal <goal>` for out_of_scope, and `--source-goal <goal>` when the source goal is unknown. Read its earlier judgements with `dagq show {t}` (membership_judgements): one that still holds needs no new row; to change a required or out_of_scope one, record the other with `--corrects <its id>`; it never goes back to undecided. A draft you drop (a duplicate, already done, not worth doing) may skip the record when it would need a new goal, as a canceled follow-up never holds a goal open. When the acceptance, the receipt, the source and the recorded decisions cannot settle it, record undecided with why, ask as step 3 says with the membership question in it, and on the answer record required or out_of_scope before you do what it says. Moving a draft to another goal neither adopts it nor raises its priority. Never weaken a goal's acceptance to leave a follow_up out: that changes the goal's intent, so ask a person (`--because scope`).\n"
    )
}

/// The Basic policy of the dagq-planner skill (ADR-t451-1 decision 5) as
/// the planners the runtime opens for a draft or a finding read it: what
/// they can recommend they decide themselves and record why; only what
/// they cannot settle goes to a person.
pub(super) const DECIDE_YOURSELF: &str = "decide what you can recommend yourself and go on, asking no one, and leave why in the record (a task's `--context`, a `note`, a `--reason`); raise to a person, with your recommendation and its confidence, only what step Ask below names.";

/// The bytes the whole prompt of a planner the runtime opens for a bundle
/// of drafts takes at most, the language's instruction included (task
/// 1571): in production it took 18,021 bytes at the median, 26,352 at p90
/// and 38,241 at most (planner 828: the source task's receipt's summary
/// 8,423, the goal 7,398, the goal's other tasks 4,303).
/// The 80,000 that held that grows by what a planner ended for a person's
/// answer left ([`PLANNER_HANDOVER_BYTES`], ADR-t1704-1).
pub const DRAFT_PLANNER_PROMPT_LIMIT: usize = 80_000 + PLANNER_HANDOVER_BYTES;

/// The bytes of the drafts' sections, and of one draft's title,
/// description and context.
pub const DRAFT_MEMBERS_BYTES: usize = 20_000;
pub const DRAFT_TITLE_BYTES: usize = 1_000;
pub const DRAFT_DESCRIPTION_BYTES: usize = 6_000;
pub const DRAFT_CONTEXT_BYTES: usize = 4_000;

/// The bytes of where the drafts came from (the source task, its landed
/// receipt, plan review's reason, the goal review's findings), and in it
/// of the source task's description and acceptance each, of the receipt's
/// summary (8,423 at most in production) and of its follow_ups.
pub const DRAFT_ORIGIN_BYTES: usize = 20_000;
pub const DRAFT_SOURCE_TEXT_BYTES: usize = 3_000;
pub const DRAFT_RECEIPT_SUMMARY_BYTES: usize = 12_000;
pub const DRAFT_RECEIPT_FOLLOW_UPS_BYTES: usize = 6_000;

/// The initial prompt of a planner the runtime opens for a bundle of drafts
/// the runtime or a job registered (ADR-0041 decision 16, ADR-t807-1): the
/// material, and the three things it may do with each draft — submit it
/// completed (adopt), cancel it with a note (drop), or ask the inbox a
/// `planner_question` and apply the answer it gets as its next turn — and,
/// for a bundle of more than one, what to weigh between its drafts. Each
/// section is held to its limit and the whole to
/// [`DRAFT_PLANNER_PROMPT_LIMIT`]; what is left out is counted and named
/// with the read-only dagq command that reads it (task 1571, ADR-t1566-1).
pub fn draft_planner_prompt(material: &DraftPlannerMaterial<'_>) -> Result<FittedPrompt> {
    let mut fit = Fit::new(DRAFT_PLANNER_PROMPT_LIMIT);
    let (first, _) = material
        .members
        .first()
        .context("a bundle of drafts has at least one")?;
    let origin = first.origin;
    let single = material.members.len() == 1;
    let ids: Vec<String> = material
        .members
        .iter()
        .map(|(target, _)| target.task.id().to_string())
        .collect();
    // The ID the commands name: the draft's, or a placeholder for each.
    let t = if single {
        ids[0].clone()
    } else {
        "ID".to_owned()
    };
    let whence = match origin {
        DraftOrigin::Reopened => {
            "plan review took it back from ready and the proposal it was reopened into was withdrawn"
        }
        DraftOrigin::FollowUp | DraftOrigin::GoalGap => "the runtime or a job registered it",
        DraftOrigin::Revisit => "a person added it and gave it a revisit time, which came",
    };
    let mut out = if single {
        format!(
            "You are a planner the dagq runtime opened for draft task {id} of the queue at {db}; no person watches this session. The {origin} draft is not ready: {whence}, and you decide what becomes of it (planner {attempt} of at most {max} the runtime opens for it).\n",
            id = ids[0],
            db = crate::application::path_text(material.db)?,
            origin = origin.as_str(),
            attempt = material.members[0].1,
            max = MAX_DRAFT_PLANNERS,
        )
    } else {
        format!(
            "You are a planner the dagq runtime opened for draft tasks {list} of the queue at {db}; no person watches this session. The {n} {origin} drafts are one bundle: the same piece of work made them ({kind} {value}). None is ready: {whence}, and you decide what becomes of each (the runtime opens at most {max} planners for a draft).\n",
            list = ids.join(", "),
            n = ids.len(),
            db = crate::application::path_text(material.db)?,
            origin = origin.as_str(),
            kind = material.key.kind.as_str(),
            value = material.key.value,
            max = MAX_DRAFT_PLANNERS,
        )
    };
    let (mut drafts, mut cuts) = (Vec::new(), Vec::new());
    for (target, attempt) in material.members {
        let task = &target.task;
        let heading = if single {
            "## The draft".to_owned()
        } else {
            format!("## Draft {} (planner {attempt} for it)", task.id())
        };
        let read = format!("read it whole with `dagq show {} --full`", task.id());
        // A draft counts once in `drafts`, however many of its fields
        // were cut.
        let (title, title_cut) =
            prompt_fit::cut_part(task.title(), DRAFT_TITLE_BYTES, Keep::Start, &read);
        let (description, description_cut) = fit.required_part(
            "drafts",
            or_none(task.description()),
            DRAFT_DESCRIPTION_BYTES,
            &read,
        );
        let (context, context_cut) = prompt_fit::cut_part(
            or_none(task.context()),
            DRAFT_CONTEXT_BYTES,
            Keep::Start,
            &read,
        );
        cuts.push(title_cut || description_cut || context_cut);
        drafts.push(format!(
            "\n{heading}\n\nTask {id}: {title}\n{category}{proposal}\n### Description\n\n{description}\n\n### Context\n\n{context}\n",
            id = task.id(),
            category = follow_up_category_line(target),
            proposal = follow_up_proposal_line(target)?,
        ));
    }
    // The drafts in their order (the oldest first) within their limit.
    let sizes: Vec<usize> = drafts.iter().map(String::len).collect();
    let kept = prompt_fit::pick(&sizes, 0..drafts.len(), usize::MAX, DRAFT_MEMBERS_BYTES);
    fit.picked("drafts", &kept, &cuts);
    let left_out: Vec<String> = material
        .members
        .iter()
        .zip(&kept)
        .filter(|(_, kept)| !**kept)
        .map(|((target, _), _)| target.task.id().to_string())
        .collect();
    let mut members: String = drafts
        .into_iter()
        .zip(&kept)
        .filter(|(_, kept)| **kept)
        .map(|(text, _)| text)
        .collect();
    if !left_out.is_empty() {
        members.push_str(&format!(
            "\n## Drafts left out\n\n{}",
            left_out_note("drafts", &left_out, "`dagq show ID --full` for each")
        ));
    }
    fit.section("drafts", &members);
    out.push_str(&members);
    // Origin is one composite item, even when several inner fields and
    // the section as a whole are cut.
    let mut origin_cut = false;
    let mut cut_origin_text =
        |text: &str, max: usize, read: &str| match prompt_fit::cut(text, max, Keep::Start, read) {
            Some((cut, _)) => {
                origin_cut = true;
                cut
            }
            None => text.to_owned(),
        };
    let mut whence_text = format!("\n## Where it came from: {}\n\n", origin.as_str());
    let out_before_origin = std::mem::take(&mut out);
    // Only a follow_up's drafts came from a run's receipt.
    let receipt_read = first.material["source_run_id"]
        .as_str()
        .map_or_else(String::new, |run| {
            format!(
                "read it whole with `dagq events --full --run {run} --kind integration_receipt`"
            )
        });
    match origin {
        DraftOrigin::FollowUp => {
            out.push_str(&format!(
                "The receipt of run {run} of task {source} proposed {what} as a follow_up: work its worker found outside that task.\n",
                run = first.material["source_run_id"].as_str().unwrap_or("(unknown)"),
                source = first.material["source_task_id"],
                what = if single {
                    "it".to_owned()
                } else {
                    format!(
                        "them ({})",
                        material
                            .members
                            .iter()
                            .map(|(t, _)| format!("task {} is its follow_up {}", t.task.id(), t.material["index"]))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                },
            ));
            if let Some(source) = material.source {
                out.push_str(&format!(
                    "\n### Source task {sid}: {title} ({status})\n\n{description}\n\nAcceptance:\n{acceptance}\n\nVerification: {verify}\nPaths: {paths}\nEvidence: {evidence}\n",
                    sid = source.id(),
                    title = short_title(source.title()),
                    status = source.status().as_str(),
                    description = cut_origin_text(or_none(source.description()), DRAFT_SOURCE_TEXT_BYTES, &format!("read it whole with `dagq show {} --full`", source.id())),
                    acceptance = cut_origin_text(or_none(source.acceptance()), DRAFT_SOURCE_TEXT_BYTES, &format!("read it whole with `dagq show {} --full`", source.id())),
                    verify = list_or_none(source.verification_commands()),
                    paths = list_or_none(source.paths()),
                    evidence = list_or_none(
                        &source
                            .required_evidence()
                            .iter()
                            .map(|check| check.as_str().to_owned())
                            .collect::<Vec<_>>()
                    ),
                ));
            }
            if let Some(receipt) = material.receipt {
                out.push_str(&format!(
                    "\n### The landed receipt\n\nSummary:\n{summary}\n\nIts follow_ups:\n{follow_ups}",
                    summary = cut_origin_text(
                        or_none(receipt["summary"].as_str().unwrap_or_default()),
                        DRAFT_RECEIPT_SUMMARY_BYTES,
                        &receipt_read,
                    ),
                    follow_ups = fenced(
                        "json",
                        &cut_origin_text(
                            &serde_json::to_string_pretty(&receipt["follow_ups"])?,
                            DRAFT_RECEIPT_FOLLOW_UPS_BYTES,
                            &receipt_read,
                        ),
                    ),
                ));
            }
        }
        DraftOrigin::Reopened => {
            for (target, _) in material.members {
                out.push_str(&format!(
                    "{it} was a ready task. The plan review of proposal {reviewed} found that it has to change and reopened it into proposal {proposal}, which was then withdrawn, so it returned to draft. The reason plan review gave:\n\n{reason}\n",
                    it = if single {
                        "It".to_owned()
                    } else {
                        format!("Task {}", target.task.id())
                    },
                    reviewed = target.material["reviewed_proposal_id"],
                    proposal = target.material["proposal_id"],
                    reason = or_none(target.material["reason"].as_str().unwrap_or_default()),
                ));
            }
        }
        DraftOrigin::Revisit => {
            out.push_str(
                "A person added it (the runtime and the jobs did not): no origin of theirs to read. It came to you because its revisit time came (below).\n",
            );
        }
        DraftOrigin::GoalGap => {
            out.push_str(if single {
                "A job that judged the goal below against its acceptance found this gap. Its findings:\n"
            } else {
                "A job that judged the goal below against its acceptance found these gaps. The findings of each draft:\n"
            });
            for (target, _) in material.members {
                if !single {
                    out.push_str(&format!("\nTask {}:\n", target.task.id()));
                }
                out.push_str(&fenced(
                    "json",
                    &serde_json::to_string_pretty(&target.material)?,
                ));
            }
        }
    }
    // Everything about where the drafts came from, within its limit.
    let origin_text = std::mem::replace(&mut out, out_before_origin);
    whence_text.push_str(&origin_text);
    let whence_text = cut_origin_text(
        &whence_text,
        DRAFT_ORIGIN_BYTES,
        &if receipt_read.is_empty() {
            "`dagq show ID --full` for the drafts".to_owned()
        } else {
            format!(
                "`dagq show ID --full` for the drafts and their source task, and {receipt_read}"
            )
        },
    );
    fit.omit("origin", usize::from(origin_cut));
    fit.section("origin", &whence_text);
    out.push_str(&whence_text);
    out.push_str(&revisit_section(&mut fit, material));
    let goals = material
        .goals
        .iter()
        .map(|(goal, closed, siblings)| {
            let heading = format!(
                "\n## Goal {gid}: {title}{closed}",
                gid = goal.id(),
                title = short_title(goal.title()),
                closed = if *closed { " (closed)" } else { "" },
            );
            (
                goal.id(),
                planner_goal(&heading, goal, (siblings, "Its other tasks"), true),
            )
        })
        .collect();
    out.push_str(&planner_goals(&mut fit, goals));
    let goalless: Vec<String> = material
        .members
        .iter()
        .filter(|(target, _)| target.task.goal_id().is_none())
        .map(|(target, _)| target.task.id().to_string())
        .collect();
    if !goalless.is_empty() {
        out.push_str(&if single {
            "\n## Goal\n\nThe draft belongs to no goal (the source's goal was closed, or it had none).\n".to_owned()
        } else {
            format!(
                "\n## Goal\n\nDraft {} belongs to no goal (the source's goal was closed, or it had none).\n",
                goalless.join(", ")
            )
        });
    }
    let (goal_of_first, run, source) = (
        first
            .task
            .goal_id()
            .map_or("?".to_owned(), |g| g.to_string()),
        first.material["source_run_id"].as_str().unwrap_or("?"),
        &first.material["source_task_id"],
    );
    let adopt = match origin {
        DraftOrigin::FollowUp => format!(
            "complete the draft with `dagq edit {t}` (acceptance, `--verify`, `--paths`, `--evidence`, and `--context` beginning with `follow-up draft (proposed by the receipt of run {run} of task {source})`),"
        ),
        DraftOrigin::GoalGap => format!(
            "complete the draft with `dagq edit {t}` (acceptance, `--verify`, `--paths`, `--evidence`, and `--context` beginning with `goal gap draft (proposed by the judgment of goal {goal_of_first})`),"
        ),
        DraftOrigin::Reopened => format!(
            "fix what plan review's reason points at with `dagq edit {t}` (its description, acceptance, `--verify`, `--paths`, `--evidence`, and a line in `--context` on the reopen of proposal {}), keeping the task's intent,",
            first.material["proposal_id"],
        ),
        DraftOrigin::Revisit => format!(
            "complete the draft with `dagq edit {t}` (acceptance, `--verify`, `--paths`, `--evidence`, and a line in `--context` that it was decided at its revisit time), keeping the person's intent,"
        ),
    };
    out.push_str(&format!(
        "\n## What to do\n\n\
         Follow the dagq-planner skill of the dagq plugin, its Basic policy above all: {DECIDE_YOURSELF} {rules} Look for tasks that already cover the {drafts} or code that already does {it} (`dagq search '<words>'`, `dagq show ID`, the source) before you decide. {RECORD_READING}\n\
         {membership}{each}Then do exactly one of these three{with_each}:\n\
         1. Adopt: {adopt} add its dependencies with `dagq dependency add`, check it with `dagq lint {t}` and submit it with `dagq submit {t}`. Say in its `--context` why you adopted it. Plan review checks it before it becomes ready.\n\
         2. Drop: when it is already done, duplicated or not worth doing, cancel it with `dagq cancel {t}` and record why with `dagq note --task {t} --text '<why>'`. When another task already covers it (a duplicate, or a completed task that already did it), cancel it with `dagq cancel {t} --duplicate-of <that task>` instead, so the queue records which task it duplicates, and still note why.\n\
         3. Ask: only for a draft you cannot decide yourself: (a) it needs a person's judgement, `scope` (the acceptance, the scope or a goal's decision would change with their intent) or `discard` (whether to throw work away), that the queue, the repository (its code and the decisions it records) and a person's precedents cannot settle; (b) your confidence in the decision is low; or (c) it is a follow_up draft past the runtime's follow_up limit, {FOLLOW_UP_ASK_DEPTH} or more follow-ups from a person's judgement, a source goal that was missing, closed or unknown at registration (even if its current goal is open), or no current goal or a closed current goal. Run `dagq ask --task {t} --kind planner_question --because scope --recommend <adopt|cancel|keep_draft> --confidence <high|low> --question '<everything the person needs, with your recommendation and why>' --option adopt --option cancel --option keep_draft` (`--because discard` when the question is whether to throw work away; for (c), recommend what you would do on your own), report briefly and stop; {BEFORE_YOU_STOP_AT_A_QUESTION} The answer arrives as `answer to ask <id>: ...`: on adopt do 1, on cancel do 2 (the note names the ask), on keep_draft leave the draft as it is, record why with `dagq note --task {t} --text '<why>'` (naming the ask) and stop. A draft kept so stays a draft until a person has the inbox record a planning request that names it; no planner of the runtime's is opened for it again, unless it has a revisit time. When you can tell when it can be decided (after a task lands, after a period to measure), keep it with `dagq revisit {t} --at <RFC 3339 time, e.g. 2026-10-04T12:00:00Z> --note '<what to look at then>'`: at that time the runtime opens a planner for it again with this decision in its prompt. You may keep a draft so yourself, without asking, when that is your recommendation; record why with `dagq note --task {t}` too.\n\
         The runtime refuses your submit of a follow_up draft past that limit unless a person answered adopt or already adopted it: ask then, as (c) says. Membership changes (`set-goal` or `judge-follow-up`) do not count as adoption or reset depth; an existing person's adopt remains valid.\n\
         When you are done, report the outcome in one or two sentences and stop; the runtime ends this session. Do not work on anything but {this}. Never open the queue database directly; use the dagq CLI only.\n",
        membership = if origin == DraftOrigin::FollowUp {
            follow_up_membership_step(&t)
        } else {
            String::new()
        },
        drafts = if single { "draft" } else { "drafts" },
        it = if single { "it" } else { "them" },
        each = if single {
            String::new()
        } else {
            format!(
                "The drafts came from one piece of work, so weigh them together first: when two of them are the same work, keep one and cancel the other with `dagq cancel ID --duplicate-of <the one you keep>`; when one needs another, add the dependency between them with `dagq dependency add`; submit those you adopt together in one proposal, `dagq submit ID ID ...` (with {}).\n",
                ids.join(", ")
            )
        },
        with_each = if single { "" } else { " for each draft, ID being its task ID" },
        this = if single { "this draft" } else { "these drafts" },
        rules = repository_rules(RUNTIME_PLANNER_ASK),
    ));
    if let Some(answer) = material.answer {
        let (question, text) = planner_answer(&mut fit, answer);
        let carried = format!(
            "\nThe planner before you asked a person (ask {aid}) about draft {task} and is gone:\n{question}\n\nanswer to ask {aid}: {text}\n\nApply this answer as step 3 says.\n",
            aid = answer.id,
            task = answer.task_id.map_or("?".to_owned(), |t| t.to_string()),
        );
        fit.section("answer", &carried);
        out.push_str(&carried);
    }
    if let Some(handover) = material.handover {
        out.push_str(&handover_section(&mut fit, handover));
    }
    Ok(fit.finish(out))
}

/// The bytes the whole prompt of a planner the runtime opens for a finding
/// takes at most, the language's instruction included (task 1571): in
/// production it took 37,974 bytes at the median, 156,358 at p90 and
/// 276,417 at most (planner 768: finding 44's evidence took 270,669).
/// The 80,000 that held that grows by what a planner ended for a person's
/// answer left ([`PLANNER_HANDOVER_BYTES`], ADR-t1704-1).
pub const FINDING_PLANNER_PROMPT_LIMIT: usize = 80_000 + PLANNER_HANDOVER_BYTES;

/// The bytes of the finding's evidence events, the newest first, and of
/// one event.
pub const FINDING_EVIDENCE_BYTES: usize = 28_000;
pub const FINDING_EVENT_BYTES: usize = 8_000;

/// The bytes of the finding's detail, and of its summary, subject and why
/// it is proposed each.
pub const FINDING_DETAIL_BYTES: usize = 8_000;
pub const FINDING_SHORT_BYTES: usize = 2_000;

/// What the initial prompt of a planner opened for a finding shows
/// (ADR-0044 decision 19).
pub struct FindingPlannerMaterial<'a> {
    pub db: &'a Path,
    /// The finding with its evidence events.
    pub finding: &'a FindingView,
    /// Which planner of the runtime's this is since the finding was marked
    /// (1-based).
    pub attempt: usize,
    /// The asks about the finding: the observer's, a person's `propose`
    /// answer, earlier planners' questions.
    pub asks: &'a [Ask],
    /// The goal of the finding's target, if it has one.
    pub goal: Option<&'a Goal>,
    pub goal_closed: bool,
    /// That goal's tasks.
    pub siblings: &'a [GoalTask],
    /// The answered `planner_question` of a planner that is gone.
    pub answer: Option<&'a Ask>,
    /// What the planner that asked it left as it ended for the answer
    /// alone (ADR-t1704-1 decision 3).
    pub handover: Option<&'a PlannerHandover>,
}

/// The initial prompt of a planner the runtime opens for a finding marked
/// for a proposal (ADR-0044 decisions 19, 20): the finding and its
/// evidence, the asks about it, the goal of its target, and what it may do:
/// a proposal of tasks for an open goal or of a new goal, a dismissal, or a
/// `planner_question` for a person. Each section is held to its limit and
/// the whole to [`FINDING_PLANNER_PROMPT_LIMIT`]: the evidence newest
/// first; what is left out is counted and named with the read-only dagq
/// command that reads it (task 1571, ADR-t1566-1).
pub fn finding_planner_prompt(material: &FindingPlannerMaterial<'_>) -> Result<FittedPrompt> {
    let mut fit = Fit::new(FINDING_PLANNER_PROMPT_LIMIT);
    let view = material.finding;
    let finding = &view.finding;
    let id = finding.id;
    let read = format!("read it whole with `dagq findings {id} --full`");
    let mut out = format!(
        "You are a planner the dagq runtime opened for finding {id} of the queue at {db}; no person watches this session. The finding is marked for a proposal: make the plan that remedies it (planner {attempt} of at most {max} the runtime opens for it).\n",
        db = crate::application::path_text(material.db)?,
        attempt = material.attempt,
        max = MAX_FINDING_PLANNERS,
    );
    let target = match (&finding.run_id, finding.task_id, finding.goal_id) {
        (Some(run), Some(task), _) => format!("run {run} (task {task})"),
        (None, Some(task), _) => format!("task {task}"),
        (_, _, Some(goal)) => format!("goal {goal}"),
        _ => "the queue".to_owned(),
    };
    let (summary, summary_cut) =
        prompt_fit::cut_part(&finding.summary, FINDING_SHORT_BYTES, Keep::Start, &read);
    let (subject, subject_cut) = prompt_fit::cut_part(
        or_none(&finding.subject),
        FINDING_SHORT_BYTES,
        Keep::Start,
        &read,
    );
    let (why, why_cut) = prompt_fit::cut_part(
        finding.propose_reason.as_deref().unwrap_or("(none)"),
        FINDING_SHORT_BYTES,
        Keep::Start,
        &read,
    );
    let (detail, detail_cut) = fit.required_part(
        "finding",
        or_none(&finding.detail),
        FINDING_DETAIL_BYTES,
        &read,
    );
    // The finding is one item, counted once whichever of its parts was cut.
    fit.omit(
        "finding",
        usize::from(summary_cut || subject_cut || why_cut || detail_cut),
    );
    let head = format!(
        "\n## Finding {id}: {summary}\n\n- kind: {kind}\n- on: {target}\n- subject: {subject}\n- impact: {impact}\n- occurrences: {occurrences}, first seen {first}, last seen {last} (Unix seconds)\n- recorded by: {by}\n- why a proposal: {why}\n\n### Detail\n\n{detail}\n",
        kind = finding.kind,
        impact = finding.impact.as_str(),
        occurrences = finding.occurrences,
        first = finding.first_seen_at,
        last = finding.last_seen_at,
        by = finding.recorded_by,
    );
    fit.section("finding", &head);
    out.push_str(&head);
    out.push_str("\n### Its evidence\n\n");
    match &view.evidence_events {
        Some(events) if !events.is_empty() => {
            // The newest evidence first within the section's limit.
            let values = events
                .iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let (kept, left_out) = fit.lines(
                "evidence",
                &values,
                (0..values.len()).rev(),
                (usize::MAX, FINDING_EVIDENCE_BYTES, FINDING_EVENT_BYTES),
                &read,
            );
            let mut evidence = fenced("json", &kept.join("\n"));
            if !left_out.is_empty() {
                let ids: Vec<String> = left_out
                    .iter()
                    .map(|&index| events[index].id.to_string())
                    .collect();
                evidence.push_str(&left_out_note(
                    "evidence events (the oldest)",
                    &ids,
                    &format!("`dagq findings {id} --full`, or one event with `dagq events --full --all --after <its ID - 1> --limit 1`"),
                ));
            }
            fit.section("evidence", &evidence);
            out.push_str(&evidence);
        }
        _ => out.push_str("(no event)\n"),
    }
    out.push_str(&format!(
        "Read more with `dagq findings {id} --full`. {RECORD_READING}\n"
    ));
    if !material.asks.is_empty() {
        out.push_str("\n### Asks about it\n\n");
        out.push_str(&planner_asks(&mut fit, material.asks, true));
    }
    match material.goal {
        Some(goal) => {
            let heading = format!(
                "\n## Goal {gid} of its target: {title}{closed}",
                gid = goal.id(),
                title = short_title(goal.title()),
                closed = if material.goal_closed {
                    " (closed)"
                } else {
                    ""
                },
            );
            let section = planner_goal(&heading, goal, (material.siblings, "Its tasks"), false);
            out.push_str(&planner_goals(&mut fit, vec![(goal.id(), section)]));
        }
        None => out.push_str(
            "\n## Goal\n\nIts target belongs to no goal. `dagq goal list` shows the open goals.\n",
        ),
    }
    out.push_str(&format!(
        "\n## What to do\n\n\
         Follow the dagq-planner skill of the dagq plugin, its Basic policy above all: {DECIDE_YOURSELF} {rules} Before you plan, look for tasks that already remedy it or code that already does (`dagq search '<words>'`, `dagq related ID` for a task, `dagq show ID`). Then do exactly one of these:\n\
         1. Tasks for an open goal: when the remedy is within an open goal's scope (the one above, or another from `dagq goal list`), add its tasks to that goal as drafts (`dagq add --goal GOAL ...`, with `--context` beginning with `from finding {id} ({kind})` and saying why you chose this remedy), check them with `dagq lint`, and submit them with `dagq submit ID... --finding {id}`.\n\
         2. A new goal: when no open goal covers it, write a draft goal (`dagq goal add --draft ...`) and its draft tasks, lint them and submit with `dagq submit --goal GOAL --finding {id}`.\n\
         Either way the submission makes finding {id} proposed with the proposal, and plan review checks it before it becomes ready; you need no person's approval for it, even for a new goal. \
         Give an improvement's tasks no `--priority`: each takes its goal's (`normal` with no goal). Put a task in an existing goal only when that goal's acceptance needs it, and an improvement only in a goal of `normal` or lower; otherwise write a new goal (its priority by the repository's rules for goal priorities, `normal` or lower for an improvement) or leave the task with no goal. Plan review checks the goal you chose.\n\
         3. Dismiss: when a task already remedies it (name the task), it no longer occurs, or it is not worth remedying, run `dagq finding dismiss {id} --reason '<why>'`, the reason saying why you decided so.\n\
         4. Ask: only when you cannot decide it yourself: (a) it needs a person's judgement, `scope` (the plan's intent, an acceptance, a contradiction with a goal's constraints or a decision the repository records, a precedent a person answered otherwise) or `discard` (whether to throw work away), that the queue, the repository (its code and the decisions it records) and a person's precedents cannot settle; or (b) your confidence in the decision is low. Run `dagq ask --finding {id} --kind planner_question --because scope --recommend <propose|dismiss> --confidence <high|low> --question '<everything the person needs, with your recommendation and why>' --option propose --option dismiss` (`--because discard` when the question is whether to throw work away), report briefly and stop; {BEFORE_YOU_STOP_AT_A_QUESTION} The answer arrives as `answer to ask <id>: ...`: follow it (propose: do 1 or 2; dismiss: do 3).\n\
         When you are done, report the outcome in one or two sentences and stop; the runtime ends this session. Do not work on anything but this finding. Never open the queue database directly; use the dagq CLI only.\n",
        kind = finding.kind,
        rules = repository_rules(RUNTIME_PLANNER_ASK),
    ));
    // A failure of the watched branch's CI (ADR-t1920-1): the fix task
    // carries the finding's detail, and a task that already fixes the
    // same tests covers it instead of a new one.
    if finding.kind == crate::domain::ci_watch::FINDING_KIND {
        let section = format!(
            "\n## A CI failure\n\n\
             The supervisor recorded this finding when the watched branch's CI turned up failures not on its list of the tests that fail already (`dagq ci failures`). Its detail is one JSON object: `tests` (the new failures; a `job:<job>/step:<step>` item is a failed step without a test name), `failed_jobs`, `range` (`from` the last green commit, `to` the first red one, `commits` between them), `url` (the CI run), `binary_contains` (whether the supervisor's build contains the range: `all`, `some`, `none` or `unknown`) and `binary_commit`.\n\
             - Copy each of them into the fix task's `--description`.\n\
             - Before you add a task, look for one that already fixes the same tests (`dagq search '<a test name>'`, `dagq list`). When one is open, add no task: run `dagq finding dismiss {id} --covered-by <task> --reason '<why>'`; that task's runs then keep these tests on their list.\n\
             - Give the fix task no `--priority`: like any improvement it takes its goal's (a goal of `normal` or lower, or no goal).\n"
        );
        fit.section("ci_failure", &section);
        out.push_str(&section);
    }
    if let Some(answer) = material.answer {
        let (question, text) = planner_answer(&mut fit, answer);
        let carried = format!(
            "\nThe planner before you asked a person (ask {aid}) and is gone:\n{question}\n\nanswer to ask {aid}: {text}\n\nApply this answer as step 4 says.\n",
            aid = answer.id,
        );
        fit.section("answer", &carried);
        out.push_str(&carried);
    }
    if let Some(handover) = material.handover {
        out.push_str(&handover_section(&mut fit, handover));
    }
    Ok(fit.finish(out))
}

/// What a planning request refers to, as its planner's prompt shows it
/// (ADR-t1394-1 decision 5).
#[derive(Debug, Clone)]
pub enum RequestRefMaterial {
    /// An ask: its question and answer.
    Ask(Ask),
    /// A task, with the receipt of its last landed run.
    Task {
        task: Box<Task>,
        receipt: Option<Value>,
    },
    /// A run, with its task and its landed receipt.
    Run {
        run: RunId,
        task: Option<TaskId>,
        receipt: Option<Value>,
    },
    /// An event, payload and all.
    Event(RunEvent),
    /// A finding with its reading and evidence.
    Finding(Box<FindingView>),
    /// A goal: its section below shows it.
    Goal(GoalId),
    /// A reference that could not be read (gone, or not of this queue).
    Unreadable {
        reference: crate::domain::plan_request::RequestRef,
        error: String,
    },
}

/// The bytes the whole prompt of a planner the runtime opens for a
/// planning request takes at most, the language's instruction included
/// (task 1571). Production had no such prompt yet: the limit is the other
/// planners' (the draft planner's p90 26,352 and largest 38,241 fit in
/// it), and the unit test's largest input
/// (`a_request_planner_prompt_of_huge_references_stays_within_its_limits`)
/// measures what the sections' limits add up to.
/// The 80,000 that held that grows by what a planner ended for a person's
/// answer left ([`PLANNER_HANDOVER_BYTES`], ADR-t1704-1).
pub const REQUEST_PLANNER_PROMPT_LIMIT: usize = 80_000 + PLANNER_HANDOVER_BYTES;

/// The bytes of the inbox's note, of the references' sections (in the
/// order the inbox gave them) and of one reference.
pub const REQUEST_NOTE_BYTES: usize = 8_000;
pub const REQUEST_REFS_BYTES: usize = 32_000;
pub const REQUEST_REF_BYTES: usize = 8_000;

/// What the initial prompt of a planner the runtime opens for a planning
/// request is made of.
pub struct RequestPlannerMaterial<'a> {
    pub db: &'a Path,
    pub request: &'a crate::domain::plan_request::PlanRequest,
    /// The sentence that points at the file the request's words were
    /// handed over in ([`crate::application::planner_handoff`]).
    pub handed: &'a str,
    /// Which planner of the runtime's this is for the request (1-based).
    pub attempt: usize,
    pub refs: &'a [RequestRefMaterial],
    /// The goals the references lead to: each with whether it is closed
    /// and its tasks.
    pub goals: &'a [(Goal, bool, Vec<GoalTask>)],
    /// The earlier planners' questions about the request.
    pub asks: &'a [Ask],
    /// The answered `planner_question` of a planner that is gone.
    pub answer: Option<&'a Ask>,
    /// What the planner that asked it left as it ended for the answer
    /// alone (ADR-t1704-1 decision 3).
    pub handover: Option<&'a PlannerHandover>,
}

/// The initial prompt of a planner the runtime opens for a planning
/// request (ADR-t1394-1 decision 5): the file the person's words were
/// handed over in, the inbox's note apart from them, what the request
/// refers to, the goals it leads to, how to look for what already covers
/// it, the Basic policy of what to raise to a person, and what it may do:
/// submit a proposal, decline the request with a reason, or ask a
/// `planner_question` about it. Each section is held to its limit and the
/// whole to [`REQUEST_PLANNER_PROMPT_LIMIT`]; what is left out is counted
/// and named with the read-only dagq command that reads it (task 1571,
/// ADR-t1566-1).
pub fn request_planner_prompt(material: &RequestPlannerMaterial<'_>) -> Result<FittedPrompt> {
    let mut fit = Fit::new(REQUEST_PLANNER_PROMPT_LIMIT);
    let request = material.request;
    let id = request.id;
    let mut out = format!(
        "You are a planner the dagq runtime opened for planning request {id} of the queue at {db}; no person watches this session. A person asked the inbox for a plan, and the inbox recorded the request for you (planner {attempt} of at most {max} the runtime opens for it).\n",
        db = crate::application::path_text(material.db)?,
        attempt = material.attempt,
        max = crate::domain::plan_request::MAX_REQUEST_PLANNERS,
    );
    out.push_str(&format!(
        "\n## Request {id}\n\n{handed} Those are the person's own words (recorded by the {by} at {at}, Unix seconds).\n",
        handed = material.handed,
        by = request.requested_by,
        at = request.created_at,
    ));
    if let Some(priority) = request.priority {
        out.push_str(&format!(
            "\nThe person gave it the priority `{p}`. The runtime gives `{p}`, as the person's, to each goal you add for it, and to each task of it you add to a goal without it as the person's or to no goal: leave `--priority` out of `goal add` and `add` (another value is refused).\n",
            p = priority.as_str(),
        ));
    }
    if let Some(note) = &request.note {
        let note = format!(
            "\nThe inbox added this, apart from the person's words:\n\n{}\n",
            fit.text(
                "note",
                note,
                REQUEST_NOTE_BYTES,
                Keep::Start,
                &format!("read it whole with `dagq requests {id}`"),
            )
        );
        fit.section("note", &note);
        out.push_str(&note);
    }
    if !material.refs.is_empty() {
        out.push_str("\n## What it refers to\n");
    }
    // Each reference within its limit, in the order the inbox gave them.
    let (mut refs, mut cuts) = (Vec::new(), Vec::new());
    for reference in material.refs {
        let (label, read, text) = request_ref(reference)?;
        let read_note = if matches!(reference, RequestRefMaterial::Unreadable { .. }) {
            read.clone()
        } else {
            format!("read it whole with {read}")
        };
        let (text, cut) = prompt_fit::cut_part(&text, REQUEST_REF_BYTES, Keep::Start, &read_note);
        refs.push((label, read, text));
        cuts.push(cut);
    }
    let sizes: Vec<usize> = refs.iter().map(|(_, _, text)| text.len()).collect();
    let kept = prompt_fit::pick(&sizes, 0..refs.len(), usize::MAX, REQUEST_REFS_BYTES);
    fit.picked("refs", &kept, &cuts);
    let mut referred = String::new();
    let mut left_out = Vec::new();
    for ((label, read, text), kept) in refs.into_iter().zip(&kept) {
        if *kept {
            referred.push_str(&text);
        } else {
            left_out.push(format!("{label} ({read})"));
        }
    }
    if !left_out.is_empty() {
        referred.push_str(&format!(
            "\n### Left out\n\n{}",
            left_out_note("references", &left_out, "the command named with each readable reference; unreadable references have no read method")
        ));
    }
    fit.section("refs", &referred);
    out.push_str(&referred);
    if material.goals.is_empty() {
        out.push_str("\n## Goals\n\nNothing it refers to belongs to a goal. `dagq goal list` shows the open goals.\n");
    }
    let goals = material
        .goals
        .iter()
        .map(|(goal, closed, tasks)| {
            let heading = format!(
                "\n## Goal {gid}: {title}{closed}",
                gid = goal.id(),
                title = short_title(goal.title()),
                closed = if *closed { " (closed)" } else { "" },
            );
            (
                goal.id(),
                planner_goal(&heading, goal, (tasks, "Its tasks"), false),
            )
        })
        .collect();
    out.push_str(&planner_goals(&mut fit, goals));
    if !material.asks.is_empty() {
        out.push_str("\n## Earlier questions about it\n\n");
        out.push_str(&planner_asks(&mut fit, material.asks, false));
    }
    out.push_str(&format!(
        "\n## What to do\n\n\
         Follow the dagq-planner skill of the dagq plugin, its Basic policy above all: {DECIDE_YOURSELF} Read the repository's AGENTS.md (or CLAUDE.md) first, and the documents it names for what you plan. {rules} Before you plan, look for tasks that already cover the request or code that already does it (`dagq search '<words>'`, `dagq related ID` for a task, `dagq show ID`, `dagq goal list`, the source). {RECORD_READING}\n\
         Then do exactly one of these:\n\
         1. Plan it: write the goal (`dagq goal add --draft ...`) or the tasks for an open goal (`dagq add --goal GOAL ...`) the request asks for, with `--context` beginning with `from request {id}` and saying why you planned it so, check them with `dagq lint`, and submit them with `dagq submit ...`. Your submission makes request {id} proposed with the proposal, and plan review checks it before it becomes ready; you need no person's approval for it, even for a new goal. You may submit more than one proposal for it.\n\
         2. Decline: when nothing should be planned of it (it is done already: name the task or the code; it duplicates a task in flight: name it; or it cannot be planned as asked: say why), run `dagq request decline {id} --reason '<why>'`. The inbox tells the person, who may ask again in other words.\n\
         3. Ask: only when you cannot decide it yourself: (a) it needs a person's judgement, `scope` (the plan's intent, an acceptance, a contradiction with a goal's constraints or a decision the repository records, a precedent a person answered otherwise) or `discard` (whether to throw work away), that the queue, the repository (its code and the decisions it records) and a person's precedents cannot settle; or (b) your confidence in the decision is low. Run `dagq ask --request {id} --kind planner_question --because scope --recommend <plan|decline> --confidence <high|low> --question '<everything the person needs, with your recommendation and why>' --option plan --option decline` (`--because discard` when the question is whether to throw work away), report briefly and stop; {BEFORE_YOU_STOP_AT_A_QUESTION} The answer arrives as `answer to ask <id>: ...`: follow it (plan: do 1 as it says; decline: do 2).\n\
         When you are done, report the outcome in one or two sentences and stop; the runtime ends this session. Do not work on anything but this request. Never open the queue database directly; use the dagq CLI only.\n",
        rules = repository_rules(RUNTIME_PLANNER_ASK),
    ));
    if let Some(answer) = material.answer {
        let (question, text) = planner_answer(&mut fit, answer);
        let carried = match answer.task_id.filter(|_| answer.request_id.is_none()) {
            // About a draft an earlier planner added for the request and
            // left outside a proposal (ADR-t2015-1).
            Some(t) => format!(
                "\nAn earlier planner of this request added draft task {t}, asked a person about it (ask {aid}) and is gone:\n{question}\n\nanswer to ask {aid}: {text}\n\nApply this answer to draft task {t} (`dagq show {t}`) first: on adopt complete it and submit it with `dagq submit {t}`, on cancel cancel it with `dagq cancel {t}` and record why with `dagq note --task {t} --text '<why>'` naming the ask, on keep_draft leave it as it is; any other answer, as it says. The request may no longer be open (proposed, declined or out of planners): then this draft is all that is left to you, and you do not decline the request.\n",
                aid = answer.id,
            ),
            None => format!(
                "\nThe planner before you asked a person (ask {aid}) and is gone:\n{question}\n\nanswer to ask {aid}: {text}\n\nApply this answer as step 3 says.\n",
                aid = answer.id,
            ),
        };
        fit.section("answer", &carried);
        out.push_str(&carried);
    }
    if let Some(handover) = material.handover {
        out.push_str(&handover_section(&mut fit, handover));
    }
    Ok(fit.finish(out))
}

/// One reference of a planning request as its planner's prompt shows it:
/// how the prompt names it, the read-only dagq command that reads it
/// whole (or why none is available), and its section.
pub(super) fn request_ref(reference: &RequestRefMaterial) -> Result<(String, String, String)> {
    let mut out = String::new();
    let (label, read) = match reference {
        RequestRefMaterial::Ask(ask) => {
            out.push_str(&format!(
                "\n### Ask {aid} ({kind})\n\n{question}\n\nanswer: {answer}\n",
                aid = ask.id,
                kind = ask.kind.as_str(),
                question = ask.question,
                answer = ask.answer.as_deref().unwrap_or("(none yet)"),
            ));
            (format!("ask {}", ask.id), "`dagq asks --all`".to_owned())
        }
        RequestRefMaterial::Task { task, receipt } => {
            out.push_str(&format!(
                "\n### Task {tid} ({status}): {title}\n\n{description}\n\nAcceptance:\n{acceptance}\n",
                tid = task.id(),
                status = task.status().as_str(),
                title = short_title(task.title()),
                description = or_none(task.description()),
                acceptance = or_none(task.acceptance()),
            ));
            push_receipt(&mut out, receipt.as_ref())?;
            (
                format!("task {}", task.id()),
                format!(
                    "`dagq show {tid} --full` and `dagq events --full --task {tid} --kind integration_receipt`",
                    tid = task.id()
                ),
            )
        }
        RequestRefMaterial::Run { run, task, receipt } => {
            out.push_str(&format!(
                "\n### Run {run}{of}\n",
                of = task
                    .map(|task| format!(" (task {task})"))
                    .unwrap_or_default(),
            ));
            push_receipt(&mut out, receipt.as_ref())?;
            (
                format!("run {run}"),
                format!("`dagq events --full --run {run} --kind integration_receipt`"),
            )
        }
        RequestRefMaterial::Event(event) => {
            out.push_str(&format!(
                "\n### Event {eid} ({kind})\n\n",
                eid = event.id,
                kind = event.kind,
            ));
            out.push_str(&fenced(
                "json",
                &serde_json::to_string_pretty(&event.payload)?,
            ));
            (
                format!("event {}", event.id),
                format!(
                    "`dagq events --full --all --after {} --limit 1`",
                    event.id.as_i64() - 1
                ),
            )
        }
        RequestRefMaterial::Finding(view) => {
            let finding = &view.finding;
            out.push_str(&format!(
                "\n### Finding {fid} ({kind}, {status}): {summary}\n\n{detail}\n\nRead its evidence with `dagq findings {fid} --full`.\n",
                fid = finding.id,
                kind = finding.kind,
                status = finding.status.as_str(),
                summary = finding.summary,
                detail = or_none(&finding.detail),
            ));
            (
                format!("finding {}", finding.id),
                format!("`dagq findings {} --full`", finding.id),
            )
        }
        RequestRefMaterial::Goal(goal) => {
            out.push_str(&format!("\n### Goal {goal}\n\nSee the goals below.\n"));
            (
                format!("goal {goal}"),
                format!("`dagq goal show {goal} --full`"),
            )
        }
        RequestRefMaterial::Unreadable { reference, error } => {
            out.push_str(&format!(
                "\n### {reference}\n\nIt could not be read: {error}\n"
            ));
            (
                reference.to_string(),
                "it could not be read, so there is no way to read it".to_owned(),
            )
        }
    };
    Ok((label, read, out))
}

/// The `summary` and `follow_ups` of a landed receipt, or that there is
/// none.
pub(super) fn push_receipt(out: &mut String, receipt: Option<&Value>) -> Result<()> {
    match receipt {
        Some(receipt) => {
            out.push_str(&format!(
                "\nIts landed receipt's summary:\n\n{summary}\n\nIts follow_ups:\n\n",
                summary = or_none(receipt["summary"].as_str().unwrap_or_default()),
            ));
            out.push_str(&fenced(
                "json",
                &serde_json::to_string_pretty(&receipt["follow_ups"])?,
            ));
        }
        None => out.push_str("\nNo landed receipt.\n"),
    }
    Ok(())
}

pub(super) fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_owned()
    } else {
        items.join(", ")
    }
}
