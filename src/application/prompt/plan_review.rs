//! The headless plan review and goal review (planning).

use super::*;

/// What the headless plan review may do beyond what needs no permission:
/// read files, and run the dagq CLI (ADR-0044 decision 22, ADR-t1063-1
/// decision 2). It runs with the reviewer's role in its environment, like
/// the review, so the CLI refuses every dagq command that writes. On Codex
/// it is `codex exec` in the read-only sandbox (ADR-t1063-1 decision 2):
/// its prompt names the repository's instructions and documents to read
/// and leans on no Claude plugin, skill or hook.
pub const PLAN_REVIEW_ACCESS: JobAccess = JobAccess::ReadFilesAndQueueCli;

/// What the headless goal review may do: the same as the plan review (read
/// files, and run the dagq CLI, whose policy lets it only read).
pub const GOAL_REVIEW_ACCESS: JobAccess = JobAccess::ReadFilesAndQueueCli;

/// Characters of a precedent's question and answer the plan review prompt
/// and the revise request quote.
pub(super) const PRECEDENT_CHARS: usize = 400;

/// Candidates of each kind (related tasks, search hits) the plan review
/// prompt lists at most for one task of the proposal.
pub const DUPLICATE_CANDIDATES: usize = 5;

/// Where the plan review starts looking for duplicates and changes already
/// made for one task of the proposal (goal 29): the tasks `dagq related`
/// ranks highest with the clues that relate them, and the tasks and landed
/// commits `dagq search` finds for the words of its title, in any status,
/// none of them the proposal's own.
#[derive(Debug, Clone, Serialize)]
pub struct DuplicateCandidates {
    pub task_id: TaskId,
    pub related: Vec<RelatedTask>,
    pub search: Vec<SearchHit>,
}

/// Expected files the summary of a ready or in-progress task lists at
/// most; the hotspot table reads them all.
pub const SUMMARY_EXPECTED_FILES: usize = 10;

/// What the headless plan review reads (ADR-0041 decision 10): the
/// proposal and its tasks, the goals they belong to, what `dagq lint`
/// found, the other proposals not yet ready (oldest submission first), the
/// ready and in-progress tasks, the asks a person answered before, the
/// files that conflict often and each task's duplicate candidates.
pub struct PlanReviewMaterial<'a> {
    pub proposal: &'a Proposal,
    pub tasks: &'a [TaskDetail],
    pub goals: &'a [Goal],
    pub lint: &'a [LintViolation],
    /// Other submitted or revising proposals with their tasks.
    pub others: &'a [(Proposal, Vec<Task>)],
    /// Ready and in-progress tasks, with their long fields; the prompt
    /// lists each in summary and gives the long fields only of those it
    /// has a reason to (task 591).
    pub queued: &'a [TaskListItem],
    /// Ready and in-progress tasks past the limit of `queued`.
    pub queued_left_out: usize,
    /// The files each task of the proposal and of `queued` is expected to
    /// touch (ADR-0069 decisions 1, 2).
    pub expected: &'a BTreeMap<TaskId, Vec<String>>,
    /// Asks a person answered, newest first ([`PRECEDENT_ASKS`] of them
    /// are fetched).
    pub precedents: &'a [Ask],
    /// The files the landings conflicted in most (`stats`
    /// `conflict_hotspots`), that main still has.
    pub hotspots: &'a [ConflictHotspot],
    /// One entry per task of the proposal, in the order of `tasks`.
    pub candidates: &'a [DuplicateCandidates],
    pub repo_root: &'a Path,
    /// The language whose instruction ends the prompt (ADR-t616-2
    /// decision 3); counted within its limit.
    pub language: Option<&'a crate::domain::language::Language>,
}

/// One line quoting an answered ask as a precedent.
pub fn precedent_line(ask: &Ask) -> String {
    let cut = |text: &str| {
        crate::application::health::truncate(text, PRECEDENT_CHARS)
            .unwrap_or_else(|| text.to_owned())
    };
    format!(
        "precedent: ask {id}{task} ({kind}) asked: {question} — a person answered: {answer}",
        id = ask.id,
        task = ask
            .task_id
            .map(|task| format!(" about task {task}"))
            .unwrap_or_default(),
        kind = ask.kind.as_str(),
        question = cut(&ask.question.replace('\n', " ")),
        answer = cut(ask.answer.as_deref().unwrap_or("(none)")),
    )
}

/// The bytes the whole plan review prompt takes at most, the language's
/// instruction included (task 1561, ADR-t1566-1 decision 4): about two
/// fifths of macOS's `ARG_MAX`, and about a third of the prompt of plan
/// review 724 that could not start.
pub const PLAN_REVIEW_PROMPT_LIMIT: usize = 400_000;

/// The bytes the sections the plan review cannot do without (the
/// instructions and the verdict's schema, the proposal's tasks and their
/// expected files, the goals, `lint`, the language's instruction) take at
/// most; past it their largest material is replaced by how to read it with
/// the read-only dagq commands (ADR-t1566-1 decisions 2, 3). Half the whole
/// limit: an ordinary proposal's required sections take about half of this
/// limit, so nothing is replaced, and past it the optional sections still
/// keep the other half of the whole. Nothing is moved to a file of the job's directory.
pub const PLAN_REVIEW_REQUIRED_LIMIT: usize = 200_000;

/// Ready and in-progress tasks the plan review prompt gives in full at
/// most, and the bytes of their full text. The full text was most of the
/// prompt that could not start; the 20 that overlap the proposal most are
/// enough to judge its dependencies, and the bytes keep one huge task from
/// filling the section. The rest is read with `dagq show ID --full`.
pub const QUEUED_FULL_TASKS: usize = 20;
pub const QUEUED_FULL_BYTES: usize = 100_000;

/// The bytes of the summaries of the ready and in-progress tasks, those
/// that overlap first: about 96 lines of an average summary. The rest is
/// read with `dagq list`.
pub const QUEUED_SUMMARY_BYTES: usize = 64_000;

/// Asks a person answered the plan review prompt quotes at most, newest
/// first, and the bytes they take: one question and answer, each up to
/// [`PRECEDENT_CHARS`] characters, may pass 2 KB in Japanese. Older ones
/// are read with `dagq asks --all`.
pub const PRECEDENT_ASKS: usize = 20;
pub const PRECEDENT_BYTES: usize = 16_000;

/// The bytes of the conflict hotspots, of the duplicate candidates and of
/// the other proposals. The hotspots (about 5 KB) are kept first, as the
/// dependencies are judged by them. The candidates take about 4 KB a task,
/// and the rest are found with `dagq related` / `dagq search`. The other
/// proposals carry their tasks' whole description and acceptance, so they
/// grow with the proposals waiting; the rest is read with
/// `dagq proposal show ID`.
pub const HOTSPOT_BYTES: usize = 16_000;
pub const CANDIDATE_BYTES: usize = 48_000;
pub const OTHER_PROPOSAL_BYTES: usize = 48_000;

/// The read-only dagq commands the plan review prompt names to read what
/// its limits left out, as it writes them (`ID` for a number): each is one
/// the plan review job's role may run (task 1561, ADR-t1566-1 decision 3).
pub const PLAN_REVIEW_READS: &[&str] = &[
    "dagq show ID --full",
    "dagq goal show ID --full",
    "dagq proposal show ID",
    "dagq lint --proposal ID",
    "dagq related ID",
    "dagq search '<words of its title>'",
    "dagq asks --all",
    "dagq stats",
    "dagq list --status ready,in_progress --limit ID",
];

/// What the note of each section that left something out may take at most;
/// the overall limit keeps it free.
pub(super) const OMISSION_NOTE_BYTES: usize = 2_000;

/// IDs a note of what was left out names at most.
pub(super) const NOTE_IDS: usize = 40;

/// The note heading the proposal's tasks when the required sections were
/// over [`PLAN_REVIEW_REQUIRED_LIMIT`] may take at most.
pub(super) const OVER_LIMIT_NOTE_BYTES: usize = 1_000;

/// Characters of a title a stub of left-out material keeps.
pub(super) const STUB_TITLE_CHARS: usize = 200;

/// The bytes a stub takes at most (its title of [`STUB_TITLE_CHARS`] in
/// up to 4 bytes each, and its fields): a smaller piece is not replaced,
/// as its stub would not be smaller.
pub(super) const STUB_BYTES: usize = 1_000;

/// The optional sections of the plan review prompt, each of which keeps
/// [`OMISSION_NOTE_BYTES`] free for its note.
pub(super) const OPTIONAL_SECTIONS: usize = 6;

/// The plan review prompt and what it takes.
#[derive(Debug, Clone)]
pub struct PlanReviewPrompt {
    pub text: String,
    pub bytes: PromptBytes,
}

/// The variable sections of the plan review prompt, each as it is
/// written into it.
#[derive(Default)]
pub(super) struct PlanSections {
    tasks: String,
    own_expected: String,
    goals: String,
    lint: String,
    others: String,
    queued: String,
    queued_full: String,
    precedents: String,
    hotspots: String,
    candidates: String,
    predicted: String,
}

/// The lines of a section to keep, in the order given: within `count`
/// lines and `bytes` (each line with its newline). A line that does not
/// fit is skipped and the next one tried, so one huge line does not hide
/// the rest.
pub(super) fn fit_lines(lines: &[String], count: usize, bytes: usize) -> Vec<bool> {
    let mut kept = vec![false; lines.len()];
    let (mut taken, mut used) = (0, 0);
    for (index, line) in lines.iter().enumerate() {
        if taken == count {
            break;
        }
        let size = line.len() + 1;
        if used + size <= bytes {
            kept[index] = true;
            taken += 1;
            used += size;
        }
    }
    kept
}

/// What a fenced block adds to its lines at most.
pub(super) fn fence_overhead(lines: &[String]) -> usize {
    let longest = lines
        .iter()
        .flat_map(|line| line.split(|c| c != '`').map(str::len))
        .max()
        .unwrap_or(0);
    2 * (longest.max(2) + 1) + 16
}

/// `lines` as one fenced JSON block, or `(none)`.
pub(super) fn json_block(lines: &[String]) -> String {
    if lines.is_empty() {
        "(none)".to_owned()
    } else {
        fenced("json", &lines.join("\n"))
    }
}

/// At most [`NOTE_IDS`] of `ids`, with how many more there are.
pub(super) fn id_list(ids: &[i64]) -> String {
    let mut text = ids
        .iter()
        .take(NOTE_IDS)
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if ids.len() > NOTE_IDS {
        text.push_str(&format!(" and {} more", ids.len() - NOTE_IDS));
    }
    text
}

/// The lines of an optional section kept within `limit` and what is left
/// of the prompt's room, with the indices left out.
pub(super) struct Fitted {
    kept: Vec<bool>,
}

impl Fitted {
    fn new(lines: &[String], count: usize, limit: usize, room: usize) -> Self {
        let budget = limit
            .min(room.saturating_sub(OMISSION_NOTE_BYTES))
            .saturating_sub(fence_overhead(lines));
        Self {
            kept: fit_lines(lines, count, budget),
        }
    }

    fn left_out(&self) -> impl Iterator<Item = usize> + '_ {
        self.kept
            .iter()
            .enumerate()
            .filter(|(_, kept)| !**kept)
            .map(|(index, _)| index)
    }

    fn keep<T: Clone>(&self, items: &[T]) -> Vec<T> {
        items
            .iter()
            .zip(&self.kept)
            .filter(|(_, kept)| **kept)
            .map(|(item, _)| item.clone())
            .collect()
    }
}

/// The JSON line of a piece of required material left out of the prompt:
/// what it is and how to read it.
pub(super) fn stub(id: i64, title: Option<&str>, bytes: usize, read: String) -> String {
    let mut stub = serde_json::json!({"id": id, "left_out_bytes": bytes, "read_with": read});
    if let Some(title) = title {
        stub["title"] = crate::application::health::truncate(title, STUB_TITLE_CHARS)
            .unwrap_or_else(|| title.to_owned())
            .into();
    }
    stub.to_string()
}

/// The membership material of a follow_up task for plan review
/// (ADR-t1504-2 decision 7), or `None` for any other task: where it came
/// from (the source goal and its state at registration, the worker's
/// category and membership_proposal, as written or null), every membership judgement (classification, acceptance
/// items, reason, evidence, destination goal, the acceptance version it
/// was judged at and the source goal's current one) and whether the latest
/// one was judged at the current version (`null` without a judgement).
pub(super) fn follow_up_membership(detail: &TaskDetail) -> Option<Value> {
    let origin = detail
        .origin
        .as_ref()
        .filter(|origin| origin.origin == crate::domain::DraftOrigin::FollowUp)?;
    let latest = detail.membership_judgements.last();
    Some(serde_json::json!({
        "origin": origin.material,
        "judgements": detail.membership_judgements,
        "latest_classification": latest.map(|row| row["classification"].clone()),
        "latest_version_matches": latest.map(|row| row["needs_recheck"] == Value::Bool(false)),
    }))
}

/// What the headless plan review is asked: the material, the checks, the
/// fixes it may make itself and the verdict schema (ADR-0041 decisions 10,
/// 11, 14, 15). The repository's own rules are not in the runtime: the job
/// reads them from the repository's documents. Each section is held to its
/// limit and the whole to [`PLAN_REVIEW_PROMPT_LIMIT`]; what is left out is
/// counted and named with the read-only dagq command that reads it (task
/// 1561, ADR-t1566-1).
pub fn plan_review_prompt(material: &PlanReviewMaterial<'_>) -> Result<PlanReviewPrompt> {
    let proposal = material.proposal;
    let to_lines = |values: Vec<Value>| values.iter().map(Value::to_string).collect::<Vec<_>>();
    let mut task_lines = to_lines(
        material
            .tasks
            .iter()
            .map(|detail| {
                let mut task = serde_json::to_value(&detail.task)?;
                task["dependencies"] = serde_json::to_value(&detail.dependencies)?;
                task["goal_dependencies"] = serde_json::to_value(&detail.goal_dependencies)?;
                if let Some(membership) = follow_up_membership(detail) {
                    task["follow_up_membership"] = membership;
                }
                Ok(task)
            })
            .collect::<Result<_>>()?,
    );
    let mut goal_lines = to_lines(
        material
            .goals
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let lint_lines = to_lines(
        material
            .lint
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let no_files = Vec::new();
    let expected = |id: TaskId| material.expected.get(&id).unwrap_or(&no_files);
    let mut expected_lines = to_lines(
        material
            .tasks
            .iter()
            .map(|detail| {
                serde_json::json!({
                    "task_id": detail.task.id(),
                    "expected_files": expected(detail.task.id()),
                })
            })
            .collect(),
    );
    let predicted = material
        .tasks
        .iter()
        .filter(|detail| detail.task.status() == crate::domain::TaskStatus::Submitted)
        .map(|detail| detail.task.id().to_string())
        .collect::<Vec<_>>();
    let mut sections = PlanSections {
        predicted: if predicted.is_empty() {
            "(none)".to_owned()
        } else {
            predicted.join(", ")
        },
        ..PlanSections::default()
    };
    let language = material
        .language
        .map_or(0, |language| language.instruction().len() + 2);
    let fixed = plan_review_text(material, &PlanSections::default()).len() + language;
    let mut omitted: BTreeMap<&'static str, usize> = BTreeMap::new();

    // The required sections, cut to their limit by replacing their largest
    // pieces with how to read them.
    let mut lint = json_block(&lint_lines);
    let before = fixed
        + json_block(&task_lines).len()
        + json_block(&expected_lines).len()
        + json_block(&goal_lines).len()
        + lint.len()
        + sections.predicted.len();
    let mut over_limit = None;
    let mut over_note = String::new();
    if before > PLAN_REVIEW_REQUIRED_LIMIT {
        let room = PLAN_REVIEW_REQUIRED_LIMIT - OVER_LIMIT_NOTE_BYTES;
        // What the required sections take at most, kept as pieces are
        // replaced: each line with its newline and each block's fences.
        let lines_bytes = |lines: &[String]| {
            lines.iter().map(|line| line.len() + 1).sum::<usize>() + fence_overhead(lines)
        };
        let mut estimate = fixed
            + lines_bytes(&task_lines)
            + lines_bytes(&expected_lines)
            + lines_bytes(&goal_lines)
            + lint.len()
            + sections.predicted.len();
        // (bytes, section, index): the largest first, then the section's
        // order, then the piece's.
        let mut pieces: Vec<(usize, usize, usize)> = Vec::new();
        pieces.extend(task_lines.iter().enumerate().map(|(i, l)| (l.len(), 0, i)));
        pieces.extend(
            expected_lines
                .iter()
                .enumerate()
                .map(|(i, l)| (l.len(), 1, i)),
        );
        pieces.extend(goal_lines.iter().enumerate().map(|(i, l)| (l.len(), 2, i)));
        if !lint_lines.is_empty() {
            pieces.push((lint.len(), 3, 0));
        }
        pieces.retain(|&(bytes, _, _)| bytes > STUB_BYTES);
        pieces.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        for (bytes, section, index) in pieces {
            if estimate <= room {
                break;
            }
            let replaced = match section {
                0 => {
                    let task = &material.tasks[index].task;
                    let id = task.id().as_i64();
                    *omitted.entry("tasks").or_default() += 1;
                    let line = &mut task_lines[index];
                    *line = stub(
                        id,
                        Some(task.title()),
                        bytes,
                        format!("dagq show {id} --full"),
                    );
                    line.len()
                }
                1 => {
                    let id = material.tasks[index].task.id().as_i64();
                    *omitted.entry("expected_files").or_default() += 1;
                    let line = &mut expected_lines[index];
                    *line = stub(
                        id,
                        None,
                        bytes,
                        format!(
                            "dagq show {id} --full (its declared paths without wildcards; without any, dagq related {id})"
                        ),
                    );
                    line.len()
                }
                2 => {
                    let goal = &material.goals[index];
                    let id = goal.id().as_i64();
                    *omitted.entry("goals").or_default() += 1;
                    let line = &mut goal_lines[index];
                    *line = stub(
                        id,
                        Some(goal.title()),
                        bytes,
                        format!("dagq goal show {id} --full"),
                    );
                    line.len()
                }
                _ => {
                    *omitted.entry("lint").or_default() += lint_lines.len();
                    lint = format!(
                        "({} findings, {bytes} bytes, left out: read them with `dagq lint --proposal {}`)",
                        lint_lines.len(),
                        proposal.id()
                    );
                    lint.len()
                }
            };
            estimate = (estimate + replaced).saturating_sub(bytes);
        }
        // Too many pieces for even their stubs: each list becomes one note.
        let proposal_id = proposal.id();
        if estimate > room {
            for (section, count) in [
                ("tasks", task_lines.len()),
                ("expected_files", expected_lines.len()),
                ("goals", goal_lines.len()),
            ] {
                if count > 0 {
                    omitted.insert(section, count);
                }
            }
            task_lines.clear();
            expected_lines.clear();
            goal_lines.clear();
            sections.predicted = format!(
                "every submitted task of the proposal ({} of them; `dagq proposal show {proposal_id}` lists them)",
                predicted.len()
            );
        }
        let reason = format!(
            "the required sections took {before} bytes, over their limit of {PLAN_REVIEW_REQUIRED_LIMIT}: their largest pieces are replaced by how to read them"
        );
        over_note = format!(
            "({reason}, with the read-only dagq commands each names (`dagq proposal show {proposal_id}` lists the proposal's tasks and goals); read them before you decide.)\n"
        );
        over_limit = Some(reason);
    }
    sections.tasks = if task_lines.is_empty() && omitted.contains_key("tasks") {
        format!(
            "{over_note}({} tasks, left out: list them with `dagq proposal show {}` and read each with `dagq show ID --full`)",
            material.tasks.len(),
            proposal.id()
        )
    } else {
        format!("{over_note}{}", json_block(&task_lines))
    };
    sections.own_expected = if expected_lines.is_empty() && omitted.contains_key("expected_files") {
        "(left out: read each task's declared paths without wildcards with `dagq show ID --full`; without any, `dagq related ID`)".to_owned()
    } else {
        json_block(&expected_lines)
    };
    sections.goals = if goal_lines.is_empty() && omitted.contains_key("goals") {
        format!(
            "({} goals, left out: read each with `dagq goal show ID --full`)",
            material.goals.len()
        )
    } else {
        json_block(&goal_lines)
    };
    sections.lint = lint;
    // A note's bytes are kept free for each optional section, which still
    // writes `(none)` or its note past the room's end.
    let mut room = PLAN_REVIEW_PROMPT_LIMIT.saturating_sub(
        OPTIONAL_SECTIONS * OMISSION_NOTE_BYTES
            + fixed
            + sections.tasks.len()
            + sections.own_expected.len()
            + sections.goals.len()
            + sections.lint.len()
            + sections.predicted.len(),
    );

    // The optional sections, in the order they keep their room: the
    // hotspots, the duplicate candidates, the other proposals, the asks a
    // person answered, the full text, the summaries. What bears on the
    // dependencies and duplicates comes first; the summaries, which a CLI
    // list replaces most easily, come last.
    let touching = |path: &str, ids: &mut dyn Iterator<Item = TaskId>| {
        ids.filter(|&id| crate::domain::claim_defer::touches(expected(id), path))
            .collect::<Vec<_>>()
    };
    // Each hotspot with the tasks expected to touch it; a queued task on
    // the same hotspot as a task of the proposal may be given in full.
    let mut eligible = BTreeSet::new();
    let mut shared_hotspots: BTreeMap<TaskId, usize> = BTreeMap::new();
    let mut hotspot_lines = Vec::new();
    for file in material.hotspots {
        let path = file.renamed_to.as_deref().unwrap_or(&file.path);
        let own = touching(path, &mut material.tasks.iter().map(|d| d.task.id()));
        let queued = touching(path, &mut material.queued.iter().map(|item| item.id));
        if !own.is_empty() {
            eligible.extend(queued.iter().copied());
            for &id in &queued {
                *shared_hotspots.entry(id).or_default() += 1;
            }
        }
        hotspot_lines.push(
            serde_json::json!({
                "path": path,
                "conflicts": file.conflicts, "tasks": file.tasks,
                "landings": file.landings, "ratio": file.ratio,
                "last_conflict_at": file.last_conflict_at, "alert": file.alert,
                "proposal_tasks": own, "queued_tasks": queued,
            })
            .to_string(),
        );
    }
    let fitted = Fitted::new(&hotspot_lines, usize::MAX, HOTSPOT_BYTES, room);
    let left_out = fitted.left_out().count();
    sections.hotspots = json_block(&fitted.keep(&hotspot_lines));
    if left_out > 0 {
        omitted.insert("hotspots", left_out);
        sections.hotspots.push_str(&format!(
            "\n({left_out} more files are left out by the limit of {HOTSPOT_BYTES} bytes; read them with `dagq stats` (conflict_hotspots))"
        ));
    }
    room = room.saturating_sub(sections.hotspots.len());

    // The best place of each queued task among the candidates.
    let mut candidate_rank: BTreeMap<TaskId, usize> = BTreeMap::new();
    for candidates in material.candidates {
        let related = candidates
            .related
            .iter()
            .map(|task| TaskId::new(task.id))
            .enumerate();
        let search = candidates
            .search
            .iter()
            .filter_map(|hit| match (hit.kind, &hit.id) {
                (
                    crate::domain::search::SearchKind::Task,
                    crate::domain::search::SearchRef::Id(id),
                ) => Some(TaskId::new(*id)),
                _ => hit.task_id.map(TaskId::new),
            })
            .enumerate();
        for (place, id) in related.chain(search) {
            eligible.insert(id);
            let best = candidate_rank.entry(id).or_insert(place);
            *best = (*best).min(place);
        }
    }
    let candidate_lines = to_lines(
        material
            .candidates
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<_>>()?,
    );
    let fitted = Fitted::new(&candidate_lines, usize::MAX, CANDIDATE_BYTES, room);
    let left_out: Vec<i64> = fitted
        .left_out()
        .map(|index| material.candidates[index].task_id.as_i64())
        .collect();
    sections.candidates = json_block(&fitted.keep(&candidate_lines));
    if !left_out.is_empty() {
        omitted.insert("candidates", left_out.len());
        sections.candidates.push_str(&format!(
            "\n(the candidates of {} tasks of the proposal are left out by the limit of {CANDIDATE_BYTES} bytes: {}; look for them with `dagq related ID` and `dagq search '<words of its title>'`)",
            left_out.len(),
            id_list(&left_out)
        ));
    }
    room = room.saturating_sub(sections.candidates.len());

    let other_blocks: Vec<String> = material
        .others
        .iter()
        .map(|(other, tasks)| {
            let earlier =
                (other.submitted_at(), other.id()) < (proposal.submitted_at(), proposal.id());
            let tasks = tasks
                .iter()
                .map(|task| {
                    serde_json::json!({
                        "id": task.id(), "status": task.status(), "title": task.title(),
                        "description": task.description(), "acceptance": task.acceptance(),
                        "paths": task.paths(),
                    })
                    .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "Proposal {} ({}, submitted {}, {} this one):\n{}",
                other.id(),
                other.status().as_str(),
                other.submitted_at(),
                if earlier { "before" } else { "after" },
                fenced("json", &tasks)
            )
        })
        .collect();
    let fitted = Fitted::new(&other_blocks, usize::MAX, OTHER_PROPOSAL_BYTES, room);
    let left_out: Vec<i64> = fitted
        .left_out()
        .map(|index| material.others[index].0.id().as_i64())
        .collect();
    let kept = fitted.keep(&other_blocks);
    sections.others = if kept.is_empty() && left_out.is_empty() {
        "(none)".to_owned()
    } else {
        kept.join("\n")
    };
    if !left_out.is_empty() {
        omitted.insert("other_proposals", left_out.len());
        sections.others.push_str(&format!(
            "\n({} more proposals are left out by the limit of {OTHER_PROPOSAL_BYTES} bytes: {}; read one with `dagq proposal show ID` and its tasks with `dagq show ID --full`)",
            left_out.len(),
            id_list(&left_out)
        ));
    }
    room = room.saturating_sub(sections.others.len());

    let precedent_lines: Vec<String> = material
        .precedents
        .iter()
        .map(|ask| format!("- {}", precedent_line(ask)))
        .collect();
    let fitted = Fitted::new(&precedent_lines, PRECEDENT_ASKS, PRECEDENT_BYTES, room);
    let left_out = fitted.left_out().count();
    let kept = fitted.keep(&precedent_lines);
    sections.precedents = if kept.is_empty() && left_out == 0 {
        "(none)".to_owned()
    } else {
        kept.join("\n")
    };
    if left_out > 0 {
        omitted.insert("precedents", left_out);
        sections.precedents.push_str(&format!(
            "\n({left_out} more of the newest asks a person answered are left out by the limit of {PRECEDENT_ASKS} asks and {PRECEDENT_BYTES} bytes; read them with `dagq asks --all`)"
        ));
    }
    if material.precedents.len() >= PRECEDENT_ASKS {
        sections.precedents.push_str(
            "\n(older asks a person answered are not listed: read them with `dagq asks --all`)",
        );
    }
    room = room.saturating_sub(sections.precedents.len());

    // The full text: the eligible tasks, the most related to the proposal
    // first: one a task of the proposal depends on, then the most hotspots
    // shared with the proposal, the most of its expected files shared, the
    // best place among the candidates, the newest.
    let own_files: Vec<&String> = material
        .tasks
        .iter()
        .flat_map(|detail| expected(detail.task.id()))
        .collect();
    let depended: BTreeSet<TaskId> = material
        .tasks
        .iter()
        .flat_map(|detail| detail.dependencies.iter().copied())
        .collect();
    let shared_files = |id: TaskId| {
        let files = expected(id);
        own_files
            .iter()
            .filter(|own| {
                crate::domain::claim_defer::touches(files, own)
                    || files.iter().any(|file| {
                        crate::domain::claim_defer::touches(std::slice::from_ref(own), file)
                    })
            })
            .count()
    };
    // (a task of the proposal depends on it, hotspots shared, expected
    // files shared, best place among the candidates)
    type Relation = (bool, usize, usize, usize);
    let mut ranked: Vec<(&TaskListItem, Relation)> = material
        .queued
        .iter()
        .filter(|item| eligible.contains(&item.id))
        .map(|item| {
            (
                item,
                (
                    depended.contains(&item.id),
                    shared_hotspots.get(&item.id).copied().unwrap_or(0),
                    shared_files(item.id),
                    candidate_rank.get(&item.id).copied().unwrap_or(usize::MAX),
                ),
            )
        })
        .collect();
    ranked.sort_by(|(a, ka), (b, kb)| {
        kb.0.cmp(&ka.0)
            .then(kb.1.cmp(&ka.1))
            .then(kb.2.cmp(&ka.2))
            .then(ka.3.cmp(&kb.3))
            .then(b.id.cmp(&a.id))
    });
    let ranked: Vec<&TaskListItem> = ranked.into_iter().map(|(item, _)| item).collect();
    let full_lines = ranked
        .iter()
        .map(serde_json::to_string)
        .collect::<serde_json::Result<Vec<_>>>()?;
    let fitted = Fitted::new(&full_lines, QUEUED_FULL_TASKS, QUEUED_FULL_BYTES, room);
    let full: BTreeSet<TaskId> = fitted.keep(&ranked).iter().map(|item| item.id).collect();
    let left_out: Vec<i64> = fitted
        .left_out()
        .map(|index| ranked[index].id.as_i64())
        .collect();
    sections.queued_full = json_block(&fitted.keep(&full_lines));
    if !left_out.is_empty() {
        omitted.insert("full_text", left_out.len());
        sections.queued_full.push_str(&format!(
            "\n({} more ready or in-progress tasks that meet this rule are left out by its limit of {QUEUED_FULL_TASKS} tasks and {QUEUED_FULL_BYTES} bytes, the most related to the proposal first: {}; read one in full with `dagq show ID --full`)",
            left_out.len(),
            id_list(&left_out)
        ));
    }
    room = room.saturating_sub(sections.queued_full.len());

    // The summaries: the eligible tasks in the order above, then the rest,
    // newest first; written newest first.
    let summary_lines: Vec<String> = material
        .queued
        .iter()
        .map(|item| {
            let files = expected(item.id);
            let mut summary = serde_json::json!({
                "id": item.id, "status": item.status, "priority": item.priority,
                "goal_id": item.goal_id, "title": item.title,
                "paths": item.details.as_ref().map(|details| &details.paths).unwrap_or(&no_files),
                "dependencies": item.dependencies, "goal_dependencies": item.goal_dependencies,
                "expected_files": files.iter().take(SUMMARY_EXPECTED_FILES).collect::<Vec<_>>(),
            });
            if files.len() > SUMMARY_EXPECTED_FILES {
                summary["more_expected_files"] = (files.len() - SUMMARY_EXPECTED_FILES).into();
            }
            if full.contains(&item.id) {
                summary["full_text_below"] = true.into();
            }
            summary.to_string()
        })
        .collect();
    let place: BTreeMap<TaskId, usize> = material
        .queued
        .iter()
        .enumerate()
        .map(|(index, item)| (item.id, index))
        .collect();
    let order: Vec<usize> = ranked
        .iter()
        .map(|item| place[&item.id])
        .chain(
            material
                .queued
                .iter()
                .enumerate()
                .filter(|(_, item)| !eligible.contains(&item.id))
                .map(|(index, _)| index),
        )
        .collect();
    let ordered: Vec<String> = order.iter().map(|&i| summary_lines[i].clone()).collect();
    let fitted = Fitted::new(&ordered, usize::MAX, QUEUED_SUMMARY_BYTES, room);
    let mut kept = vec![false; summary_lines.len()];
    for (&index, &keep) in order.iter().zip(&fitted.kept) {
        kept[index] = keep;
    }
    let cut = fitted.left_out().count();
    let shown: Vec<String> = summary_lines
        .iter()
        .zip(&kept)
        .filter(|(_, keep)| **keep)
        .map(|(line, _)| line.clone())
        .collect();
    sections.queued = json_block(&shown);
    if material.queued_left_out > 0 {
        sections.queued.push_str(&format!(
            "\n({} more ready or in-progress tasks, those of the lowest IDs, are left out of this list)",
            material.queued_left_out
        ));
    }
    if cut > 0 {
        sections.queued.push_str(&format!(
            "\n({cut} more ready or in-progress tasks, the least related to the proposal, are left out of this list by its limit of {QUEUED_SUMMARY_BYTES} bytes; list them with `dagq list --status ready,in_progress --limit 200` and read one with `dagq show ID --full`)"
        ));
    }
    if cut + material.queued_left_out > 0 {
        omitted.insert("summaries", cut + material.queued_left_out);
    }

    let body = plan_review_text(material, &sections);
    let body_len = body.trim_end().len();
    let text = crate::domain::language::with_instruction(body, material.language);
    let mut bytes = BTreeMap::from([
        ("tasks", sections.tasks.len()),
        ("expected_files", sections.own_expected.len()),
        ("goals", sections.goals.len()),
        ("lint", sections.lint.len()),
        ("other_proposals", sections.others.len()),
        ("summaries", sections.queued.len()),
        ("full_text", sections.queued_full.len()),
        ("precedents", sections.precedents.len()),
        ("hotspots", sections.hotspots.len()),
        ("candidates", sections.candidates.len()),
        (
            "language",
            material.language.map_or(0, |_| text.len() - body_len),
        ),
    ]);
    let counted: usize = bytes.values().sum();
    bytes.insert("instructions", text.len() - counted);
    debug_assert!(text.len() <= PLAN_REVIEW_PROMPT_LIMIT, "{}", text.len());
    Ok(PlanReviewPrompt {
        bytes: PromptBytes {
            total: text.len(),
            limit: PLAN_REVIEW_PROMPT_LIMIT,
            sections: bytes,
            omitted,
            over_limit,
        },
        text,
    })
}

/// The plan review prompt with its variable sections written in.
pub(super) fn plan_review_text(
    material: &PlanReviewMaterial<'_>,
    sections: &PlanSections,
) -> String {
    let proposal = material.proposal;
    let PlanSections {
        tasks,
        own_expected,
        goals,
        lint,
        others,
        queued,
        queued_full,
        precedents,
        hotspots,
        candidates,
        predicted,
    } = sections;
    format!(
        "You are the plan review of dagq proposal {id}: decide whether the queue may run its tasks as written, before they become ready.\n\
         Read only. Do not change any file and do not run dagq commands that write: the runtime lets you run the dagq commands that read (`dagq show ID`, `dagq proposal show ID`, `dagq search`, `dagq related`, `dagq findings`, `dagq stats`, ...) and refuses the rest.\n\
         {RECORD_READING}\n\
         The material below is held to limits: a list cut to fit says how many it left out and which read-only dagq command reads them; read what you need with it.\n\n\
         First read the repository's own rules in {repo}: its instructions (AGENTS.md and CLAUDE.md), the documents and rules they name (the plan review's part of them above all), and the documents the tasks name. \
         Apply what they say (the verification each kind of change needs, the declared paths, the required evidence, the rules for the records they keep, ...); the runtime has no such rules of its own. \
         Where the repository has no AGENTS.md, judge a task's verification, paths and evidence in this order: CLAUDE.md; then what the README, the CI configuration and the build configuration show; when none of them settles it, it needs a person: a concern.\n\n\
         The proposal was submitted {submitted} and was sent back {revises} time(s) before (at most {max}; a revise past that goes to a person as a concern).\n\
         Each task of the proposal and each goal below records where it comes from (origin: human, by a person's request or a person's own add; ai, by a planner or the runtime on its own; unknown, from before the queue recorded it, which counts as a person's) and who set its priority (priority_by: human for a person; a task's is its own priority's setter, else its goal's).\n\n\
         Tasks of the proposal:\n{tasks}\n\n\
         Files each task of the proposal is expected to touch (its declared paths without wildcards; without any, the files the landings of its 3 most related completed tasks changed; a guess, so check it against the source):\n{own_expected}\n\n\
         Goals they belong to (description, acceptance, constraints; constraints win over a task's description):\n{goals}\n\n\
         The mechanical checks (`dagq lint`) found:\n{lint}\n\n\
         Other proposals not ready yet:\n{others}\n\n\
         Ready and in-progress tasks, in summary, newest first (expected_files as for the proposal's tasks, and for an in-progress task also what its run changed so far; full_text_below marks a task given in full below; `dagq show ID --full` reads any of them in full):\n{queued}\n\n\
         In full, the ready and in-progress tasks among the candidates below or expected to touch a hotspot a task of the proposal is expected to touch, the most related to the proposal first:\n{queued_full}\n\n\
         Asks a person answered before (newest first):\n{precedents}\n\n\
         Files the landings conflicted in most lately (`dagq stats` conflict_hotspots: conflicts, tasks, landings that changed the file, their ratio; alert when over the thresholds), each with the tasks of the proposal (proposal_tasks) and the ready and in-progress tasks (queued_tasks) expected to touch it:\n{hotspots}\n\n\
         Candidates of duplicates and of changes already made, one line per task of the proposal (related: the tasks `dagq related` ranks highest, in any status, with the clues that relate them; search: the tasks and landed commits `dagq search` finds for the words of the task's title, with their status; at most {most} of each, none of the proposal's own tasks; an empty list means none was found):\n{candidates}\n\n\
         Check the meaning of the plan:\n\
         - a task that repeats another task (ready, in progress, in another proposal, or already landed); start from its candidates above, a completed or canceled one included, and judge from their titles, clues and the source whether the task really repeats one;\n\
         - a task whose change has already landed (read the source; a completed candidate or a landed commit is where to look);\n\
         - a contradiction with a decision the repository records (in its instructions or the decision records they name) or with the goal's constraints;\n\
         - an acceptance criterion that contradicts the task's own description or a sibling task's acceptance (for example a change of a type whose acceptance says a test file that uses the type is not changed);\n\
         - dependencies: a task depends on another only when its work needs the other's landing first (a prerequisite of its content). Sharing a file or a hotspot alone is no reason for a dependency: the runtime defers for a while the claim of a task that overlaps a run in progress on a file that conflicts often, and a rebase or the landing settles any other overlap, so add none for it (the hotspots above are for finding repeats and partial overlaps). A missing prerequisite is a revise, or add_dependency when it is plain; you may read the source to see which files and code a task of the proposal really needs. A dependency of a task of a higher-priority goal on a task of a lower-priority goal makes the lower one inherit the higher priority: read its reason in the tasks' notes or context, and revise when it only avoids a conflict on the same files or gives no reason;\n\
         - a task that partly repeats a ready or in-progress task (the overlap goes once the scope of one is cut): not pass but revise, saying in the reason which part to cut and which of the two keeps it;\n\
         - a contradiction with another proposal: with one submitted before this one, send this one back; with one submitted after, pass this one (the later one is checked against it);\n\
         - a ready task that has to change for this proposal to hold: name it in reopen, and the runtime takes it out of the claim for a planner to fix; an in-progress task is never changed: send this proposal back asking for a task that fixes it after it lands and depends on it;\n\
         - a follow_up's membership (its follow_up_membership: the planner's judgements of whether its source goal's acceptance needs it, each with the acceptance items, reason, evidence, destination goal and the acceptance version it was judged at against the source goal's current one): start from the planner's mapping and check that it holds against the source goal's acceptance; you need not repeat the whole investigation, but never pass a judgement on its form alone. When it looks doubtful (the reason names no item of the acceptance, the evidence disagrees with the receipt or the diff it cites, the destination is an unrelated catch-all goal, the acceptance was weakened so that the follow-up falls out of scope, the version does not match), read the evidence around it (the source run's receipt and commits, `dagq goal show ID --full` for the goal's acceptance and its history) before you decide. A wrong mapping the planner can fix is a revise; an acceptance weakened to drop a follow-up needs a person's intent, a concern;\n\
         - every finding of `dagq lint` is one to fix.\n\n\
         Decide one verdict:\n\
         - pass: the tasks may run as written, after the actions below.\n\
         - revise: findings the planner can fix without a person's judgment (wording, acceptance, verification, paths, a split, a scope that partly overlaps another task, a missing task, a missing dependency on a prerequisite of a task's content, a dependency of a higher-priority goal's task on a lower-priority goal's task that only avoids a conflict on the same files). Each reason says what to change.\n\
         - concern: findings that need a judgment beyond a planner's fix: a doubtful duplicate, a change that looks already done, a contradiction with a decision the repository records or with the goal's constraints, a change of the plan's intent. A concern is not only for a person: you judge it too, with a recommendation and a confidence, and the runtime applies what you are sure of.\n\
         With a concern, give what you recommend and how sure you are; the runtime applies a sure recommendation itself and asks a person only what the record cannot settle. \
         recommendation is ready (the tasks may run as written, after the actions below), send_back (the planner fixes it, as a revise; it counts toward the revises above) or cancel. \
         confidence is high when the queue's records, the repository's documents and decisions and the answered asks settle it, low when they do not or you are unsure. \
         reason_category is scope when your recommendation would let a task through against a decision the repository records, the goal's constraints or a person's precedent; discard when you recommend cancel; null otherwise. \
         A high ready or send_back with reason_category null is applied without a person (a ready as a pass, its actions included); a low confidence, scope, discard, or a send_back past the revises above goes to a person with your recommendation.\n\
         When a finding is of the same kind as an answered ask above, put that ask's id in precedents and say in the reason how the person answered then.\n\n\
         actions are the only changes you make yourself, and only with pass (or a concern whose high ready is applied): add_dependency (a task of the proposal waits for another task whose landing its work needs; never only for a shared file or hotspot), lower_priority (never raise one; on the AI's task it takes the task's own priority off, so the task takes its goal's), cancel_duplicate (only an obvious duplicate; a doubtful one, or a change that looks already made, is a concern). Everything else is the planner's.\n\n\
         Priorities and goals, by those records:\n\
         - a priority a person set (priority_by human), and a task or goal of a person's (origin human or unknown): its priority and the goals its tasks belong to are the person's decision. Do not change them: the runtime refuses to change a priority a person set, and does not apply a lower_priority to it. When one looks wrong, say so in a concern with reason_category scope.\n\
         - the AI's task (origin ai): it has no priority of its own and takes its goal's (a pass takes off an own priority the AI set). Check instead that a new goal's priority follows the repository's rules for goal priorities, and that each task put in an existing goal is needed by that goal's acceptance; when either does not hold, revise, so the planner fixes the goal's priority, or moves the task to a new goal or leaves it with no goal. An improvement (a proposal that remedies a finding) stays below high by the goal it belongs to, not by a priority of its tasks.\n\n\
         Whatever the verdict, also estimate the weight of each submitted task of the proposal (tasks {predicted}), one entry per task in predictions, from what you read: \
         a worker (one Claude Opus session in its own Git worktree) implements the task, runs the checks the repository's instructions ask of a worker (formatting, lint, the tests of the change, ...), commits and writes a receipt; \
         then a headless review (pass / revise / concern) and `integrate`'s verification after the rebase onto main follow, and a failure, a conflict or missing evidence resumes the run. \
         size is S, M or L; nature is mechanical, implementation, design_judgment or investigation; uncertainty is 0 to 1 (1 the least certain); \
         expected_output_tokens is the output tokens (thinking included) of one worker run: a small one about 5000, a large one about 250000, the median about 35000; \
         rework_probability is 0 to 1, the chance the run is resumed or review answers revise or concern; reason is one sentence. The estimate is recorded only and changes nothing of the verdict.\n\n\
         {codes}\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [{{\"text\": string, \"codes\": [string]}}], \"summary\": string, \
         \"actions\": [{{\"action\": \"add_dependency\", \"task_id\": int, \"depends_on\": int}} | {{\"action\": \"lower_priority\", \"task_id\": int, \"priority\": \"low\" | \"normal\" | \"high\" | \"urgent\"}} | {{\"action\": \"cancel_duplicate\", \"task_id\": int, \"duplicate_of\": int}}], \
         \"reopen\": [{{\"task_id\": int, \"reason\": string}}], \"precedents\": [int], \
         \"recommendation\": \"ready\" | \"send_back\" | \"cancel\", \"confidence\": \"high\" | \"low\", \"reason_category\": \"scope\" | \"discard\" | null, \
         \"predictions\": [{{\"task_id\": int, \"size\": \"S\" | \"M\" | \"L\", \"nature\": \"mechanical\" | \"implementation\" | \"design_judgment\" | \"investigation\", \"uncertainty\": number, \"expected_output_tokens\": int, \"rework_probability\": number, \"reason\": string}}]}}\n\
         reasons lists each finding (empty for pass); summary is one or two sentences; actions, reopen and precedents may be empty; recommendation, confidence and reason_category go with a concern only; predictions has one entry for each submitted task and no other.\n",
        id = proposal.id(),
        repo = material.repo_root.display(),
        submitted = proposal.submitted_at(),
        revises = proposal.revise_count(),
        max = MAX_PLAN_REVISES,
        most = DUPLICATE_CANDIDATES,
        codes = reason_codes_section(review_reason::PLAN_REVIEW_CODES),
    )
}

/// What the goal review job is shown about one goal (ADR-0047 decision
/// 43): the goal, each of its tasks with what landed for it, the
/// follow-ups found from it with their membership judgements (ADR-t1504-2),
/// the goal's notes and edits, and the goal's earlier reviews. Each value
/// is one JSON line of the prompt.
pub struct GoalReviewMaterial<'a> {
    pub goal: Value,
    pub tasks: Vec<Value>,
    pub follow_ups: Vec<Value>,
    pub events: Vec<Value>,
    pub previous: Vec<Value>,
    pub gaps_in_a_row: usize,
    pub repo_root: &'a Path,
}

/// The bytes the whole goal review prompt takes at most, the language's
/// instruction included (task 1571, ADR-t1566-1 decision 4): the largest
/// in production, goal review job 24 of 2026-10-02, took 218,427 bytes, of
/// which its 50 tasks took 200,200.
pub const GOAL_REVIEW_PROMPT_LIMIT: usize = 200_000;

/// The bytes of the goal itself (5,241 at most in production): it is
/// required, and past this its longest fields are cut.
pub const GOAL_REVIEW_GOAL_BYTES: usize = 16_000;

/// The bytes of the goal's tasks, each with what landed for it, and of
/// one task: in production a task took 4,000 bytes on average and 12,126
/// at most, most of it its landed receipt.
pub const GOAL_REVIEW_TASKS_BYTES: usize = 110_000;
pub const GOAL_REVIEW_TASK_BYTES: usize = 8_000;

/// The bytes of the stubs (ID, title, status) of the tasks left out.
pub const GOAL_REVIEW_STUB_BYTES: usize = 8_000;

/// The bytes of the follow-ups, of the notes and edits (10,877 at most in
/// production) and of the earlier reviews, and of one item of them.
pub const GOAL_REVIEW_FOLLOW_UPS_BYTES: usize = 16_000;
pub const GOAL_REVIEW_EVENTS_BYTES: usize = 16_000;
pub const GOAL_REVIEW_PREVIOUS_BYTES: usize = 12_000;
pub const GOAL_REVIEW_ITEM_BYTES: usize = 4_000;

/// The read-only dagq commands the goal review prompt names to read what
/// its limits left out, as it writes them (`ID` for a number): each is one
/// the goal review job's role may run (task 1571, ADR-t1566-1 decision 3).
pub const GOAL_REVIEW_READS: &[&str] = &[
    "dagq show ID --full",
    "dagq goal show ID --full",
    "dagq events --full --all --goal ID",
    "dagq events --full --goal ID --kind goal_review_finished",
    "dagq events --full --task ID --kind integration_receipt",
];

/// The prompt of the headless goal review (ADR-0047 decision 43): whether
/// the goal whose tasks all ended met its acceptance. Each section is held
/// to its limit and the whole to [`GOAL_REVIEW_PROMPT_LIMIT`]: the tasks
/// that landed come first and the newest first, the others after them;
/// the notes and the earlier reviews newest first. What is left out is
/// counted and named with the read-only dagq command that reads it (task
/// 1571, ADR-t1566-1).
pub fn goal_review_prompt(material: &GoalReviewMaterial<'_>) -> FittedPrompt {
    let goal_id = material.goal["id"].clone();
    let goal_read = format!("read it whole with `dagq goal show {goal_id} --full`");
    let mut fit = Fit::new(GOAL_REVIEW_PROMPT_LIMIT);
    let block = |lines: &[String]| {
        if lines.is_empty() {
            "(none)".to_owned()
        } else {
            fenced("json", &lines.join("\n"))
        }
    };
    let goal = match shrink(&material.goal, GOAL_REVIEW_GOAL_BYTES, &goal_read) {
        Some((shrunk, left_out)) => {
            fit.omit("goal", 1);
            fit.over(format!(
                "goal: {left_out} bytes left out by its limit of {GOAL_REVIEW_GOAL_BYTES}"
            ));
            shrunk.to_string()
        }
        None => material.goal.to_string(),
    };
    let goal = block(&[goal]);
    fit.section("goal", &goal);
    // The tasks that landed, newest first, then the others, newest first.
    let tasks = &material.tasks;
    let landed = |task: &Value| task.get("landed").is_some();
    let order: Vec<usize> = (0..tasks.len())
        .rev()
        .filter(|&index| landed(&tasks[index]))
        .chain(
            (0..tasks.len())
                .rev()
                .filter(|&index| !landed(&tasks[index])),
        )
        .collect();
    let task_read = "read it whole with `dagq show ID --full` and what landed for it with `dagq events --full --task ID --kind integration_receipt`";
    let (kept, left_out) = fit.lines(
        "tasks",
        tasks,
        order,
        (usize::MAX, GOAL_REVIEW_TASKS_BYTES, GOAL_REVIEW_TASK_BYTES),
        task_read,
    );
    let mut task_text = block(&kept);
    if !left_out.is_empty() {
        let stubs: Vec<String> = left_out
            .iter()
            .map(|&index| {
                let task = &tasks[index];
                serde_json::json!({"id": task["id"], "title": task["title"], "status": task["status"], "landed": landed(task)}).to_string()
            })
            .collect();
        let sizes: Vec<usize> = stubs.iter().map(String::len).collect();
        let shown = prompt_fit::pick(&sizes, 0..stubs.len(), usize::MAX, GOAL_REVIEW_STUB_BYTES);
        let shown: Vec<String> = stubs
            .into_iter()
            .zip(&shown)
            .filter(|(_, shown)| **shown)
            .map(|(stub, _)| stub)
            .collect();
        let ids: Vec<String> = left_out
            .iter()
            .map(|&index| tasks[index]["id"].to_string())
            .collect();
        task_text.push_str(&format!(
            "The tasks left out, in summary:\n{}{}",
            block(&shown),
            left_out_note(
                "tasks",
                &ids,
                "`dagq show ID --full` for each, and `dagq events --full --task ID --kind integration_receipt` for what landed for it"
            )
        ));
    }
    fit.section("tasks", &task_text);
    let newest_first = |items: &[Value]| (0..items.len()).rev().collect::<Vec<_>>();
    let listed = |fit: &mut Fit,
                  name: &'static str,
                  items: &[Value],
                  bytes: usize,
                  what: &str,
                  read: &str| {
        let (kept, left_out) = fit.lines(
            name,
            items,
            newest_first(items),
            (usize::MAX, bytes, GOAL_REVIEW_ITEM_BYTES),
            read,
        );
        let mut text = block(&kept);
        if !left_out.is_empty() {
            let ids: Vec<String> = left_out
                .iter()
                .map(|&index| {
                    let item = &items[index];
                    item.get("task_id")
                        .or_else(|| item.get("id"))
                        .or_else(|| item.get("at"))
                        .map_or_else(|| format!("#{}", index + 1), Value::to_string)
                })
                .collect();
            text.push_str(&left_out_note(what, &ids, read));
        }
        fit.section(name, &text);
        text
    };
    let follow_ups = listed(
        &mut fit,
        "follow_ups",
        &material.follow_ups,
        GOAL_REVIEW_FOLLOW_UPS_BYTES,
        "follow-ups",
        &format!("`dagq goal show {goal_id} --full`, and `dagq show ID --full` for each"),
    );
    let events = listed(
        &mut fit,
        "events",
        &material.events,
        GOAL_REVIEW_EVENTS_BYTES,
        "notes and edits (the oldest)",
        &format!("`dagq events --full --all --goal {goal_id}`"),
    );
    let previous = listed(
        &mut fit,
        "previous",
        &material.previous,
        GOAL_REVIEW_PREVIOUS_BYTES,
        "earlier reviews (the oldest)",
        &format!("`dagq events --full --goal {goal_id} --kind goal_review_finished`"),
    );
    fit.finish(format!(
        "You are the goal review of the dagq queue, a headless job. Every task of goal {goal_id} ended (completed or canceled): judge whether the goal met its acceptance. Change nothing: read the repository's documents and source in {repo} (the main checkout, where the tasks landed) and run read-only dagq commands (`dagq show ID`, `dagq goal show {goal_id} --full`, `dagq findings`, `dagq events --goal {goal_id} --full`, `dagq search ...`) as you need. \
         The material below is held to limits: a section that left something out says how many and how to read them, and an item cut short says so in its `cut`.\n\n\
         The goal:\n{goal}\n\n\
         Its tasks, each with the run that landed it (the receipt's summary, its evidence and its follow_ups) when it was completed by a run:\n{task_text}\n\n\
         The follow-ups registered from its tasks' receipts, or that belong to it now, wherever they belong (goal_id), with their membership judgements (classification, the acceptance items, reason, evidence, acceptance_version against current_acceptance_version, needs_recheck):\n{follow_ups}\n\n\
         The goal's notes, edits and earlier decisions:\n{events}\n\n\
         The goal's earlier reviews (gaps verdicts in a row before this one: {gaps}; after {max} in a row a gaps verdict is turned into a question to a person):\n{previous}\n\n\
         Split the acceptance into its items and check each against what landed, with evidence you saw (a commit, a file, a test, a receipt). \
         A follow-up judged out_of_scope is not part of the acceptance: leave its work out of your judgement and do not wait for it or list it as a gap. A follow-up judged required is part of it: judge the goal with its work, as with the goal's own tasks. \
         The runtime starts this review only when each follow-up of the goal that is not completed or canceled is judged against the current acceptance. Then answer one of:\n\
         - achieved: every item is met. The runtime closes the goal as achieved and records your criteria.\n\
         - gaps: some items are not met and the work to meet them is clear and within the goal. List each missing piece as a gap with a title and a description a planner can turn into a task; the runtime registers each as a draft of the goal and a planner of the runtime decides it. The goal stays open.\n\
         - ask: only when a person has to decide: the acceptance should change, the goal should be abandoned or split, or you cannot judge it. Write the question; the person answers achieved, abandoned, gaps (the gaps you listed, or `gaps: <what>`) or keep_open. reason_category is scope (the acceptance, the scope or a decision changes) or discard (work would be thrown away).\n\n\
         Answer with one JSON object and nothing else, matching this schema:\n\
         {{\"verdict\": \"achieved\" | \"gaps\" | \"ask\", \"criteria\": [{{\"criterion\": string, \"met\": bool, \"evidence\": [string]}}], \"gaps\": [{{\"title\": string, \"description\": string, \"criterion\": string}}], \"summary\": string, \"question\": string, \"options\": [string], \"reason_category\": \"scope\" | \"discard\"}}\n\
         criteria has one entry for each item of the acceptance; gaps is empty unless the verdict is gaps (or ask, to offer them); summary is one or two sentences; question, options and reason_category are for ask only.\n",
        repo = material.repo_root.display(),
        gaps = material.gaps_in_a_row,
        max = crate::domain::goal_review::MAX_GOAL_GAPS,
    ))
}

/// What the supervisor sends the live planner a revise goes back to
/// (ADR-0041 decisions 12, 13): a planner of the runtime's gets it as its
/// next turn (ADR-t1433-2); nothing is typed into a planner.
pub fn plan_revise_request(proposal: ProposalId, reasons: &[String]) -> String {
    let reasons = if reasons.is_empty() {
        "- (none given)".to_owned()
    } else {
        reasons
            .iter()
            .map(|reason| format!("- {reason}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Plan review sent proposal {proposal} back. Fix what these reasons point at:\n{reasons}\n\
         Then submit it again with `dagq submit --proposal {proposal}`. A fix that changes the plan's intent (acceptance, scope, the relation to the goal) needs the person: ask them here first."
    )
}
