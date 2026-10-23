# A plan the person asks for: hand it to a runtime planner

Read this when the person asks the inbox for new work or a change of plan (a goal, tasks, a fix of something they saw, "plan this", "drop that goal"), and when `report the request's proposal` or `rephrase or drop the request` reaches you. People open no planners (`dagq plan` is refused), and the inbox plans nothing (no `add`, `goal add`, `edit` or `submit`): it records the person's words as a **planning request**, and the supervisor opens a runtime planner (the `dagq-planner` skill) that plans it or declines it, asking only what a person must decide.

## Record the request

On the person's word (flags: `"$DAGQ" request add --help`):

```sh
"$DAGQ" request add --text '<the person's own words>' [--note '<what you add>'] [--ref task:N ...] [--priority LEVEL]
```

- `--text` holds the person's own words, never your summary. `--note` is what you add (what you read in `status`, an ask's context), kept apart from their words.
- `--priority` only when the person's words give one; do not infer one from the subject. Words that name different priorities for different parts: leave it out and, once the plan is in, set each on the person's word (`goal edit ID --priority`, `set-priority`).
- `--ref` names what the words refer to; the planner reads each. A `request a plan for …` attention names the draft, finding, goal or task to refer to.

Long or multi-line words, or words with quotes, go through a file or stdin (a quoted heredoc keeps them as written):

```sh
"$DAGQ" request add --text - --ref goal:N <<'WORDS'
<the person's own words, as many lines as they wrote>
WORDS
```

Tell the person the request's ID. The person may type the same command in a terminal without `DAGQ_ROLE` (recorded as theirs).

## What the runtime does with it

A runtime planner takes each open request while a supervisor runs (none without one: `up`, `dagq-recover` section 5). It plans it (`proposed`, and plan review checks the proposal), declines it with a reason, or asks a `planner_question`; a request no planner could decide ends `exhausted`. Follow them with `"$DAGQ" requests` (`requests --help`); `dagq planners` lists the open planners.

The runtime records a request itself when a Spike task (an investigation of a premise) completes: no person asked, and a new planner decides from its result whether to implement, investigate further, take an alternative or a smaller scope, or stop. Its proposal and its decline reach you as news only; tell the person when they follow that goal. Only `exhausted`, or a `planner_question` (another Spike past the goal's limit of Spike cycles among them), needs them. A `ready` Spike in `candidates`' `deferred` with `spike_limit` or `investigation_limit` is waiting for a running Spike or investigation to end, not stuck.

## Tell the person the outcome

`watch` brings each outcome; `status` does not list them.

- `report the request's proposal`: tell the person the request and the proposal (`"$DAGQ" proposal show ID`, its tasks with `show`). Nothing else waits on them.
- `rephrase or drop the request` (declined or exhausted): show the person the reason. On their word record a new request in other words, naming the old one's references again; or leave it.

## A planner_question about a request

Options `plan` / `decline`, with the planner's `recommendation` and `confidence`. Show it as every ask (the skill's step 3) and write the person's answer as given. The runtime delivers the answer to the planner, or to a new one; do not carry it out yourself.

## More words for a planner still at work

A follow-up to an open runtime planner goes as its next turn, in the same forms as `request add` (`"$DAGQ" planner request --help`):

```sh
"$DAGQ" planner request <planner id> --text - <<'WORDS'
<the person's own words>
WORDS
```

The planner id is in `dagq planners` or the request's planners. It is refused for a planner that is closed or asked to exit; then, or for a new plan, record a new request.
