---
name: dagq
description: Register and inspect dagq goals and tasks through the locally built dagq binary. Use when the user brings a development problem to queue for dagq (register it as a goal, decompose it into tasks with title, description, acceptance criteria, verification commands, dependencies and context), lint and submit them for plan review, edit a draft or submitted task, list goals or tasks, check a goal's progress or a task's status or run result, close a goal after reviewing its tasks' receipts and follow_ups, adopt or reject a draft goal, decide findings, read events, run timelines, KPIs and completion forecasts, mark a change, record or read notes, or find the dagq binary and queue database.
---

# dagq: register and inspect tasks

dagq runs development tasks in cmux workspaces and isolated Git worktrees. This skill drives the `dagq` binary; every command prints JSON on stdout, and a runtime error prints `{"error": ...}` on stderr with exit status 1. Never read or modify the SQLite queue file directly (no `sqlite3`, no editing); the binary is the only interface.

A goal is the problem several tasks solve together; a task is one unit of work a session executes in its own worktree. Registering, submitting and closing belong to a planner session (`dagq-planner`); the supervisor runs a headless plan review of each submitted proposal, then runs and lands the queue; its asks and attention go to the inbox (`dagq-inbox`); what a person does by hand (up / down, recovery, a review by hand) is `dagq-recover`.

Reference files, read only when needed, all in `${CLAUDE_PLUGIN_ROOT}/skills/dagq/`: `reference/locate.md` (install, version warnings, missing or moved queue), `reference/inspect.md` (inspect commands, fields, statuses, priority, editing), `reference/register.md` (registering in detail), `reference/goal-close.md` (closing a goal), `reference/kpi.md` (KPIs, change marks, the weekly throughput review, forecasts, reports, push), `reference/provider.md` (a worker's provider and route, fallbacks, turns) and `reference/authority.md` (what each role is refused, the inbox's delegated record).

Every state change is checked against your role (`DAGQ_ROLE`) by a default-deny policy; a refusal (`<role> may not ...`) changes nothing and is recorded. Take it as the answer and never work around it; the check is advisory on the host, not a sandbox (`reference/authority.md`).

## 1. Locate the binary and the queue

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/dagq" --resolve
```

It prints `binary`, `binary_version`, `plugin_version` and the queue (`db`, `db_exists`, `runs_dir`, `source`). Report `binary_version` and `db` to the user once per session. Then:

- `{"error": ...}` (no binary): pass on its install steps; retry once installed.
- `{"warning": ...}` on stderr (plugin and binary differ in major.minor): report it and continue.
- `db_exists: false`: run `"${CLAUDE_PLUGIN_ROOT}/bin/dagq" init` once, unless the repository was moved or renamed; then do not `init` and read `reference/locate.md`.

The queue is per repository, resolved from the current directory: run dagq in the tasks' repository. Use `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` below.

## 2. Register a goal and decompose it into tasks

Hear the problem → `goal add` → decompose it into tasks, each registered with `add --goal` → `lint` and `submit` them for plan review, which makes them `ready`. Look for duplicates and done work with `search` before `add` and `related ID` before `submit`; cancel one with `--duplicate-of X` (`reference/inspect.md`). A task's prompt shows its goal, its dependencies' receipts and siblings in progress, so siblings agree on names.

Skip the goal only for a one-shot task that finishes the problem by itself; when a second task will exist, a later one needs this one's decisions, or unsure, register a goal.

### Register the goal

Collect title, description, acceptance, constraints and doc (what each holds, and draft goals: `reference/register.md`).

```sh
"$DAGQ" goal add "TITLE" --description "..." --acceptance "..." --constraints "..." --doc docs/adr/NNNN-name.md
```

A goal has no verification commands; a goal-level check is a final task depending on the others. `goal add --draft` makes a draft goal, adopted by `submit --goal ID` or rejected by `goal close ID --verdict abandoned`. Findings and proposals: `reference/observer.md`.

### Register the tasks

Split the goal into tasks, each one session in one worktree. Per task: title, description, acceptance, `--verify`, `--depends-on` (or `--depends-on-goal`), `--context`, `--evidence`, `--paths`, `--change` (the kind of change; the values are the repository's `[tasks] changes`, their meaning its rules), and the worker's `--provider claude|codex` / `--headless` (default: Claude interactive; `edit` changes them; `reference/provider.md`). `--kind` is still accepted but is going away: give it to no new task. What each means is in `reference/register.md`, the combinations per changed target in `reference/scope.md`.

```sh
"$DAGQ" add "TITLE" --goal 1 \
  --description "..." --acceptance "..." --context "..." \
  --verify "cargo fmt --all --check" --verify "cargo test --locked" \
  --change feature --depends-on 3
"$DAGQ" lint ID...               # the fixed rules; each violation {code, task_id, reason}
"$DAGQ" submit ID...             # or --goal GOAL; prints the proposal
"$DAGQ" candidates
```

`add` makes a `draft`, never claimed; `submit` puts drafts in one proposal for plan review, whose `pass` makes them `ready`, `revise` returns them to `draft` to fix and `submit --proposal ID` again, `concern` asks the person. Only plan review readies a task (`ready --bypass-review` only on a person's explicit word). `--priority` (default `normal`) orders claiming. Proposals, withdrawing, editing, dependencies and priority levels: `reference/register.md`.

## 3. Inspect

`goal list`, `goal show ID`, `list` (unfinished tasks, paged by `--before NEXT`), `show ID`, `graph [--goal ID]` (waits, `critical`, claim order), `search` / `related`, `findings`, `events` (`--full`, filters), `timeline RUN` (where a run's time went; a run's `requested_provider`, `actual_provider`, `worker_mode` and `provider_switched` also in `show`), `observe --history`, `notes` / `note`, `stats` (time per run, goal and session kind, `alerts`), `kpi` / `mark` / `marks` / `forecast` / `report` (`reference/kpi.md`). `show`, `goal show` and `doctor` cut long texts; `--full` gives them whole. Flags, fields and statuses: `reference/inspect.md`.

## 4. Report results

Judge completion only from `show`: the run's `status`, `result_commit`, `last_error`, and the `validation_finished` event. A Stop hook, an idle session or a receipt file is not success. Summarize: task status, latest run status, branch and commit, and the next step.

A goal is closed once, by the planner, after every task is `completed` or `canceled`, the drafts from receipts' `follow_ups` are decided with the user, and the receipts' `summary` meets the goal's acceptance (gaps become new tasks on it first). Read `reference/goal-close.md` before running `goal close`.

## 5. Language

When your prompt, a request the runtime sends you, or the status the SessionStart hook prints (`language.instruction`) names a language, write everything you address to people in it: replies, ask questions and option descriptions, goal and task titles, descriptions and context, notes, findings, verdict reasons, and receipt summaries and follow_ups (the landing commit message is built from the task title and the receipt summary). Keep code, identifiers, CLI flags, ask option values and quoted runtime output as they are. When none is named, follow the conversation and the repository's rules. The language is `[language] tag` of the repository's `dagq.toml` over the user's `$XDG_CONFIG_HOME/dagq/config.toml`; `"$DAGQ" doctor` shows it (`language`) and where it came from.
