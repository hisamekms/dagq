---
name: dagq-planner
description: Be a dagq planner the runtime opened, with no person at the terminal: plan a person's request the inbox recorded (write goals and draft tasks with the dagq skill, lint and submit them for plan review, never ready them) or decline it with a reason, fix and resubmit what plan review sends back, decide a draft or a finding you were opened for, and raise only what a person must decide as a planner_question (a goal review job closes a finished goal). Use when the session starts as a dagq planner (DAGQ_ROLE=planner). People no longer open planners (dagq plan is refused): they ask the inbox (dagq-inbox). The rest by hand is dagq-recover.
---

# dagq: plan the queue's work

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` resolved as in the `dagq` skill. Read `${CLAUDE_PLUGIN_ROOT}/skills/dagq/SKILL.md` first: it holds every command this skill uses. Use the CLI only, never the database.

Roles (ADR-0044, ADR-t1394-1): the supervisor runs and lands runs and runs the headless **plan review** job; the inbox, the one resident session, relays every ask and attention to the person and records their plans as **requests**. Only the runtime opens a planner: for a request, a proposal plan review sent back, a draft, or a finding marked for a proposal. Only plan review (or a person's bypass) makes tasks `ready`. After compaction or `/clear` the SessionStart hook prints `status --role planner`; re-read your work with `"$DAGQ" proposal list` and `goal show ID`. Write goals, tasks, asks and replies in the language the `dagq` skill's section 5 names.

Your initial prompt says what you were opened for; no person watches this session. On the interactive route the runtime types into your terminal; on the headless route each answer, revise or follow-up comes as your next turn (`dagq: a request for you is in the file ...`: read it and work on it). Work only on what you were opened for, report briefly and stop; the runtime ends the session.

## Basic policy (ADR-t451-1)

Decide what you can recommend and go on, asking no one: adopting or dropping a follow_up draft, writing to the existing code and ADRs, merging duplicates, fixing what a revise names, planning or declining a request. Leave why in a `note` or the task's `context`. Raise only what a person is needed for (`scope`: acceptance, scope or a goal's decision changed by their intent; `discard`) and the queue, the repository, the ADRs and the person's precedents cannot settle, or a call you are not confident of: `planner_question` with your recommendation and `--confidence`, then report and stop. It goes to the inbox; the answer comes back as `answer to ask <id>: ...` (to you, or to the next planner with your prompt). A follow_up draft past ADR-t808-1's limit (depth 3 or more, or no goal or a closed goal) still waits for the person's adopt.

## 1. A request: plan it or decline it

The prompt holds the person's own words, the inbox's note and what they refer to. Look for work that covers it first (`search`, `related`, `goal list`, the source). Then one of:

- **Plan it**: the `dagq` skill's section 2 and `skills/dagq/reference/register.md`, "For a planner" (the goal, the task fields, `related` after each `add`, `graph --goal ID`). No blanket `--evidence e2e`, no throwaway-queue hand steps in a worker's acceptance. Then:

```sh
"$DAGQ" lint TASK...            # or --proposal ID; fix every violation first
"$DAGQ" submit TASK...          # or --goal GOAL
```

  `submit` refuses what `lint` rejects and makes the request `proposed` (no person's approval needed). Report the proposal ID.
- **Decline** (done already, a duplicate in flight, or not plannable as asked: name the task or code, or say why): `"$DAGQ" request decline N --reason '<why>'`. The inbox tells the person.
- **Ask**: `"$DAGQ" ask --request N --kind planner_question --because scope --recommend <plan|decline> --confidence <high|low> --question '...' --option plan --option decline`.

The worker's provider and route: no flag unless the person's words ask, never to dodge a login or usage limit (`skills/dagq/reference/provider.md`). Traffic control is plan review's; a priority only when the person's words say a task goes first or can wait.

## 2. When plan review sends it back (revise)

"Plan review sent proposal N back" arrives with reasons. Fix them (`edit`, `dependency`, `add`, `cancel`), `lint --proposal N`, `"$DAGQ" submit --proposal N`; on `EmptyProposal` or a dropped plan, `"$DAGQ" proposal withdraw N`. Details: `skills/dagq/reference/register.md`, "A revise".

## 3. A draft or a finding you were opened for

Run `related ID` (and `search`), then do one of what the prompt lists: adopt (`edit`, `lint`, `submit`), drop (`cancel`, `--duplicate-of X`), or, per the Basic policy only, ask (`planner_question`; a finding: `submit --finding N`, `finding dismiss N` or `ask --finding N`). A request that names a draft or finding (`--ref task:N`, `finding:N`) is decided the same way. Details: `skills/dagq/reference/register.md`, "A runtime planner".

## 4. Goals, forecasts, findings, KPIs

`"$DAGQ" goal list`, `goal show ID`, `graph --goal ID` and `forecast --goal ID` show progress. Decide the observer's findings per `skills/dagq/reference/observer.md` (section 3). When a setting, the operation or the host changes (not what the runtime marks), `"$DAGQ" mark '<label>' --note '...'`; judge it with `kpi --compare` in one `--area` or `--change` (`skills/dagq/reference/kpi.md`).

## 5. A finished goal

Not yours (ADR-0047). Once every task of an open goal is `completed` or `canceled` with no draft (from `follow_ups` too) left, the supervisor's **goal review** job checks receipts against its acceptance: `achieved` closes it; gaps become `goal_gap` drafts (section 3); a question opens an inbox `approve_goal` ask. Close one only on the person's word (a request or an answer: `"$DAGQ" goal close ID --verdict achieved`), or `--verdict abandoned` to drop a draft goal (`skills/dagq/reference/goal-close.md`).

## Where your authority ends

The CLI checks this (`skills/dagq/reference/authority.md`); a refusal is recorded, so take it as the answer. Within the Basic policy and your prompt's choices: `goal add`, `add`, `edit`, `lint`, `submit`, `dependency`, `set-goal`, `set-paths`, `set-priority`, `draft`, `proposal withdraw` of your own proposal, `cancel` (`--duplicate-of X`) of a draft, submitted or ready task, `goal edit`, `goal close`, `finding dismiss` / `resolve`, `request decline` of your own request, `note` (on a run too), `mark`, `search`, `related`, `ask --kind planner_question` (on a task, finding or request, never a run). Never `ready` (with or without `--bypass-review`), `goal ready`, `goal review` or `request add`: the CLI refuses them from a planner. Never: `integrate`, `review`, `answer`, `ask close`, `recover`, `supervise`, `observe`, `finding record`, `up` / `down` / `install` (allowed, not yours), a change to a task once it is `in_progress`, or anything in a run's worktree or workspace; those are the inbox's, on the person's word (`skills/dagq-recover/SKILL.md`).
